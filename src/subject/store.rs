//! Subjects and signals, owned by virtual objects, read through an in-process model.
//!
//! This replaces the `subjects` and `signals` tables. The durable record is the state of the
//! `Issue` / `PullRequest` / `SlackThread` object that owns the work (see
//! [`crate::restate::subject_state`] for the layout); this type is how the rest of the daemon
//! reads and writes it.
//!
//! # Why there is a read model at all
//!
//! Object state is read across keys through Restate's `state` table, which is an HTTP call and
//! a Datafusion scan. The board reads subjects on every push, and `subject_view` is called from
//! 37 places, all of them synchronous. Making those async would cascade through the notifier,
//! the event broadcast and every handler that renders a card — far more change than the
//! migration itself, for a read that has to be fast anyway.
//!
//! So reads come from an in-memory map, and it is a **cache, not a second source of truth**:
//! it is built from object state at startup, refreshed from it on a timer, and thrown away on
//! exit. Nothing reconciles it, because there is nothing to reconcile it *with* — if it and the
//! objects disagree, the objects are right and the next refresh fixes it.
//!
//! # How a write happens
//!
//! Only an object can write its own state, and only inside its own handler. Callers here are
//! usually not in one, so a write does two things:
//!
//! 1. updates the read model immediately, so the caller's next read is consistent; and
//! 2. sends the durable write to the owning object through the ingress.
//!
//! The send is fire-and-forget by necessity — these call sites are synchronous — which means a
//! write is durable once Restate accepts it, and a rejected send leaves the read model ahead of
//! the record until the next refresh corrects it. Failures are logged loudly rather than
//! swallowed, because a silently-dropped durable write is the one failure this design cannot
//! detect on its own.

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, warn};

use crate::restate::ingress::Ingress;
use crate::restate::subject_state::{self as layout, StoredSubject, SubjectStateReader};
use crate::signal::{Signal, Source};
use crate::subject::{Handled, Subject, SubjectKey};

/// The handler each object exposes for a durable state write.
const PUT_SUBJECT: &str = "put_subject";
const PUT_SIGNAL: &str = "put_signal";
const DROP_SIGNAL: &str = "drop_signal";

pub struct SubjectStore {
    /// Subject key → the subject and its signals, as last read from or written to object state.
    model: RwLock<BTreeMap<String, StoredSubject>>,
    /// Signals attributed to nothing yet.
    unattributed: RwLock<Vec<Signal>>,
    reader: SubjectStateReader,
    ingress: Arc<Ingress>,
}

impl SubjectStore {
    pub fn new(cfg: &crate::config::RestateConfig, ingress: Arc<Ingress>) -> Self {
        Self {
            model: RwLock::new(BTreeMap::new()),
            unattributed: RwLock::new(Vec::new()),
            reader: SubjectStateReader::new(cfg),
            ingress,
        }
    }

    /// Rebuild the read model from object state.
    ///
    /// Called at startup and on a timer. A whole-map replace rather than a merge: a subject
    /// deleted or merged away upstream has to disappear here too, and a merge would keep it
    /// forever.
    pub async fn refresh(&self) -> Result<usize> {
        let subjects = self.reader.all().await?;
        let n = subjects.len();
        let unattributed = self.reader.unattributed(200).await.unwrap_or_default();
        *self.model.write().expect("subject model poisoned") = subjects;
        *self
            .unattributed
            .write()
            .expect("unattributed model poisoned") = unattributed;
        debug!("subject model: {n} subject(s) from object state");
        Ok(n)
    }

    // ---- reads ---------------------------------------------------------------------

    pub fn get(&self, key: &str) -> Option<Subject> {
        self.read().get(key).map(|s| s.subject.clone())
    }

    pub fn list(&self) -> Vec<Subject> {
        self.read().values().map(|s| s.subject.clone()).collect()
    }

    pub fn signals_for(&self, key: &str) -> Vec<Signal> {
        self.read()
            .get(key)
            .map(|s| s.signals.clone())
            .unwrap_or_default()
    }

    /// One signal by id, wherever it is attributed.
    ///
    /// A scan of the model rather than an index: signals are keyed by subject because that is
    /// how everything reads them, and a by-id lookup happens on operator actions rather than
    /// on the ingest path.
    pub fn get_signal(&self, id: &str) -> Option<Signal> {
        let model = self.read();
        for stored in model.values() {
            if let Some(s) = stored.signals.iter().find(|s| s.id == id) {
                return Some(s.clone());
            }
        }
        self.unattributed_all().into_iter().find(|s| s.id == id)
    }

    /// Every signal, newest first.
    pub fn all_signals(&self) -> Vec<Signal> {
        let mut out: Vec<Signal> = self
            .read()
            .values()
            .flat_map(|s| s.signals.iter().cloned())
            .collect();
        out.extend(self.unattributed_all());
        out.sort_by_key(|s| std::cmp::Reverse(s.occurred_at));
        out
    }

    pub fn recent(&self, limit: usize) -> Vec<Signal> {
        let mut all = self.all_signals();
        all.truncate(limit);
        all
    }

    pub fn signals_since(&self, since: DateTime<Utc>) -> Vec<Signal> {
        self.all_signals()
            .into_iter()
            .filter(|s| s.occurred_at >= since)
            .collect()
    }

    /// Substring search over title and body.
    ///
    /// The `signals` table had no FTS index, so this is the same capability it had — a scan
    /// with a `LIKE` — done in memory instead of in SQL.
    pub fn search_signals(&self, query: &str, limit: usize) -> Vec<Signal> {
        let needle = query.to_ascii_lowercase();
        let mut hits: Vec<Signal> = self
            .all_signals()
            .into_iter()
            .filter(|s| {
                s.title.to_ascii_lowercase().contains(&needle)
                    || s.body
                        .as_deref()
                        .is_some_and(|b| b.to_ascii_lowercase().contains(&needle))
            })
            .collect();
        hits.truncate(limit);
        hits
    }

    pub fn unattributed_all(&self) -> Vec<Signal> {
        self.unattributed
            .read()
            .expect("unattributed model poisoned")
            .clone()
    }

    /// Subjects filed under this one.
    pub fn children(&self, key: &str) -> Vec<SubjectKey> {
        self.read()
            .values()
            .filter(|s| s.subject.parent.as_ref().is_some_and(|p| p.as_str() == key))
            .map(|s| s.subject.key.clone())
            .collect()
    }

    /// The subject carrying this deterministic merge key, if any.
    ///
    /// Slack-rank only, by construction: the merge key is an environment id and nothing else
    /// sets one.
    pub fn by_merge_key(&self, merge_key: &str) -> Option<Subject> {
        self.read()
            .values()
            .map(|s| &s.subject)
            .find(|s| s.merge_key.as_deref() == Some(merge_key))
            .cloned()
    }

    /// Whether this signal has been seen before, by its dedup identity.
    ///
    /// The in-memory half of the guard that `UNIQUE(source, external_id, version)` used to
    /// provide. The durable half is the object write itself: a second `put_signal` with the
    /// same signal id overwrites rather than duplicating, because the state key *is* the id.
    pub fn already_seen(&self, sig: &Signal) -> bool {
        let want = layout::dedup_key(sig);
        let model = self.read();
        model
            .values()
            .flat_map(|s| s.signals.iter())
            .chain(
                self.unattributed
                    .read()
                    .expect("unattributed model poisoned")
                    .iter(),
            )
            .any(|s| layout::dedup_key(s) == want)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, StoredSubject>> {
        self.model.read().expect("subject model poisoned")
    }

    // ---- writes --------------------------------------------------------------------

    /// Create or replace a subject.
    pub fn upsert(&self, subject: &Subject) {
        let key = subject.key.to_string();
        {
            let mut model = self.model.write().expect("subject model poisoned");
            let entry = model.entry(key.clone()).or_insert_with(|| StoredSubject {
                subject: subject.clone(),
                signals: Vec::new(),
            });
            entry.subject = subject.clone();
        }
        self.send(&subject.key, PUT_SUBJECT, subject.clone());
    }

    /// Attribute a signal to a subject, or to the unattributed lane when `subject` is `None`.
    pub fn put_signal(&self, subject: Option<&SubjectKey>, sig: &Signal) {
        match subject {
            Some(key) => {
                {
                    let mut model = self.model.write().expect("subject model poisoned");
                    if let Some(entry) = model.get_mut(key.as_str()) {
                        entry.signals.retain(|s| s.id != sig.id);
                        entry.signals.push(sig.clone());
                        entry.signals.sort_by_key(|s| s.occurred_at);
                    }
                }
                // Dropped from the lane the moment it has a home, or the board would show it
                // twice — once as activity and once as unresolvable.
                self.unattributed
                    .write()
                    .expect("unattributed model poisoned")
                    .retain(|s| s.id != sig.id);
                self.send(key, PUT_SIGNAL, sig.clone());
            }
            None => {
                let mut lane = self
                    .unattributed
                    .write()
                    .expect("unattributed model poisoned");
                lane.retain(|s| s.id != sig.id);
                lane.push(sig.clone());
                self.send_raw("Issue", layout::UNATTRIBUTED, PUT_SIGNAL, sig.clone());
            }
        }
    }

    /// Move every signal from `key` to `canonical`, and mark `key` merged away.
    ///
    /// Returns how many signals moved. Two objects are involved, so this cannot be one atomic
    /// write the way the SQL transaction it replaces was: the read model is updated together,
    /// but the durable sends land independently. A send that fails leaves signals on the old
    /// object, where the next refresh will show them — visibly wrong rather than silently lost,
    /// which is the better of the two failure modes available here.
    pub fn merge_into(&self, key: &SubjectKey, canonical: &SubjectKey) -> usize {
        let moved: Vec<Signal> = {
            let mut model = self.model.write().expect("subject model poisoned");
            let taken = match model.get_mut(key.as_str()) {
                Some(entry) => std::mem::take(&mut entry.signals),
                None => Vec::new(),
            };
            if let Some(target) = model.get_mut(canonical.as_str()) {
                for s in &taken {
                    target.signals.retain(|x| x.id != s.id);
                    target.signals.push(s.clone());
                }
                target.signals.sort_by_key(|s| s.occurred_at);
            }
            if let Some(entry) = model.get_mut(key.as_str()) {
                entry.subject.same_as = Some(canonical.clone());
            }
            taken
        };

        for s in &moved {
            self.send(canonical, PUT_SIGNAL, s.clone());
            self.send_raw(
                layout::service_for_key(key),
                key.as_str(),
                DROP_SIGNAL,
                s.id.clone(),
            );
        }
        if let Some(subject) = self.get(key.as_str()) {
            self.send(key, PUT_SUBJECT, subject);
        }
        moved.len()
    }

    /// Set triage state.
    pub fn set_handled(&self, key: &str, handled: Handled, until: Option<DateTime<Utc>>) {
        self.mutate(key, |s| {
            s.handled = handled;
            s.snoozed_until = until;
        });
    }

    pub fn set_summary(&self, key: &str, summary: &str, reasoned: bool) {
        self.mutate(key, |s| {
            s.summary = Some(summary.to_string());
            if reasoned {
                s.last_reasoned_at = Some(Utc::now());
            }
        });
    }

    pub fn set_tags(&self, key: &str, tags: &[String], pinned: bool) {
        self.mutate(key, |s| {
            s.tags = tags.to_vec();
            s.tags_pinned = pinned;
        });
    }

    pub fn set_live(&self, key: &str, live: bool) {
        self.mutate(key, |s| s.live = live);
    }

    pub fn set_merge_key(&self, key: &str, merge_key: &str) {
        self.mutate(key, |s| s.merge_key = Some(merge_key.to_string()));
    }

    pub fn clear_same_as(&self, key: &str) {
        self.mutate(key, |s| s.same_as = None);
    }

    pub fn set_parent(&self, child: &str, parent: Option<&str>) {
        let parsed = parent.and_then(|p| SubjectKey::parse(p).ok());
        self.mutate(child, |s| s.parent = parsed);
    }

    /// Read-modify-write one subject, in the model and then durably.
    fn mutate(&self, key: &str, f: impl FnOnce(&mut Subject)) {
        let updated = {
            let mut model = self.model.write().expect("subject model poisoned");
            match model.get_mut(key) {
                Some(entry) => {
                    f(&mut entry.subject);
                    entry.subject.updated_at = Utc::now();
                    Some(entry.subject.clone())
                }
                None => None,
            }
        };
        match updated {
            Some(s) => self.send(&s.key.clone(), PUT_SUBJECT, s),
            // Nothing to mutate. Not an error: the caller may be acting on a subject the
            // model has not seen yet, and the object is authoritative either way.
            None => debug!("subject {key}: no such subject in the model"),
        }
    }

    /// Send a durable write to the object that owns `key`.
    fn send(&self, key: &SubjectKey, handler: &str, payload: impl serde::Serialize) {
        self.send_raw(layout::service_for_key(key), key.as_str(), handler, payload);
    }

    /// Fire-and-forget a durable write.
    ///
    /// Spawned because every caller here is synchronous. The alternative — making the 37
    /// `subject_view` call sites and everything above them async — is a far larger change than
    /// this migration, for a read path that has to be fast anyway.
    fn send_raw(
        &self,
        service: &'static str,
        key: &str,
        handler: &str,
        payload: impl serde::Serialize,
    ) {
        // Serialized here rather than in the task: the payload types are borrowed or non-`Send`,
        // and a `Value` crosses the spawn boundary without constraining every caller.
        let payload = match serde_json::to_value(&payload) {
            Ok(v) => v,
            Err(e) => {
                warn!("durable write {service}/{key}/{handler}: not serializable: {e}");
                return;
            }
        };
        let ingress = self.ingress.clone();
        let key = key.to_string();
        let handler = handler.to_string();
        tokio::spawn(async move {
            if let Err(e) = ingress
                .send_object(service, &key, &handler, None, &payload)
                .await
            {
                // Loud: a dropped durable write is the one failure this design cannot detect
                // for itself, because the read model will happily keep serving the value.
                warn!("durable write {service}/{key}/{handler} failed: {e:#}");
            }
        });
    }
}

/// A signal's source, for the ingest-side dedup decision.
pub fn source_of(sig: &Signal) -> Source {
    sig.source
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restate::subject_state::StoredSubject;
    use crate::signal::{Severity, SignalKind};
    use crate::subject::SubjectRank;

    fn store() -> SubjectStore {
        let cfg = crate::config::RestateConfig::default();
        // Offline: a real ingress on the default config points at the developer's own running
        // Restate server. See `Ingress::offline`.
        SubjectStore::new(&cfg, Arc::new(Ingress::offline()))
    }

    fn subject(key: &str, rank: SubjectRank) -> Subject {
        Subject {
            key: SubjectKey::parse(key).unwrap(),
            rank,
            title: "t".into(),
            summary: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_reasoned_at: None,
            live: false,
            tags: vec![],
            tags_pinned: false,
            handled: Handled::Open,
            snoozed_until: None,
            same_as: None,
            parent: None,
            merge_key: None,
        }
    }

    fn sig(id: &str, external: &str) -> Signal {
        Signal {
            id: id.into(),
            source: Source::GitHub,
            external_id: external.into(),
            kind: SignalKind::Mention,
            title: format!("signal {id}"),
            body: Some("pool exhausted".into()),
            url: None,
            actor: None,
            keys: vec![],
            severity: Severity::Notice,
            version: None,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: serde_json::json!({}),
            tags: vec![],
        }
    }

    /// Seed the model directly, standing in for a refresh from object state.
    fn seed(s: &SubjectStore, subject: Subject) {
        s.model.write().unwrap().insert(
            subject.key.to_string(),
            StoredSubject {
                subject,
                signals: vec![],
            },
        );
    }

    #[tokio::test]
    async fn a_signal_attributed_to_a_subject_leaves_the_unattributed_lane() {
        let s = store();
        seed(&s, subject("o/r#412", SubjectRank::Issue));

        // Arrives with nowhere to go.
        s.put_signal(None, &sig("a", "x"));
        assert_eq!(s.unattributed_all().len(), 1);

        // Then attribution finds it a home. It must not appear in both places, or the board
        // shows it as activity *and* as unresolvable.
        let key = SubjectKey::parse("o/r#412").unwrap();
        s.put_signal(Some(&key), &sig("a", "x"));
        assert!(s.unattributed_all().is_empty(), "still in the lane");
        assert_eq!(s.signals_for("o/r#412").len(), 1);
    }

    #[tokio::test]
    async fn recording_the_same_signal_twice_does_not_duplicate_it() {
        let s = store();
        seed(&s, subject("o/r#412", SubjectRank::Issue));
        let key = SubjectKey::parse("o/r#412").unwrap();
        s.put_signal(Some(&key), &sig("a", "x"));
        s.put_signal(Some(&key), &sig("a", "x"));
        assert_eq!(
            s.signals_for("o/r#412").len(),
            1,
            "the signal id is the state key, so a repeat overwrites"
        );
    }

    /// The replacement for `UNIQUE(source, external_id, version)`.
    #[tokio::test]
    async fn a_previously_seen_dedup_identity_is_recognized() {
        let s = store();
        seed(&s, subject("o/r#412", SubjectRank::Issue));
        let key = SubjectKey::parse("o/r#412").unwrap();
        let first = sig("a", "o/r#412");
        s.put_signal(Some(&key), &first);

        // A different signal id, same upstream event and version: already seen.
        let redelivered = sig("b", "o/r#412");
        assert!(s.already_seen(&redelivered));

        // A new upstream version is new activity, not a duplicate.
        let mut changed = sig("c", "o/r#412");
        changed.version = Some("v2".into());
        assert!(!s.already_seen(&changed));
    }

    #[tokio::test]
    async fn a_merge_moves_signals_and_marks_the_loser() {
        let s = store();
        seed(&s, subject("chan/111.222", SubjectRank::SlackThread));
        seed(&s, subject("o/r#412", SubjectRank::Issue));
        let thread = SubjectKey::parse("chan/111.222").unwrap();
        let issue = SubjectKey::parse("o/r#412").unwrap();
        s.put_signal(Some(&thread), &sig("a", "x"));
        s.put_signal(Some(&thread), &sig("b", "y"));

        let moved = s.merge_into(&thread, &issue);
        assert_eq!(moved, 2);
        assert!(
            s.signals_for("chan/111.222").is_empty(),
            "signals must not be left on the merged-away subject"
        );
        assert_eq!(s.signals_for("o/r#412").len(), 2);
        assert_eq!(
            s.get("chan/111.222")
                .unwrap()
                .same_as
                .map(|k| k.into_string()),
            Some("o/r#412".to_string()),
            "the loser has to point at the winner or activity stops forwarding"
        );
    }

    #[tokio::test]
    async fn search_and_recency_read_across_every_subject() {
        let s = store();
        seed(&s, subject("o/r#1", SubjectRank::Issue));
        seed(&s, subject("o/r#2", SubjectRank::Issue));
        let k1 = SubjectKey::parse("o/r#1").unwrap();
        let k2 = SubjectKey::parse("o/r#2").unwrap();
        let mut older = sig("a", "x");
        older.occurred_at = Utc::now() - chrono::Duration::hours(1);
        s.put_signal(Some(&k1), &older);
        s.put_signal(Some(&k2), &sig("b", "y"));

        // Newest first, across subjects.
        let recent = s.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, "b");

        assert_eq!(s.search_signals("pool", 10).len(), 2);
        assert!(s.search_signals("nothing here", 10).is_empty());
        // Case-insensitive, as the SQL `LIKE` it replaces was.
        assert_eq!(s.search_signals("POOL", 10).len(), 2);
        assert_eq!(s.get_signal("b").map(|s| s.id), Some("b".to_string()));
        assert!(s.get_signal("absent").is_none());
    }

    #[tokio::test]
    async fn children_and_merge_keys_resolve_from_the_model() {
        let s = store();
        seed(&s, subject("o/r#412", SubjectRank::Issue));
        seed(&s, subject("o/r!987", SubjectRank::PullRequest));
        s.set_parent("o/r!987", Some("o/r#412"));
        let kids = s.children("o/r#412");
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].as_str(), "o/r!987");

        s.set_merge_key("o/r#412", "prod-eu");
        assert_eq!(
            s.by_merge_key("prod-eu").map(|x| x.key.into_string()),
            Some("o/r#412".to_string())
        );
        assert!(s.by_merge_key("prod-us").is_none());
    }

    #[tokio::test]
    async fn triage_and_summary_land_on_the_subject() {
        let s = store();
        seed(&s, subject("o/r#412", SubjectRank::Issue));
        s.set_handled("o/r#412", Handled::Acknowledged, None);
        s.set_summary("o/r#412", "the pool saturates", true);
        s.set_live("o/r#412", true);

        let got = s.get("o/r#412").unwrap();
        assert_eq!(got.handled, Handled::Acknowledged);
        assert_eq!(got.summary.as_deref(), Some("the pool saturates"));
        assert!(got.live);
        assert!(got.last_reasoned_at.is_some());

        // Mutating a subject the model has never seen is a no-op, not a panic: the object is
        // authoritative and the next refresh will bring it in.
        s.set_handled("o/r#999", Handled::Resolved, None);
        assert!(s.get("o/r#999").is_none());
    }
}
