//! The `Persona` virtual object — one modelled person, owning their harvest loop.
//!
//! Keyed by slug (`pcholakov`). An object rather than a workflow for exactly the reasons
//! [`super::repo_indexer`] is one: harvesting is **recurring**, **resumable**, and needs a
//! **cursor**. A workflow instance per pass would be lifecycle for nothing.
//!
//! Three things the object buys, in order of how much they matter:
//!
//! 1. **One harvest per person at a time.** Two concurrent passes would issue the same two
//!    searches, read the same pull requests, and both advance the backward cursor — so the
//!    walk would skip a window while paying twice for the one before it. Per-key exclusivity
//!    forbids that; a "harvest now" button is a call that queues behind the scheduled pass
//!    rather than racing it.
//! 2. **A cadence that adapts.** While the history walk is still going the tick is short,
//!    because every step is a bounded page and the point is to finish. Once it has reached the
//!    floor the tick drops to a slow refresh that only picks up new activity.
//! 3. **A durable timer.** The loop survives a restart, which during a `tilt up` rebuild
//!    cycle is most of them.
//!
//! # It is not a subject
//!
//! Nothing here `record`s a signal, and there is deliberately no `triage`, no `same_as` and no
//! `attention`. A persona never appears on the board and never competes for attention — see
//! [`crate::persona`] on why that keeps the "`person` is never a subject" rule intact. What
//! this object holds is a *process*; the profile itself is in SQLite, because it is the
//! expensive artifact and `data/restate` is wiped whenever vqueues are toggled.

use std::sync::Arc;

use restate_sdk::prelude::*;

use crate::persona::{harvest::Trigger, Engine};

const TICKS: &str = "ticks";
const NEXT_TICK_AT: &str = "next_tick_at";
/// Whether the backward history walk has reached its floor.
const BACKFILL_COMPLETE: &str = "backfill_complete";
const EVIDENCE: &str = "evidence";
/// The evidence watermark at the last profile submission, so a tick that harvested nothing
/// new does not re-submit a `PersonaProfile` it already submitted.
const PROFILED_AT_WATERMARK: &str = "profiled_at_watermark";

pub struct Persona {
    engine: Arc<Engine>,
}

impl Persona {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

#[restate_sdk::object]
impl Persona {
    /// Arm this persona's harvest loop.
    ///
    /// Idempotent by staleness, with the same caution as every other durable-timer loop here:
    /// the timer survives a restart, so re-arming on every boot would multiply the tick rate
    /// by the number of restarts. Returns whether this call armed it.
    #[handler]
    async fn start(&self, ctx: ObjectContext<'_>) -> HandlerResult<bool> {
        let slug = ctx.key().to_string();
        if !self.engine.enabled {
            return Err(TerminalError::new(
                "personas are disabled — set `[personas] enabled = true`",
            )
            .into());
        }
        let steady = self.engine.harvest_interval;
        let now = ctx.run(|| async { Ok(now_millis()) }).await?;
        let next: Option<u64> = ctx.get(NEXT_TICK_AT).await?;
        let stale = match next {
            None => true,
            // Two intervals of slack, so a tick that is merely late is not treated as a
            // dropped loop and re-armed alongside itself.
            Some(at) => now > at + 2 * steady.as_millis() as u64,
        };
        if !stale {
            return Ok(false);
        }
        tracing::info!("persona {slug}: arming the harvest loop");
        ctx.set(NEXT_TICK_AT, now);
        ctx.object_client::<PersonaClient>(slug).tick().send();
        Ok(true)
    }

    /// One bounded harvest pass, then reschedule.
    #[handler]
    async fn tick(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let slug = ctx.key().to_string();

        // A persona the operator deleted ends its own loop, rather than warning on every tick
        // forever against a slug that no longer exists. This is the failure a `RepoIndexer`
        // had against a repo renamed upstream — its timer kept firing against a deleted row,
        // and because the error was swallowed as transient nothing ever retired the chain.
        //
        // Checked before the reschedule below, and journalled so a replay takes the same
        // branch as the original run.
        let engine = self.engine.clone();
        let known = {
            let slug = slug.clone();
            ctx.run(|| {
                let engine = engine.clone();
                let slug = slug.clone();
                async move { Ok(engine.store.get_persona(&slug).ok().flatten().is_some()) }
            })
            .await?
        };
        if !known {
            tracing::info!("persona {slug}: deleted, stopping this loop");
            return Ok(Json(
                serde_json::json!({ "persona": slug, "stopped": true }),
            ));
        }

        let now = ctx.run(|| async { Ok(now_millis()) }).await?;

        // A tick from a forked chain retires instead of rescheduling.
        //
        // `tick` arms its successor as its first act, so anything that invokes it out of band
        // forks the loop: two chains, then three, each spending background GitHub budget on the
        // same persona forever. `poke` exists precisely so nothing needs to, and it is still the
        // right entry point — but "nothing should call this" is not a property the code enforced,
        // and one afternoon of verifying by hand left `lukebond` ticking three times an interval.
        //
        // The self-heal: a tick that fires when the recorded next-tick is still meaningfully in
        // the future was scheduled by a chain that has since been superseded. It returns, and its
        // chain ends there. N chains collapse to one within an interval, with no bookkeeping of
        // invocation ids to cancel by. Same shape as `debounce::due`, and safe for the same
        // reason: both are exclusive handlers on one key and cannot interleave.
        //
        // The slop matters — a timer firing a few seconds early must not retire the only chain.
        const SLOP_MS: u64 = 30_000;
        if let Some(next) = ctx.get::<u64>(NEXT_TICK_AT).await? {
            if next > now + SLOP_MS {
                tracing::info!(
                    "persona {slug}: duplicate tick chain retiring (next due in {}s)",
                    (next - now) / 1000
                );
                return Ok(Json(serde_json::json!({
                    "persona": slug,
                    "retired_duplicate": true,
                })));
            }
        }

        // Reschedule before the work, for the reason every loop here does: each step below
        // ends in `?`, and a harvest that stops because one pass failed is a profile that
        // silently stops accumulating. The send is journalled, so a retry replays it rather
        // than arming a second timer.
        let backfilling = !ctx.get::<bool>(BACKFILL_COMPLETE).await?.unwrap_or(false);
        let interval = if backfilling {
            self.engine.backfill_interval
        } else {
            self.engine.harvest_interval
        };
        ctx.set(NEXT_TICK_AT, now + interval.as_millis() as u64);
        ctx.object_client::<PersonaClient>(slug)
            .tick()
            .send_after(interval);
        // Background: this is the loop's own tick, not somebody waiting.
        self.harvest_once(ctx, Trigger::Scheduled).await
    }

    /// One pass with **no** rescheduling — the "harvest now" button, and the create/link paths.
    ///
    /// Separate from `tick` for the reason [`super::repo_indexer::RepoIndexer::poke`] is:
    /// `tick` arms the next timer as its first act, so calling it out of band forks the loop
    /// and every later press adds another chain. Leaving the timer alone is also the right
    /// meaning — pressing the button is a reason to harvest now, not a reason to harvest more
    /// often from now on.
    ///
    /// **Interactive priority.** An operator action is never paced and never refused, per the
    /// standing rule in AGENTS.md. Getting this wrong is what made a freshly created persona
    /// sit at `0 ev` forever while the code index held the budget at its reserve.
    #[handler]
    async fn poke(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        self.harvest_once(ctx, Trigger::Operator).await
    }

    /// This person was just active somewhere.
    ///
    /// Called from ingest whenever a signal's actor resolves to a modelled persona (see
    /// [`crate::restate::pipeline`]). A profile that only refreshes on a twelve-hour timer is
    /// stale exactly when it matters — you are about to ask somebody about the thing they were
    /// talking about ten minutes ago.
    ///
    /// Debounced rather than immediate, on the same durable timer the subjects use: somebody
    /// posting nine messages in a minute is one refresh, not nine. Cheap even so — the Slack
    /// evidence this picks up is a SQL query over signals already stored.
    #[handler]
    async fn engaged(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        super::debounce::arm::<PersonaClient>(&ctx, self.engine.engagement_debounce).await?;
        Ok(())
    }

    /// The debounced target of [`Self::engaged`].
    ///
    /// Background priority: this is *their* activity prompting a refresh, not the operator
    /// asking, so it must not spend the reserve the watchers depend on. It still pays for
    /// itself, because the Slack half costs nothing.
    #[handler]
    async fn refresh(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        if !super::debounce::due(&ctx).await? {
            return Ok(Json(serde_json::json!({ "coalesced": true })));
        }
        self.harvest_once(ctx, Trigger::Scheduled).await
    }

    /// Harvest progress. **Shared**, so the personas page never queues behind a pass that is
    /// mid-model-call.
    #[handler]
    async fn status(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let slug = ctx.key().to_string();
        let (cursor, complete) = self
            .engine
            .store
            .persona_harvest_cursor(&slug)
            .map_err(|e| TerminalError::new(format!("{e:#}")))?;
        Ok(Json(serde_json::json!({
            "persona": slug,
            "ticks": ctx.get::<u64>(TICKS).await?.unwrap_or(0),
            "evidence": ctx.get::<u64>(EVIDENCE).await?.unwrap_or(0),
            "backfill_complete": complete,
            "walked_back_to": cursor,
        })))
    }
}

impl Persona {
    /// The work half of a tick: harvest, record what it achieved, and re-profile if the
    /// evidence actually moved.
    async fn harvest_once(
        &self,
        ctx: ObjectContext<'_>,
        trigger: Trigger,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let slug = ctx.key().to_string();
        let engine = self.engine.clone();
        let report = {
            let slug = slug.clone();
            ctx.run(|| {
                let engine = engine.clone();
                let slug = slug.clone();
                async move {
                    match engine.harvest(&slug, trigger).await {
                        Ok(r) => Ok(Json(r)),
                        Err(e) => {
                            // A search that rate-limited or an Ollama that is down is
                            // transient; the next tick retries. Swallowed rather than failing
                            // the invocation so the loop keeps its cadence — the same call
                            // the repo indexer makes, and for the same reason.
                            tracing::warn!("persona {slug}: harvest failed: {e:#}");
                            Ok(Json(crate::persona::harvest::Harvested {
                                persona: slug,
                                notes: vec![format!("{e:#}")],
                                ..Default::default()
                            }))
                        }
                    }
                }
            })
            .await?
            .into_inner()
        };

        let ticks: u64 = ctx.get(TICKS).await?.unwrap_or(0);
        ctx.set(TICKS, ticks + 1);
        ctx.set(EVIDENCE, report.total as u64);
        ctx.set(BACKFILL_COMPLETE, report.complete);

        // Re-profile only when the evidence set actually changed. The watermark is
        // `{count}@{newest ingested_at}` rather than the newest excerpt's id, precisely so
        // that the *backward* walk moves it — see `Store::persona_evidence_watermark`.
        //
        // Compared against what this object last submitted rather than against the persona
        // row's own `evidence_watermark`: the row is written by the profile pass, which runs
        // minutes later in a different invocation, so reading it here would re-submit on every
        // tick until that pass landed.
        let watermark = {
            let engine = self.engine.clone();
            let slug = slug.clone();
            ctx.run(|| {
                let engine = engine.clone();
                let slug = slug.clone();
                async move {
                    Ok(engine
                        .store
                        .persona_evidence_watermark(&slug)
                        .ok()
                        .flatten())
                }
            })
            .await?
        };
        let mut profiling = false;
        if let Some(watermark) = watermark.filter(|_| self.engine.auto_profile) {
            let last: Option<String> = ctx.get(PROFILED_AT_WATERMARK).await?;
            if last.as_deref() != Some(watermark.as_str()) {
                ctx.set(PROFILED_AT_WATERMARK, watermark.clone());
                ctx.workflow_client::<crate::restate::workflows::persona::PersonaProfileClient>(
                    format!("{slug}@{watermark}"),
                )
                .run()
                .send();
                profiling = true;
            }
        }

        Ok(Json(serde_json::json!({
            "persona": slug,
            "ticks": ticks + 1,
            "written": report.written,
            "evidence": report.total,
            "by_source": report.by_source,
            "walked_back_to": report.walked_back_to,
            "backfill_complete": report.complete,
            "profiling": profiling,
            "notes": report.notes,
            // Separate from `notes`: things that have not happened yet rather than things to
            // fix. See `Harvested::waiting`.
            "waiting": report.waiting,
        })))
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lets the shared debounce schedule `refresh` on this object.
///
/// The trait is named for the subjects' `analyze` pass; what it really means is "the debounced
/// pass on this object", and for a persona that is the harvest refresh.
impl super::debounce::AnalyzeClient for PersonaClient<'_> {
    fn send_analyze_after(ctx: &ObjectContext<'_>, delay: std::time::Duration) {
        ctx.object_client::<PersonaClient>(ctx.key().to_string())
            .refresh()
            .send_after(delay);
    }
}
