//! The persona workflows: `PersonaProfile` and `PersonaPredict`.
//!
//! Both are keyed so that re-running is free, which is the property that makes them worth
//! being workflows at all:
//!
//! | Workflow | Key | A refused submission means |
//! |---|---|---|
//! | `PersonaProfile` | `{slug}@{evidence watermark}` | nothing new has been harvested, so the profile is current |
//! | `PersonaPredict` | `{slug}@{kind}@{produced_by}@{subject}@{watermark}` | this exact question has been answered; the answer is already on screen |
//!
//! `PersonaProfile` is genuinely expensive — one local model pass per [`Facet`], over months
//! of somebody's writing — and genuinely resumable, which is the other half of the workflow
//! test. `PersonaPredict` is one model call but wants the same identity property: the operator
//! selects three personas against a pull request, changes nothing, and selects them again.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::WorkflowOps;
use crate::persona::PredictionKind;
use crate::restate::scopes;

// ---- PersonaProfile ----------------------------------------------------------

/// Re-distil one persona's profile from everything harvested about them.
///
/// Keyed `{slug}@{watermark}`, where the watermark is
/// [`crate::store::Store::persona_evidence_watermark`] — `{count}@{newest ingested_at}`, not
/// the newest excerpt's id. The distinction is the whole reason the key works: the backward
/// history walk adds *older* material, which would leave a newest-id token unchanged, so
/// every backfill pass would be refused as a duplicate and the profile would stay frozen at
/// whatever the first pass happened to see.
pub struct PersonaProfile {
    ops: Arc<WorkflowOps>,
}

impl PersonaProfile {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }

    /// One GPU, one lane. Every facet pass is an on-device model call.
    pub const SCOPE: &'static str = scopes::LOCAL_LLM;

    pub fn key(slug: &str, watermark: &str) -> String {
        format!("{slug}@{watermark}")
    }
}

#[restate_sdk::workflow]
impl PersonaProfile {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("PersonaProfile", &key, self.distil(ctx)).await
    }
}

impl PersonaProfile {
    async fn distil(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let (slug, watermark) = super::split_versioned(ctx.key());
        let (slug, watermark) = (slug.to_string(), watermark.to_string());
        let ops = self.ops.clone();
        let summary = ctx
            .run(|| {
                let ops = ops.clone();
                let slug = slug.clone();
                async move {
                    let profile = ops.personas.distil(&slug).await.map_err(classify)?;
                    // The profile itself is in SQLite; what comes back through the journal is
                    // a count, for the same reason every other handler here returns one.
                    Ok(Json(serde_json::json!({
                        "persona": slug,
                        "traits": profile.traits.len(),
                        "removed": profile.removed.len(),
                        "evidence": profile.stats.evidence,
                    })))
                }
            })
            .await?
            .into_inner();
        tracing::info!("persona profile {slug} @{watermark}: {summary}");
        Ok(Json(summary))
    }
}

// ---- PersonaPredict ----------------------------------------------------------

/// Predict what one persona would do about one subject.
///
/// Keyed `{slug}@{kind}@{produced_by}@{subject}@{watermark}` — five positional components,
/// parsed by [`PredictKey`]. Positional rather than named because the key has to be
/// *recoverable*: `produced_by` is in there so that asking a cloud model for its own read of
/// the same subject is a different key rather than a refused duplicate of the local one, the
/// same way `subject_explanations` keys on `produced_by` so a second opinion can sit beside
/// the first.
pub struct PersonaPredict {
    ops: Arc<WorkflowOps>,
}

impl PersonaPredict {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }

    /// The local model does the predicting unless the operator named another, in which case
    /// the scope is wrong for it — but a per-invocation scope is not something the ingress
    /// offers, and getting this wrong costs a queued call rather than a wrong answer.
    pub const SCOPE: &'static str = scopes::LOCAL_LLM;
}

#[restate_sdk::workflow]
impl PersonaPredict {
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("PersonaPredict", &key, self.predict(ctx)).await
    }
}

impl PersonaPredict {
    async fn predict(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let Some(parsed) = PredictKey::parse(ctx.key()) else {
            return Err(TerminalError::new(format!(
                "'{}' is not a prediction key (expected \
                 slug@kind@produced_by@subject@watermark)",
                ctx.key()
            ))
            .into());
        };

        // Step 1: assemble the dossier. A store read, journalled so a model failure in step 3
        // does not re-gather it.
        let ops = self.ops.clone();
        let dossier = {
            let subject = parsed.subject.clone();
            ctx.run(|| {
                let ops = ops.clone();
                let subject = subject.clone();
                async move {
                    // The *same* gather as `Explain`, deliberately: a prediction and an
                    // explanation should disagree because they are different questions, not
                    // because they were shown different facts.
                    let g = super::explain::gather(&ops, &subject).map_err(classify)?;
                    Ok(serde_json::to_string_pretty(&g).unwrap_or_default())
                }
            })
            .await?
        };

        // Step 2: the diff, for a predicted code review. Read from the pull request's own
        // object state, which costs one ingress round trip and no API call — and is `None`
        // rather than fatal when nothing has been stored yet, because the predictor says so
        // in its caveats instead of inventing a review.
        let diff = if parsed.kind == PredictionKind::CodeReview && parsed.subject.contains('!') {
            let stored = ctx
                .object_client::<crate::restate::objects::pull_request::PullRequestClient>(
                    parsed.subject.clone(),
                )
                .diff()
                .call()
                .await?
                .into_inner();
            stored.map(|d| crate::prdiff::render_for_prompt(&d.report))
        } else {
            None
        };

        // Step 3: the model call.
        let reasoner = match parsed.model() {
            Some((provider, model)) => self.ops.reasoner_for(provider, model),
            // The persona tier — Claude by default — not the local explainer. A prediction is
            // an opinion about a person in the same cited-JSON contract the profile uses, so it
            // belongs on the tier that can hold that contract. See `[personas] profile_tier`.
            None => self.ops.personas.opinion_reasoner(),
        };
        let ops = self.ops.clone();
        let produced_by = parsed.produced_by.clone();
        let out = ctx
            .run(|| {
                let ops = ops.clone();
                let parsed = parsed.clone();
                let dossier = dossier.clone();
                let diff = diff.clone();
                let reasoner = reasoner.clone();
                let produced_by = produced_by.clone();
                async move {
                    let p = ops
                        .personas
                        .predict(
                            &parsed.slug,
                            &parsed.subject,
                            parsed.kind,
                            &parsed.watermark,
                            dossier,
                            diff,
                            &produced_by,
                            reasoner.as_ref(),
                        )
                        .await
                        .map_err(classify)?;
                    Ok(Json(serde_json::json!({
                        "persona": p.persona,
                        "subject": p.subject_key,
                        "kind": p.kind.as_str(),
                        "would_engage": p.would_engage,
                        "recommendation": p.recommendation,
                        "points": p.points.len(),
                    })))
                }
            })
            .await?
            .into_inner();
        Ok(Json(out))
    }
}

/// A `PersonaPredict` key, taken apart.
///
/// Five positional components separated by `@`, and the split is bounded at five so the
/// **watermark keeps its own `@`s**. That is not hypothetical: a GitHub signal id is
/// `github/24800345076@2026-07-27T21:35:05Z`, so a real key carries two more separators than
/// the format suggests. Splitting greedily is the bug that made every `Explain` on a
/// notification-sourced subject fail instantly with a subject key that did not exist — see
/// [`super::split_versioned`], which documents the same failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictKey {
    pub slug: String,
    pub kind: PredictionKind,
    /// `local`, or `{provider}~{model}` for a model the operator named. `~` rather than `/`
    /// because a subject key already contributes slashes and the pair reads better apart.
    pub produced_by: String,
    pub subject: String,
    pub watermark: String,
}

impl PredictKey {
    pub fn new(
        slug: &str,
        kind: PredictionKind,
        produced_by: &str,
        subject: &str,
        watermark: &str,
    ) -> Self {
        Self {
            slug: slug.to_string(),
            kind,
            produced_by: produced_by.to_string(),
            subject: subject.to_string(),
            watermark: watermark.to_string(),
        }
    }

    /// `local`, or a label naming the provider and model the operator picked.
    pub fn model_label(provider: &str, model: &str) -> String {
        format!("{provider}~{model}")
    }

    /// The provider and model this key names, or `None` for the local default.
    pub fn model(&self) -> Option<(&str, &str)> {
        self.produced_by.split_once('~')
    }

    pub fn format(&self) -> String {
        format!(
            "{}@{}@{}@{}@{}",
            self.slug,
            self.kind.as_str(),
            self.produced_by,
            self.subject,
            self.watermark
        )
    }

    pub fn parse(key: &str) -> Option<Self> {
        let mut parts = key.splitn(5, '@');
        let slug = parts.next()?;
        let kind = PredictionKind::parse(parts.next()?)?;
        let produced_by = parts.next()?;
        let subject = parts.next()?;
        let watermark = parts.next()?;
        if slug.is_empty() || subject.is_empty() {
            return None;
        }
        Some(Self {
            slug: slug.to_string(),
            kind,
            produced_by: produced_by.to_string(),
            subject: subject.to_string(),
            watermark: watermark.to_string(),
        })
    }
}

/// Error classification, per the standing rule that Restate retries anything not terminal.
///
/// A missing persona, a missing subject, and a disabled feature are all decisions rather than
/// flakes: nothing a retry does brings them back, and retrying forever against a slug the
/// operator deleted is the failure a `RepoIndexer` tick had against a renamed repo. A model
/// that is down or a search that rate-limited is exactly what durable execution is for.
fn classify(e: anyhow::Error) -> HandlerError {
    let msg = format!("{e:#}");
    if is_decision(&msg) {
        TerminalError::new(msg).into()
    } else {
        HandlerError::from(anyhow::anyhow!(msg))
    }
}

/// Whether a failure is a decision rather than a flake.
///
/// Split out from [`classify`] so it can be tested: `HandlerError` wraps its terminal-ness in
/// a private field, so the only way to assert this classification is to assert on the
/// predicate that drives it.
fn is_decision(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    [
        "no persona",
        "no subject",
        "are disabled",
        "needs a handle",
        "already belongs to",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip, and the failure it exists to prevent: a watermark containing `@`
    /// eating the components after it.
    #[test]
    fn prediction_keys_round_trip_with_at_signs_in_the_watermark() {
        // The real shape, from a real notification-sourced subject: the watermark carries two
        // more `@` than the format suggests.
        let key = PredictKey::new(
            "pcholakov",
            PredictionKind::CodeReview,
            "local",
            "restatedev/restate-cloud!1235",
            "github/24800345076@2026-07-27T21:35:05Z+0",
        );
        let formatted = key.format();
        let parsed = PredictKey::parse(&formatted).expect("must parse");
        assert_eq!(parsed, key, "formatted {formatted}");
        assert_eq!(parsed.subject, "restatedev/restate-cloud!1235");
        assert_eq!(
            parsed.watermark,
            "github/24800345076@2026-07-27T21:35:05Z+0"
        );

        // The other subject shapes: slashes, dots, `#`, and a prefixed incident reference.
        for subject in [
            "restatedev/restate#412",
            "C02ABC/1721822400.001",
            "incident:INC-448",
        ] {
            let k = PredictKey::new("p", PredictionKind::IssueResponse, "local", subject, "w");
            assert_eq!(PredictKey::parse(&k.format()).unwrap().subject, subject);
        }
    }

    /// The model rides in the key so a cloud second read sits beside the local one rather
    /// than being refused as a duplicate of it.
    #[test]
    fn the_model_is_recoverable_from_the_key() {
        let local = PredictKey::new("p", PredictionKind::CodeReview, "local", "o/r!1", "w");
        assert_eq!(local.model(), None);

        let label = PredictKey::model_label("anthropic", "claude-opus-5");
        let cloud = PredictKey::new("p", PredictionKind::CodeReview, &label, "o/r!1", "w");
        assert_eq!(cloud.model(), Some(("anthropic", "claude-opus-5")));
        // Different keys, so both answers can exist at once.
        assert_ne!(local.format(), cloud.format());
        assert_eq!(
            PredictKey::parse(&cloud.format()).unwrap().model(),
            Some(("anthropic", "claude-opus-5"))
        );
    }

    #[test]
    fn malformed_keys_are_refused_rather_than_guessed() {
        assert!(PredictKey::parse("pcholakov").is_none());
        assert!(PredictKey::parse("pcholakov@code_review").is_none());
        // An unknown kind: guessing one would predict a code review on a Slack thread.
        assert!(PredictKey::parse("p@telepathy@local@o/r!1@w").is_none());
        // An empty subject would gather a dossier for nothing.
        assert!(PredictKey::parse("p@code_review@local@@w").is_none());
    }

    /// Only a decision is terminal. A rate limit or a model that is down must be retried,
    /// which is the whole reason these are workflows.
    #[test]
    fn only_decisions_are_terminal() {
        for decided in [
            "no persona 'pcholakov'",
            "no subject o/r#1",
            "personas are disabled — set `[personas] enabled = true`",
            "github 'pcholakov' already belongs to the persona 'pav'",
        ] {
            assert!(is_decision(decided), "{decided} should be terminal");
        }
        for flake in [
            "connection refused (os error 61)",
            "GitHub returned 403: rate limit exceeded",
            "ollama is not running",
            "error sending request for url (http://localhost:11434/api/chat)",
        ] {
            assert!(!is_decision(flake), "{flake} should be retried");
        }
    }
}
