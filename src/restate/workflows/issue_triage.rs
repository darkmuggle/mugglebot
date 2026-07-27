//! `IssueTriage` — an assigned issue, read against its actual source.
//!
//! Keyed `{owner}/{repo}#{n}@{sha}` (plus `#a{n}` for an explicit redo). This is the
//! sharpest example of keying for free re-runs: "re-triage an issue whose code hasn't
//! moved" is a key collision, so you get the previous analysis back instantly with no
//! model call. New commit, new key, real work.
//!
//! That replaces a hand-rolled equivalent — the reasoner cache comparing the commit
//! it is about to read against the commit the last analysis read — with the
//! runtime's own identity.
//!
//! The pipeline: shallow read-only checkout → deterministic file selection (no model,
//! so it works with nothing reachable) → characterize on the local coder model →
//! propose N *distinct* approaches → plain-English rendering. Nothing here writes to
//! a repository: no commit, no push, no PR.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::{split_versioned, WorkflowOps};
use crate::restate::scopes;

pub struct IssueTriage {
    ops: Arc<WorkflowOps>,
}

impl IssueTriage {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
}

#[restate_sdk::workflow]
impl IssueTriage {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("IssueTriage", &key, self.triage(ctx)).await
    }
}

impl IssueTriage {
    async fn triage(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        // `{issue}@{sha}` — and the sha may carry an `#a2` redo suffix, which is part
        // of what makes the key distinct and is otherwise ignored here.
        let (issue_key, version) = split_versioned(ctx.key());
        let issue_key = issue_key.to_string();

        // The step's result is journalled, so it has to be serializable: `Json`
        // over `serde_json::Value` keeps the store's own types free of a Restate
        // dependency, which is what lets the pipeline stay testable without one.
        let ops = self.ops.clone();
        let key_for_run = issue_key.clone();
        let triaged = ctx
            .run(|| async move {
                let done = ops.triager.triage(&key_for_run).await.map_err(classify)?;
                let value =
                    serde_json::to_value(&done).map_err(|e| TerminalError::new(e.to_string()))?;
                Ok(Json(value))
            })
            .await?
            .into_inner();

        tracing::info!(
            "triage for {issue_key} @{version}: {} ({} patch option(s))",
            triaged
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            triaged
                .get("patches")
                .and_then(|v| v.as_array())
                .map(Vec::len)
                .unwrap_or(0)
        );
        Ok(Json(triaged))
    }
}

/// See [`super::root_cause::classify`] for the rule. The additions here are the
/// checkout failures that will never succeed on retry.
fn classify(e: anyhow::Error) -> HandlerError {
    let msg = format!("{e:#}");
    let lower = msg.to_ascii_lowercase();
    let terminal = [
        "no assigned issue",
        "not found",
        "404",
        "401",
        "authentication failed",
        "repository not found",
        "no such repo",
        "triage is unavailable",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if terminal {
        TerminalError::new(msg).into()
    } else {
        HandlerError::from(anyhow::anyhow!(msg))
    }
}

/// Reading source is exactly the work that shouldn't leave the machine, so this
/// pipeline runs on-device — and one Ollama means a queue of one.
pub const SCOPE: &str = scopes::LOCAL_LLM;

/// Per-repo, because two clones of the *same* repo at once is a corrupt working tree.
pub fn checkout_limit_key(repo: &str) -> String {
    format!("{}/{repo}", scopes::CHECKOUT)
}
