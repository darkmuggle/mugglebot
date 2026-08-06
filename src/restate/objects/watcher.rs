//! The `Watcher` virtual object — one per source, driving its own poll loop.
//!
//! Keyed by watcher name (`github-notifications`, `github-assigned`, `slack`,
//! `granola`). Three things this buys over the `tokio` loop it replaces:
//!
//! 1. **Two polls can never overlap.** That used to be implicit in the shape of the
//!    loop, and it broke the moment you added a "poll now" button: two concurrent
//!    polls double-read the cursor and either duplicate or skip. Here "poll now" is
//!    just another call, and it queues behind the scheduled one.
//! 2. **The cadence is a durable timer.** A restart doesn't skip a beat, and adaptive
//!    backoff is a longer `send_after` rather than a sleeping task.
//! 3. **The cursor is object state**, so a restart resumes rather than re-reading from
//!    the top (a replay) or from now (a gap).
//!
//! The watchers themselves stay in the daemon — they hold the HTTP clients and the
//! tokens, and Slack's Socket Mode is a persistent connection that cannot be a poll
//! handler at all. The object addresses them by name.
//!
//! One caveat, straight from Restate's own cron guidance: a failing poll being
//! retried while the next tick arrives can overlap. Exclusivity serializes them, and
//! [`super::debounce`]-style staleness isn't needed here because a poll is idempotent
//! by contract — re-emitting a signal is free, the ingress key dedups it.

use std::sync::Arc;

use restate_sdk::prelude::*;

use crate::restate::pipeline::IngestOps;
use crate::subject::SubjectKey;

/// Object state keys.
const CURSOR: &str = "cursor";
const NEXT_POLL_AT: &str = "next_poll_at";
const POLLS: &str = "polls";

pub struct Watcher {
    ops: Arc<IngestOps>,
}

impl Watcher {
    pub fn new(ops: Arc<IngestOps>) -> Self {
        Self { ops }
    }
}

#[restate_sdk::object]
impl Watcher {
    /// Arm the loop if it isn't already armed. Called for each enabled watcher at
    /// boot.
    ///
    /// Idempotent on purpose: the timer armed by a previous process is *durable*, so
    /// arming again on every restart would multiply the poll rate by the number of
    /// times you've restarted. It re-arms only when the recorded next poll is long
    /// past — which is what recovers the loop after the cluster state is wiped.
    #[handler]
    async fn start(&self, ctx: ObjectContext<'_>) -> HandlerResult<bool> {
        let name = ctx.key().to_string();
        let Some(watcher) = self.ops.watcher(&name) else {
            return Err(TerminalError::new(format!("no watcher named '{name}'")).into());
        };
        let interval = watcher.interval();
        let now = ctx.run(|| async { Ok(now_millis()) }).await?;
        let next: Option<u64> = ctx.get(NEXT_POLL_AT).await?;
        let stale = match next {
            None => true,
            // Two intervals of slack: a timer that merely hasn't fired yet is fine,
            // one that's long overdue means the timer itself is gone.
            Some(at) => now > at + 2 * interval.as_millis() as u64,
        };
        if !stale {
            tracing::debug!("watcher '{name}': loop already armed");
            return Ok(false);
        }
        tracing::info!(
            "watcher '{name}': arming poll loop every {}s",
            interval.as_secs()
        );
        ctx.set(NEXT_POLL_AT, now);
        ctx.object_client::<WatcherClient>(name).poll().send();
        Ok(true)
    }

    /// One poll, then schedule the next.
    ///
    /// The steps are separate `ctx.run` blocks so a rate limit during classification
    /// doesn't re-insert the signals that already landed.
    #[handler]
    async fn poll(&self, ctx: ObjectContext<'_>) -> HandlerResult<u64> {
        let name = ctx.key().to_string();
        let Some(watcher) = self.ops.watcher(&name) else {
            return Err(TerminalError::new(format!("no watcher named '{name}'")).into());
        };
        let interval = watcher.interval();
        let cursor: Option<String> = ctx.get(CURSOR).await?;

        // Arm the next poll FIRST, before anything that can fail.
        //
        // A watcher that stops polling because one poll failed is a source that goes
        // quietly stale, which is the failure this whole design exists to avoid — and
        // every step below ends in `?`. Rescheduling up front is safe because the send
        // is journalled: a retry of this handler replays it rather than arming a second
        // timer, and a poll is idempotent by contract anyway.
        let now = ctx.run(|| async { Ok(now_millis()) }).await?;
        ctx.set(NEXT_POLL_AT, now + interval.as_millis() as u64);
        ctx.object_client::<WatcherClient>(name.clone())
            .poll()
            .send_after(interval);

        let ops = self.ops.clone();
        let name_for_run = name.clone();
        let outcome = ctx
            .run(|| {
                let ops = ops.clone();
                let name = name_for_run.clone();
                let cursor = cursor.clone();
                async move {
                    match ops.poll_once(&name, cursor.as_deref()).await {
                        Ok(o) => Ok(Json(o)),
                        Err(e) => {
                            // Poll failures are transient by nature (rate limits, a
                            // flaky network, an expired conditional request), so they're
                            // recorded and swallowed rather than failing the invocation
                            // — the next tick, already armed above, tries again.
                            ops.record_failure(&name, &format!("{e:#}"));
                            tracing::warn!("watcher '{name}' poll error: {e:#}");
                            Ok(Json(crate::restate::pipeline::PollOutcome::default()))
                        }
                    }
                }
            })
            .await?
            .into_inner();

        if let Some(cursor) = &outcome.cursor {
            ctx.set(CURSOR, cursor.clone());
        }
        let polls: u64 = ctx.get(POLLS).await?.unwrap_or(0);
        ctx.set(POLLS, polls + 1);

        // Hand each new signal to the subject that owns it. A `send` rather than a
        // call: the subject's exclusive handler serializes the attribution write
        // against every other writer for that key — including the other GitHub watcher
        // — and this poll has no reason to wait for the analysis it triggers.
        for (signal_id, subject_key) in &outcome.routed {
            if subject_key.is_empty() {
                continue; // the unattributed lane; the signal is stored, nothing owns it
            }
            let Ok(key) = SubjectKey::parse(subject_key) else {
                tracing::warn!("watcher '{name}': '{subject_key}' is not a subject key");
                continue;
            };
            super::send_record(&ctx, &key, signal_id);
        }

        // Per-message Slack tagging, off the poll path so a model call never stalls
        // ingest.
        // Tell each modelled person's object they were active. Debounced on the other side,
        // so a burst of nine messages is one refresh — see `Persona::engaged`.
        for slug in &outcome.engaged {
            ctx.object_client::<super::persona::PersonaClient>(slug.clone())
                .engaged()
                .send();
        }

        if !outcome.to_classify.is_empty() {
            let ops = self.ops.clone();
            let ids = outcome.to_classify.clone();
            ctx.run(|| {
                let ops = ops.clone();
                let ids = ids.clone();
                async move { Ok(ops.classify(&ids).await.unwrap_or(0) as u64) }
            })
            .await?;
        }

        if !outcome.routed.is_empty() || outcome.refreshed > 0 || outcome.resolved > 0 {
            let ops = self.ops.clone();
            ctx.run(|| {
                let ops = ops.clone();
                async move {
                    ops.repair();
                    ops.push_board();
                    Ok(())
                }
            })
            .await?;
        }
        if outcome.new_count > 0 || outcome.resolved > 0 || outcome.refreshed > 0 {
            tracing::info!(
                "watcher '{name}': {} new, {} refreshed, {} gone upstream",
                outcome.new_count,
                outcome.refreshed,
                outcome.resolved
            );
        }
        Ok(polls + 1)
    }

    /// Cursor, poll count, and next scheduled poll — for the health panel.
    #[handler]
    async fn status(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        Ok(Json(serde_json::json!({
            "name": ctx.key(),
            "cursor": ctx.get::<String>(CURSOR).await?,
            "polls": ctx.get::<u64>(POLLS).await?.unwrap_or(0),
            "next_poll_at": ctx.get::<u64>(NEXT_POLL_AT).await?,
        })))
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
