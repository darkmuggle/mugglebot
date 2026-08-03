//! Attaching a signal to its subject — the write half of attribution.
//!
//! [`resolve::attribute`](super::resolve::attribute) decides *which* subject owns a
//! signal from its keys alone. This is what happens next: create the subject if it's
//! new, follow a merge pointer if it's been merged away, wire up the hierarchy
//! links, and un-mute a snoozed subject the operator has just re-entered.
//!
//! In Phase 2 this becomes the body of the `Issue`/`PullRequest`/`SlackThread`
//! virtual objects' `record` handler, where the per-key exclusive handler removes
//! the read-modify-write race this code currently has to be careful about.

use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;

use super::projection::Board;
use super::resolve::{attribute, Attribution};
use super::{Handled, Subject, SubjectKey, SubjectRank};
use tracing::debug;

use crate::signal::Signal;
use crate::store::Store;

pub struct Attributor {
    store: Arc<Store>,
    board: Board,
}

impl Attributor {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            board: Board::new(store.clone()),
            store,
        }
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    // Board reads, delegated: callers hold one handle for "attribute this" and
    // "render that", which is how the old correlator was used.
    pub fn subject_view(&self, key: &str) -> Result<Option<super::SubjectView>> {
        match SubjectKey::parse(key) {
            Ok(k) => self.board.view(&k),
            Err(_) => Ok(None),
        }
    }

    /// Every subject, incidents included. The tool/MCP surface, where a caller asking for
    /// "subjects" means all of them.
    pub fn subject_views(&self, active_only: bool) -> Result<Vec<super::SubjectView>> {
        self.board.views(active_only)
    }

    /// The **main board**: everything except incidents.
    ///
    /// Split here rather than in the UI because the two boards are two answers to two
    /// questions — "what does my work need from me" and "what is on fire" — and a reader who
    /// went looking for an incident on the main board would be reading a list that silently
    /// omits them. Filtering in the component instead would leave incidents inside the
    /// board's own counts while rendering in none of its groups.
    pub fn board_views(&self, active_only: bool) -> Result<Vec<super::SubjectView>> {
        Ok(self
            .board
            .views(active_only)?
            .into_iter()
            .filter(|v| v.subject.rank != super::SubjectRank::Incident)
            .collect())
    }

    /// The **incidents board**: only incidents.
    ///
    /// `active_only` means what incident.io says, not what the operator has read — see
    /// `projection::upstream_finished`. A closed incident leaves here without anyone
    /// acknowledging it.
    pub fn incident_views(&self, active_only: bool) -> Result<Vec<super::SubjectView>> {
        Ok(self
            .board
            .views(active_only)?
            .into_iter()
            .filter(|v| v.subject.rank == super::SubjectRank::Incident)
            .collect())
    }

    pub fn refresh_subject_metadata(&self, key: &str) -> Result<()> {
        match SubjectKey::parse(key) {
            Ok(k) => self.board.refresh_metadata(&k),
            Err(_) => Ok(()),
        }
    }

    /// Place a freshly-ingested signal on its subject. `Ok(None)` means the signal
    /// resolved to nothing and belongs in the unattributed lane.
    ///
    /// Idempotent: a signal that already carries a subject keeps it, so a re-ingest
    /// or a replay can't move work.
    pub fn attach(&self, s: &Signal) -> Result<Option<SubjectKey>> {
        if let Some(existing) = &s.subject {
            return Ok(SubjectKey::parse(existing).ok());
        }
        let attribution = attribute(&s.keys);
        let Some(resolved) = attribution.subject.clone() else {
            return Ok(None);
        };

        let key = self.canonical(resolved, &attribution)?;
        let now = Utc::now();
        let existing = self.store.get_subject(key.as_str())?;
        match &existing {
            None => {
                let subject = Subject::new(key.clone(), s, now);
                self.store.upsert_subject(&subject)?;
                if let Some(parent) = self.parent_for(&key, &attribution) {
                    self.store
                        .set_subject_parent(key.as_str(), Some(parent.as_str()))?;
                }
                if let Some(mk) = &attribution.merge_key {
                    self.store.set_subject_merge_key(key.as_str(), mk)?;
                }
            }
            Some(subject) => {
                // Sticky snooze, inverted from the old per-signal version: a snoozed
                // subject stays muted as new activity lands, *unless* the operator is
                // personally in this signal — in which case they've re-entered the
                // work and it comes back. Uncertainty fails closed, because a false
                // reopen re-raises a notification they deliberately silenced.
                if subject.handled == Handled::Snoozed && s.is_user_engaged() {
                    self.store.set_handled(key.as_str(), Handled::Open, None)?;
                }
                // A PR whose parent issue only became discoverable later (a closing
                // keyword added in a comment) gets linked on the next signal.
                if subject.parent.is_none() {
                    if let Some(parent) = self.parent_for(&key, &attribution) {
                        self.store
                            .set_subject_parent(key.as_str(), Some(parent.as_str()))?;
                    }
                }
            }
        }

        self.store.set_signal_subject(&s.id, Some(key.as_str()))?;
        self.link_secondaries(&key, &attribution)?;
        self.board.refresh_metadata(&key)?;
        Ok(Some(key))
    }

    /// Follow `same_as` to the subject that actually owns the work, and let a
    /// Slack-rank merge key redirect to an existing conversation about the same
    /// customer environment.
    fn canonical(&self, key: SubjectKey, attribution: &Attribution) -> Result<SubjectKey> {
        // Deterministic Slack-rank grouping: before creating a second alert thread
        // for `env-2abc`, join the one that exists.
        if key.rank() == SubjectRank::SlackThread {
            if let Some(mk) = &attribution.merge_key {
                if self.store.get_subject(key.as_str())?.is_none() {
                    if let Some(existing) = self.store.subject_by_merge_key(mk)? {
                        return self.follow_same_as(existing.key);
                    }
                }
            }
        }
        self.follow_same_as(key)
    }

    /// Walk a merge chain to its end. Bounded, because a cycle here would hang
    /// ingest — and a cycle is possible if two merges race.
    fn follow_same_as(&self, mut key: SubjectKey) -> Result<SubjectKey> {
        for _ in 0..8 {
            let Some(subject) = self.store.get_subject(key.as_str())? else {
                return Ok(key);
            };
            match subject.same_as {
                Some(next) if next != key => key = next,
                _ => return Ok(key),
            }
        }
        Ok(key)
    }

    /// The issue a PR belongs under, if this attribution named one.
    fn parent_for(&self, key: &SubjectKey, attribution: &Attribution) -> Option<SubjectKey> {
        if key.rank() != SubjectRank::PullRequest {
            return None;
        }
        attribution
            .secondary
            .iter()
            .chain(attribution.subject.iter())
            .find(|k| k.rank() == SubjectRank::Issue)
            .cloned()
    }

    /// Record the lower-ranked subjects a signal also named, so a later
    /// notification about the PR — or about the Slack thread — lands on the same
    /// place rather than starting a second card.
    fn link_secondaries(&self, owner: &SubjectKey, attribution: &Attribution) -> Result<()> {
        for lower in &attribution.secondary {
            if lower == owner {
                continue;
            }
            // A PR named alongside its issue is filed under it. Recorded even
            // though the PR has no card yet — the signal that reveals the closing
            // keyword usually belongs to the issue, so waiting for the PR's card
            // would mean never recording the link at all.
            if owner.rank() == SubjectRank::Issue && lower.rank() == SubjectRank::PullRequest {
                self.store
                    .set_subject_parent(lower.as_str(), Some(owner.as_str()))?;
                continue;
            }
            // A Slack thread that resolved upward is subordinate: its content becomes
            // context on the owner, and it must not keep a card of its own. This is the
            // late-resolution case — an alert thread that names the issue on message
            // twelve, by which point it already has a card, a summary, and possibly its
            // own root-cause report.
            //
            // `merge_subject_into` moves the signals as well as setting the pointer. A
            // pointer on its own would hide the card *and* everything attributed to it,
            // because the board filters out merged-away subjects.
            if lower.rank() == SubjectRank::SlackThread
                && self.store.get_subject(lower.as_str())?.is_some()
            {
                let moved = self
                    .store
                    .merge_subject_into(lower.as_str(), owner.as_str())?;
                self.board.refresh_metadata(owner)?;
                debug!("demoted {lower} under {owner}, moving {moved} signal(s)");
            }
        }
        Ok(())
    }

    /// Re-create metadata for any subject whose row was removed while its signals
    /// remain. Lossless: membership lives on the signals.
    pub fn repair_orphaned_subjects(&self) -> Result<usize> {
        let keys = self.store.orphaned_subject_keys()?;
        let mut repaired = 0;
        for raw in &keys {
            let Ok(key) = SubjectKey::parse(raw) else {
                continue;
            };
            let signals = self.store.signals_for_subject(raw)?;
            let Some(first) = signals.first() else {
                continue;
            };
            let mut subject = Subject::new(key.clone(), first, Utc::now());
            subject.created_at = first.occurred_at;
            subject.summary = Some(super::projection::deterministic_summary(&signals));
            self.store.upsert_subject(&subject)?;
            repaired += 1;
        }
        Ok(repaired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{ResolutionKey, Severity, SignalKind, Source};

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().unwrap())
    }

    fn sig(ext: &str, keys: Vec<(&str, &str)>) -> Signal {
        Signal {
            id: Signal::make_id(Source::GitHub, ext, None),
            source: Source::GitHub,
            external_id: ext.into(),
            version: None,
            kind: SignalKind::Mention,
            title: format!("signal {ext}"),
            body: None,
            url: None,
            actor: None,
            keys: keys
                .into_iter()
                .map(|(k, v)| ResolutionKey::new(k, v))
                .collect(),
            severity: Severity::Notice,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: serde_json::json!({}),
            tags: Vec::new(),
        }
    }

    /// A closed issue or merged PR leaves the board on its own.
    ///
    /// The reconciler has always recorded this — `upstream_gone` is set when an item drops out of
    /// the poll's listing — and the active-board filter never read it, looking only at `same_as`
    /// and operator triage. So merged PRs and closed issues stayed on the board indefinitely.
    #[test]
    fn work_whose_upstream_is_gone_drops_off_the_active_board() {
        let store = store();
        let a = Attributor::new(store.clone());

        // The external ids the participation watcher actually emits — the `assigned/` prefix is
        // what tells the reconciler which half of the GitHub signals this snapshot covers.
        let live = sig(
            &crate::watchers::assigned::external_id("o/r", 412),
            vec![("issue", "o/r#412")],
        );
        store.insert_signal(&live).unwrap();
        a.attach(&live).unwrap();
        let merged = sig(
            &crate::watchers::assigned::external_id("o/r", 987),
            vec![("pr", "o/r#987")],
        );
        store.insert_signal(&merged).unwrap();
        a.attach(&merged).unwrap();

        let active: Vec<String> = a
            .subject_views(true)
            .unwrap()
            .into_iter()
            .map(|v| v.subject.key.into_string())
            .collect();
        assert_eq!(active.len(), 2, "both are live to begin with: {active:?}");

        // The PR merges: the next poll no longer lists it, and the reconciler marks it gone.
        store
            .resolve_missing_assigned_issues(
                &[crate::watchers::assigned::external_id("o/r", 412)]
                    .into_iter()
                    .collect(),
            )
            .unwrap();

        let active: Vec<String> = a
            .subject_views(true)
            .unwrap()
            .into_iter()
            .map(|v| v.subject.key.into_string())
            .collect();
        assert!(
            active.iter().any(|k| k == "o/r#412"),
            "the open issue must stay: {active:?}"
        );
        assert!(
            !active.iter().any(|k| k.contains("987")),
            "a merged PR must not linger on the board: {active:?}"
        );

        // ...and it is still there when the operator asks for everything, because it happened.
        let all = a.subject_views(false).unwrap();
        assert_eq!(all.len(), 2, "history is not deleted, only hidden");
    }

    #[test]
    fn two_signals_about_one_issue_land_on_one_subject() {
        let store = store();
        let a = Attributor::new(store.clone());
        // A review request naming the PR and the issue it closes, then a CI failure
        // naming only the PR. Under the old entity-overlap grouping these could
        // fragment; keyed on identity they cannot.
        let s1 = sig("1", vec![("pr", "o/r#987"), ("issue", "o/r#412")]);
        let s2 = sig("2", vec![("pr", "o/r#987"), ("branch", "o/r@fix")]);
        store.insert_signal(&s1).unwrap();
        store.insert_signal(&s2).unwrap();

        assert_eq!(
            a.attach(&s1).unwrap(),
            Some(SubjectKey::issue("o/r", 412)),
            "the issue outranks the PR"
        );
        assert_eq!(
            a.attach(&s2).unwrap(),
            Some(SubjectKey::pull_request("o/r", 987))
        );
        // ...and the PR is filed under the issue, so the board shows one card.
        let pr = store.get_subject("o/r!987").unwrap().unwrap();
        assert_eq!(pr.parent, Some(SubjectKey::issue("o/r", 412)));
        assert_eq!(
            store.subject_children("o/r#412").unwrap(),
            vec![SubjectKey::pull_request("o/r", 987)]
        );
    }

    #[test]
    fn an_unresolvable_signal_gets_no_subject() {
        let store = store();
        let a = Attributor::new(store.clone());
        let s = sig("3", vec![("repo", "o/r"), ("branch", "o/r@main")]);
        store.insert_signal(&s).unwrap();
        assert_eq!(a.attach(&s).unwrap(), None);
        assert!(store.list_subjects().unwrap().is_empty());
    }

    #[test]
    fn attaching_is_idempotent() {
        let store = store();
        let a = Attributor::new(store.clone());
        let s = sig("4", vec![("issue", "o/r#1")]);
        store.insert_signal(&s).unwrap();
        let first = a.attach(&s).unwrap().unwrap();
        let stored = store.get_signal(&s.id).unwrap().unwrap();
        // Second pass over the stored signal (which now carries its subject) must
        // not move it or mint anything new.
        assert_eq!(a.attach(&stored).unwrap(), Some(first));
        assert_eq!(store.list_subjects().unwrap().len(), 1);
    }

    #[test]
    fn two_alerts_about_one_environment_merge_deterministically() {
        let store = store();
        let a = Attributor::new(store.clone());
        let mut first = sig("5", vec![("environment", "env-2abc")]);
        first
            .keys
            .push(ResolutionKey::new("slack_thread", "C1/100.1"));
        let mut second = sig("6", vec![("environment", "env-2abc")]);
        second
            .keys
            .push(ResolutionKey::new("slack_thread", "C1/200.2"));
        store.insert_signal(&first).unwrap();
        store.insert_signal(&second).unwrap();

        let k1 = a.attach(&first).unwrap().unwrap();
        let k2 = a.attach(&second).unwrap().unwrap();
        // Different Slack threads, same customer environment: one subject, no LLM.
        assert_eq!(k1, k2);
        assert_eq!(store.list_subjects().unwrap().len(), 1);
    }

    #[test]
    fn activity_forwards_through_a_merge_pointer() {
        let store = store();
        let a = Attributor::new(store.clone());
        let s1 = sig("7", vec![("issue", "o/r#1")]);
        store.insert_signal(&s1).unwrap();
        a.attach(&s1).unwrap();
        // #1 is merged into #2 (a duplicate); later activity about #1 must land on #2
        // rather than reviving a card the operator has already collapsed.
        store
            .upsert_subject(&Subject::new(SubjectKey::issue("o/r", 2), &s1, Utc::now()))
            .unwrap();
        store.merge_subject_into("o/r#1", "o/r#2").unwrap();

        let s2 = sig("8", vec![("issue", "o/r#1")]);
        store.insert_signal(&s2).unwrap();
        assert_eq!(a.attach(&s2).unwrap(), Some(SubjectKey::issue("o/r", 2)));
    }

    /// The late-resolution case: an alert thread that names its issue on message twelve.
    ///
    /// Demoting it must move its signals, not just set the pointer. The board hides a
    /// merged-away subject, so a pointer on its own hides the card *and* everything
    /// attributed to it — eleven messages of incident history disappearing silently.
    #[test]
    fn demoting_a_slack_thread_moves_its_signals_rather_than_hiding_them() {
        let store = store();
        let a = Attributor::new(store.clone());

        // Messages one and two: nothing but the conversation, so it owns them.
        let mut first = sig("1", vec![("slack_thread", "C1/100.1")]);
        first.source = Source::Slack;
        let mut second = sig("2", vec![("slack_thread", "C1/100.1")]);
        second.source = Source::Slack;
        store.insert_signal(&first).unwrap();
        store.insert_signal(&second).unwrap();
        let conversation = a.attach(&first).unwrap().expect("attributed");
        a.attach(&second).unwrap();
        assert_eq!(
            store
                .signals_for_subject(conversation.as_str())
                .unwrap()
                .len(),
            2
        );

        // Message three finally names the issue.
        let mut third = sig(
            "3",
            vec![("slack_thread", "C1/100.1"), ("issue", "o/r#412")],
        );
        third.source = Source::Slack;
        store.insert_signal(&third).unwrap();
        let issue = a.attach(&third).unwrap().expect("attributed");
        assert_eq!(issue, SubjectKey::issue("o/r", 412));

        // The conversation is demoted...
        let demoted = store.get_subject(conversation.as_str()).unwrap().unwrap();
        assert_eq!(demoted.same_as, Some(issue.clone()));
        // ...and all three messages are now on the issue, where the board can see them.
        assert_eq!(store.signals_for_subject(issue.as_str()).unwrap().len(), 3);
        assert!(store
            .signals_for_subject(conversation.as_str())
            .unwrap()
            .is_empty());

        // And the board shows exactly one card for the work.
        let active: Vec<_> = a
            .subject_views(true)
            .unwrap()
            .into_iter()
            .map(|v| v.subject.key)
            .collect();
        assert_eq!(active, vec![issue]);
    }

    /// The message *after* a demotion must still forward.
    ///
    /// The demoted subject keeps a row with no signals, which looks exactly like an empty
    /// subject to the metadata refresh — and deleting it drops the `same_as` pointer, so
    /// the next message mints a fresh card for work already filed under the issue. The
    /// symptom appears one message after the merge, nowhere near the cause.
    #[test]
    fn the_message_after_a_demotion_still_forwards() {
        let store = store();
        let a = Attributor::new(store.clone());

        let mut first = sig("1", vec![("slack_thread", "C1/100.1")]);
        first.source = Source::Slack;
        store.insert_signal(&first).unwrap();
        let conversation = a.attach(&first).unwrap().expect("attributed");

        let mut naming = sig("2", vec![("slack_thread", "C1/100.1"), ("issue", "o/r#77")]);
        naming.source = Source::Slack;
        store.insert_signal(&naming).unwrap();
        let issue = a.attach(&naming).unwrap().expect("attributed");

        // The tombstone survives a metadata refresh over the now-empty subject.
        a.refresh_subject_metadata(conversation.as_str()).unwrap();
        let tombstone = store
            .get_subject(conversation.as_str())
            .unwrap()
            .expect("the forwarding tombstone must survive");
        assert_eq!(tombstone.same_as, Some(issue.clone()));

        // ...so the next message forwards instead of minting a second card.
        let mut third = sig("3", vec![("slack_thread", "C1/100.1")]);
        third.source = Source::Slack;
        store.insert_signal(&third).unwrap();
        assert_eq!(a.attach(&third).unwrap(), Some(issue.clone()));
        assert_eq!(store.list_subjects().unwrap().len(), 2, "no duplicate card");
        assert_eq!(store.signals_for_subject(issue.as_str()).unwrap().len(), 3);
    }

    #[test]
    fn a_merge_cycle_does_not_hang_ingest() {
        let store = store();
        let a = Attributor::new(store.clone());
        let s = sig("9", vec![("issue", "o/r#1")]);
        store.insert_signal(&s).unwrap();
        a.attach(&s).unwrap();
        store
            .upsert_subject(&Subject::new(SubjectKey::issue("o/r", 2), &s, Utc::now()))
            .unwrap();
        // Two racing merges could write this pair; ingest must survive it.
        store.merge_subject_into("o/r#1", "o/r#2").unwrap();
        store.merge_subject_into("o/r#2", "o/r#1").unwrap();
        let s2 = sig("10", vec![("issue", "o/r#1")]);
        store.insert_signal(&s2).unwrap();
        assert!(a.attach(&s2).unwrap().is_some());
    }

    #[test]
    fn re_entering_a_snoozed_subject_un_mutes_it() {
        let store = store();
        let a = Attributor::new(store.clone());
        let s1 = sig("11", vec![("issue", "o/r#5")]);
        store.insert_signal(&s1).unwrap();
        let key = a.attach(&s1).unwrap().unwrap();
        store
            .set_handled(key.as_str(), Handled::Snoozed, None)
            .unwrap();

        // Ordinary activity leaves it muted...
        let mut quiet = sig("12", vec![("issue", "o/r#5")]);
        quiet.kind = SignalKind::Other;
        store.insert_signal(&quiet).unwrap();
        a.attach(&quiet).unwrap();
        assert_eq!(
            store.get_subject(key.as_str()).unwrap().unwrap().handled,
            Handled::Snoozed
        );

        // ...but the operator being asked into it brings it back.
        let mut engaged = sig("13", vec![("issue", "o/r#5")]);
        engaged.kind = SignalKind::Mention;
        store.insert_signal(&engaged).unwrap();
        a.attach(&engaged).unwrap();
        assert_eq!(
            store.get_subject(key.as_str()).unwrap().unwrap().handled,
            Handled::Open
        );
    }
}
