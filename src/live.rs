//! Live assist (Phase 4) — domain types.
//!
//! Threads the user is active in (detected via their Slack `user_id`) get a
//! debounced re-analysis that produces grounded [`Hint`]s: informational hints,
//! next-step suggestions, and **flags** on the user's own messages
//! (`factual_error` / `risky_action`). A high-confidence flag drives LCARS
//! red-alert + a Critical macOS notification. Strictly advisory — it warns and
//! cites, never edits or sends anything.
//!
//! The engine that produces these lives in [`crate::live_engine`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintKind {
    /// Background the user may not have connected (a runbook, a past incident).
    Hint,
    /// A sensible next step / generic mitigation to consider.
    Suggestion,
    /// A flag on one of the user's own messages.
    Flag,
}

impl HintKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HintKind::Hint => "hint",
            HintKind::Suggestion => "suggestion",
            HintKind::Flag => "flag",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hint" => Some(HintKind::Hint),
            "suggestion" => Some(HintKind::Suggestion),
            "flag" => Some(HintKind::Flag),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagType {
    FactualError,
    RiskyAction,
}

impl FlagType {
    pub fn as_str(self) -> &'static str {
        match self {
            FlagType::FactualError => "factual_error",
            FlagType::RiskyAction => "risky_action",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "factual_error" => Some(FlagType::FactualError),
            "risky_action" => Some(FlagType::RiskyAction),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintState {
    Active,
    Dismissed,
    /// Dismissed as wrong — fed back to memory so it isn't re-raised.
    FalsePositive,
}

impl HintState {
    pub fn as_str(self) -> &'static str {
        match self {
            HintState::Active => "active",
            HintState::Dismissed => "dismissed",
            HintState::FalsePositive => "false_positive",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(HintState::Active),
            "dismissed" => Some(HintState::Dismissed),
            "false_positive" => Some(HintState::FalsePositive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hint {
    pub id: String,
    pub thread_id: String,
    pub kind: HintKind,
    /// Set only when `kind == Flag`.
    pub flag_type: Option<FlagType>,
    pub text: String,
    pub rationale: Option<String>,
    /// Signal / context / memory ids this hint is built from — the citation.
    pub citations: Vec<String>,
    pub confidence: f64,
    pub state: HintState,
    pub created_at: DateTime<Utc>,
}
