//! The `Issue` virtual object — the top of the hierarchy.
//!
//! Keyed `owner/repo#412` (or `owner/repo~7` for a discussion). An issue is the
//! durable statement of what the work is, so this is where a PR's activity and a
//! Slack thread's context ultimately land.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::state;
use crate::restate::ops::{Recorded, SubjectOps};
use crate::subject::{Handled, SubjectKey};

pub struct Issue {
    ops: Arc<SubjectOps>,
}

impl Issue {
    pub fn new(ops: Arc<SubjectOps>) -> Self {
        Self { ops }
    }
}

#[restate_sdk::object]
impl Issue {
    /// Attribute a signal to this issue and arm the re-analysis debounce.
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
                super::debounce::arm::<IssueClient>(&ctx, self.ops.debounce).await?;
            }
            // This subject was merged away; the canonical one owns the signal and the
            // debounce that follows it.
            Recorded::Forwarded(canonical) => {
                super::send_record(&ctx, &canonical, &signal_id);
            }
        }
        Ok(())
    }

    /// The debounced analysis pass.
    ///
    /// Fired by a durable timer armed in [`Self::record`]. The stale check is what
    /// makes coalescing work, and it is only safe because both handlers are
    /// exclusive on the same key.
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

/// Reject an unparseable key rather than creating a subject nothing can address.
pub(crate) fn parse_key(raw: &str) -> Result<SubjectKey, TerminalError> {
    SubjectKey::parse(raw).map_err(|e| TerminalError::new(format!("{e:#}")))
}

/// A failure in our own store or reasoner is terminal for this invocation: Restate
/// retries transient errors *forever*, and re-running a broken analysis on a loop
/// would spend the operator's model budget on the same failure.
pub(crate) fn terminal(e: impl std::fmt::Display) -> TerminalError {
    TerminalError::new(format!("{e}"))
}

/// Lets the shared debounce schedule `analyze` on *this* object without knowing
/// which of the three it is.
impl super::debounce::AnalyzeClient for IssueClient<'_> {
    fn send_analyze_after(ctx: &ObjectContext<'_>, delay: std::time::Duration) {
        ctx.object_client::<IssueClient>(ctx.key().to_string())
            .analyze()
            .send_after(delay);
    }
}
