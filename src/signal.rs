//! The normalized signal every watcher emits. One type the whole system speaks —
//! nothing source-specific leaks past ingest.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    GitHub,
    Slack,
    Granola,
    /// incident.io. Its own source rather than a flavour of Slack: an incident is a
    /// first-class piece of work with a lifecycle (`triage` → `active` → `closed`), and it
    /// is reconciled against that lifecycle rather than against a notification feed.
    IncidentIo,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::GitHub => "github",
            Source::Slack => "slack",
            Source::Granola => "granola",
            Source::IncidentIo => "incident_io",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "github" => Some(Source::GitHub),
            "slack" => Some(Source::Slack),
            "granola" => Some(Source::Granola),
            "incident_io" => Some(Source::IncidentIo),
            _ => None,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    ReviewRequested,
    Mention,
    Assigned,
    CiFailure,
    ThreadReply,
    Alert,
    MeetingNote,
    Other,
}

/// `Info < Notice < Warning < Critical` — declaration order defines the ordering,
/// so `severity >= threshold` comparisons work directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Notice,
    Warning,
    Critical,
}

/// Something a signal names, used to work out which subject it belongs to: an
/// issue, a PR, a branch, a commit, a Slack thread, a repo, an environment, a
/// person.
///
/// A resolution key is not an identity. Only three kinds of key can *own* a
/// signal (see [`crate::subject::resolve`]); the rest are how you find the owner,
/// and context for the reasoner once you have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionKey {
    pub kind: String,
    pub value: String,
}

impl ResolutionKey {
    pub fn new(kind: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub source: Source,
    pub external_id: String,
    pub kind: SignalKind,
    pub title: String,
    pub body: Option<String>,
    pub url: Option<String>,
    pub actor: Option<String>,
    /// Everything this signal names. The currency of attribution.
    pub keys: Vec<ResolutionKey>,
    pub severity: Severity,
    /// Upstream version of a *mutable* event — GitHub's `updated_at`, Slack's
    /// `edited_ts`. Part of the dedup key, because a notification thread
    /// legitimately re-fires when a new comment lands: keying on the id alone would
    /// swallow real activity, and keying on id-plus-version distinguishes "the same
    /// event" from "the same thread, changed".
    pub version: Option<String>,
    /// The signal is gone upstream — the notification is no longer unread, the
    /// assigned issue was closed. Distinct from operator triage, which lives on the
    /// subject: this is a fact about the source, not a decision about the work.
    #[serde(default)]
    pub upstream_gone: bool,
    pub occurred_at: DateTime<Utc>,
    pub ingested_at: DateTime<Utc>,
    /// The subject that owns this signal — `None` means the unattributed lane.
    pub subject: Option<String>,
    pub raw: serde_json::Value,
    /// Categorical routing tags. Populated at ingest by the classifier (Slack
    /// messages are classified per-message); empty until then.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Signal {
    /// Deterministic internal id — stable across re-ingests, unique per upstream
    /// event. Doubles as the dedup key alongside
    /// `UNIQUE(source, external_id, version)`.
    pub fn make_id(source: Source, external_id: &str, version: Option<&str>) -> String {
        match version {
            Some(v) => format!("{}/{}@{}", source.as_str(), external_id, v),
            None => format!("{}/{}", source.as_str(), external_id),
        }
    }

    /// The key that makes ingest exactly-once.
    ///
    /// Submitted as the Restate `idempotency-key` (Phase 3), and mirrored by the
    /// store's unique index as the long-horizon backstop — the ingress only
    /// remembers an idempotent result for its retention window, so deleting
    /// `restate-data` must not resurrect last month's notifications.
    pub fn dedup_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.source.as_str(),
            self.external_id,
            self.version.as_deref().unwrap_or("-")
        )
    }

    /// Whether the user is personally engaged in this signal's discussion — they
    /// authored it, were @-mentioned, or were directly asked to act. Drives
    /// live-assist follow, and revives a snoozed subject the user re-enters.
    pub fn is_user_engaged(&self) -> bool {
        // Direct participation on Slack: you posted, or someone @-mentioned you.
        let slack_engaged = self.raw_flag("is_self") || self.raw_flag("mentions_me");
        // On GitHub these kinds are the user being asked into the conversation.
        let github_engaged = matches!(
            self.kind,
            SignalKind::Mention | SignalKind::ReviewRequested | SignalKind::Assigned
        );
        slack_engaged || github_engaged
    }

    fn raw_flag(&self, key: &str) -> bool {
        self.raw.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
    }
}
