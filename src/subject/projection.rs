//! The board projection: turning stored subjects and signals into what the UI and
//! MCP read.
//!
//! This is a *read* path over SQLite, deliberately. Restate virtual-object state is
//! addressable only by key, and the board is a cross-key query — "every subject
//! needing attention, ranked, filtered by source and severity". So subject handlers
//! write their state through to these tables (Phase 2) and every list view stays a
//! `SELECT`.
//!
//! Derived values — attention, severity, decorations — are computed here on every
//! read rather than stored. A stored "needs attention" flag drifts the moment a
//! subject is acknowledged elsewhere, and a stored "AI done" flag lies after a
//! failed pass. Computing them from the artifacts that actually exist means the
//! badge can't disagree with the panel underneath it.

use anyhow::Result;
use chrono::Utc;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::{union_keys, Attention, Decorations, Handled, Subject, SubjectKey, SubjectView};
use crate::signal::{Severity, Signal};
use crate::store::Store;

pub struct Board {
    pub(crate) store: Arc<Store>,
}

impl Board {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Recompute a subject's deterministic title/summary and bump `updated_at`,
    /// preserving any LLM summary already recorded (only fills a blank one).
    pub fn refresh_metadata(&self, key: &SubjectKey) -> Result<()> {
        let signals = self.store.signals_for_subject(key.as_str())?;
        if signals.is_empty() {
            self.store.delete_subject_if_empty(key.as_str())?;
            return Ok(());
        }
        let Some(mut subject) = self.store.get_subject(key.as_str())? else {
            return Ok(());
        };
        subject.updated_at = Utc::now();
        subject.title = super::title_from(&signals[0]);
        if subject.summary.is_none() || subject.last_reasoned_at.is_none() {
            subject.summary = Some(deterministic_summary(&signals));
        }
        self.store.upsert_subject(&subject)?;
        Ok(())
    }

    pub fn view(&self, key: &SubjectKey) -> Result<Option<SubjectView>> {
        let Some(subject) = self.store.get_subject(key.as_str())? else {
            return Ok(None);
        };
        let signals = self.store.signals_for_subject(key.as_str())?;
        let keys = union_keys(&signals);
        let severity = signals
            .iter()
            .map(|s| s.severity)
            .max()
            .unwrap_or(Severity::Info);
        let edges = self.store.edges_for_subject(key.as_str())?;
        let context = self.store.subject_context(key.as_str())?;
        let children = self.store.subject_children(key.as_str())?;
        // The PR critiques are keyed by the issue, so an issue view carries its
        // attempts and a PR view carries none of its own — which is the nesting the
        // board renders.
        let pull_requests = self.store.pr_fixes_for_issue(key.as_str())?;
        let explanations = self.store.explanations(key.as_str())?;
        // Passed in rather than re-queried: `attention` needs the same critiques to
        // count judged PRs and attribute their cost, and the board renders every subject
        // on every push.
        let attention = self.attention(&subject, &signals, severity, &pull_requests)?;
        // Only a *reasoned* summary can produce a headline. `deterministic_summary` is a
        // preview of the newest event body — fine as a placeholder in the detail view,
        // but presenting a chat message's first sentence as "what this needs from you"
        // would be a claim the board hasn't earned.
        let headline = subject.last_reasoned_at.and_then(|_| {
            // The title is passed so a headline that merely restates it is refused — the
            // board's one line for *new* information must not spend it repeating the line
            // above. See `subject::headline_is_noise`.
            super::headline_for(subject.summary.as_deref(), &subject.title)
        });
        // The summary the *view* renders, with its dead blocks dropped. A display transform:
        // the stored summary keeps every word, because the explainer and the MCP surface read it
        // and they are not the detail view. See `subject::trim_summary`.
        let mut subject = subject;
        if let Some(full) = subject.summary.as_deref() {
            let trimmed = super::trim_summary(full, &subject.title);
            if trimmed.len() != full.trim().len() {
                subject.summary = Some(trimmed);
            }
        }
        let review = review_state(&signals, &pull_requests);
        let cleared = gates_passed(&signals, &pull_requests);
        Ok(Some(SubjectView {
            subject,
            headline,
            review_state: review,
            gates_passed: cleared,
            signals,
            keys,
            severity,
            edges,
            context,
            children,
            pull_requests,
            explanations,
            attention,
        }))
    }

    /// Every subject as a view, newest activity first.
    ///
    /// `active_only` drops handled work and anything merged away: a subject with
    /// `same_as` set forwards its activity to the canonical one, so showing both
    /// would be showing the same work twice.
    pub fn views(&self, active_only: bool) -> Result<Vec<SubjectView>> {
        let mut out = Vec::new();
        for s in self.store.list_subjects()? {
            if active_only
                && (s.same_as.is_some()
                    || matches!(s.handled, Handled::Resolved | Handled::Snoozed))
            {
                continue;
            }
            if let Some(view) = self.view(&s.key)? {
                // Finished upstream, and nothing left unread about it. Both halves are
                // required, and the second one used to be the whole test — which was wrong.
                //
                // `upstream_gone` records that a **notification is no longer unread**, not that
                // the work is over: `resolve_missing_github_notifications` sets it for any
                // signal absent from GitHub's *unread* feed. For a pull request you opened
                // yourself that is every signal it will ever have — you get no review request,
                // no mention, no assignment on your own PR, only CI notifications, and those go
                // read within minutes. So your own PRs appeared while CI was still unread and
                // then silently vanished, still open, still yours. Measured on the live board:
                // of sixteen subjects this filter was hiding, ten were open and four of those
                // were pull requests the operator had just opened.
                //
                // So the drop now needs positive evidence the work is over — see
                // [`upstream_finished`] — with "every notification read" kept alongside it, so a
                // comment on a merged PR is still news rather than being swallowed by the merge.
                //
                // Guarded on non-empty, because "no signals" is a subject mid-creation rather than
                // one whose work is finished.
                if active_only
                    && !view.signals.is_empty()
                    && view.signals.iter().all(|sig| sig.upstream_gone)
                    && upstream_finished(&view.signals)
                {
                    continue;
                }
                out.push(view);
            }
        }
        Ok(out)
    }

    /// Derive the two things the board actually reports: whether this needs the
    /// operator, and what the AI has made of it.
    fn attention(
        &self,
        subject: &Subject,
        signals: &[Signal],
        severity: Severity,
        pull_requests: &[crate::store::PrFix],
    ) -> Result<Attention> {
        let key = subject.key.as_str();
        let mut decorated = Decorations {
            // `last_reasoned_at` distinguishes a real grounded summary from the
            // deterministic one-liner every subject gets for free — but a pass that
            // stored the prompt back at us set that timestamp too, so the text has to
            // agree that it's a summary before the facet claims one exists.
            summary: subject.last_reasoned_at.is_some()
                && super::is_usable_summary(subject.summary.as_deref().unwrap_or_default()),
            tags: !subject.tags.is_empty(),
            dashboard: self
                .store
                .browser_investigations_for_subject(key)?
                .iter()
                .any(|i| i.findings.as_deref().is_some_and(|f| !f.trim().is_empty())),
            root_cause: self.store.get_root_cause(key)?.map(|r| r.status.clone()),
            ..Default::default()
        };
        let triage_rows = self.store.issue_triage_for_subject(key)?;
        decorated.triage = triage_rows.first().map(|t| t.status.clone());
        decorated.prs_judged = pull_requests.len();
        // The tier that answered is recorded per judgment, so cost attribution is real
        // rather than assumed.
        for fix in pull_requests {
            match fix.analyzed_by.as_deref() {
                Some("local") | None => decorated.local_passes += 1,
                _ => decorated.cloud_passes += 1,
            }
        }
        // Tag classification, triage, and root-cause searching are on-device by
        // policy; summaries go through the routed tier.
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
        if decorated.dashboard {
            decorated.cloud_passes += 1;
        }

        // Handled work never asks for attention — that is what handling it meant.
        let review = review_state(signals, pull_requests);
        let (needed, reason) = if matches!(subject.handled, Handled::Resolved | Handled::Snoozed) {
            (false, None)
        } else if review.as_deref() == Some("changes_requested") {
            // Somebody is saying no. That outranks every other reason to look at it.
            (true, Some("changes requested".to_string()))
        } else if gates_passed(signals, pull_requests) {
            // Every gate cleared: a human said yes and nothing is still failing. Being
            // *in* a pull request stops being a reason to look at it once the answer to
            // "does this need me" has been given by a human and by CI.
            //
            // Said out loud rather than left as a bare `None`. "Nothing is asking" and
            // "this passed everything and is ready" are different states that the board
            // was rendering identically, and the second one is the one worth seeing.
            (false, Some("all gates passed — nothing to do".to_string()))
        } else if self
            .store
            .list_hints(Some(key))?
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
}

/// The pull request's review decision, from the most recent signal that carries one.
///
/// The watcher records it per notification (see `watchers::github::reduce_reviews`), so
/// the newest signal holding the field is the current answer — an older notification
/// still says "changes_requested" long after the changes were made and approved.
///
/// `upstream_gone` signals are deliberately included: a notification that has since
/// cleared still reported the review state at the time, and it is the most recent thing
/// anyone said about the review. Skipping them would lose an approval the moment GitHub
/// marked the notification read.
pub fn review_state(signals: &[Signal], pull_requests: &[crate::store::PrFix]) -> Option<String> {
    // Newest first, then take the first signal that actually carries the field. Most
    // don't — a CI notification has no review decision — and absence is not "unreviewed",
    // so the search continues rather than stopping at the newest signal.
    let mut newest: Vec<&Signal> = signals.iter().collect();
    newest.sort_by_key(|s| std::cmp::Reverse(s.occurred_at));
    let from_signals = newest.into_iter().find_map(|s| {
        s.raw
            .get("review_state")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    });
    if from_signals.is_some() {
        return from_signals;
    }
    // Fallback: a pull request reviewed before the watcher started recording the
    // decision has no signal carrying it, and may never get another notification to fix
    // that. The PR-fix pass reads those same reviews and stores the verdict, so read it
    // back rather than leaving the board permanently wrong about work already signed off.
    //
    // Second, not first, because it is the older of the two: a notification's copy is
    // from the moment it fired, where this one is from whenever the analysis last ran.
    // One block still outranks any approval, as in the live reduction.
    let mut fallback = None;
    for state in pull_requests
        .iter()
        .filter_map(|f| f.review_state.as_deref())
    {
        if state == "changes_requested" {
            return Some(state.to_string());
        }
        if fallback.is_none() || state == "approved" {
            fallback = Some(state.to_string());
        }
    }
    fallback
}

/// Is the work itself over upstream — merged, closed, or no longer yours?
///
/// Called only for a subject whose every signal has already gone `upstream_gone`, and its job
/// is to decide what that *means*. Two reconcilers set that flag and they carry very different
/// weight:
///
/// - **A notification left the unread feed.** You read it. That is all it says. For a pull
///   request you opened yourself it is every signal there will ever be — no review request,
///   no mention, no assignment, just CI — so treating it as "finished" made your own open PRs
///   disappear from the board within minutes of opening them.
/// - **An assigned issue left the assigned listing** (`assigned/` external id, see
///   [`crate::store::Store::ASSIGNED_PREFIX`]). It closed, or somebody else took it. That
///   genuinely is the work leaving your plate, and it is the case the board has always drawn
///   this conclusion from correctly.
///
/// So: an explicit terminal `state` decides it, an assigned card going quiet decides it, and a
/// read notification decides nothing.
///
/// Absence of `state` is **not** finished. A signal enrichment never filled in tells us
/// nothing, and treating unknown as over is exactly how open work vanishes. Erring the other
/// way leaves a merged thing on the board until the operator resolves it — a button, not a
/// mystery.
pub fn upstream_finished(signals: &[Signal]) -> bool {
    // An assigned card that fell out of the listing: off your plate, whatever else is known.
    if signals.iter().any(|s| {
        s.external_id
            .starts_with(crate::store::Store::ASSIGNED_PREFIX)
    }) {
        return true;
    }
    let mut newest: Vec<&Signal> = signals.iter().collect();
    newest.sort_by_key(|s| std::cmp::Reverse(s.occurred_at));
    newest
        .into_iter()
        .find_map(|s| s.raw.get("state").and_then(|v| v.as_str()))
        .is_some_and(|state| matches!(state, "merged" | "closed"))
}

/// Has this pull request cleared every gate that could make it somebody's business?
///
/// Two conditions, and they are the two things a merge actually waits on:
///
/// 1. **A human said yes.** `approved` here is GitHub's own review *decision*, which
///    already accounts for how many approvals the branch requires and for CODEOWNERS —
///    so one `approved` is "all the required approvals", not "somebody clicked approve".
/// 2. **Nothing is currently failing.** Judged on the signals still standing, because a
///    notification GitHub has marked read is the source saying the matter is closed.
///    This is the part that was wrong: severity was maxed over *every* signal ever
///    attached, so a CI failure that had since been fixed — in the case that prompted
///    this, a workflow run that was *skipped* — pinned an approved PR in Decide for
///    good. A failure that is genuinely still red is still standing, and still counts.
///
/// Deliberately **not** required: positive evidence that CI ran and went green. Plenty of
/// pull requests never produce a CI notification at all, and demanding one would leave
/// most approved work unmarked — which is the opposite of the point. "Approved and
/// nothing failing" is the claim; "verified green" is not.
pub fn gates_passed(signals: &[Signal], pull_requests: &[crate::store::PrFix]) -> bool {
    if review_state(signals, pull_requests).as_deref() != Some("approved") {
        return false;
    }
    standing_severity(signals) < Severity::Warning
}

/// The worst severity among signals that have **not** cleared upstream.
///
/// `SubjectView::severity` is the max over all of them, which is right for reporting what
/// happened to this work; it is wrong for deciding what is asking for you *now*.
fn standing_severity(signals: &[Signal]) -> Severity {
    signals
        .iter()
        .filter(|s| !s.upstream_gone)
        .map(|s| s.severity)
        .max()
        .unwrap_or(Severity::Info)
}

pub fn deterministic_summary(signals: &[Signal]) -> String {
    // Lead with real content — the newest message — rather than a key dump, which
    // is useless for a chat message. This is only the fallback headline until the
    // LLM writes a proper summary; keep it a single readable line.
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
    use crate::signal::{Severity, SignalKind, Source};
    use chrono::TimeZone;

    /// A PR notification carrying a review decision, at a given minute.
    fn sig(minute: u32, review: Option<&str>) -> Signal {
        Signal {
            id: format!("s{minute}"),
            source: Source::GitHub,
            external_id: format!("n{minute}"),
            version: None,
            kind: SignalKind::ReviewRequested,
            title: "linkerd multi replica".into(),
            body: None,
            url: None,
            actor: Some("lukebond".into()),
            keys: Vec::new(),
            severity: Severity::Notice,
            upstream_gone: false,
            occurred_at: Utc.with_ymd_and_hms(2026, 7, 28, 7, minute, 0).unwrap(),
            ingested_at: Utc::now(),
            subject: None,
            raw: match review {
                Some(r) => serde_json::json!({ "review_state": r }),
                None => serde_json::json!({ "subject_type": "CheckSuite" }),
            },
            tags: Vec::new(),
        }
    }

    #[test]
    fn no_signal_carries_a_review_decision() {
        assert_eq!(review_state(&[sig(1, None)], &[]), None);
        assert_eq!(review_state(&[], &[]), None);
    }

    #[test]
    fn the_newest_decision_wins() {
        // The whole point: an older notification still says "changes_requested" long
        // after the changes were made and the PR approved. Reading the first one found
        // would leave an approved PR asking for attention for ever.
        let signals = [
            sig(10, Some("changes_requested")),
            sig(40, Some("approved")),
            sig(20, Some("commented")),
        ];
        assert_eq!(review_state(&signals, &[]).as_deref(), Some("approved"));
    }

    #[test]
    fn a_later_signal_without_a_decision_does_not_erase_one() {
        // CI notifications carry no review field. Absence is not "unreviewed", so the
        // search falls through to the newest signal that actually has one.
        let signals = [sig(10, Some("approved")), sig(50, None)];
        assert_eq!(review_state(&signals, &[]).as_deref(), Some("approved"));
    }

    /// A PR-fix row carrying the reviewers' recorded verdict.
    fn fix(review_state: &str) -> crate::store::PrFix {
        crate::store::PrFix {
            issue_key: "restatedev/nuon-byoc!140".into(),
            pr_repo: "restatedev/nuon-byoc".into(),
            pr_number: 140,
            pr_title: "Restate-cloud image bump to PR1200".into(),
            pr_url: None,
            pr_author: Some("darkmuggle".into()),
            pr_state: Some("open".into()),
            files: Vec::new(),
            verdict: "fixes".into(),
            confidence: 0.95,
            implementation: None,
            critique: None,
            conversation: None,
            review_state: Some(review_state.to_string()),
            also_fixes: Vec::new(),
            analyzed_by: Some("local".into()),
            created_at: "2026-07-27T00:00:00Z".into(),
            updated_at: "2026-07-27T00:00:00Z".into(),
        }
    }

    #[test]
    fn an_approval_is_read_back_from_the_stored_pr_fix() {
        // The historical case: the PR was approved before the watcher recorded review
        // state, so no signal carries it and none ever will unless the PR moves again.
        // The PR-fix pass reads those reviews too, so the verdict is recoverable.
        let stored = fix("approved");
        assert_eq!(
            review_state(&[sig(1, None)], &[stored]).as_deref(),
            Some("approved"),
        );
    }

    #[test]
    fn a_signal_decision_beats_the_stored_conversation() {
        // The signal is current; the stored row is a snapshot from whenever the analysis
        // last ran. An approval since withdrawn must not win.
        let stored = fix("approved");
        assert_eq!(
            review_state(&[sig(9, Some("changes_requested"))], &[stored]).as_deref(),
            Some("changes_requested"),
        );
    }

    #[test]
    fn a_block_in_a_stored_row_outranks_an_approval() {
        let stored = vec![fix("approved"), fix("changes_requested")];
        assert_eq!(
            review_state(&[sig(1, None)], &stored).as_deref(),
            Some("changes_requested"),
        );
    }

    #[test]
    fn an_unreviewed_pr_fix_row_says_nothing() {
        let mut stored = fix("approved");
        stored.review_state = None;
        assert_eq!(review_state(&[sig(1, None)], &[stored]), None);
    }

    /// End-to-end through a real store: an approved pull request reports "approved" and
    /// stops asking for the operator, which is the whole behaviour.
    #[test]
    fn an_approved_pull_request_stops_asking_for_attention() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let board = Board::new(store.clone());
        let key = SubjectKey::pull_request("restatedev/nuon-byoc", 140);

        let mut signal = sig(10, None);
        signal.subject = Some(key.as_str().to_string());
        // `is_user_engaged` on a review-requested signal is what used to make this need
        // attention for ever, regardless of anyone having reviewed it.
        store.insert_signal(&signal).unwrap();
        store
            .set_signal_subject(&signal.id, Some(key.as_str()))
            .unwrap();
        let subject = Subject::new(key.clone(), &signal, Utc::now());
        store.upsert_subject(&subject).unwrap();

        // Unreviewed: it wants you.
        let view = board.view(&key).unwrap().expect("view");
        assert_eq!(view.review_state, None);
        assert!(
            view.attention.needed,
            "an unreviewed PR should want attention"
        );

        // Approved: it does not, and it says why.
        store.put_pr_fix(&fix("approved")).unwrap();
        let view = board.view(&key).unwrap().expect("view");
        assert_eq!(view.review_state.as_deref(), Some("approved"));
        assert!(
            !view.attention.needed,
            "an approved PR should not want attention: {:?}",
            view.attention.reason
        );

        // Blocked: it wants you again, and for the right reason rather than the generic
        // "you're in this one".
        let mut blocked = fix("changes_requested");
        blocked.pr_number = 141;
        store.put_pr_fix(&blocked).unwrap();
        let view = board.view(&key).unwrap().expect("view");
        assert_eq!(view.review_state.as_deref(), Some("changes_requested"));
        assert!(view.attention.needed);
        assert_eq!(view.attention.reason.as_deref(), Some("changes requested"));
    }

    /// A pull request you opened yourself must not vanish because its CI notification was
    /// read.
    ///
    /// The case reported from the live board: four pull requests opened within an hour, all
    /// present in the store, none on the board. Their only signals were CI notifications —
    /// authoring a PR gets you no review request, no mention, no assignment — and once those
    /// went read every signal was `upstream_gone`, which the active filter treated as "this
    /// work is over".
    #[test]
    fn an_open_pull_request_survives_all_its_notifications_being_read() {
        let read_ci = |minute: u32, state: &str| {
            let mut s = sig(minute, None);
            s.kind = SignalKind::CiFailure;
            s.upstream_gone = true;
            s.raw = serde_json::json!({ "state": state, "subject_type": "CheckSuite" });
            s
        };

        // Still open: read notifications say nothing about whether the work is done.
        assert!(!upstream_finished(&[
            read_ci(10, "open"),
            read_ci(20, "open")
        ]));
        // Merged: over, and the board should stop carrying it.
        assert!(upstream_finished(&[
            read_ci(10, "open"),
            read_ci(20, "merged")
        ]));
        // A closed issue is equally over.
        assert!(upstream_finished(&[read_ci(30, "closed")]));

        // The newest report of state wins — a merge followed by a reopen is open again.
        assert!(!upstream_finished(&[
            read_ci(40, "merged"),
            read_ci(50, "open")
        ]));

        // No state recorded is not evidence of anything. Erring here is what made open work
        // disappear, so unknown must read as *not* finished.
        assert!(!upstream_finished(&[sig(10, None)]));
        assert!(!upstream_finished(&[]));

        // The other reconciler still counts. An `assigned/` card only goes quiet when the
        // issue closed or somebody else took it — that *is* the work leaving your plate, and
        // it needs no `state` to say so.
        let mut assigned = sig(10, None);
        assigned.external_id = format!("{}o/r#412", crate::store::Store::ASSIGNED_PREFIX);
        assigned.upstream_gone = true;
        assigned.raw = serde_json::json!({});
        assert!(
            upstream_finished(&[assigned]),
            "an assigned card that left the listing is off your plate"
        );
    }

    /// End to end: a subject whose notifications are all read stays on the active board while
    /// it is open, and leaves once it is merged.
    #[test]
    fn the_active_board_keeps_open_work_and_drops_finished_work() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let board = Board::new(store.clone());
        let key = SubjectKey::pull_request("restatedev/restate-cloud", 1282);

        let mut signal = sig(10, None);
        signal.subject = Some(key.as_str().to_string());
        signal.kind = SignalKind::CiFailure;
        // Read, as a CI notification on your own PR is within minutes.
        signal.upstream_gone = true;
        signal.raw = serde_json::json!({ "state": "open" });
        store.insert_signal(&signal).unwrap();
        store
            .set_signal_subject(&signal.id, Some(key.as_str()))
            .unwrap();
        store
            .upsert_subject(&Subject::new(key.clone(), &signal, Utc::now()))
            .unwrap();

        let on_board = |board: &Board| {
            board
                .views(true)
                .unwrap()
                .iter()
                .any(|v| v.subject.key == key)
        };
        assert!(
            on_board(&board),
            "an open PR whose CI notification was read is still work"
        );

        // Merged: now it is over.
        let mut merged = sig(40, None);
        merged.id = "s-merged".into();
        merged.external_id = "n-merged".into();
        merged.subject = Some(key.as_str().to_string());
        merged.upstream_gone = true;
        merged.raw = serde_json::json!({ "state": "merged" });
        store.insert_signal(&merged).unwrap();
        store
            .set_signal_subject(&merged.id, Some(key.as_str()))
            .unwrap();
        assert!(
            !on_board(&board),
            "a merged PR with nothing unread has left the board"
        );
    }

    /// A CI failure that has since cleared upstream must not hold an approved pull
    /// request in Decide.
    ///
    /// This is the case that prompted the rule, taken from the live board:
    /// `restate-cloud!1263` was approved, its only warning was a `ci_failure`
    /// notification GitHub had already marked read — for a workflow run that was
    /// *skipped*, not failed — and it sat in Decide anyway, because severity was maxed
    /// over every signal ever attached rather than the ones still standing.
    #[test]
    fn a_cleared_ci_failure_does_not_hold_an_approved_pr() {
        let mut cleared = sig(5, None);
        cleared.kind = SignalKind::CiFailure;
        cleared.severity = Severity::Warning;
        cleared.upstream_gone = true;
        let approved = sig(30, Some("approved"));

        let signals = [cleared.clone(), approved.clone()];
        assert!(
            gates_passed(&signals, &[]),
            "an approved PR whose CI failure has cleared has nothing left to do",
        );

        // ...and a failure that is genuinely still red still counts, which is the half of
        // the rule that keeps it honest.
        let mut standing = cleared;
        standing.upstream_gone = false;
        assert!(
            !gates_passed(&[standing, approved], &[]),
            "an approved PR with CI still failing is not finished",
        );
    }

    /// The gate is both halves. Nothing failing is not enough on its own — an unreviewed
    /// pull request with green CI still needs a human to say yes.
    #[test]
    fn green_ci_alone_does_not_pass_the_gates() {
        assert!(!gates_passed(&[sig(10, None)], &[]));
        assert!(!gates_passed(&[sig(10, Some("commented"))], &[]));
        assert!(!gates_passed(&[], &[]));
    }

    /// End-to-end: the flag the board renders, and the reason it renders beside it.
    #[test]
    fn a_pr_that_cleared_its_gates_is_marked_as_such() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let board = Board::new(store.clone());
        let key = SubjectKey::pull_request("restatedev/restate-cloud", 1263);

        let mut signal = sig(10, None);
        signal.subject = Some(key.as_str().to_string());
        store.insert_signal(&signal).unwrap();
        store
            .set_signal_subject(&signal.id, Some(key.as_str()))
            .unwrap();
        store
            .upsert_subject(&Subject::new(key.clone(), &signal, Utc::now()))
            .unwrap();

        let view = board.view(&key).unwrap().expect("view");
        assert!(!view.gates_passed, "unreviewed: no claim to make");

        // Keyed to this subject — `pr_fixes_for_issue` is what the view reads.
        let mut approved = fix("approved");
        approved.issue_key = key.as_str().to_string();
        store.put_pr_fix(&approved).unwrap();
        let view = board.view(&key).unwrap().expect("view");
        assert!(view.gates_passed);
        assert!(!view.attention.needed);
        // Stated positively. "Nothing is asking" and "this passed everything" were
        // rendering identically, and the second is the one worth seeing.
        assert_eq!(
            view.attention.reason.as_deref(),
            Some("all gates passed — nothing to do"),
        );
    }
}
