//! The ingest pipeline: what used to be the body of `poll_loop`.
//!
//! That function was 250 lines doing, per poll: enrich Slack links → queue a browser
//! investigation → queue issue triage → insert → attribute → notify → collect Slack
//! messages to classify → spawn re-analysis → spawn root-cause investigation → spawn
//! handled-subject triage → reconcile the upstream snapshot → repair orphans → push
//! the board, four times. Everything was interleaved, so nothing could be retried
//! independently and a failure anywhere lost the rest.
//!
//! Here it is a set of ordinary async functions with one job each. The [`Watcher`
//! object](super::objects::watcher) calls them as separate `ctx.run` steps, so a
//! rate limit during enrichment doesn't re-insert the signals that already landed.
//!
//! [`Watcher`]: super::objects::watcher::Watcher

use anyhow::Result;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::browser::BrowserDriver;
use crate::context::ContextManager;
use crate::correlation::Analyst;
use crate::event::Event;
use crate::signal::{Signal, SignalKind, Source};
use crate::store::Store;
use crate::subject::Attributor;
use crate::watchers::{PollBatch, Watcher};

/// Everything the pipeline needs. Held by the `Watcher` object.
pub struct IngestOps {
    pub store: Arc<Store>,
    pub attributor: Arc<Attributor>,
    pub analyst: Arc<Analyst>,
    pub context: Arc<ContextManager>,
    pub browser: Arc<BrowserDriver>,
    pub events: broadcast::Sender<Event>,
    /// The watchers themselves, by name. They live in the daemon because they hold
    /// the HTTP clients and the tokens; the object addresses them by name.
    pub watchers: Vec<Arc<dyn Watcher>>,
    /// Submits workflows — used by the scheduler object's ticks.
    pub ingress: Arc<crate::restate::ingress::Ingress>,
    /// The repo index, for the push sweep. The scheduler's `commit-poll` tick asks it what
    /// moved; the full crawl still goes through the `RepoIndex` workflow.
    pub repos: Arc<crate::repos::RepoIndex>,
    /// For the push sweep's pull-request check. `None` when no token is stored, which makes
    /// the check a no-op rather than an error — the sweep's commit half still works.
    pub github: Option<crate::github::GithubClient>,
    /// The org the repo index crawls, and where the managed contexts tree lives.
    pub org: String,
    pub contexts_dir: std::path::PathBuf,
}

/// What one poll produced, in a form the object can journal.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PollOutcome {
    /// Newly-inserted signals, as `(signal_id, subject_key)`, where the key is where
    /// the **pure resolver** says the signal belongs. Empty key → the unattributed
    /// lane.
    ///
    /// Routing information only. The *write* happens in the subject's own exclusive
    /// handler, which is the whole point: this poll runs inside the `Watcher` object,
    /// serialized per watcher, and two watchers can carry activity about one issue.
    pub routed: Vec<(String, String)>,
    /// Slack messages to classify per-message, off the poll path.
    pub to_classify: Vec<String>,
    pub new_count: usize,
    pub refreshed: usize,
    pub resolved: usize,
    /// The watcher's resume point after this poll, to be stored in object state.
    pub cursor: Option<String>,
}

impl IngestOps {
    pub fn org(&self) -> &str {
        &self.org
    }

    /// Whether the org's repo list has been fully enumerated.
    ///
    /// Compares the rows in `repo_index` against the count the last **completed** crawl saw
    /// from GitHub (see [`crate::repos::ENUMERATED_KEY`]). Not a heuristic on the row count:
    /// "2 repos" is indistinguishable from a complete two-repo org without knowing what
    /// GitHub said.
    ///
    /// A missing marker means no crawl has ever finished — including the case where none
    /// could, because no GitHub token was stored. That deliberately reports incomplete, so
    /// storing a token is picked up on the next catch-up tick rather than a day later.
    pub fn repo_index_looks_complete(&self) -> bool {
        repo_index_looks_complete(&self.store)
    }

    /// Context sources due a refresh, as `(id, version)` where the version is the
    /// ETag or mtime. The version is what makes an unchanged source a free
    /// submission rather than a re-summarize.
    pub fn context_sources_due(&self) -> Vec<(String, String)> {
        let default = "6h";
        let now = chrono::Utc::now();
        self.store
            .list_context()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.is_due(now, default))
            .map(|c| {
                let version = c
                    .etag
                    .clone()
                    .or_else(|| c.last_modified.clone())
                    .or_else(|| c.mtime.clone())
                    .unwrap_or_else(|| "initial".into());
                (c.id, version)
            })
            .collect()
    }

    /// Every repo in the code-derived index, for arming an indexer each.
    /// Which repositories have been pushed to since the last sweep. See
    /// [`crate::repos::RepoIndex::poll_pushed`].
    pub async fn poll_pushed_repos(&self) -> anyhow::Result<Vec<String>> {
        self.repos.poll_pushed().await
    }

    /// Re-evaluate the open pull requests in `repo` whose head commit has moved since the
    /// diff we hold was read. Returns how many were dispatched.
    ///
    /// **One API call per pushed repo**, not per pull request: `open_pulls` returns every
    /// open PR with its head sha, so the comparison is free once the list is in hand. Only
    /// pull requests the board actually tracks are considered — the operator's, not the
    /// org's, which on a busy repo is the difference between a handful and a hundred.
    ///
    /// A diff with no recorded sha counts as stale. Those predate the field, and treating
    /// "unknown" as current would leave every one of them frozen at whatever commit it was
    /// first read at — which is the bug this exists to fix, preserved forever in the rows
    /// that already had it.
    pub async fn restale_pull_requests(&self, repo: &str) -> u64 {
        let Some(gh) = self.github.as_ref() else {
            return 0;
        };
        let tracked: Vec<i64> = self
            .store
            .list_subjects()
            .unwrap_or_default()
            .into_iter()
            // Same filter the board's active view uses: work the operator has resolved or
            // snoozed is not work a push should drag back into an AI pass.
            .filter(|s| {
                !matches!(
                    s.handled,
                    crate::subject::Handled::Resolved | crate::subject::Handled::Snoozed
                )
            })
            .filter_map(|s| crate::prdiff::parse_pr_key(s.key.as_str()))
            .filter(|(r, _)| r == repo)
            .map(|(_, n)| n)
            .collect();
        if tracked.is_empty() {
            return 0;
        }
        let pulls = match gh.open_pulls(repo, 100).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("commit poll: open pulls for {repo} unavailable: {e:#}");
                return 0;
            }
        };
        let mut dispatched = 0;
        for pull in pulls {
            let number = pull.number as i64;
            if !tracked.contains(&number) {
                continue;
            }
            let Some(head) = pull.head_sha.as_deref() else {
                continue;
            };
            let stored = crate::prdiff::stored(&self.ingress, repo, number)
                .await
                .ok()
                .flatten();
            // No stored diff at all means nobody has looked yet — the pane offers that read
            // as a button, and doing it here would diff every PR on the board unasked.
            let Some(stored) = stored else { continue };
            if crate::prdiff::diff_is_current(stored.report.head_sha.as_deref(), head) {
                continue;
            }
            // Keyed by the sha, so this is a refused redo if the same push is seen twice and
            // real work exactly once per commit — the same property `PrCritique` gets from
            // keying on the head sha rather than on activity.
            let key = format!("{}@{head}", crate::prdiff::pr_key(repo, number));
            match self
                .ingress
                .submit_workflow(
                    "PrDiff",
                    &key,
                    Some(crate::restate::workflows::rest::PrDiff::SCOPE),
                )
                .await
            {
                Ok(true) => {
                    tracing::info!(
                        "commit poll: {repo}#{number} was pushed to ({} → {head}) — re-diffing",
                        stored.report.head_sha.as_deref().unwrap_or("unknown"),
                    );
                    dispatched += 1;
                }
                Ok(false) => {}
                Err(e) => tracing::debug!("commit poll: {repo}#{number} not re-diffed ({e:#})"),
            }
        }
        dispatched
    }

    pub fn indexed_repos(&self) -> Vec<String> {
        self.store
            .list_repos()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !r.archived)
            .map(|r| r.full_name)
            .collect()
    }

    /// Walk the managed contexts tree and register/reload what changed.
    pub async fn sync_contexts_dir(&self) -> Result<usize> {
        let n = self.context.sync_dir(&self.contexts_dir).await?;
        let filled = self.context.backfill_tag_summaries().await;
        if filled > 0 {
            debug!("tags: backfilled {filled} summary(ies)");
        }
        Ok(n)
    }

    /// Investigations waiting for the browser. The workflow id is the claim now, so
    /// this is a plain read rather than a claim-a-row transaction.
    pub fn pending_browser_investigations(&self) -> Vec<String> {
        self.store
            .list_browser_investigations_pending()
            .unwrap_or_default()
    }

    /// Assigned issues waiting for triage, as `(issue_key, version)`. The version is the
    /// head sha when known — so unchanged code is a free submission — and otherwise the
    /// queue row's timestamp, which changes when the operator asks again.
    ///
    /// Includes `running`, matching the browser queue. Under the old worker a `running`
    /// row meant "claimed", and a worker that died left it claimed forever — which is why
    /// there used to be a requeue-at-startup step. Now the workflow id is the claim:
    /// Restate resumes an interrupted invocation by itself, and re-submitting a key that
    /// is genuinely still running is refused for free. The only case that needs the
    /// re-submission is a wiped cluster, where the invocation is gone and the row is all
    /// that is left — and wiping is exactly what enabling vqueues requires.
    pub fn pending_triage(&self) -> Vec<(String, String)> {
        self.store
            .list_issue_triage()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.status == "pending" || t.status == "running")
            .map(|t| {
                let version = t
                    .head_sha
                    .clone()
                    .unwrap_or_else(|| format!("queued-{}", t.updated_at));
                (t.issue_key, version)
            })
            .collect()
    }

    pub fn watcher(&self, name: &str) -> Option<Arc<dyn Watcher>> {
        self.watchers.iter().find(|w| w.name() == name).cloned()
    }

    /// One poll, all the way to attributed signals. Does **not** analyze — that is
    /// the subject's debounced pass, and keeping it out of here is what stops a busy
    /// poll from blocking on a model.
    pub async fn poll_once(&self, name: &str, cursor: Option<&str>) -> Result<PollOutcome> {
        let Some(watcher) = self.watcher(name) else {
            anyhow::bail!("no watcher named '{name}'");
        };
        if let Some(cursor) = cursor {
            watcher.restore_cursor(cursor);
        }
        let batch = watcher.poll().await?;
        let mut outcome = self.absorb(name, batch).await?;
        outcome.cursor = watcher.cursor();
        let _ = self
            .store
            .record_health(name, true, None, outcome.cursor.as_deref());
        let _ = self.events.send(Event::Health(
            self.store.source_health().unwrap_or_default(),
        ));
        Ok(outcome)
    }

    /// Record a failed poll so the UI shows the source as unhealthy rather than
    /// silently stale.
    pub fn record_failure(&self, name: &str, error: &str) {
        let _ = self.store.record_health(name, false, Some(error), None);
        let _ = self.events.send(Event::Health(
            self.store.source_health().unwrap_or_default(),
        ));
    }

    async fn absorb(&self, name: &str, batch: PollBatch) -> Result<PollOutcome> {
        let mut out = PollOutcome::default();
        for sig in batch.signals {
            let mut sig = sig;
            // Enrich a linked-out Slack message with a summary of the (public) page
            // it points at, before storing.
            crate::enrich::slack_links(&mut sig, &self.context).await;
            crate::enrich::queue_dashboard_investigation(&mut sig, &self.store, &self.browser);
            // Queue triage for an assigned issue *before* the insert-dedup check:
            // the signal is re-emitted every poll and only inserts once, but a
            // triage that previously failed (or whose code has moved on) still
            // deserves another run, and the queue decides that for itself.
            crate::enrich::queue_issue_triage(&sig, &self.store);

            match self.store.insert_signal(&sig) {
                Ok(true) => {
                    out.new_count += 1;
                    // Resolve, don't attach. Attribution is a *write* to the subject —
                    // its links, counters and debounce state — and doing it here would
                    // put that write inside the `Watcher` object's handler, which is
                    // exclusive per watcher rather than per subject. Two watchers
                    // carrying activity about one issue would then interleave on it,
                    // which is exactly the race the subject objects exist to remove.
                    // The resolver is pure, so calling it here is free and tells us
                    // only where to send the record.
                    let routed = crate::subject::resolve::attribute(&sig.keys)
                        .subject
                        .map(|k| k.into_string())
                        .unwrap_or_default();
                    if sig.source == Source::Slack && !is_env_alert(&sig) {
                        out.to_classify.push(sig.id.clone());
                    }
                    let _ = self.events.send(Event::Signal(sig.clone()));
                    out.routed.push((sig.id.clone(), routed));
                }
                // The watcher refreshed source-provided context on an existing
                // signal (a CI log excerpt, say). Worth re-broadcasting the board so
                // open detail panels update, but not a new event.
                Ok(false) => out.refreshed += 1,
                Err(e) => warn!("store insert failed: {e:#}"),
            }
        }

        if let Some(snapshot) = batch.snapshot {
            out.resolved = self.reconcile(name, &snapshot.active_ids)?;
        }
        Ok(out)
    }

    /// Reconcile against a complete upstream listing: anything absent is gone
    /// upstream. Each GitHub watcher is authoritative only for its own listing.
    fn reconcile(&self, name: &str, active_ids: &BTreeSet<String>) -> Result<usize> {
        // Compared against the watcher's own constant, not a literal: the name is also the
        // health key and the UI's pill id, and three hand-written copies of it is three
        // chances for a rename to leave one behind — which is precisely what happened.
        let gone = if name == crate::watchers::incident::NAME {
            self.store.resolve_missing_incidents(active_ids)?
        } else if name == "github-assigned" {
            self.store.resolve_missing_assigned_issues(active_ids)?
        } else {
            self.store
                .resolve_missing_github_notifications(active_ids)?
        };
        for sig in &gone {
            if let Some(key) = &sig.subject {
                self.attributor.refresh_subject_metadata(key)?;
            }
            let _ = self.events.send(Event::Signal(sig.clone()));
        }
        Ok(gone.len())
    }

    /// Classify each new Slack message into the tag vocabulary. Off the poll path so
    /// per-message model calls never stall ingest, and skipped entirely while the
    /// vocabulary is empty — there is nothing to route to yet.
    pub async fn classify(&self, signal_ids: &[String]) -> Result<usize> {
        let vocab_ready = self
            .store
            .list_tags()
            .map(|t| !t.is_empty())
            .unwrap_or(false);
        if !vocab_ready || signal_ids.is_empty() {
            return Ok(0);
        }
        let mut done = 0;
        for id in signal_ids {
            let Some(sig) = self.store.get_signal(id)? else {
                continue;
            };
            let text = format!("{} {}", sig.title, sig.body.as_deref().unwrap_or(""));
            let tags = self.analyst.classify_text(&text).await;
            if tags.is_empty() {
                continue;
            }
            self.store.set_signal_tags(id, &tags)?;
            if let Ok(Some(sig)) = self.store.get_signal(id) {
                let _ = self.events.send(Event::Signal(sig));
            }
            done += 1;
        }
        Ok(done)
    }

    /// Does this subject look broken enough to be worth a root-cause investigation?
    ///
    /// The gate is deliberately narrow: investigation is the most expensive thing
    /// MuggleBot does (GitHub search, a commit-log scan, several local passes, one
    /// cloud call), so firing it on "Ben mentioned you in #eng" would rate-limit the
    /// search API to no purpose.
    pub fn worth_investigating(&self, subject_key: &str) -> bool {
        let Ok(Some(view)) = self.attributor.subject_view(subject_key) else {
            return false;
        };
        if view.subject.handled.is_handled() {
            return false;
        }
        let looks_broken = view.severity >= crate::signal::Severity::Warning
            || view
                .signals
                .iter()
                .any(|s| matches!(s.kind, SignalKind::Alert | SignalKind::CiFailure));
        if !looks_broken {
            return false;
        }
        // Don't redo work. A failed report is worth retrying (the failure was often a
        // rate limit); a complete one is not, until the operator asks.
        match self.store.get_root_cause(subject_key) {
            Ok(Some(report)) if report.status != "failed" => false,
            Ok(_) => true,
            Err(e) => {
                debug!("root-cause lookup for {subject_key} failed: {e:#}");
                false
            }
        }
    }

    /// The newest attributed signal id, which versions the `RootCause` workflow key:
    /// nothing new has arrived → same key → nothing to re-investigate.
    pub fn watermark(&self, subject_key: &str) -> String {
        self.store
            .signals_for_subject(subject_key)
            .ok()
            .and_then(|sigs| sigs.last().map(|s| s.id.clone()))
            .unwrap_or_else(|| "empty".into())
    }

    pub fn push_board(&self) {
        if let Ok(views) = self.attributor.subject_views(true) {
            let _ = self.events.send(Event::Board(views));
        }
    }

    /// Re-create metadata for any subject whose row was hand-deleted while its
    /// signals remain.
    pub fn repair(&self) -> usize {
        match self.attributor.repair_orphaned_subjects() {
            Ok(n) => n,
            Err(e) => {
                warn!("subject metadata repair failed: {e:#}");
                0
            }
        }
    }
}

/// An environment alert is already routed by its environment key, so it bypasses the
/// fuzzy tag classifier entirely.
fn is_env_alert(sig: &Signal) -> bool {
    sig.keys.iter().any(|k| k.kind == "environment")
}

/// See [`IngestOps::repo_index_looks_complete`]. A free function over the store so the
/// decision can be tested without assembling a daemon.
pub fn repo_index_looks_complete(store: &Store) -> bool {
    let Some(expected) = meta_count(store, crate::repos::ENUMERATED_KEY) else {
        // No crawl has ever finished — including "couldn't, no GitHub token was stored".
        return false;
    };
    if store.list_repos().map(|r| r.len()).unwrap_or(0) < expected {
        return false;
    }
    // Every repo has a row, but the cards are written a bounded batch per crawl, so the
    // enumeration finishing is not the work finishing. Without this the cadence would drop to
    // daily with 142 of 147 repos still uncarded.
    meta_count(store, crate::repos::PENDING_KEY) == Some(0)
}

fn meta_count(store: &Store, key: &str) -> Option<usize> {
    let raw = store.meta_get(key).ok()??;
    String::from_utf8(raw).ok()?.trim().parse::<usize>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RepoEntry;

    fn repo(store: &Store, name: &str) {
        store
            .put_repo(
                &RepoEntry {
                    full_name: name.into(),
                    description: None,
                    topics: vec![],
                    language: None,
                    archived: false,
                    pushed_at: None,
                    readme_etag: None,
                    readme: None,
                    summary: None,
                    indexed_sha: None,
                    digest: None,
                    kind: None,
                    kind_pinned: false,
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                },
                false,
            )
            .unwrap();
    }

    /// The exact state observed live: 2 repos of 164, and nothing scheduled to fix it for 24
    /// hours. The repo list is the input to component carding, commit summaries, the
    /// dependency graph and scoring, so an incomplete crawl starves all of them — the
    /// scheduler has to be able to tell it is incomplete in order to catch up.
    #[test]
    fn an_interrupted_crawl_reads_as_incomplete() {
        let store = Store::open_in_memory().unwrap();

        // No crawl has ever finished — including "couldn't, no GitHub token stored". Reported
        // incomplete on purpose: storing a token should be picked up on the next catch-up
        // tick, not a day later.
        assert!(!repo_index_looks_complete(&store));

        // A crawl that enumerated 164 and wrote 2 before being interrupted.
        repo(&store, "o/a");
        repo(&store, "o/b");
        store
            .meta_put(crate::repos::ENUMERATED_KEY, b"164")
            .unwrap();
        assert!(
            !repo_index_looks_complete(&store),
            "2 rows against 164 enumerated must not read as done"
        );
    }

    #[test]
    fn a_finished_crawl_reads_as_complete() {
        let store = Store::open_in_memory().unwrap();
        repo(&store, "o/a");
        repo(&store, "o/b");
        store.meta_put(crate::repos::ENUMERATED_KEY, b"2").unwrap();
        store.meta_put(crate::repos::PENDING_KEY, b"0").unwrap();
        // A genuinely two-repo org is complete at two rows, and is indistinguishable from the
        // interrupted case above without the count from GitHub — which is why the marker
        // holds a number rather than a flag.
        assert!(repo_index_looks_complete(&store));
    }

    /// Enumeration finishing is not the work finishing: rows are written for every repo on the
    /// first pass, cards a bounded batch at a time. Treating a full row count as done drops
    /// the cadence to daily with almost every repo still uncarded.
    #[test]
    fn every_row_present_but_cards_outstanding_is_not_complete() {
        let store = Store::open_in_memory().unwrap();
        repo(&store, "o/a");
        repo(&store, "o/b");
        store.meta_put(crate::repos::ENUMERATED_KEY, b"2").unwrap();
        store.meta_put(crate::repos::PENDING_KEY, b"142").unwrap();
        assert!(!repo_index_looks_complete(&store));

        store.meta_put(crate::repos::PENDING_KEY, b"0").unwrap();
        assert!(repo_index_looks_complete(&store));
    }

    /// A crawl old enough to predate the pending marker must not read as complete on the
    /// strength of its row count alone.
    #[test]
    fn a_missing_pending_marker_is_not_complete() {
        let store = Store::open_in_memory().unwrap();
        repo(&store, "o/a");
        store.meta_put(crate::repos::ENUMERATED_KEY, b"1").unwrap();
        assert!(!repo_index_looks_complete(&store));
    }

    #[test]
    fn a_corrupt_marker_fails_towards_catching_up() {
        let store = Store::open_in_memory().unwrap();
        repo(&store, "o/a");
        for junk in [&b""[..], b"many", b"-1", &[0xff, 0xfe][..]] {
            store.meta_put(crate::repos::ENUMERATED_KEY, junk).unwrap();
            assert!(
                !repo_index_looks_complete(&store),
                "an unreadable marker must mean catch up, not stop"
            );
        }
    }
}
