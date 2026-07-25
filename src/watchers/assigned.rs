//! The assigned-issues watcher — every issue assigned to you gets a board card.
//!
//! The GitHub watcher next door polls `/notifications`, which is an *event* feed:
//! it tells you what changed. Assignment isn't an event, it's a standing state. An
//! issue assigned to you three weeks ago with no activity since produces no
//! notification at all, so it never reaches the board — which is exactly the issue
//! most likely to have fallen off your radar.
//!
//! So this watcher polls `/issues?filter=assigned` directly and emits a signal per
//! open assigned issue. Signals are keyed `assigned/owner/repo#N`, distinct from
//! the notification watcher's ids, so both can surface the same issue without one
//! suppressing the other; correlation then groups them into one thread through the
//! shared `issue` entity, so the board shows one card rather than two.
//!
//! Because the signal is re-emitted every poll and the store dedups on
//! `(source, external_id)`, an assigned issue stays on the board — with your triage
//! state intact — until it's closed or reassigned.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::time::Duration;
use tracing::debug;

use super::{PollBatch, SourceSnapshot, Watcher};
use crate::config::{self, Assigned as AssignedCfg};
use crate::github::{GithubClient, IssueHit};
use crate::signal::{Entity, Severity, Signal, SignalKind, Source, State};

/// Cap per poll. More assigned issues than this and the board is not the problem.
const MAX_ASSIGNED: usize = 100;

pub struct AssignedWatcher {
    client: GithubClient,
    interval: Duration,
}

impl AssignedWatcher {
    pub fn new(cfg: &AssignedCfg, token: String) -> Result<Self> {
        Ok(Self {
            client: GithubClient::new(token)?,
            interval: config::parse_duration(&cfg.poll_interval)
                .unwrap_or(Duration::from_secs(300)),
        })
    }
}

/// The stable id for an assigned-issue signal.
pub fn external_id(repo: &str, number: u64) -> String {
    format!("assigned/{repo}#{number}")
}

/// `owner/repo#number` — the key the triage store uses.
pub fn issue_key(repo: &str, number: u64) -> String {
    format!("{repo}#{number}")
}

/// Turn an assigned issue into a signal.
///
/// Severity is `Notice`, not `Warning`: an assigned issue is work you own, not
/// something on fire. Raising it higher would fire a macOS notification for every
/// open assignment on every restart.
pub fn signal_for(issue: &IssueHit) -> Signal {
    let external = external_id(&issue.repo, issue.number);
    let occurred = issue
        .updated_at
        .as_deref()
        .or(issue.created_at.as_deref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let mut entities = vec![
        Entity::new("repo", issue.repo.clone()),
        // The same entity shape the notification watcher emits, so a notification
        // about this issue correlates into the same thread instead of a second card.
        Entity::new("issue", format!("{}#{}", issue.repo, issue.number)),
    ];
    for label in &issue.labels {
        entities.push(Entity::new("label", label.clone()));
    }

    Signal {
        id: Signal::make_id(Source::GitHub, &external),
        source: Source::GitHub,
        external_id: external,
        kind: SignalKind::Assigned,
        title: format!(
            "Assigned: {} ({}#{})",
            issue.title, issue.repo, issue.number
        ),
        body: issue.body.clone(),
        url: Some(issue.url.clone()),
        actor: None,
        entities,
        severity: Severity::Notice,
        state: State::Unseen,
        occurred_at: occurred,
        ingested_at: Utc::now(),
        thread: None,
        raw: serde_json::json!({
            "assigned_issue": true,
            "repo": issue.repo,
            "number": issue.number,
            "issue_key": issue_key(&issue.repo, issue.number),
            "labels": issue.labels,
            "state": issue.state,
            "created_at": issue.created_at,
            "updated_at": issue.updated_at,
        }),
        tags: Vec::new(),
    }
}

#[async_trait]
impl Watcher for AssignedWatcher {
    fn name(&self) -> &'static str {
        "github-assigned"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn poll(&self) -> Result<PollBatch> {
        let issues = self.client.assigned_issues(MAX_ASSIGNED).await?;
        debug!("assigned: {} open issue(s)", issues.len());
        let signals: Vec<Signal> = issues.iter().map(signal_for).collect();
        // A full listing, so the reconciler can resolve issues that were closed or
        // unassigned since the last poll rather than leaving them on the board.
        let active_ids: BTreeSet<String> = signals.iter().map(|s| s.external_id.clone()).collect();
        Ok(PollBatch {
            signals,
            snapshot: Some(SourceSnapshot {
                source: Source::GitHub,
                active_ids,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> IssueHit {
        IssueHit {
            repo: "restatedev/restate".into(),
            number: 412,
            title: "Connection pool leaks under load".into(),
            state: "open".into(),
            kind: "issue".into(),
            url: "https://github.com/restatedev/restate/issues/412".into(),
            body: Some("The pool grows without bound".into()),
            labels: vec!["bug".into()],
            created_at: Some("2026-07-01T10:00:00Z".into()),
            updated_at: Some("2026-07-20T10:00:00Z".into()),
            closed_at: None,
        }
    }

    #[test]
    fn signal_is_stable_across_polls() {
        // Re-emitting must dedup, or every poll would create a new card.
        assert_eq!(signal_for(&hit()).id, signal_for(&hit()).id);
        assert_eq!(
            signal_for(&hit()).external_id,
            "assigned/restatedev/restate#412"
        );
    }

    /// The shared `issue` entity is what merges this card with any notification
    /// about the same issue, rather than showing the user two of them.
    #[test]
    fn carries_the_issue_entity_for_correlation() {
        let s = signal_for(&hit());
        assert!(s
            .entities
            .contains(&Entity::new("issue", "restatedev/restate#412")));
        assert!(s
            .entities
            .contains(&Entity::new("repo", "restatedev/restate")));
    }

    #[test]
    fn assigned_work_does_not_masquerade_as_an_incident() {
        let s = signal_for(&hit());
        assert_eq!(s.kind, SignalKind::Assigned);
        assert_eq!(
            s.severity,
            Severity::Notice,
            "an open assignment must not fire a Critical notification on every restart"
        );
    }

    #[test]
    fn triage_key_is_carried_for_the_worker() {
        let s = signal_for(&hit());
        assert_eq!(s.raw["issue_key"], "restatedev/restate#412");
        assert_eq!(s.raw["assigned_issue"], true);
    }

    #[test]
    fn occurred_at_follows_the_issue_not_the_poll() {
        let s = signal_for(&hit());
        assert_eq!(s.occurred_at.to_rfc3339(), "2026-07-20T10:00:00+00:00");
    }

    /// An issue with no timestamps must still produce a card, not vanish.
    #[test]
    fn missing_timestamps_fall_back_to_now() {
        let mut h = hit();
        h.updated_at = None;
        h.created_at = None;
        let s = signal_for(&h);
        assert_eq!(s.raw["number"].as_u64(), Some(412));
        assert!(s.occurred_at <= Utc::now());
    }
}
