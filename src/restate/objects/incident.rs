//! The `Incident` virtual object — one production incident, from incident.io.
//!
//! Keyed `incident:INC-448`. Two things make it unlike the three GitHub/Slack subjects it
//! otherwise mirrors:
//!
//! **It is never merged away.** The other objects all carry `mark_same_as`, because a Slack
//! thread that turns out to be about a filed issue should stop being its own card. An
//! incident is the opposite: it stays its own card on the incidents board for its whole
//! life, and the issues, pull requests and commits it turns out to be about are attached to
//! it as *edges*. Absorbing it into an issue would take the incident off the incidents board
//! and hide the very thing that board exists to track.
//!
//! **Its lifecycle comes from upstream, not from the operator.** A GitHub subject leaves the
//! board when you acknowledge or resolve it. An incident leaves when incident.io says it is
//! closed — `triage` / `active` / `post-incident` are open, everything else is over. The
//! watcher reconciles that (see [`crate::watchers::incident`]), so the board is a mirror of
//! what is actually burning rather than of what you have read.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::issue::{parse_key, terminal};
use super::state;
use crate::restate::ops::{Recorded, SubjectOps};
use crate::subject::Handled;

pub struct Incident {
    ops: Arc<SubjectOps>,
}

impl Incident {
    pub fn new(ops: Arc<SubjectOps>) -> Self {
        Self { ops }
    }
}

#[restate_sdk::object]
impl Incident {
    /// Attribute a signal to this incident.
    ///
    /// Takes the signal **id** for the same reason the others do: the body is already in
    /// SQLite, and passing it here would put it in the invocation journal to be replayed on
    /// every retry.
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
            Recorded::Muted => {}
            Recorded::Analyze => {
                super::debounce::arm::<IncidentClient>(&ctx, self.ops.debounce).await?;
            }
            // Cannot happen — an incident is never marked `same_as`, so nothing forwards
            // away from it. Handled rather than ignored so that if that ever changes, the
            // signal follows the pointer instead of being dropped on the floor here.
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

    /// Operator triage. Still offered: an incident you have read is worth marking read, even
    /// though it is upstream that decides when it leaves the board.
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

    /// The read surface. **Shared**, so reading the incidents board never queues behind an
    /// in-progress analysis.
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

/// Lets the shared debounce schedule `analyze` on this object without knowing which kind it
/// is.
impl super::debounce::AnalyzeClient for IncidentClient<'_> {
    fn send_analyze_after(ctx: &ObjectContext<'_>, delay: std::time::Duration) {
        ctx.object_client::<IncidentClient>(ctx.key().to_string())
            .analyze()
            .send_after(delay);
    }
}
