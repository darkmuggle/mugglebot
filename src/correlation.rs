//! Correlation & de-duplication.
//!
//! Phase 1: deterministic grouping of signals into threads by shared entities +
//! time proximity (see [`Correlator::ingest`]). Phase 2: an LLM judges candidate
//! thread pairs — `same` / `related` / `distinct` — building a persisted relation
//! graph, with human override pins (provenance `user`) that always win and
//! trigger a re-analysis constrained by those pins.
//!
//! This module defines the correlation domain types and the engine. The engine
//! lives in [`engine`] to keep the type surface (shared with the store, server,
//! and MCP tools) readable.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::signal::{Entity, Severity, Signal, State};

/// A correlated topic: the signals grouped by shared entities + time, plus the
/// summary and (Phase 2) relation edges. Membership is derived from
/// `signals.thread`; this carries the thread-level metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub title: String,
    /// Deterministic one-liner always; replaced/extended by the LLM summary once
    /// a reasoning pass runs.
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_reasoned_at: Option<DateTime<Utc>>,
    /// The user is active in this thread (Phase 4 live-assist).
    pub live: bool,
    /// Tags the pre-process classified this thread into, drawn from the context
    /// library's vocabulary — the categorical routing key for grounding.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The tags were set by a human (on the board) and must not be overwritten by
    /// the classifier — mirrors relation pins.
    #[serde(default)]
    pub tags_pinned: bool,
}

/// A thread with its members and derived attributes, as returned to clients.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadView {
    #[serde(flatten)]
    pub thread: Thread,
    pub signals: Vec<Signal>,
    pub entities: Vec<Entity>,
    pub severity: Severity,
    pub state: State,
    pub edges: Vec<Edge>,
    pub context: Vec<ThreadContext>,
    /// Does this need the operator, and has the AI actually looked at it?
    pub attention: Attention,
}

/// The two questions the board exists to answer.
///
/// The five-state triage machine (`Unseen`/`Seen`/`Acknowledged`/`Resolved`/
/// `Snoozed`) is bookkeeping — it records what you *did*, which is not what you want
/// to read at a glance. What you want is: **does this need me**, and **has the AI
/// been over it** (and at whose expense). So the board renders those two, and the
/// underlying state stays available for filtering without being the headline.
#[derive(Debug, Clone, Serialize)]
pub struct Attention {
    /// Needs a human. Derived — not a stored flag to keep in sync.
    pub needed: bool,
    /// Why, in a few words, so the badge is explainable rather than mysterious.
    pub reason: Option<String>,
    /// Which AI decorations exist on this thread. This is the "has the AI paid
    /// attention?" indicator: an undecorated thread is one you're reading raw.
    pub decorated: Decorations,
}

/// Per-facet record of what the AI has produced for a thread, and where the work
/// ran.
///
/// Split by tier because "has the AI paid attention" and "what did it cost me" are
/// different questions: `local_passes` is work that ran on this machine (fans up,
/// battery down), `cloud_passes` is metered.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Decorations {
    /// A grounded summary has been written (not just the deterministic one-liner).
    pub summary: bool,
    /// Routing tags were classified.
    pub tags: bool,
    /// Tailored mitigations were generated and cached.
    pub mitigations: bool,
    /// A dashboard behind a linked alert was actually read.
    pub dashboard: bool,
    /// Root-cause investigation status: `complete`, `running`, `failed`, or absent.
    pub root_cause: Option<String>,
    /// Assigned-issue triage status, if this thread is an assigned issue.
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
            || self.mitigations
            || self.dashboard
            || self.root_cause.is_some()
            || self.triage.is_some()
            || self.prs_judged > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// Duplicates of one underlying issue.
    Same,
    /// Distinct but connected.
    Related,
    /// Explicitly unrelated — a negative edge that stops future regrouping.
    Distinct,
}

impl RelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationKind::Same => "same",
            RelationKind::Related => "related",
            RelationKind::Distinct => "distinct",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "same" => Some(RelationKind::Same),
            "related" => Some(RelationKind::Related),
            "distinct" => Some(RelationKind::Distinct),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// The LLM's verdict.
    Llm,
    /// A human override (a pin) — authoritative, and used as a hard constraint
    /// when the affected threads are re-analyzed.
    User,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Llm => "llm",
            Provenance::User => "user",
        }
    }
}

/// An edge in the relation graph between two threads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub thread_a: String,
    pub thread_b: String,
    pub kind: RelationKind,
    pub provenance: Provenance,
    pub confidence: f64,
    pub rationale: String,
    /// Signals the verdict weighed — the citation for the edge.
    pub signals: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Text,
    Url,
}

impl ContextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextKind::Text => "text",
            ContextKind::Url => "url",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(ContextKind::Text),
            "url" => Some(ContextKind::Url),
            _ => None,
        }
    }
}

/// Ad-hoc grounding attached to a single thread (free text or a URL). Attaching
/// or editing it re-runs that thread's analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadContext {
    pub id: String,
    pub thread_id: String,
    pub kind: ContextKind,
    pub content: String,
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub mod engine;
pub mod llm;
pub use engine::Correlator;
pub use llm::Analyst;
