//! The `PullRequest` virtual object — one attempt at the work.
//!
//! Keyed `owner/repo!987`. The `!` rather than `#` is deliberate: issue and PR
//! numbering are independent, so `o/r#5` and `o/r!5` are different things.
//!
//! A PR is filed under the issue it closes (`subject_links`), so a review request on
//! the PR and a comment on the issue read as one piece of work.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::issue::{parse_key, terminal};
use super::state;
use crate::restate::ops::{Recorded, SubjectOps};
use crate::subject::Handled;

pub struct PullRequest {
    ops: Arc<SubjectOps>,
}

impl PullRequest {
    pub fn new(ops: Arc<SubjectOps>) -> Self {
        Self { ops }
    }
}

#[restate_sdk::object]
impl PullRequest {
    /// Attribute a signal to this pull request.
    ///
    /// Takes the signal **id**: the body is already in SQLite, and passing it here
    /// would put it in the invocation journal to be replayed on every retry.
    #[handler]
    async fn record(&self, ctx: ObjectContext<'_>, signal_id: String) -> HandlerResult<()> {
        let key = parse_key(ctx.key())?;
        let count: u32 = ctx.get(state::SIGNAL_COUNT).await?.unwrap_or(0);
        ctx.set(state::SIGNAL_COUNT, count + 1);
        let outcome = ctx
            .run(|| async {
                Ok(Json(
                    self.ops.record(&key, &signal_id).await.map_err(terminal)?,
                ))
            })
            .await?
            .into_inner();
        match outcome {
            // Arming a timer for muted work would buy a pass the Analyst refuses.
            Recorded::Muted => {}
            Recorded::Analyze => {
                super::debounce::arm::<PullRequestClient>(&ctx, self.ops.debounce).await?;
            }
            // This subject was merged away; the canonical one owns the signal and the
            // debounce that follows it.
            Recorded::Forwarded(canonical) => {
                super::send_record(&ctx, &canonical, &signal_id);
            }
        }
        Ok(())
    }

    /// The debounced analysis pass — see [`super::issue::Issue::analyze`].
    #[handler]
    async fn analyze(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        let key = parse_key(ctx.key())?;
        if !super::debounce::due(&ctx).await? {
            return Ok(());
        }
        ctx.run(|| async { Ok(self.ops.analyze(&key).await.map_err(terminal)?) })
            .await?;
        Ok(())
    }

    #[handler]
    async fn triage(&self, ctx: ObjectContext<'_>, handled: String) -> HandlerResult<()> {
        let key = parse_key(ctx.key())?;
        let handled = Handled::parse(&handled)
            .ok_or_else(|| TerminalError::new(format!("unknown triage state '{handled}'")))?;
        ctx.run(|| async {
            Ok(self
                .ops
                .triage(&key, handled, None)
                .await
                .map_err(terminal)?)
        })
        .await?;
        Ok(())
    }

    #[handler]
    async fn set_tags(&self, ctx: ObjectContext<'_>, tags: Json<Vec<String>>) -> HandlerResult<()> {
        let key = parse_key(ctx.key())?;
        let tags = tags.into_inner();
        ctx.run(|| async {
            Ok(self
                .ops
                .set_tags(&key, tags.clone())
                .await
                .map_err(terminal)?)
        })
        .await?;
        Ok(())
    }

    /// Merged away into `canonical`; activity recorded here forwards there.
    #[handler]
    async fn mark_same_as(&self, ctx: ObjectContext<'_>, canonical: String) -> HandlerResult<()> {
        let key = parse_key(ctx.key())?;
        let canonical = parse_key(&canonical)?;
        ctx.run(|| async {
            Ok(self
                .ops
                .mark_same_as(&key, &canonical)
                .await
                .map_err(terminal)?)
        })
        .await?;
        Ok(())
    }

    /// The read surface. **Shared**, so the board reading two hundred subjects never
    /// queues behind an in-progress analysis.
    #[handler]
    async fn get(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = parse_key(ctx.key())?;
        let view = self
            .ops
            .attributor
            .subject_view(key.as_str())
            .map_err(terminal)?;
        Ok(Json(serde_json::to_value(view).map_err(terminal)?))
    }

    /// Store this pull request's summarized diff.
    ///
    /// Exclusive, so a refresh landing while the board reads gets serialized rather than
    /// interleaved. The payload *is* the diff — the one place this codebase deliberately
    /// ships a body through the ingress rather than an id, because the body is the fact
    /// being stored and there is nowhere else it lives. [`crate::prdiff::trim_for_state`]
    /// is what keeps that payload bounded; the caller applies it before sending.
    #[handler]
    async fn put_diff(
        &self,
        ctx: ObjectContext<'_>,
        diff: Json<crate::prdiff::StoredDiff>,
    ) -> HandlerResult<()> {
        let diff = diff.into_inner();
        let key = ctx.key().to_string();
        ctx.set(state::DIFF, Json(diff));
        tracing::debug!("stored diff for {key}");
        Ok(())
    }

    /// This pull request's stored diff, or `null` if it has never been read.
    ///
    /// **Shared**: reading a diff must never queue behind an analysis pass on the same PR,
    /// which is exactly when somebody is most likely to be looking at it.
    #[handler]
    async fn diff(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> HandlerResult<Json<Option<crate::prdiff::StoredDiff>>> {
        let stored: Option<Json<crate::prdiff::StoredDiff>> = ctx.get(state::DIFF).await?;
        Ok(Json(stored.map(|d| d.into_inner())))
    }

    /// Store this pull request's code review. Same shape as [`Self::put_diff`], and stored
    /// separately for the same reason it is computed separately: the diff is a fact, the
    /// review is a judgment about it, and one arriving does not imply the other.
    #[handler]
    async fn put_review(
        &self,
        ctx: ObjectContext<'_>,
        review: Json<crate::prdiff::StoredReview>,
    ) -> HandlerResult<()> {
        let key = ctx.key().to_string();
        ctx.set(state::REVIEW, review);
        tracing::debug!("stored review for {key}");
        Ok(())
    }

    /// This pull request's stored review, or `null` if it has not been reviewed. **Shared**,
    /// for the same reason [`Self::diff`] is.
    #[handler]
    async fn review(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> HandlerResult<Json<Option<crate::prdiff::StoredReview>>> {
        let stored: Option<Json<crate::prdiff::StoredReview>> = ctx.get(state::REVIEW).await?;
        Ok(Json(stored.map(|r| r.into_inner())))
    }

    /// Receive a durable subject write. See [`super::put_subject`].
    #[handler]
    async fn put_subject(
        &self,
        ctx: ObjectContext<'_>,
        subject: Json<crate::subject::Subject>,
    ) -> HandlerResult<()> {
        super::put_subject(&ctx, subject.into_inner()).await
    }

    /// Receive a durable signal write. See [`super::put_signal`].
    #[handler]
    async fn put_signal(
        &self,
        ctx: ObjectContext<'_>,
        signal: Json<crate::signal::Signal>,
    ) -> HandlerResult<()> {
        super::put_signal(&ctx, signal.into_inner()).await
    }

    /// Drop a signal that has moved to another subject. See [`super::drop_signal`].
    #[handler]
    async fn drop_signal(&self, ctx: ObjectContext<'_>, id: String) -> HandlerResult<()> {
        super::drop_signal(&ctx, id).await
    }
}

/// Lets the shared debounce schedule `analyze` on *this* object without knowing
/// which of the three it is.
impl super::debounce::AnalyzeClient for PullRequestClient<'_> {
    fn send_analyze_after(ctx: &ObjectContext<'_>, delay: std::time::Duration) {
        ctx.object_client::<PullRequestClient>(ctx.key().to_string())
            .analyze()
            .send_after(delay);
    }
}
