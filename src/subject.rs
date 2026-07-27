//! Subjects — the durable pieces of work the board is built from.
//!
//! A subject is what a signal is *about*, and there are three kinds, ranked:
//!
//! > **GitHub issue > pull request > Slack thread**
//!
//! Each is keyed by its real upstream identity rather than a synthetic id, which
//! is what lets any watcher, workflow, or tool address one without a lookup table.
//! Attribution climbs as far *up* that ranking as it can resolve (see [`resolve`]),
//! and the highest rank that resolves owns the signal.
//!
//! Everything else a signal mentions — repo, environment, service, channel,
//! person, branch, commit — is a *resolution key* and context. Nothing is keyed on
//! those: they're long-lived and shared, so keying a subject on one collapses a
//! repository's whole history into a single card.
//!
//! This replaces the earlier synthetic `Thread`, which was invented by the
//! grouping engine and keyed by an internal id.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

use crate::signal::{ResolutionKey, Severity, Signal};

pub mod attach;
pub mod projection;
pub mod resolve;
pub mod store;

pub use attach::Attributor;

/// How authoritative a subject is. Declaration order defines the ordering, so
/// `rank > other.rank` works directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectRank {
    /// A Slack conversation — a subject only when no GitHub artifact resolves.
    SlackThread,
    /// One attempt at the work.
    PullRequest,
    /// The durable statement of what the work is. GitHub Discussions share this
    /// rank: a discussion is also a standing statement of a problem rather than an
    /// attempt at one. Their key form differs (`~` vs `#`) because issue and
    /// discussion numbering are independent, so `repo#5` and `repo~5` are
    /// genuinely different things.
    Issue,
}

impl SubjectRank {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectRank::SlackThread => "slack_thread",
            SubjectRank::PullRequest => "pull_request",
            SubjectRank::Issue => "issue",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "slack_thread" => Some(SubjectRank::SlackThread),
            "pull_request" => Some(SubjectRank::PullRequest),
            "issue" => Some(SubjectRank::Issue),
            _ => None,
        }
    }
}

/// The identity of a subject, and its address everywhere: a SQLite column, a URL
/// path segment, an MCP argument, and (from Phase 2) a Restate virtual-object key.
///
/// One canonical string form with a validating parser beats structured access,
/// because every one of those consumers wants the string.
///
/// | Form | Kind |
/// |---|---|
/// | `owner/repo#412` | issue |
/// | `owner/repo~7` | discussion (issue rank) |
/// | `owner/repo!987` | pull request |
/// | `C02ABC/1721822400.001` | Slack thread (`channel/thread_ts`) |
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubjectKey(String);

impl SubjectKey {
    pub fn issue(repo: &str, number: u64) -> Self {
        Self(format!("{repo}#{number}"))
    }

    pub fn discussion(repo: &str, number: u64) -> Self {
        Self(format!("{repo}~{number}"))
    }

    pub fn pull_request(repo: &str, number: u64) -> Self {
        Self(format!("{repo}!{number}"))
    }

    pub fn slack_thread(channel_and_ts: &str) -> Self {
        Self(channel_and_ts.to_string())
    }

    /// Parse a key, rejecting anything whose kind can't be determined. Called on
    /// every externally-supplied key (MCP arguments, URL segments) so a typo fails
    /// loudly instead of creating an unreachable subject.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            bail!("empty subject key");
        }
        let key = Self(s.to_string());
        // `rank` is what makes a key meaningful; if it can't be determined, the key
        // isn't one.
        key.try_rank()?;
        Ok(key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn rank(&self) -> SubjectRank {
        // Keys in the store were validated on the way in.
        self.try_rank().unwrap_or(SubjectRank::SlackThread)
    }

    fn try_rank(&self) -> Result<SubjectRank> {
        let s = &self.0;
        if s.contains('#') || s.contains('~') {
            Ok(SubjectRank::Issue)
        } else if s.contains('!') {
            Ok(SubjectRank::PullRequest)
        } else if s.contains('/') {
            // `channel/thread_ts` — the only remaining shape.
            Ok(SubjectRank::SlackThread)
        } else {
            bail!("'{s}' is not a subject key (expected owner/repo#N, owner/repo~N, owner/repo!N, or channel/ts)")
        }
    }

    /// `owner/repo` for a GitHub subject; `None` for a Slack thread.
    pub fn repo(&self) -> Option<&str> {
        let idx = self.0.find(['#', '~', '!'])?;
        Some(&self.0[..idx])
    }

    /// The upstream number for a GitHub subject.
    pub fn number(&self) -> Option<u64> {
        let idx = self.0.find(['#', '~', '!'])?;
        self.0[idx + 1..].parse().ok()
    }
}

impl fmt::Display for SubjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the operator has done about a subject.
///
/// This lives on the *subject*, not on each signal, which is the point: half a
/// PR's CI failures being acknowledged was never a coherent thing to express, and
/// the old "a thread is only as handled as its least-handled member" min-fold
/// existed to paper over that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Handled {
    Open,
    Seen,
    Acknowledged,
    Snoozed,
    Resolved,
}

impl Handled {
    pub fn as_str(self) -> &'static str {
        match self {
            Handled::Open => "open",
            Handled::Seen => "seen",
            Handled::Acknowledged => "acknowledged",
            Handled::Snoozed => "snoozed",
            Handled::Resolved => "resolved",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" | "unseen" => Some(Handled::Open),
            "seen" => Some(Handled::Seen),
            "acknowledged" => Some(Handled::Acknowledged),
            "snoozed" => Some(Handled::Snoozed),
            "resolved" => Some(Handled::Resolved),
            _ => None,
        }
    }

    /// Settled work: never re-analyzed on a cloud model, and muted for
    /// notifications. Only the local reopen classifier may look at it.
    pub fn is_handled(self) -> bool {
        matches!(
            self,
            Handled::Acknowledged | Handled::Snoozed | Handled::Resolved
        )
    }
}

/// A durable piece of work, plus what MuggleBot knows about it.
///
/// Small and hot by design: bodies, artifacts, and embeddings live in their own
/// tables and are referenced from here. From Phase 2 this is the state of a
/// Restate virtual object, which is the other reason to keep it small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subject {
    pub key: SubjectKey,
    pub rank: SubjectRank,
    pub title: String,
    /// Deterministic one-liner always; replaced by the LLM summary once a
    /// reasoning pass runs.
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_reasoned_at: Option<DateTime<Utc>>,
    /// The operator is active here (live assist).
    pub live: bool,
    /// Categorical routing tags from the shared vocabulary.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tags were set by a human and must not be overwritten by the classifier.
    #[serde(default)]
    pub tags_pinned: bool,
    /// Operator triage state.
    pub handled: Handled,
    pub snoozed_until: Option<DateTime<Utc>>,
    /// Set when this subject was merged into another: activity forwards there and
    /// it no longer appears on the board on its own.
    pub same_as: Option<SubjectKey>,
    /// Parent issue, for a PR resolved through a closing keyword.
    pub parent: Option<SubjectKey>,
    /// Deterministic merge key within the Slack rank — an environment id. Two alert threads
    /// naming the same environment are the same incident, and this is what makes that a lookup
    /// rather than a model judgment.
    ///
    /// A field on the record rather than a column, now that the subject *is* its object's state:
    /// the `subjects` table was the only thing carrying it.
    #[serde(default)]
    pub merge_key: Option<String>,
}

impl Subject {
    /// A fresh subject for `key`, titled from the signal that created it.
    pub fn new(key: SubjectKey, s: &Signal, now: DateTime<Utc>) -> Self {
        Self {
            rank: key.rank(),
            key,
            title: title_from(s),
            summary: None,
            created_at: now,
            updated_at: now,
            last_reasoned_at: None,
            live: false,
            tags: Vec::new(),
            tags_pinned: false,
            handled: Handled::Open,
            snoozed_until: None,
            same_as: None,
            parent: None,
            merge_key: None,
        }
    }
}

/// A subject with its members and derived attributes, as returned to clients.
#[derive(Debug, Clone, Serialize)]
pub struct SubjectView {
    #[serde(flatten)]
    pub subject: Subject,
    pub signals: Vec<Signal>,
    /// Resolution keys and context drawn from the members, for display.
    pub keys: Vec<ResolutionKey>,
    pub severity: Severity,
    pub edges: Vec<crate::correlation::Edge>,
    pub context: Vec<crate::correlation::SubjectContext>,
    /// Child PRs (on an issue) and contributing Slack threads/meetings.
    #[serde(default)]
    pub children: Vec<SubjectKey>,
    /// The attempts at this issue: each open PR with what it implements, MuggleBot's
    /// critique of the diff, and what reviewers actually said.
    ///
    /// On the view rather than fetched separately because the nesting *is* the answer
    /// to "what's the state of this?" — an issue whose PRs you have to click through
    /// to see reads as an issue nobody is working on.
    #[serde(default)]
    pub pull_requests: Vec<crate::store::PrFix>,
    /// Distilled explanations of this subject and everything under it — the local one the
    /// board writes on its own, and the cloud one if the operator asked for a second
    /// opinion. Both, so the panel can show them side by side and label which is which.
    pub explanations: Vec<crate::store::Explanation>,
    /// Does this need the operator, and has the AI actually looked at it?
    pub attention: Attention,
}

/// The two questions the board exists to answer.
///
/// Triage state is bookkeeping — it records what you *did*, which is not what you
/// want to read at a glance. What you want is: **does this need me**, and **has the
/// AI been over it** (and at whose expense).
#[derive(Debug, Clone, Serialize)]
pub struct Attention {
    /// Needs a human. Derived — not a stored flag to keep in sync.
    pub needed: bool,
    /// Why, in a few words, so the badge is explainable rather than mysterious.
    pub reason: Option<String>,
    /// Which AI decorations exist. An undecorated subject is one you're reading raw.
    pub decorated: Decorations,
}

/// Per-facet record of what the AI has produced for a subject, and where the work
/// ran.
///
/// Split by tier because "has the AI paid attention" and "what did it cost me" are
/// different questions: `local_passes` ran on this machine (fans up, battery down),
/// `cloud_passes` is metered.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Decorations {
    /// A grounded summary has been written (not just the deterministic one-liner).
    pub summary: bool,
    /// Routing tags were classified.
    pub tags: bool,
    /// A dashboard behind a linked alert was actually read.
    pub dashboard: bool,
    /// Root-cause investigation status: `complete`, `running`, `failed`, or absent.
    pub root_cause: Option<String>,
    /// Assigned-issue triage status, if this subject is an assigned issue.
    pub triage: Option<String>,
    /// How many associated pull requests have been judged.
    pub prs_judged: usize,
    /// Completed AI artifacts produced on-device.
    pub local_passes: u32,
    /// Completed AI artifacts that cost a metered call.
    pub cloud_passes: u32,
}

impl Decorations {
    /// Has the AI done anything at all here?
    pub fn any(&self) -> bool {
        self.summary
            || self.tags
            || self.dashboard
            || self.root_cause.is_some()
            || self.triage.is_some()
            || self.prs_judged > 0
    }
}

/// Resolution-key kinds that exist only as internal grouping keys and carry no
/// display value (opaque ids like a Slack conversation ts). Kept on the signal,
/// hidden from the view.
const HIDDEN_KINDS: &[&str] = &["slack_thread"];

/// The distinct resolution keys across a subject's members, for display.
pub fn union_keys(signals: &[Signal]) -> Vec<ResolutionKey> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for s in signals {
        for k in &s.keys {
            if HIDDEN_KINDS.contains(&k.kind.to_ascii_lowercase().as_str()) {
                continue;
            }
            let dedup = format!(
                "{}:{}",
                k.kind.to_ascii_lowercase(),
                k.value.to_ascii_lowercase()
            );
            if seen.insert(dedup) {
                out.push(k.clone());
            }
        }
    }
    out
}

pub fn title_from(s: &Signal) -> String {
    let t = s.title.trim();
    if t.is_empty() {
        format!("{} · {}", s.source, s.external_id)
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_forms_parse_to_their_ranks() {
        let cases = [
            ("restatedev/restate#412", SubjectRank::Issue),
            ("restatedev/restate~7", SubjectRank::Issue),
            ("restatedev/restate!987", SubjectRank::PullRequest),
            ("C02ABC/1721822400.001", SubjectRank::SlackThread),
        ];
        for (raw, rank) in cases {
            let k = SubjectKey::parse(raw).expect(raw);
            assert_eq!(k.rank(), rank, "{raw}");
            assert_eq!(k.as_str(), raw);
        }
    }

    #[test]
    fn a_bare_word_is_not_a_key() {
        // Rejecting these is the point: a subject nobody can address is worse than
        // an error, because it silently accumulates activity nothing displays.
        for bad in ["", "   ", "restate", "412"] {
            assert!(SubjectKey::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn issue_outranks_pr_outranks_slack() {
        assert!(SubjectRank::Issue > SubjectRank::PullRequest);
        assert!(SubjectRank::PullRequest > SubjectRank::SlackThread);
    }

    #[test]
    fn discussion_and_issue_numbering_do_not_collide() {
        let issue = SubjectKey::issue("o/r", 5);
        let discussion = SubjectKey::discussion("o/r", 5);
        assert_ne!(issue, discussion);
        assert_eq!(issue.rank(), discussion.rank());
        assert_eq!(issue.number(), discussion.number());
    }

    #[test]
    fn repo_and_number_come_back_out() {
        let k = SubjectKey::pull_request("restatedev/restate", 987);
        assert_eq!(k.repo(), Some("restatedev/restate"));
        assert_eq!(k.number(), Some(987));
        let slack = SubjectKey::slack_thread("C02ABC/1721822400.001");
        assert_eq!(slack.repo(), None);
        assert_eq!(slack.number(), None);
    }

    #[test]
    fn handled_states_that_settle_work() {
        assert!(!Handled::Open.is_handled());
        assert!(!Handled::Seen.is_handled());
        for h in [Handled::Acknowledged, Handled::Snoozed, Handled::Resolved] {
            assert!(h.is_handled(), "{h:?}");
        }
    }
}
