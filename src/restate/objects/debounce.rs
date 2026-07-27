//! Re-analysis debounce, as a durable timer on the subject.
//!
//! Any activity marks a subject live and schedules a re-analysis: **1 minute after
//! the last activity, with a 5-minute hard cap** so a fast-moving conversation still
//! gets analyzed. Coalescing many events into one model call is the whole point — a
//! busy incident thread would otherwise re-summarize on every message.
//!
//! The pre-Restate version kept the pending set in a `HashMap` behind a tick loop,
//! which lost every pending re-analysis on restart. During development that is most
//! of them: `tilt up` rebuilds the binary on each save. A durable timer survives,
//! and the stale-deadline check below is safe precisely because `record` and
//! `analyze` are both exclusive handlers on the same key — they cannot interleave.

use restate_sdk::prelude::*;
use std::time::Duration;

use super::state;

/// Debounce settings, resolved from `[live]` at boot.
#[derive(Debug, Clone, Copy)]
pub struct Debounce {
    pub quiet: Duration,
    /// Hard cap measured from the first activity in this window.
    pub max: Duration,
}

impl Default for Debounce {
    fn default() -> Self {
        Self {
            quiet: Duration::from_secs(60),
            max: Duration::from_secs(300),
        }
    }
}

/// Arm (or push out) the deadline and schedule the pass.
///
/// Every call schedules its own timer; the ones whose deadline has since moved
/// return early from [`due`]. Cancelling instead would mean tracking invocation ids
/// in state to cancel them by, which is more moving parts for the same outcome.
pub async fn arm<C>(ctx: &ObjectContext<'_>, debounce: Debounce) -> Result<(), TerminalError>
where
    C: AnalyzeClient + ?Sized,
{
    let now = ctx.run(|| async { Ok(now_millis()) }).await?;
    let first: u64 = match ctx.get(state::FIRST_ACTIVITY).await? {
        Some(t) => t,
        None => {
            ctx.set(state::FIRST_ACTIVITY, now);
            now
        }
    };
    let quiet_deadline = now + debounce.quiet.as_millis() as u64;
    let hard_deadline = first + debounce.max.as_millis() as u64;
    let deadline = quiet_deadline.min(hard_deadline);
    ctx.set(state::DEBOUNCE_DEADLINE, deadline);

    let delay = Duration::from_millis(deadline.saturating_sub(now));
    C::send_analyze_after(ctx, delay);
    Ok(())
}

/// Is the pass that just woke up the current one?
///
/// A later `record` moved the deadline and armed its own timer, so this one is stale
/// and does nothing. Returning `true` also clears the window, so the next activity
/// starts a fresh hard cap.
pub async fn due(ctx: &ObjectContext<'_>) -> Result<bool, TerminalError> {
    let now = ctx.run(|| async { Ok(now_millis()) }).await?;
    match ctx.get::<u64>(state::DEBOUNCE_DEADLINE).await? {
        // No deadline recorded means a *previous* pass already ran and cleared the
        // window: this timer is a leftover from an earlier `arm`. Treating the
        // absence as "due" (which an earlier version of this did) makes every
        // coalesced timer do the work anyway — the exclusive handler serializes
        // them, so it looks fine and quietly costs N model passes instead of one.
        None => Ok(false),
        // A later `record` pushed the deadline out; that call armed its own timer.
        // The slop tolerates scheduling jitter, or a timer firing a millisecond
        // early would drop the analysis entirely.
        Some(deadline) if deadline > now + 500 => Ok(false),
        Some(_) => {
            ctx.clear(state::DEBOUNCE_DEADLINE);
            ctx.clear(state::FIRST_ACTIVITY);
            Ok(true)
        }
    }
}

/// Wall-clock inside a `ctx.run`, so the journal records the value the first
/// attempt saw rather than re-reading the clock on replay.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lets [`arm`] schedule `analyze` without knowing which of the three objects it is
/// scheduling on. Implemented per object in [`super`].
pub trait AnalyzeClient {
    fn send_analyze_after(ctx: &ObjectContext<'_>, delay: Duration);
}
