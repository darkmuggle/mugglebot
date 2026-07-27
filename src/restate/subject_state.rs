//! Subjects and their signals **as virtual object state**.
//!
//! This is the layout the `Issue`, `PullRequest` and `SlackThread` objects write, and the
//! reader that reconstructs the board from it across every key. It replaces the `subjects` and
//! `signals` tables: the object that owns a piece of work is the authority on it, and Restate's
//! `state` table answers the cross-key questions the board asks.
//!
//! # The layout
//!
//! Per object, keyed by [`SubjectKey`]:
//!
//! | State key | Value |
//! |---|---|
//! | `subject` | the [`Subject`] record as JSON — including its parent and merge key |
//! | `signal:<id>` | one [`Signal`] as JSON |
//!
//! One state key per signal rather than one key holding a list, for two reasons. A list is a
//! read-modify-write, so two handlers appending concurrently lose one of the two — and while a
//! virtual object serializes handlers *for one key*, a merge moves signals between two objects,
//! which is two keys. And per-signal keys let the `state` table count and filter signals
//! server-side instead of shipping every list to be counted here.
//!
//! # What has no object of its own
//!
//! Two things the tables used to carry needed a new home rather than a translation:
//!
//! - **Unattributed signals.** A signal that resolves to no subject has no subject object to
//!   live on, so it lives on a singleton [`UNATTRIBUTED`] object until attribution claims it.
//! - **The dedup backstop.** `UNIQUE(source, external_id, version)` was the guard beyond
//!   Restate's idempotency retention window. It becomes one [`Seen`]-object key per tuple:
//!   cheap, since an object is a RocksDB key, and it keeps the guarantee that a notification
//!   re-delivered after the retention window doesn't mint a second signal.
//!
//! # What this costs
//!
//! Every board read is HTTP to the admin API plus a Datafusion scan, and it deserializes one
//! JSON value per signal. That is fine for a panel and for a board of tens of subjects; it is
//! not a per-signal hot path. And the state lives in the Restate cluster, so a wipe — which
//! enabling vqueues requires — takes the board with it. Both are consequences of the design
//! being deliberate here, not oversights.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::restate::state::{ObjectState, StateReader};
use crate::signal::Signal;
use crate::subject::{Subject, SubjectKey, SubjectRank};

/// State key holding the subject record.
pub const SUBJECT: &str = "subject";
/// Prefix for per-signal state keys: `signal:<signal id>`.
pub const SIGNAL_PREFIX: &str = "signal:";
/// The object key of the singleton that holds signals attributed to nothing.
///
/// Not a real subject: it never appears on the board, is never analyzed, and exists so an
/// unresolvable event has somewhere to be visible instead of being dropped. Minting a subject
/// per unresolvable event is what fills a board with near-identical one-signal cards.
pub const UNATTRIBUTED: &str = "~unattributed";

/// Object service names, as Restate knows them. Used to address the `state` table.
pub const SERVICES: &[(&str, SubjectRank)] = &[
    ("Issue", SubjectRank::Issue),
    ("PullRequest", SubjectRank::PullRequest),
    ("SlackThread", SubjectRank::SlackThread),
];

/// Build the state key for a signal.
pub fn signal_key(id: &str) -> String {
    format!("{SIGNAL_PREFIX}{id}")
}

/// The dedup identity of a signal: source, upstream id, and upstream version.
///
/// The version is part of it because a notification thread legitimately re-fires when a new
/// comment lands — keying on the id alone would swallow real activity.
pub fn dedup_key(signal: &Signal) -> String {
    format!(
        "{}:{}:{}",
        signal.source.as_str(),
        signal.external_id,
        signal.version.as_deref().unwrap_or("-")
    )
}

/// One subject reconstructed from object state.
#[derive(Debug, Clone)]
pub struct StoredSubject {
    pub subject: Subject,
    pub signals: Vec<Signal>,
}

/// Reads subjects and their signals out of object state.
pub struct SubjectStateReader {
    reader: StateReader,
}

impl SubjectStateReader {
    pub fn new(cfg: &crate::config::RestateConfig) -> Self {
        Self {
            reader: StateReader::new(cfg),
        }
    }

    /// Every subject across all three object services, with its signals.
    ///
    /// One query per service rather than one per subject: the board renders every subject on
    /// every push, and a query per key would be hundreds of round trips per repaint.
    pub async fn all(&self) -> Result<BTreeMap<String, StoredSubject>> {
        let mut out = BTreeMap::new();
        for (service, _) in SERVICES {
            let per_key = self
                .reader
                .service_state(service)
                .await
                .with_context(|| format!("reading {service} state"))?;
            for (key, state) in per_key {
                if key == UNATTRIBUTED {
                    continue;
                }
                if let Some(stored) = parse(&key, &state) {
                    out.insert(key, stored);
                }
            }
        }
        Ok(out)
    }

    /// Signals attributed to nothing, newest first.
    pub async fn unattributed(&self, limit: usize) -> Result<Vec<Signal>> {
        let mut found = Vec::new();
        for (service, _) in SERVICES {
            let per_key = self.reader.service_state(service).await?;
            if let Some(state) = per_key.get(UNATTRIBUTED) {
                found.extend(signals_of(state));
            }
        }
        // Newest first: the unattributed lane is a tail to skim, not a history to read.
        found.sort_by_key(|s| std::cmp::Reverse(s.occurred_at));
        found.truncate(limit);
        Ok(found)
    }
}

/// Reconstruct one subject from its state map. `None` when the object has state but no subject
/// record — which is the normal condition for an object that has been addressed but never
/// recorded anything, and is not an error.
fn parse(key: &str, state: &ObjectState) -> Option<StoredSubject> {
    let raw = state.get(SUBJECT)?;
    // A subject that fails to deserialize is a real problem, but not one worth taking the whole
    // board down for: skip it and let the rest render. Logged, because silently losing a card
    // is how a board starts lying.
    let subject: Subject = match serde_json::from_str(raw) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("subject state for {key} did not parse: {e}");
            return None;
        }
    };
    let mut signals = signals_of(state);
    // Oldest first: every consumer reads a subject's activity chronologically, and the newest
    // signal is the watermark, so ordering here saves each of them sorting.
    signals.sort_by_key(|s| s.occurred_at);
    Some(StoredSubject { subject, signals })
}

/// Every `signal:*` value in a state map, deserialized. Unparseable entries are skipped with a
/// warning rather than failing the read.
fn signals_of(state: &ObjectState) -> Vec<Signal> {
    state
        .iter()
        .filter(|(k, _)| k.starts_with(SIGNAL_PREFIX))
        .filter_map(|(k, v)| match serde_json::from_str::<Signal>(v) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("signal state {k} did not parse: {e}");
                None
            }
        })
        .collect()
}

/// The object service that owns a subject of this rank.
pub fn service_for(rank: SubjectRank) -> &'static str {
    match rank {
        SubjectRank::Issue => "Issue",
        SubjectRank::PullRequest => "PullRequest",
        SubjectRank::SlackThread => "SlackThread",
    }
}

/// The service that owns a subject key, from the key's own shape.
pub fn service_for_key(key: &SubjectKey) -> &'static str {
    service_for(key.rank())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Severity, SignalKind, Source};
    use chrono::Utc;

    fn signal(id: &str, external: &str, version: Option<&str>) -> Signal {
        Signal {
            id: id.into(),
            source: Source::GitHub,
            external_id: external.into(),
            kind: SignalKind::Mention,
            title: "t".into(),
            body: None,
            url: None,
            actor: None,
            keys: vec![],
            severity: Severity::Notice,
            version: version.map(str::to_string),
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: serde_json::json!({}),
            tags: vec![],
        }
    }

    #[test]
    fn a_signal_gets_its_own_state_key() {
        assert_eq!(signal_key("sig-1"), "signal:sig-1");
        assert!(signal_key("sig-1").starts_with(SIGNAL_PREFIX));
    }

    /// The dedup identity has to distinguish "the same event again" from "the same thread,
    /// changed" — that is what the version is for, and it is why the replaced unique index
    /// spanned three columns rather than two.
    #[test]
    fn the_dedup_key_separates_a_repeat_from_a_change() {
        let a = signal("s1", "o/r#412", Some("v1"));
        let b = signal("s2", "o/r#412", Some("v2"));
        let again = signal("s3", "o/r#412", Some("v1"));

        assert_eq!(dedup_key(&a), dedup_key(&again), "same event, same key");
        assert_ne!(dedup_key(&a), dedup_key(&b), "new version is new activity");
        // A versionless signal still has a stable key rather than colliding with every other
        // versionless signal from the same source.
        let none = signal("s4", "o/r#999", None);
        assert_eq!(dedup_key(&none), "github:o/r#999:-");
        assert_ne!(dedup_key(&none), dedup_key(&a));
    }

    #[test]
    fn signals_are_read_back_oldest_first_and_junk_is_skipped() {
        let mut state = ObjectState::new();
        let older = signal("a", "x", None);
        let mut newer = signal("b", "y", None);
        newer.occurred_at = older.occurred_at + chrono::Duration::seconds(60);
        state.insert(signal_key("b"), serde_json::to_string(&newer).unwrap());
        state.insert(signal_key("a"), serde_json::to_string(&older).unwrap());
        // Not a signal, and not valid JSON for one: must not take the read down.
        state.insert(signal_key("c"), "{ not json".into());
        state.insert("unrelated".into(), "5".into());

        let got = signals_of(&state);
        assert_eq!(got.len(), 2, "the unparseable entry is skipped, not fatal");
        let mut sorted = got;
        sorted.sort_by_key(|s| s.occurred_at);
        assert_eq!(sorted[0].id, "a");
        assert_eq!(sorted[1].id, "b");
    }

    #[test]
    fn an_object_with_no_subject_record_is_not_a_subject() {
        let mut state = ObjectState::new();
        // A `RepoIndexer`-style object, or a subject object addressed but never recorded on.
        state.insert("ticks".into(), "3".into());
        assert!(parse("o/r#1", &state).is_none());
    }

    #[test]
    fn each_rank_maps_to_its_own_service() {
        assert_eq!(service_for(SubjectRank::Issue), "Issue");
        assert_eq!(service_for(SubjectRank::PullRequest), "PullRequest");
        assert_eq!(service_for(SubjectRank::SlackThread), "SlackThread");
        // Every service in the table is reachable from a rank, or `all()` would silently
        // read a service nothing writes.
        for (name, rank) in SERVICES {
            assert_eq!(service_for(*rank), *name);
        }
    }
}
