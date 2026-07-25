//! The correlation engine: deterministic grouping (Phase 1) and the view-building
//! helpers shared by the server API and MCP tools. The LLM relation-graph pass
//! (Phase 2) is added by [`super::llm`].

use anyhow::Result;
use chrono::Utc;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::{Attention, Decorations, Thread, ThreadView};
use crate::signal::{Entity, Severity, Signal, State};
use crate::store::Store;

pub struct Correlator {
    pub(crate) store: Arc<Store>,
    pub(crate) window: Duration,
}

impl Correlator {
    pub fn new(store: Arc<Store>, window: Duration) -> Self {
        Self { store, window }
    }

    /// Deterministically place a freshly-ingested signal into a thread: join the
    /// existing thread that shares the most entities within the correlation
    /// window, else start a new one. Returns the thread id. Idempotent — a signal
    /// that already carries a thread keeps it.
    pub fn ingest(&self, s: &Signal) -> Result<String> {
        if let Some(existing) = &s.thread {
            return Ok(existing.clone());
        }
        let keys = entity_keys(&s.entities);
        // Match strictly on the signal's *strongest* identity, so the hierarchy
        // (environment > issue > PR > branch) actually decides grouping rather
        // than being outvoted by a count of weaker shared entities. A signal
        // naming issue #412 belongs to #412's thread even if some branch it also
        // mentions happens to match elsewhere.
        let match_keys = controlling_keys(&keys);
        let matched = if match_keys.is_empty() {
            None
        } else {
            self.best_matching_thread(s, &match_keys)?
        };

        let now = Utc::now();
        let thread_id = match &matched {
            Some(id) => id.clone(),
            None => {
                let id = format!("thr/{}", crate::store::new_id());
                self.store.upsert_thread(&Thread {
                    id: id.clone(),
                    title: thread_title(s),
                    summary: None,
                    created_at: now,
                    updated_at: now,
                    last_reasoned_at: None,
                    live: false,
                    tags: Vec::new(),
                    tags_pinned: false,
                })?;
                id
            }
        };

        // Sticky snooze: a snoozed thread is muted, so new activity landing on it
        // stays hidden rather than reviving it — unless the user is personally
        // engaged in this new signal, which un-mutes the thread. Only applies when
        // joining an existing thread (computed over its members before this one).
        if matched.is_some() && !s.is_user_engaged() {
            let members = self.store.signals_for_thread(&thread_id)?;
            if aggregate_state(&members) == State::Snoozed {
                self.store.set_state(&s.id, State::Snoozed)?;
            }
        }

        self.store.set_signal_thread(&s.id, Some(&thread_id))?;
        self.refresh_thread_metadata(&thread_id)?;
        Ok(thread_id)
    }

    /// Find the existing thread whose members share the most entities with `s`,
    /// within the correlation window **of `s` itself**. Ties broken by recency.
    ///
    /// The window is measured around `s.occurred_at`, not around wall-clock now.
    /// Anchoring it to `now` silently breaks every catch-up ingest: on a first
    /// poll, a restart, or any backlog, signals arrive hours after they happened,
    /// so a `now - 30m` cutoff excludes *all* of them — every signal looks like it
    /// has no neighbours and each gets its own thread. That failure is invisible
    /// (no error, just fragmented threads) and is exactly what produces a board
    /// full of near-identical one-signal cards.
    fn best_matching_thread(&self, s: &Signal, keys: &BTreeSet<String>) -> Result<Option<String>> {
        let window = chrono::Duration::from_std(self.window).unwrap_or_default();
        let candidates = self.store.signals_since(s.occurred_at - window)?;
        let mut best: Option<(u8, usize, chrono::DateTime<Utc>, String)> = None;
        for c in candidates {
            if c.id == s.id {
                continue;
            }
            // Both directions: a backlog can deliver signals newest-first, so a
            // candidate may legitimately be *after* the one being placed.
            if (c.occurred_at - s.occurred_at).abs() > window {
                continue;
            }
            let Some(tid) = c.thread.clone() else {
                continue;
            };
            let candidate_keys = entity_keys(&c.entities);
            let shared = candidate_keys.intersection(keys).count();
            if shared == 0 {
                continue;
            }
            // Prefer a match on the strongest shared identity. Sharing an issue
            // outranks sharing a PR, which outranks sharing a branch — so a CI run
            // whose branch also appears on an unrelated thread still lands on the
            // issue's thread when both are candidates.
            let strength = candidate_keys
                .intersection(keys)
                .map(|k| identity_rank(k))
                .max()
                .unwrap_or(0);
            let better = match &best {
                None => true,
                Some((bstr, bs, bt, _)) => (strength, shared, c.occurred_at) > (*bstr, *bs, *bt),
            };
            if better {
                best = Some((strength, shared, c.occurred_at, tid));
            }
        }
        Ok(best.map(|(_, _, _, tid)| tid))
    }

    /// Recompute a thread's deterministic title/summary and bump `updated_at`,
    /// preserving any LLM summary already recorded (only fills a blank one).
    pub fn refresh_thread_metadata(&self, thread_id: &str) -> Result<()> {
        let signals = self.store.signals_for_thread(thread_id)?;
        if signals.is_empty() {
            self.store.delete_thread_if_empty(thread_id)?;
            return Ok(());
        }
        let Some(mut thread) = self.store.get_thread(thread_id)? else {
            return Ok(());
        };
        thread.updated_at = Utc::now();
        // Title: keep the earliest signal's title as the anchor.
        thread.title = thread_title(&signals[0]);
        if thread.summary.is_none() || thread.last_reasoned_at.is_none() {
            thread.summary = Some(deterministic_summary(&signals));
        }
        self.store.upsert_thread(&thread)?;
        Ok(())
    }

    /// Restore metadata for any signal group whose thread row was removed by a
    /// previously interrupted merge. Signal membership remains intact, so this is
    /// a lossless repair and makes the group visible to the board again.
    pub fn repair_orphaned_threads(&self) -> Result<usize> {
        let ids = self.store.orphaned_thread_ids()?;
        for id in &ids {
            let signals = self.store.signals_for_thread(id)?;
            let Some(first) = signals.first() else {
                continue;
            };
            let now = Utc::now();
            self.store.upsert_thread(&Thread {
                id: id.clone(),
                title: thread_title(first),
                summary: Some(deterministic_summary(&signals)),
                created_at: first.occurred_at,
                updated_at: now,
                last_reasoned_at: None,
                live: false,
                tags: Vec::new(),
                tags_pinned: false,
            })?;
        }
        Ok(ids.len())
    }

    // ---- views --------------------------------------------------------------

    pub fn thread_view(&self, thread_id: &str) -> Result<Option<ThreadView>> {
        let Some(thread) = self.store.get_thread(thread_id)? else {
            return Ok(None);
        };
        let signals = self.store.signals_for_thread(thread_id)?;
        let entities = union_entities(&signals);
        let severity = signals
            .iter()
            .map(|s| s.severity)
            .max()
            .unwrap_or(Severity::Info);
        let state = aggregate_state(&signals);
        let edges = self.store.edges_for_thread(thread_id)?;
        let context = self.store.thread_context(thread_id)?;
        let attention = self.attention(&thread, &signals, severity, state)?;
        Ok(Some(ThreadView {
            thread,
            signals,
            entities,
            severity,
            state,
            edges,
            context,
            attention,
        }))
    }

    /// Derive the two things the board actually reports: whether this needs the
    /// operator, and what the AI has made of it.
    ///
    /// Both are computed rather than stored. A stored "needs attention" flag drifts
    /// the moment a signal is acknowledged elsewhere, and a stored "AI done" flag
    /// lies after a failed pass — deriving them from the artifacts that actually
    /// exist means the badge can't disagree with the panel underneath it.
    fn attention(
        &self,
        thread: &Thread,
        signals: &[Signal],
        severity: Severity,
        state: State,
    ) -> Result<Attention> {
        let mut decorated = Decorations {
            // `last_reasoned_at` distinguishes a real grounded summary from the
            // deterministic one-liner every thread gets for free.
            summary: thread.last_reasoned_at.is_some(),
            tags: !thread.tags.is_empty(),
            mitigations: self
                .store
                .get_thread_mitigations(&thread.id)?
                .is_some_and(|m| m.as_array().is_some_and(|a| !a.is_empty())),
            dashboard: self
                .store
                .browser_investigations_for_thread(&thread.id)?
                .iter()
                .any(|i| i.findings.as_deref().is_some_and(|f| !f.trim().is_empty())),
            root_cause: self
                .store
                .get_root_cause(&thread.id)?
                .map(|r| r.status.clone()),
            ..Default::default()
        };
        let triage_rows = self.store.issue_triage_for_thread(&thread.id)?;
        decorated.triage = triage_rows.first().map(|t| t.status.clone());
        for row in &triage_rows {
            let judged = self.store.pr_fixes_for_issue(&row.issue_key)?;
            decorated.prs_judged += judged.len();
            // The tier that answered is recorded per judgment, so cost attribution
            // is real rather than assumed.
            for fix in &judged {
                match fix.analyzed_by.as_deref() {
                    Some("local") | None => decorated.local_passes += 1,
                    _ => decorated.cloud_passes += 1,
                }
            }
        }
        // Tag classification, triage, and root-cause searching are on-device by
        // policy; summaries and mitigations go through the routed tier.
        if decorated.tags {
            decorated.local_passes += 1;
        }
        if decorated.triage.as_deref() == Some("complete") {
            decorated.local_passes += 1;
        }
        if decorated.root_cause.as_deref() == Some("complete") {
            decorated.local_passes += 1;
        }
        if decorated.summary {
            decorated.cloud_passes += 1;
        }
        if decorated.mitigations {
            decorated.cloud_passes += 1;
        }
        if decorated.dashboard {
            decorated.cloud_passes += 1;
        }

        // Attention. Handled work never asks for attention — that is what handling
        // it meant.
        let (needed, reason) = if matches!(state, State::Resolved | State::Snoozed) {
            (false, None)
        } else if self
            .store
            .list_hints(Some(&thread.id))?
            .iter()
            .any(|h| matches!(h.kind, crate::live::HintKind::Flag))
        {
            (
                true,
                Some("live-assist flagged something you said".to_string()),
            )
        } else if severity >= Severity::Critical {
            (true, Some("critical".to_string()))
        } else if severity >= Severity::Warning {
            (true, Some("warning".to_string()))
        } else if signals.iter().any(|s| s.is_user_engaged()) {
            (true, Some("you're in this one".to_string()))
        } else if triage_rows.iter().any(|t| t.status != "complete") {
            (true, Some("assigned to you".to_string()))
        } else {
            // Informational, or already acknowledged: on the board, not asking.
            (false, None)
        };
        Ok(Attention {
            needed,
            reason,
            decorated,
        })
    }

    /// All threads as views, newest activity first. `active_only` drops threads
    /// that are resolved or snoozed — both are "handled" and stay off the board
    /// (a snoozed thread stays hidden even as new signals land on it).
    pub fn thread_views(&self, active_only: bool) -> Result<Vec<ThreadView>> {
        let mut out = Vec::new();
        for t in self.store.list_threads()? {
            if let Some(view) = self.thread_view(&t.id)? {
                if active_only && matches!(view.state, State::Resolved | State::Snoozed) {
                    continue;
                }
                out.push(view);
            }
        }
        Ok(out)
    }
}

/// Entity kinds that describe *where* a signal happened or *who* was involved
/// rather than *what* it is about. They stay on the signal for display and
/// grounding, but must not, on their own, glue signals into a thread: every
/// notification in a repo shares its `repo`, every message in a Slack channel
/// shares its `channel`, and a chatty person shares their `person` across
/// unrelated topics — correlating on any of these collapses everything under
/// that repo/channel/person into one thread. Grouping is driven by strong,
/// topic-identifying keys instead (`pr`, `issue`, `discussion`, `commit`,
/// `slack_thread`, `meeting`, …).
const CONTEXT_ONLY_KINDS: &[&str] = &["repo", "channel", "person"];

/// Default branch names. A branch entity is a *topic* only when it's a feature
/// branch: `main` is shared by every CI run in a repository forever, so grouping
/// on it would collapse the repo's entire history into one thread — the same
/// failure `repo` is excluded for. CI on a default branch is instead attributed to
/// the merged PR (and through it the issue) that produced the commit; see
/// [`crate::watchers::github`].
const DEFAULT_BRANCHES: &[&str] = &["main", "master", "trunk", "develop", "development"];

/// How authoritative an identity is, highest first. This is the precedence the
/// board is built on:
///
/// **issue > pull request > branch.**
///
/// An issue is the durable statement of *what the work is*; a PR is one attempt at
/// it; a branch is where that attempt happens. So when a signal can be attributed
/// upward — a branch to its PR, a PR to the issue it closes — it must be, and the
/// highest available identity is what groups it. Otherwise CI runs cluster by
/// branch, PRs cluster separately, and the issue everyone is actually working on
/// ends up as a fourth card with none of the activity attached to it.
///
/// `environment` sits at the top for tenant alerts: an env id names a specific
/// customer's environment, which is the most specific thing an alert can be about.
pub(crate) fn identity_rank(key: &str) -> u8 {
    match key.split_once(':').map(|(kind, _)| kind).unwrap_or("") {
        "environment" => 5,
        "issue" => 4,
        "pr" => 3,
        "discussion" => 3,
        "branch" | "commit" => 2,
        "slack_thread" | "meeting" => 2,
        _ => 1,
    }
}

/// The subset of `keys` at the highest identity rank present.
///
/// Matching strictly on these is what makes the hierarchy real: a signal carrying
/// both `issue:repo#412` and `branch:repo@fix-pool` groups by the *issue*, and will
/// not be pulled onto a branch-only thread just because the branch also matches.
fn controlling_keys(keys: &BTreeSet<String>) -> BTreeSet<String> {
    let Some(top) = keys.iter().map(|k| identity_rank(k)).max() else {
        return BTreeSet::new();
    };
    keys.iter()
        .filter(|k| identity_rank(k) == top)
        .cloned()
        .collect()
}

pub(crate) fn entity_keys(entities: &[Entity]) -> BTreeSet<String> {
    entities
        .iter()
        .filter(|e| {
            let kind = e.kind.to_ascii_lowercase();
            !CONTEXT_ONLY_KINDS.contains(&kind.as_str()) && !is_default_branch(&kind, &e.value)
        })
        .map(|e| {
            format!(
                "{}:{}",
                e.kind.to_ascii_lowercase(),
                e.value.to_ascii_lowercase()
            )
        })
        .collect()
}

/// Is this entity a repository's default branch (`owner/repo@main`)? Those are
/// context, not identity — see [`DEFAULT_BRANCHES`].
fn is_default_branch(kind: &str, value: &str) -> bool {
    if kind != "branch" {
        return false;
    }
    let branch = value.rsplit_once('@').map(|(_, b)| b).unwrap_or(value);
    DEFAULT_BRANCHES.contains(&branch.to_ascii_lowercase().as_str())
}

/// Entity kinds that exist only as internal grouping keys and carry no display
/// value (opaque ids like a Slack conversation ts). Kept on the raw signal for
/// correlation, but hidden from the thread view, summary, and chips.
const HIDDEN_KINDS: &[&str] = &["slack_thread"];

fn union_entities(signals: &[Signal]) -> Vec<Entity> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for s in signals {
        for e in &s.entities {
            if HIDDEN_KINDS.contains(&e.kind.to_ascii_lowercase().as_str()) {
                continue;
            }
            let key = format!(
                "{}:{}",
                e.kind.to_ascii_lowercase(),
                e.value.to_ascii_lowercase()
            );
            if seen.insert(key) {
                out.push(e.clone());
            }
        }
    }
    out
}

/// Aggregate a thread's state: the least-triaged member state wins, so a thread
/// with any unseen signal reads as unseen.
fn aggregate_state(signals: &[Signal]) -> State {
    let rank = |s: State| match s {
        State::Unseen => 0,
        State::Seen => 1,
        State::Acknowledged => 2,
        State::Snoozed => 3,
        State::Resolved => 4,
    };
    signals
        .iter()
        .map(|s| s.state)
        .min_by_key(|s| rank(*s))
        .unwrap_or(State::Unseen)
}

fn thread_title(s: &Signal) -> String {
    let t = s.title.trim();
    if t.is_empty() {
        format!("{} · {}", s.source, s.external_id)
    } else {
        t.to_string()
    }
}

pub(crate) fn deterministic_summary(signals: &[Signal]) -> String {
    // Lead with real content — the newest message — rather than an entity dump,
    // which is useless for a chat message. This is only the fallback headline
    // until the LLM writes a proper summary; keep it a single readable line.
    if let Some(s) = signals.iter().max_by_key(|s| s.occurred_at) {
        let body = s.body.as_deref().unwrap_or("").trim();
        let text = if body.is_empty() {
            s.title.trim()
        } else {
            body
        };
        if !text.is_empty() {
            // Collapse newlines/runs of whitespace into a one-line preview.
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut preview: String = flat.chars().take(200).collect();
            if flat.chars().count() > 200 {
                preview.push('…');
            }
            if signals.len() > 1 {
                return format!("{preview} · +{} more event(s)", signals.len() - 1);
            }
            return preview;
        }
    }
    // Content-less signals: fall back to a source count.
    let mut sources: BTreeSet<&str> = BTreeSet::new();
    for s in signals {
        sources.insert(s.source.as_str());
    }
    let src_str = sources.into_iter().collect::<Vec<_>>().join("/");
    format!("{} event(s) from {}.", signals.len(), src_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Entity, SignalKind, Source, State};

    fn sig(ext: &str, ents: Vec<Entity>) -> Signal {
        Signal {
            id: Signal::make_id(Source::GitHub, ext),
            source: Source::GitHub,
            external_id: ext.into(),
            kind: SignalKind::CiFailure,
            title: format!("signal {ext}"),
            body: None,
            url: None,
            actor: None,
            entities: ents,
            severity: Severity::Warning,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: serde_json::Value::Null,
            tags: Vec::new(),
        }
    }

    #[test]
    fn groups_by_shared_entity_within_window() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));

        let a = sig("1", vec![Entity::new("service", "foo")]);
        store.insert_signal(&a).unwrap();
        let ta = c.ingest(&a).unwrap();

        let b = sig("2", vec![Entity::new("service", "foo")]);
        store.insert_signal(&b).unwrap();
        let tb = c.ingest(&b).unwrap();

        assert_eq!(ta, tb, "shared entity within window → same thread");

        // Unrelated entity → its own thread.
        let d = sig("3", vec![Entity::new("service", "bar")]);
        store.insert_signal(&d).unwrap();
        let td = c.ingest(&d).unwrap();
        assert_ne!(ta, td);

        let view = c.thread_view(&ta).unwrap().unwrap();
        assert_eq!(view.signals.len(), 2);
        assert_eq!(view.severity, Severity::Warning);
        assert_eq!(view.state, State::Unseen);
    }

    #[test]
    fn alerts_group_by_environment_id() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));

        // Two different alerts (OOMKilled, CPUHigh) about the same environment
        // collapse onto one thread — the env id is the authoritative anchor.
        let a = sig("oom", vec![Entity::new("environment", "env-201kbht")]);
        store.insert_signal(&a).unwrap();
        let ta = c.ingest(&a).unwrap();
        let b = sig("cpu", vec![Entity::new("environment", "env-201kbht")]);
        store.insert_signal(&b).unwrap();
        let tb = c.ingest(&b).unwrap();
        assert_eq!(ta, tb, "same environment id → one thread");

        // A different environment gets its own thread.
        let d = sig("other", vec![Entity::new("environment", "env-201other")]);
        store.insert_signal(&d).unwrap();
        let td = c.ingest(&d).unwrap();
        assert_ne!(ta, td, "different environment → separate thread");
    }

    #[test]
    fn repairs_signals_left_without_thread_metadata() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));
        let signal = sig("orphan", vec![Entity::new("service", "foo")]);
        store.insert_signal(&signal).unwrap();
        store
            .set_signal_thread(&signal.id, Some("thr/missing"))
            .unwrap();

        assert_eq!(c.repair_orphaned_threads().unwrap(), 1);
        let view = c.thread_view("thr/missing").unwrap().unwrap();
        assert_eq!(view.signals.len(), 1);
        assert_eq!(view.state, State::Unseen);
    }

    #[test]
    fn environment_anchor_beats_a_coincidental_shared_entity() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));

        // A thread anchored to env A that also happens to mention service "x".
        let a = sig(
            "a",
            vec![
                Entity::new("environment", "env-201aaa"),
                Entity::new("service", "x"),
            ],
        );
        store.insert_signal(&a).unwrap();
        let ta = c.ingest(&a).unwrap();

        // A new alert for env B that also mentions service "x": the environment is
        // authoritative, so it must NOT be pulled onto env A's thread by the
        // coincidental shared service.
        let b = sig(
            "b",
            vec![
                Entity::new("environment", "env-201bbb"),
                Entity::new("service", "x"),
            ],
        );
        store.insert_signal(&b).unwrap();
        let tb = c.ingest(&b).unwrap();
        assert_ne!(ta, tb, "environment anchor overrides the shared service");
    }

    #[test]
    fn channel_alone_does_not_group_but_topic_does() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));

        // Two messages sharing only a channel (context-only) → separate threads,
        // instead of a whole channel collapsing into one.
        let a = sig("1", vec![Entity::new("channel", "#eng")]);
        store.insert_signal(&a).unwrap();
        let ta = c.ingest(&a).unwrap();
        let b = sig("2", vec![Entity::new("channel", "#eng")]);
        store.insert_signal(&b).unwrap();
        let tb = c.ingest(&b).unwrap();
        assert_ne!(ta, tb, "channel alone must not glue messages together");

        // A GitHub PR signal and a Slack message linking the same PR share the
        // `pr` entity → one thread (the Slack chatter is a duplicate of the PR).
        let gh = sig(
            "pr-note",
            vec![
                Entity::new("pr", "octo/repo#17"),
                Entity::new("repo", "octo/repo"),
            ],
        );
        store.insert_signal(&gh).unwrap();
        let tgh = c.ingest(&gh).unwrap();
        let slack = sig(
            "slack-msg",
            vec![
                Entity::new("channel", "#eng"),
                Entity::new("pr", "octo/repo#17"),
            ],
        );
        store.insert_signal(&slack).unwrap();
        let tslack = c.ingest(&slack).unwrap();
        assert_eq!(
            tgh, tslack,
            "shared PR entity unifies Slack chatter with GitHub"
        );
    }

    /// The bug that produced a board full of near-identical one-signal cards: the
    /// window was measured from wall-clock `now`, so any catch-up ingest (first
    /// poll, restart, backlog) placed every signal in its own thread because they
    /// all looked older than the cutoff.
    /// Re-runs correlation over a real signal corpus and reports the grouping.
    ///
    /// Ignored by default (it needs a database): point `MUGGLEBOT_REGROUP_DB` at a
    /// copy of a live store and run
    /// `cargo test regroup_real_corpus -- --ignored --nocapture`.
    /// Kept because the failure this guards against — fragmented threads — is only
    /// visible at corpus scale, not in a two-signal unit test.
    #[test]
    #[ignore = "needs MUGGLEBOT_REGROUP_DB pointing at a store copy"]
    fn regroup_real_corpus() {
        let path = match std::env::var("MUGGLEBOT_REGROUP_DB") {
            Ok(p) => p,
            Err(_) => return,
        };
        let store = Arc::new(Store::open(std::path::Path::new(&path)).unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));

        let mut signals = store.recent(100_000).unwrap();
        // Oldest first, the order ingest sees live.
        signals.sort_by_key(|s| s.occurred_at);
        let total = signals.len();
        for s in &signals {
            let mut s = s.clone();
            s.thread = None;
            store.set_signal_thread(&s.id, None).unwrap();
            c.ingest(&s).unwrap();
        }

        let mut by_thread: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for s in store.recent(100_000).unwrap() {
            if let Some(t) = s.thread {
                by_thread.entry(t).or_default().push(s.title);
            }
        }
        let singletons = by_thread.values().filter(|v| v.len() == 1).count();
        println!(
            "REGROUP: {total} signals -> {} threads ({singletons} singletons)",
            by_thread.len()
        );
        for (tid, titles) in by_thread.iter().filter(|(_, v)| v.len() > 1) {
            println!("  {tid}  x{}", titles.len());
            for title in titles.iter().take(4) {
                println!("      {}", &title[..title.len().min(70)]);
            }
        }
    }

    #[test]
    fn backlog_ingest_still_correlates() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let correlator = Correlator::new(store.clone(), Duration::from_secs(1800));
        let hours_ago = Utc::now() - chrono::Duration::hours(8);
        let topic = || vec![Entity::new("pr", "octo/repo#17")];

        let mut a = sig("1", topic());
        a.occurred_at = hours_ago;
        let mut b = sig("2", topic());
        b.occurred_at = hours_ago + chrono::Duration::minutes(10);

        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let ta = correlator.ingest(&a).unwrap();
        let tb = correlator.ingest(&b).unwrap();
        assert_eq!(
            ta, tb,
            "signals 10 minutes apart must group even when ingested hours later"
        );
    }

    /// Two signals genuinely far apart in time are still separate topics.
    #[test]
    fn the_window_still_separates_distant_signals() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let correlator = Correlator::new(store.clone(), Duration::from_secs(1800));
        let topic = || vec![Entity::new("pr", "octo/repo#17")];
        let mut a = sig("1", topic());
        a.occurred_at = Utc::now() - chrono::Duration::hours(8);
        let mut b = sig("2", topic());
        b.occurred_at = Utc::now();
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        assert_ne!(
            correlator.ingest(&a).unwrap(),
            correlator.ingest(&b).unwrap()
        );
    }

    /// issue > pr > branch. A signal naming an issue belongs to the issue's thread
    /// even when a weaker identity it also carries matches somewhere else.
    #[test]
    fn the_strongest_identity_controls_grouping() {
        assert!(identity_rank("issue:o/r#1") > identity_rank("pr:o/r#2"));
        assert!(identity_rank("pr:o/r#2") > identity_rank("branch:o/r@fix"));
        assert!(identity_rank("environment:env-2abc") > identity_rank("issue:o/r#1"));

        let keys: BTreeSet<String> = ["issue:o/r#1", "pr:o/r#2", "branch:o/r@fix"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let controlling = controlling_keys(&keys);
        assert_eq!(controlling.len(), 1);
        assert!(controlling.contains("issue:o/r#1"));
    }

    /// `main` is shared by every CI run in a repo forever — grouping on it would
    /// collapse the repo's whole history into one thread, the same reason `repo` is
    /// excluded.
    #[test]
    fn a_default_branch_is_not_an_identity() {
        let keys = entity_keys(&[
            Entity::new("branch", "octo/repo@main"),
            Entity::new("repo", "octo/repo"),
        ]);
        assert!(keys.is_empty(), "main + repo are both context: {keys:?}");

        // A feature branch is a real identity.
        let keys = entity_keys(&[Entity::new("branch", "octo/repo@fix/pool-leak")]);
        assert_eq!(keys.len(), 1);
    }

    /// CI resolved up to its PR, and the PR up to its issue, must land on the
    /// issue's thread — that is the whole point of the hierarchy.
    #[test]
    fn ci_attributed_to_an_issue_joins_that_issues_thread() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let correlator = Correlator::new(store.clone(), Duration::from_secs(1800));
        let now = Utc::now();

        // The issue's own notification.
        let mut issue = sig(
            "issue",
            vec![
                Entity::new("issue", "octo/repo#412"),
                Entity::new("repo", "octo/repo"),
            ],
        );
        issue.occurred_at = now;
        store.insert_signal(&issue).unwrap();
        let issue_thread = correlator.ingest(&issue).unwrap();

        // A CI run resolved branch → PR → issue: issue controls, PR rides along.
        let mut ci = sig(
            "ci",
            vec![
                Entity::new("issue", "octo/repo#412"),
                Entity::new("pr", "octo/repo#500"),
                Entity::new("repo", "octo/repo"),
            ],
        );
        ci.occurred_at = now + chrono::Duration::minutes(5);
        store.insert_signal(&ci).unwrap();
        assert_eq!(
            correlator.ingest(&ci).unwrap(),
            issue_thread,
            "CI attributed to an issue must join that issue's thread"
        );
    }

    #[test]
    fn snoozed_thread_hides_and_mutes_new_signals() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));

        let a = sig("1", vec![Entity::new("service", "foo")]);
        store.insert_signal(&a).unwrap();
        let tid = c.ingest(&a).unwrap();

        // Snooze the whole thread → it drops off the active board.
        store.set_state(&a.id, State::Snoozed).unwrap();
        assert!(
            c.thread_views(true).unwrap().is_empty(),
            "snoozed thread is hidden from the active board"
        );

        // A new (non-engaged) signal on the same thread inherits Snoozed, so the
        // thread stays hidden rather than resurfacing.
        let b = sig("2", vec![Entity::new("service", "foo")]);
        store.insert_signal(&b).unwrap();
        let tb = c.ingest(&b).unwrap();
        assert_eq!(tb, tid, "shared entity → same thread");
        assert_eq!(
            store.get_signal(&b.id).unwrap().unwrap().state,
            State::Snoozed,
            "new activity on a snoozed thread is muted"
        );
        assert!(
            c.thread_views(true).unwrap().is_empty(),
            "snoozed thread stays hidden after new activity"
        );
    }

    #[test]
    fn engagement_revives_a_snoozed_thread() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));

        let a = sig("1", vec![Entity::new("service", "foo")]);
        store.insert_signal(&a).unwrap();
        let tid = c.ingest(&a).unwrap();
        store.set_state(&a.id, State::Snoozed).unwrap();

        // A signal the user is engaged in (a mention) is NOT muted, so the thread
        // comes back onto the board.
        let mut b = sig("2", vec![Entity::new("service", "foo")]);
        b.kind = SignalKind::Mention;
        store.insert_signal(&b).unwrap();
        let tb = c.ingest(&b).unwrap();
        assert_eq!(tb, tid);
        assert_eq!(
            store.get_signal(&b.id).unwrap().unwrap().state,
            State::Unseen,
            "an engaging signal is left un-muted"
        );
        let views = c.thread_views(true).unwrap();
        assert_eq!(views.len(), 1, "engagement revives the snoozed thread");
        assert_eq!(views[0].state, State::Unseen);
    }

    #[test]
    fn ingest_is_idempotent() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let c = Correlator::new(store.clone(), Duration::from_secs(1800));
        let mut a = sig("1", vec![Entity::new("repo", "o/r")]);
        store.insert_signal(&a).unwrap();
        let t1 = c.ingest(&a).unwrap();
        a.thread = Some(t1.clone());
        let t2 = c.ingest(&a).unwrap();
        assert_eq!(t1, t2);
    }
}
