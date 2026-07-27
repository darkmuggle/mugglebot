//! Human gates: the confirmation that isn't a promise.
//!
//! "Copilot, not autopilot" is currently guaranteed by omission — there are no write
//! tools, so nothing can act. That holds until the first one is wanted, and "we'll add
//! a confirmation dialog then" is not a design.
//!
//! A durable promise makes the gate real. A handler that reaches a gated step blocks
//! on a signal; the UI resolves it. Three properties fall out that a dialog box does
//! not have:
//!
//! 1. **The pending decision survives a restart** rather than being silently
//!    abandoned. A dialog that vanishes when the daemon restarts is an action that
//!    quietly didn't happen.
//! 2. **It has an id**, so the audit log records *what* was authorized, by whom, and
//!    when — not just that something was.
//! 3. **An un-answered gate is visible** on the board as blocked work.
//!
//! Rejection is `reject(TerminalError)`, which fails the invocation with a recorded
//! reason rather than leaving it hanging.
//!
//! **No gated action ships in v1.** The mechanism exists now because retrofitting
//! authorization onto a pipeline that already acts is the wrong order.

use anyhow::Result;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// The signal name a gated handler awaits. One per gate kind, so two gates in one
/// invocation can't be confused for each other.
pub const APPROVED: &str = "approved";

/// What the operator is being asked to authorize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateRequest {
    /// The subject this action belongs to, so the board can show it in context.
    pub subject: String,
    /// What would happen, in one line. This is the whole basis of the decision, so it
    /// has to be specific: "roll back deploy #4821 in production", not "apply
    /// mitigation".
    pub action: String,
    /// Why MuggleBot thinks it should happen, with citations.
    pub rationale: String,
    /// Whether the action can be undone. A gate on an irreversible action should read
    /// differently from one on a reversible one.
    pub reversible: bool,
}

/// Block until the operator answers.
///
/// Returns `Ok(())` on approval. A rejection arrives as a `TerminalError`, which fails
/// the surrounding invocation — deliberately: a rejected action must not be retried,
/// and Restate retries anything that isn't terminal.
pub async fn await_approval(
    ctx: &WorkflowContext<'_>,
    request: &GateRequest,
) -> Result<(), HandlerError> {
    tracing::warn!(
        "gate: awaiting approval — {} on {} ({})",
        request.action,
        request.subject,
        if request.reversible {
            "reversible"
        } else {
            "IRREVERSIBLE"
        }
    );
    let approved: bool = ctx.promise(APPROVED).await?;
    if !approved {
        return Err(
            TerminalError::new(format!("the operator declined: {}", request.action)).into(),
        );
    }
    Ok(())
}

/// Resolve a pending gate. Called by the UI and by the `resolve_gate` tool.
///
/// The workflow's *shared* handler does this, which is why a gated invocation can be
/// answered while it is still blocked — a exclusive handler would deadlock against the
/// run handler it is trying to unblock.
pub async fn resolve(ctx: &SharedWorkflowContext<'_>, approved: bool) -> Result<(), TerminalError> {
    ctx.resolve_promise(APPROVED, approved);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gate_request_states_what_would_happen_specifically() {
        // The point of the struct: a gate the operator can't evaluate from its own
        // text is a rubber stamp, so `action` is required and free-form rather than an
        // enum of vague categories.
        let r = GateRequest {
            subject: "restatedev/restate#412".into(),
            action: "roll back deploy #4821 in production".into(),
            rationale: "error rate tracks the deploy marker [browser:b7]".into(),
            reversible: true,
        };
        let round: GateRequest = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(round.action, r.action);
        assert!(round.reversible);
    }
}
