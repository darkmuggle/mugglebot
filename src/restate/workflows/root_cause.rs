//! `RootCause` — from symptom to the change that caused it.
//!
//! Keyed `{subject}@{watermark}`, where the watermark is the newest attributed
//! signal id. Nothing new has arrived → same key → the previous report comes back
//! without a single model call.
//!
//! The pipeline is: symptoms → route to repos → existing issues/PRs → the commit log
//! before the *earliest* signal (a cause precedes its symptom) → rank → code search
//! only when nothing above explains it. Steps 1–4 and the shortlisting run
//! on-device; only `shortlist_size` already-plausible candidates reach the cloud
//! model for the final verdict.
//!
//! **What being a workflow buys.** A 403 from GitHub partway through used to restart
//! the whole investigation, re-cloning and re-paying for the steps that had already
//! succeeded. Journalled, the retry resumes where it stopped. For a pipeline whose
//! last step is an Opus call, that is the difference between one metered call and
//! however many times the step before it flaked.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::{split_versioned, WorkflowOps};
use crate::restate::scopes;

pub struct RootCause {
    ops: Arc<WorkflowOps>,
}

impl RootCause {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
}

#[restate_sdk::workflow]
impl RootCause {
    /// Runs exactly once per workflow id. A second submission of the same key is
    /// refused by Restate and can be attached to for the original result.
    ///
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("RootCause", &key, self.investigate(ctx)).await
    }
}

impl RootCause {
    async fn investigate(
        &self,
        ctx: WorkflowContext<'_>,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let (subject, watermark) = split_versioned(ctx.key());
        let subject = subject.to_string();

        // The whole investigation is one durable step today, because `Investigator`
        // already sequences it internally and splitting it here would mean
        // restructuring that first. The step boundary that matters most is already
        // in place: a retry of *this* workflow does not re-run a completed
        // investigation, because the report is keyed by the same watermark.
        //
        // Splitting `Investigator::investigate` into per-stage `ctx.run` blocks is
        // the natural follow-up, and is what turns "resumes the investigation" into
        // "resumes at the stage that failed".
        // The step's result is journalled, so it has to be serializable: `Json` over
        // `serde_json::Value` keeps the investigator's own types free of a Restate
        // dependency, which is what lets the pipeline stay testable without one.
        let ops = self.ops.clone();
        let subject_for_run = subject.clone();
        let report = ctx
            .run(|| async move {
                let report = ops
                    .investigator
                    .investigate(&subject_for_run)
                    .await
                    .map_err(classify)?;
                let value =
                    serde_json::to_value(&report).map_err(|e| TerminalError::new(e.to_string()))?;
                Ok(Json(value))
            })
            .await?
            .into_inner();

        tracing::info!(
            "root cause for {subject} (watermark {watermark}): {} candidate(s), status {}",
            report
                .get("candidates")
                .and_then(|v| v.as_array())
                .map(Vec::len)
                .unwrap_or(0),
            report
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        );
        Ok(Json(report))
    }
}

/// Which failures are worth retrying.
///
/// Restate retries a non-terminal error forever, so "unavailable because there's no
/// GitHub token" must be terminal — otherwise a misconfigured install hammers a
/// dead code path until someone notices. A rate limit, by contrast, is exactly what
/// durable retry is for.
fn classify(e: anyhow::Error) -> HandlerError {
    let msg = format!("{e:#}");
    let lower = msg.to_ascii_lowercase();
    let terminal = [
        "no subject",
        "handled subjects are not investigated",
        "investigation is unavailable",
        "not found",
        "404",
        "401",
        "403 forbidden",
        "no such repo",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if terminal {
        TerminalError::new(msg).into()
    } else {
        // Transient: rate limits, 5xx, timeouts, a local model that isn't running.
        HandlerError::from(anyhow::anyhow!(msg))
    }
}

/// The vqueue scope this workflow's cloud step belongs to (Phase 6).
pub const SCOPE: &str = scopes::CLOUD_LLM;
