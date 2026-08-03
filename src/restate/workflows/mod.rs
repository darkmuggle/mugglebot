//! Workflows: the expensive, multi-step pipelines whose results land on a subject.
//!
//! Three properties make something a workflow rather than an object handler:
//! **multiple expensive steps** (so resuming mid-way is worth real money and
//! minutes), **a natural once-per-subject identity**, and sometimes **a need to be
//! interacted with while running**.
//!
//! ## Keys chosen so that re-running is free
//!
//! Restate refuses a second submission of the same workflow id and lets you attach
//! to the first result. So `{issue}@{sha}` means "re-triage an issue whose code
//! hasn't moved" *is* a key collision: you get the previous analysis back, instantly,
//! with no model call. That is precisely the logic the reasoner cache reproduces by
//! hand today (comparing the commit it's about to read against the commit the last
//! analysis read). New commit, new key, real work. An `#a{n}` suffix exists for the
//! one case that must bypass it: an explicit redo on unchanged code.
//!
//! `RootCause` keyed `{subject}@{watermark}` gets the same property from the latest
//! attributed signal id: nothing new has arrived, nothing to re-investigate.
//!
//! ## Why ingest is *not* a workflow
//!
//! It is high-frequency and single-purpose. Modelling it as a workflow would mint an
//! instance per event, each with its own retention and lifecycle, for no gain — the
//! exactly-once property it needs comes from the ingress idempotency key.
//!
//! ## Error classification
//!
//! Restate retries by default and forever, so every step has to say which kind of
//! failure it is. 4xx that won't change (404, a revoked token's 401, "no such repo")
//! and unparseable-after-N-attempts model output are **terminal** — they fail the
//! invocation and surface on the subject. Rate limits, 5xx, timeouts, connection
//! resets, and "Ollama isn't running" are **transient**: that is exactly what durable
//! execution is for, and it is why a failed investigation is retried rather than
//! silently dropped.

use std::sync::Arc;

use crate::correlation::Analyst;
use crate::rootcause::Investigator;
use crate::store::Store;
use crate::triage::Triager;

pub mod explain;
pub mod issue_triage;
pub mod rest;
pub mod root_cause;

/// Handles the workflows need. Same pattern as [`crate::restate::SubjectOps`]: the
/// handler bodies stay thin wrappers over functions that are callable — and
/// testable — without a Restate server.
pub struct WorkflowOps {
    pub store: Arc<Store>,
    /// Reads subjects and their hierarchy — what `Explain` gathers from.
    pub attributor: Arc<crate::subject::Attributor>,
    /// The routed tier.
    pub reasoner: Arc<dyn crate::reasoner::Reasoner>,
    /// Writes explanations. **Local**, like everything else automatic. What the metered
    /// tier used to buy here is bought instead by [`explain::verify`], which removes any
    /// claim the dossier can't support — a guarantee rather than a better guess.
    pub explainer: Arc<dyn crate::reasoner::Reasoner>,
    /// The cloud tier — `claude-opus-5`. Held here for `SecondOpinion`, which runs when the
    /// operator presses the button.
    ///
    /// It is no longer true that only operator-initiated work reaches a cloud model:
    /// root-cause investigation's deep ranking pass is on this tier too, automatically. See
    /// `[reasoner] cloud`. Unmetered via the CLI bridge, but off the machine.
    pub cloud: Arc<dyn crate::reasoner::Reasoner>,
    pub investigator: Arc<Investigator>,
    pub triager: Arc<Triager>,
    pub analyst: Arc<Analyst>,
    pub repos: Arc<crate::repos::RepoIndex>,
    pub browser: Arc<crate::browser::BrowserDriver>,
    pub context: Arc<crate::context::ContextManager>,
    /// Reads pull request diffs and summarizes them on-device. Built once at boot with the
    /// token as it stood; a token added later is picked up on the next restart, same as
    /// every other GitHub-reading component here.
    pub diffs: Arc<crate::prdiff::DiffReader>,
    /// Build a reasoner for a provider and model the **operator** named.
    ///
    /// A factory rather than a set of handles because the point is that the choice isn't
    /// ours: the re-dispatch button offers every model the config knows about, and holding
    /// one handle per possibility would mean building them all at boot. A closure also keeps
    /// the Ollama key read at point of use, which is the standing rule for credentials —
    /// a key rotated through the config page takes effect on the next call, not the next
    /// restart.
    ///
    /// Only the operator-initiated paths may call this. Nothing automatic gets to pick a
    /// model, which is what keeps "what does this daemon do on its own?" answerable.
    pub reasoner_factory: ReasonerFactory,
}

/// Builds a reasoner from a provider label and model name. See
/// [`WorkflowOps::reasoner_factory`].
pub type ReasonerFactory =
    Arc<dyn Fn(&str, &str) -> Arc<dyn crate::reasoner::Reasoner> + Send + Sync>;

impl WorkflowOps {
    /// The diff reader, for the `PrDiff` workflow.
    pub fn diff_reader(&self) -> &crate::prdiff::DiffReader {
        &self.diffs
    }

    /// A reasoner for an operator-named provider and model.
    pub fn reasoner_for(&self, provider: &str, model: &str) -> Arc<dyn crate::reasoner::Reasoner> {
        (self.reasoner_factory)(provider, model)
    }

    /// Drive the browser over one investigation and file what it saw.
    ///
    /// The claim/complete/fail bookkeeping stays in the store, but the *queue* part of
    /// it is gone: the workflow id is the claim, so a crashed run doesn't leave a row
    /// marked `running` that nothing will ever pick up.
    pub async fn browser_read(&self, id: &str) -> anyhow::Result<serde_json::Value> {
        let Some(job) = self.store.get_browser_investigation(id)? else {
            anyhow::bail!("no such investigation {id}");
        };
        let findings = self.browser.investigate(&job.url, &job.prompt).await?;
        let done = self
            .store
            .complete_browser_investigation(id, &findings.text)?;
        Ok(serde_json::to_value(&done)?)
    }

    /// Judge the open PRs that may already fix an assigned issue.
    ///
    /// Keyed by the *issue*, versioned by the PR's head sha: the question is "does
    /// anything open already fix this?", and the answer only changes when a diff does.
    pub async fn pr_critique(
        &self,
        issue_key: &str,
        _sha: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let fixes = self.triager.judge_pr_fixes(issue_key).await?;
        Ok(serde_json::to_value(&fixes)?)
    }

    /// Re-ingest one context source.
    pub async fn context_refresh(&self, id: &str) -> anyhow::Result<bool> {
        self.context.refresh(id).await
    }

    /// Collapse `drop` into `keep`.
    pub async fn merge(&self, keep: &str, drop: &str) -> anyhow::Result<String> {
        self.analyst.merge(keep, drop)
    }
}

/// Split a workflow key into the subject part and the version part.
///
/// `restatedev/restate#412@abc1234` → `("restatedev/restate#412", "abc1234")`.
///
/// The split is on the **first** `@`, and that matters. Subject keys, PR keys and org names
/// never contain one — but the *version* routinely does: a GitHub signal id is
/// `github/24800345076@2026-07-27T21:35:05Z`, so a key is
/// `owner/repo!1235@github/24800345076@2026-07-27T21:35:05Z+0` with three of them.
///
/// This used to split on the last `@`, which handed the subject half the watermark. Every
/// `Explain`, `SecondOpinion` and `RootCause` on a notification-sourced subject failed instantly
/// with "no subject owner/repo!1235@github/24800345076" — and because the invocation *completed*
/// (failed, not stuck), the UI showed no error and no result, which reads as a hung button.
///
/// Assigned-watcher subjects were unaffected, their ids carrying no timestamp, which is why this
/// worked when tested by hand and failed on the board.
pub fn split_versioned(key: &str) -> (&str, &str) {
    match key.split_once('@') {
        Some((subject, version)) => (subject, version),
        None => (key, ""),
    }
}

/// Run a workflow handler with the dispatch strip watching.
///
/// Wraps the body so the three states an operator cannot otherwise distinguish — started,
/// finished, failed — each leave a mark. The submitting side already recorded `Queued`
/// when the ingress accepted it; this is the other half.
///
/// Failure messages come through `AsRef<dyn Error>` because `HandlerError` implements no
/// `Display` of its own, and the inner error's message is the whole value of showing it:
/// "no such repo `foo/bar`" tells the operator what to fix, "failed" does not.
///
/// A Restate retry re-invokes the handler, so this runs again on each attempt and the row
/// flips back to `Running` — which is correct: the work genuinely is in flight again.
pub async fn tracked<T>(
    kind: &'static str,
    key: &str,
    body: impl std::future::Future<Output = restate_sdk::prelude::HandlerResult<T>>,
) -> restate_sdk::prelude::HandlerResult<T> {
    crate::dispatch::running(kind, key);
    match body.await {
        Ok(value) => {
            crate::dispatch::done(kind, key);
            Ok(value)
        }
        Err(e) => {
            crate::dispatch::failed(
                kind,
                key,
                AsRef::<dyn std::error::Error + Send + Sync>::as_ref(&e),
            );
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_keys_split_on_the_version() {
        assert_eq!(
            split_versioned("restatedev/restate#412@abc1234"),
            ("restatedev/restate#412", "abc1234")
        );
        // An explicit redo suffix rides along with the version, so it stays part of
        // what makes the key distinct.
        assert_eq!(split_versioned("o/r#1@sha#a2"), ("o/r#1", "sha#a2"));
        // A Slack subject key has slashes and dots but no `@`.
        assert_eq!(
            split_versioned("C02ABC/1721822400.001@sig-9"),
            ("C02ABC/1721822400.001", "sig-9")
        );
        assert_eq!(split_versioned("o/r#1"), ("o/r#1", ""));
    }

    /// The version routinely contains `@`, and splitting on the last one gave the subject half
    /// the watermark.
    ///
    /// This is the real key from a real failure: a GitHub signal id is
    /// `github/<id>@<timestamp>`, so the key has three `@`. Splitting on the last one produced
    /// the subject `restatedev/restate-cloud!1235@github/24800345076`, which does not exist —
    /// so every Explain, SecondOpinion and RootCause on a notification-sourced subject failed
    /// instantly, showing neither an error nor a result.
    #[test]
    fn a_watermark_containing_at_signs_does_not_eat_the_subject() {
        assert_eq!(
            split_versioned(
                "restatedev/restate-cloud!1235@github/24800345076@2026-07-27T21:35:05Z+0"
            ),
            (
                "restatedev/restate-cloud!1235",
                "github/24800345076@2026-07-27T21:35:05Z+0"
            )
        );

        // The assigned-watcher shape, which has no timestamp — this is why the bug survived a
        // hand test and only appeared on the board.
        assert_eq!(
            split_versioned("coreos/mantle!1133@github/assigned/coreos/mantle#1133+0"),
            ("coreos/mantle!1133", "github/assigned/coreos/mantle#1133+0")
        );

        // Every other caller's key shape still splits where it should.
        assert_eq!(split_versioned("o/r!987@abc123"), ("o/r!987", "abc123"));
        assert_eq!(
            split_versioned("restatedev@495877"),
            ("restatedev", "495877")
        );
        assert_eq!(
            split_versioned("ctx/18c62ad3@W/\"etag-value\""),
            ("ctx/18c62ad3", "W/\"etag-value\"")
        );
    }
}
