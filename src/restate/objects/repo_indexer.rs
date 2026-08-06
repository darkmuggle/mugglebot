//! The `RepoIndexer` virtual object — one per repo, owning that repo's indexing.
//!
//! Keyed by `owner/repo`. This is the *process*; the index itself is in SQLite (see the
//! `commit_summaries` / `component_summaries` / `repo_deps` tables and the note there on
//! why). The split matters here more than anywhere: the index is the expensive artifact —
//! thousands of local model calls — and `data/restate` is wiped whenever vqueues are
//! toggled.
//!
//! What the object gets us, in order of how much it matters:
//!
//! 1. **One indexer per repo at a time.** Two concurrent indexers would clone the same
//!    repo into the same directory. The `checkout` vqueue's per-repo limit key guards the
//!    clone; the object's per-key exclusivity guards the whole pass, including the writes.
//! 2. **Bounded batches on a durable timer.** A first index over a large org is hours of
//!    work. In batches, every tick leaves the index strictly more complete, and a restart
//!    resumes rather than starting over — which for a one-time cost is the difference
//!    between paying it once and paying it every time the daemon bounces.
//! 3. **A resume cursor that is not a guess.** Progress lives in SQL (which shas have
//!    summaries), so the object holds only the cadence and the tick count. Nothing to
//!    reconcile.
//!
//! Cadence adapts: while a repo is still being indexed it ticks fast to burn through the
//! backlog, and once complete it drops to a slow refresh that only picks up new commits.

use std::sync::Arc;
use std::time::Duration;

use restate_sdk::prelude::*;

use crate::codeindex::CodeIndexer;

const TICKS: &str = "ticks";
const NEXT_TICK_AT: &str = "next_tick_at";
const COMPLETE: &str = "complete";

/// Progress, written into this object's own state after every batch.
///
/// The object is the authority on how far it has got, and the board reads these across all
/// keys with one SQL query over Restate's `state` table (see [`crate::restate::state`]). Before
/// this, the panel re-derived the same numbers by counting rows in SQLite — two accounts of one
/// fact, which can disagree, and did: the object could hold `complete = true` while the panel
/// was still adding up partial counts.
const COMPONENTS: &str = "components";
const COMMITS_CACHED: &str = "commits_cached";
const COMMITS_SUMMARIZED: &str = "commits_summarized";
const DEP_EDGES: &str = "dep_edges";
/// How far back the history walk has reached, RFC3339. Absent means it hasn't started, which
/// is a different state from "no commits to do" and reads identically without it.
const HISTORY_BACK_TO: &str = "history_back_to";

/// While there is a backlog. Short, so a first index makes visible progress.
const CATCHUP: Duration = Duration::from_secs(30);
/// Once caught up: only new commits arrive, and `RepoIndex` refreshes the repo cards on
/// its own schedule anyway.
const STEADY: Duration = Duration::from_secs(3_600);

pub struct RepoIndexer {
    indexer: Arc<CodeIndexer>,
    /// Every repo we index, for resolving dependency edges. An edge to a repo we don't
    /// have is true and useless — nothing can look inside it.
    repos: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// Pushes progress to the board after each batch, so the panel updates when a card lands
    /// rather than when a poll happens to fire.
    events: tokio::sync::broadcast::Sender<crate::event::Event>,
}

impl RepoIndexer {
    pub fn new(
        indexer: Arc<CodeIndexer>,
        repos: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        events: tokio::sync::broadcast::Sender<crate::event::Event>,
    ) -> Self {
        Self {
            indexer,
            repos,
            events,
        }
    }
}

#[restate_sdk::object]
impl RepoIndexer {
    /// Arm this repo's indexing loop. Idempotent, with the same durable-timer caution as
    /// the other loops: the timer survives a restart, so re-arming on every boot would
    /// multiply the tick rate by the number of restarts.
    #[handler]
    async fn start(&self, ctx: ObjectContext<'_>) -> HandlerResult<bool> {
        let repo = ctx.key().to_string();
        if !self.indexer.enabled() {
            return Err(TerminalError::new(
                "code indexing needs a stored GitHub token and `git` on PATH",
            )
            .into());
        }
        let now = ctx.run(|| async { Ok(now_millis()) }).await?;
        let next: Option<u64> = ctx.get(NEXT_TICK_AT).await?;
        let stale = match next {
            None => true,
            Some(at) => now > at + 2 * STEADY.as_millis() as u64,
        };
        if !stale {
            return Ok(false);
        }
        tracing::info!("index {repo}: arming");
        ctx.set(NEXT_TICK_AT, now);
        ctx.object_client::<RepoIndexerClient>(repo).tick().send();
        Ok(true)
    }

    /// One bounded batch, then reschedule.
    #[handler]
    async fn tick(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let repo = ctx.key().to_string();

        // A repo that has left the index ends its own loop. `index_repo` refuses it, and that
        // refusal is swallowed as transient — correct for a clone timeout, wrong here, because
        // nothing a later tick does brings the repo back. Left alone the chain reschedules
        // itself forever, warning on every tick: observed on `restatedev/wip-agent`, renamed
        // upstream to `restatedev/agent`, whose new name the crawl picked up while the old
        // name's timer kept running against a row that no longer exists.
        //
        // Checked before the reschedule below, since afterwards the next timer is already
        // armed. Journalled so a replay takes the same branch as the original run.
        let indexer = self.indexer.clone();
        let known = {
            let repo = repo.clone();
            ctx.run(|| {
                let indexer = indexer.clone();
                let repo = repo.clone();
                async move { Ok(indexer.knows_repo(&repo)) }
            })
            .await?
        };
        if !known {
            // Info, not warn: this is the loop retiring cleanly. `start` re-arms it if the repo
            // returns — the stale-timer check there sees a `NEXT_TICK_AT` that never advanced.
            tracing::info!("index {repo}: no longer in the repo index, stopping this loop");
            return Ok(Json(serde_json::json!({ "repo": repo, "stopped": true })));
        }

        // Reschedule before the work, for the same reason the watcher does: every step
        // below ends in `?`, and an index that stops because one batch failed is an index
        // that silently never finishes. The send is journalled, so a retry replays it
        // rather than arming a second timer.
        let was_complete: bool = ctx.get(COMPLETE).await?.unwrap_or(false);
        let interval = if was_complete { STEADY } else { CATCHUP };
        let now = ctx.run(|| async { Ok(now_millis()) }).await?;
        ctx.set(NEXT_TICK_AT, now + interval.as_millis() as u64);
        ctx.object_client::<RepoIndexerClient>(repo.clone())
            .tick()
            .send_after(interval);
        self.index_once(ctx).await
    }

    /// One bounded batch, and **no** rescheduling — for the push sweep.
    ///
    /// Separate from `tick` because `tick` unconditionally arms the next timer, so calling it
    /// out of band forks the loop: the poked tick schedules its own successor alongside the
    /// chain that was already running, and every later poke adds another. Measured on the live
    /// board — one poke of `restatedev/restate` left three chains scheduled where there had
    /// been two, and an actively-pushed repo would keep multiplying its own tick rate.
    ///
    /// Leaving the timer alone is also the right semantics: a push is a reason to index *now*,
    /// not a reason to change how often this repo is indexed.
    #[handler]
    async fn poke(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        self.index_once(ctx).await
    }
}

impl RepoIndexer {
    /// The work half of a tick: index a batch, record what it achieved, announce it.
    async fn index_once(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let repo = ctx.key().to_string();
        let indexer = self.indexer.clone();
        let known = (self.repos)();
        let repo_for_run = repo.clone();
        let progress = ctx
            .run(|| {
                let indexer = indexer.clone();
                let known = known.clone();
                let repo = repo_for_run.clone();
                async move {
                    match indexer.index_repo(&repo, &known).await {
                        Ok(p) => Ok(Json(p)),
                        Err(e) => {
                            // A clone that timed out or a model that is down is transient;
                            // the next tick retries. Swallowed rather than failing the
                            // invocation so the loop keeps its cadence.
                            tracing::warn!("index {repo}: {e:#}");
                            Ok(Json(crate::codeindex::IndexProgress::default()))
                        }
                    }
                }
            })
            .await?
            .into_inner();

        let ticks: u64 = ctx.get(TICKS).await?.unwrap_or(0);
        ctx.set(TICKS, ticks + 1);
        ctx.set(COMPLETE, progress.complete());
        // The object records what it achieved. Everything the board shows for this repo now
        // comes from here rather than from a parallel count over SQLite.
        ctx.set(COMPONENTS, progress.components_total);
        ctx.set(COMMITS_CACHED, progress.commits_total);
        ctx.set(COMMITS_SUMMARIZED, progress.commits_done);
        ctx.set(DEP_EDGES, progress.dep_edges as u64);
        if let Some(back_to) = progress.history_back_to.clone() {
            ctx.set(HISTORY_BACK_TO, back_to);
        }
        // Pushed after the state write, so a client that reacts by re-reading sees the same
        // numbers the event carried. Send failures are ignored: an event nobody is listening
        // for is the normal case, and indexing must not depend on a UI being open.
        let _ = self
            .events
            .send(crate::event::Event::IndexProgress(Box::new(
                crate::event::IndexProgressEvent {
                    repo: repo.clone(),
                    components: progress.components_total,
                    commits_cached: progress.commits_total,
                    commits_summarized: progress.commits_done,
                    dep_edges: progress.dep_edges,
                    history_back_to: progress.history_back_to.clone(),
                    last_commit: progress.last_commit.clone(),
                    complete: progress.complete(),
                },
            )));
        Ok(Json(serde_json::json!({
            "repo": repo,
            "ticks": ticks + 1,
            "commits_done": progress.commits_done,
            "commits_total": progress.commits_total,
            "components": progress.components_total,
            "components_written": progress.components_written,
            "components_pending": progress.components_pending,
            "commits_fetched": progress.commits_fetched,
            "commits_summarized": progress.commits_summarized,
            "commits_skipped": progress.commits_skipped,
            "dep_edges": progress.dep_edges,
            "history_complete": progress.history_complete,
            "complete": progress.complete(),
        })))
    }

    /// Indexing progress, for the board. **Shared**, so reading it never queues behind a
    /// batch that is mid-model-call.
    #[handler]
    async fn status(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let repo = ctx.key().to_string();
        let (done, total) = self
            .indexer
            .store
            .commit_index_progress(&repo)
            .map_err(|e| TerminalError::new(format!("{e:#}")))?;
        Ok(Json(serde_json::json!({
            "repo": repo,
            "ticks": ctx.get::<u64>(TICKS).await?.unwrap_or(0),
            "complete": ctx.get::<bool>(COMPLETE).await?.unwrap_or(false),
            "commits_done": done,
            "commits_total": total,
            "components": self
                .indexer
                .store
                .components_for_repo(&repo)
                .map(|c| c.len())
                .unwrap_or(0),
        })))
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
