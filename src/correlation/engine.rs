//! The correlation engine: deterministic grouping (Phase 1) and the view-building
//! helpers shared by the server API and MCP tools. The LLM relation-graph pass
//! (Phase 2) is added by [`super::llm`].

use anyhow::Result;
use chrono::Utc;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::{Thread, ThreadView};
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
        // An environment id (`env-2…`/`acc-1…`/`org-1…`) is the authoritative
        // grouping key: when a signal names one, match strictly on it so every
        // alert about that tenant collapses onto its one thread — bypassing the
        // general most-shared-entity match (and, downstream, the fuzzy LLM
        // regrouping) that a signal without such a strong anchor would need.
        let env_keys: BTreeSet<String> = keys
            .iter()
            .filter(|k| k.starts_with("environment:"))
            .cloned()
            .collect();
        let match_keys = if env_keys.is_empty() {
            &keys
        } else {
            &env_keys
        };
        let matched = if match_keys.is_empty() {
            None
        } else {
            self.best_matching_thread(s, match_keys)?
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

    /// Find the existing thread whose members (within the window) share the most
    /// entities with `s`. Ties broken by recency.
    fn best_matching_thread(&self, s: &Signal, keys: &BTreeSet<String>) -> Result<Option<String>> {
        let since = Utc::now() - chrono::Duration::from_std(self.window).unwrap_or_default();
        let candidates = self.store.signals_since(since)?;
        let mut best: Option<(usize, chrono::DateTime<Utc>, String)> = None;
        for c in candidates {
            if c.id == s.id {
                continue;
            }
            let Some(tid) = c.thread.clone() else {
                continue;
            };
            let shared = entity_keys(&c.entities).intersection(keys).count();
            if shared == 0 {
                continue;
            }
            let better = match &best {
                None => true,
                Some((bs, bt, _)) => shared > *bs || (shared == *bs && c.occurred_at > *bt),
            };
            if better {
                best = Some((shared, c.occurred_at, tid));
            }
        }
        Ok(best.map(|(_, _, tid)| tid))
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
        Ok(Some(ThreadView {
            thread,
            signals,
            entities,
            severity,
            state,
            edges,
            context,
        }))
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

pub(crate) fn entity_keys(entities: &[Entity]) -> BTreeSet<String> {
    entities
        .iter()
        .filter(|e| !CONTEXT_ONLY_KINDS.contains(&e.kind.to_ascii_lowercase().as_str()))
        .map(|e| {
            format!(
                "{}:{}",
                e.kind.to_ascii_lowercase(),
                e.value.to_ascii_lowercase()
            )
        })
        .collect()
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
