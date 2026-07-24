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
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::GitHub => "github",
            Source::Slack => "slack",
            Source::Granola => "granola",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "github" => Some(Source::GitHub),
            "slack" => Some(Source::Slack),
            "granola" => Some(Source::Granola),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Unseen,
    Seen,
    Acknowledged,
    Resolved,
    Snoozed,
}

/// A correlation-relevant entity extracted from a signal: a repo, service,
/// channel, or person. Shared entities within a time window are what group
/// signals into a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub kind: String,
    pub value: String,
}

impl Entity {
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
    pub entities: Vec<Entity>,
    pub severity: Severity,
    pub state: State,
    pub occurred_at: DateTime<Utc>,
    pub ingested_at: DateTime<Utc>,
    pub thread: Option<String>,
    pub raw: serde_json::Value,
    /// Categorical routing tags. Populated at ingest by the classifier (Slack
    /// messages are classified per-message); empty until then.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Signal {
    /// Deterministic internal id — stable across re-ingests, unique per upstream
    /// event. Doubles as the dedup key alongside `UNIQUE(source, external_id)`.
    pub fn make_id(source: Source, external_id: &str) -> String {
        format!("{}/{}", source.as_str(), external_id)
    }

    /// Whether the user is personally engaged in this signal's discussion — they
    /// authored it, were @-mentioned, or were directly asked to act. Drives
    /// live-assist follow, and revives a snoozed thread the user re-enters.
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
