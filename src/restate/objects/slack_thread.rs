//! The `SlackThread` virtual object — a conversation that *is* the work.
//!
//! Keyed `channel/thread_ts`. This exists only when no GitHub artifact resolves: an
//! alert in `#alerts` about a service with no filed issue, a DM, an incident thread.
//! When a conversation does resolve upward its content becomes context on the issue
//! or PR and no object is created — evidence about the work, not a second card.
//!
//! Late resolution is the interesting case: an alert thread often names the issue on
//! message twelve, by which point this object already has analysis and notifications
//! of its own. `mark_same_as` demotes it rather than leaving two cards.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::issue::{parse_key, terminal};
use super::state;
use crate::restate::ops::{Recorded, SubjectOps};
use crate::subject::Handled;

pub struct SlackThread {
    ops: Arc<SubjectOps>,
}

impl SlackThread {
    pub fn new(ops: Arc<SubjectOps>) -> Self {
        Self { ops }
    }
}

#[restate_sdk::object]
impl SlackThread {
    /// Attribute a signal to this conversation.
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
                super::debounce::arm::<SlackThreadClient>(&ctx, self.ops.debounce).await?;
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
impl super::debounce::AnalyzeClient for SlackThreadClient<'_> {
    fn send_analyze_after(ctx: &ObjectContext<'_>, delay: std::time::Duration) {
        ctx.object_client::<SlackThreadClient>(ctx.key().to_string())
            .analyze()
            .send_after(delay);
    }
}
