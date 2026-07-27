//! Correlation: the relation graph over subjects.
//!
//! Attribution is deterministic and lives in [`crate::subject::resolve`] — most of
//! what used to need correlation is now just "which subject does this key name?".
//! What's left is the genuinely ambiguous part, and it stays a model's job: two
//! issues filed for one bug, an alert thread and the issue about it, a PR that
//! fixes something already fixed.
//!
//! So this module owns the **edges**: an LLM judges candidate subject pairs —
//! `same` / `related` / `distinct` — building a persisted graph, with human
//! override pins (provenance `user`) that always win and constrain the next
//! re-analysis.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

/// An edge in the relation graph between two subjects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub subject_a: String,
    pub subject_b: String,
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

/// Ad-hoc grounding attached to a single subject (free text or a URL). Attaching
/// or editing it re-runs that subject's analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectContext {
    pub id: String,
    pub subject_key: String,
    pub kind: ContextKind,
    pub content: String,
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub mod llm;
pub use llm::Analyst;
