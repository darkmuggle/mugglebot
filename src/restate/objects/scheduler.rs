//! The `Scheduler` virtual object — recurring work, on durable timers.
//!
//! Keyed by task name (`repo-index`, `context-refresh`, `contexts-dir`,
//! `browser-queue`, `triage-queue`). Each tick submits workflows and reschedules
//! itself, replacing five `tokio::spawn` loops.
//!
//! What that changes, beyond tidiness: those loops slept in the process, so a restart
//! mid-cycle skipped a refresh silently, and a claim-a-row worker that died left rows
//! marked `running` that nothing would pick up again. Here the cadence survives the
//! restart, and the *work* is a workflow whose key makes a redundant submission free —
//! so a tick that fires twice costs two refused submissions rather than two crawls of
//! an org.
//!
//! The scheduler only ever *submits*. It never does the work itself, which is why a
//! tick is fast and can't be the thing that's stuck.

use std::sync::Arc;
use std::time::Duration;

use restate_sdk::prelude::*;

use crate::restate::pipeline::IngestOps;
use crate::restate::{scopes, workflows};

const NEXT_TICK_AT: &str = "next_tick_at";
const TICKS: &str = "ticks";

/// The recurring tasks, with their cadences.
pub const REPO_INDEX: &str = "repo-index";
pub const CONTEXT_REFRESH: &str = "context-refresh";
pub const CONTEXTS_DIR: &str = "contexts-dir";
pub const BROWSER_QUEUE: &str = "browser-queue";
pub const TRIAGE_QUEUE: &str = "triage-queue";
/// Arms a `RepoIndexer` per repo. A task rather than a boot-time loop because the org's
/// repo list grows: a repo added after boot has to get an indexer without a restart.
pub const CODE_INDEX: &str = "code-index";

pub struct Scheduler {
    ops: Arc<IngestOps>,
}

impl Scheduler {
    pub fn new(ops: Arc<IngestOps>) -> Self {
        Self { ops }
    }
}

/// How often each task ticks, and what it submits.
fn cadence(task: &str) -> Duration {
    match task {
        // Steady state only — see `repo_index_cadence`, which overrides this until the
        // org has been enumerated once. A *no-change* crawl is a shallow fetch per repo
        // and no model calls, so daily is right for that; the first crawl is a model call
        // per repo and is the one that gets interrupted.
        REPO_INDEX => Duration::from_secs(86_400),
        CONTEXT_REFRESH => Duration::from_secs(300),
        // Files change under you while you edit them; this is the one that wants to
        // be tight.
        CONTEXTS_DIR => Duration::from_secs(15),
        BROWSER_QUEUE | TRIAGE_QUEUE => Duration::from_secs(15),
        // Arming is idempotent and cheap; each indexer then runs on its own cadence.
        CODE_INDEX => Duration::from_secs(600),
        _ => Duration::from_secs(300),
    }
}

/// The cadence for the next tick, given what the store now holds.
///
/// Only `repo-index` differs from its static cadence, and it matters a lot: the org list is
/// the *input* to everything downstream — the code indexer can only work on repos that are in
/// `repo_index` — so an incomplete crawl starves component carding, commit summaries, the
/// dependency graph and scoring all at once.
///
/// The failure this fixes, observed live: a first crawl characterizes every repo with a local
/// model call, so it runs for a long time; a restart part-way through loses it; and at a daily
/// cadence the next attempt is **tomorrow**. The index sat at 2 repos out of 164 with nothing
/// scheduled to fix it. Catching up on a short cadence until the org has been enumerated once
/// turns a 24-hour hole into a few minutes.
fn cadence_now(ops: &IngestOps, task: &str) -> Duration {
    match task {
        REPO_INDEX if !ops.repo_index_looks_complete() => REPO_INDEX_CATCHUP,
        other => cadence(other),
    }
}

/// While the repo index is incomplete.
///
/// Short, because nothing downstream can start without the repo list — but comfortably longer
/// than a bounded crawl takes, since two overlapping crawls would clone the same repo into the
/// same directory. See `repos::CHARACTERIZE_BATCH`.
const REPO_INDEX_CATCHUP: Duration = Duration::from_secs(300);

#[restate_sdk::object]
impl Scheduler {
    /// Arm this task if it isn't already armed. Same durable-timer caution as
    /// [`super::watcher::Watcher::start`]: re-arming on every restart would multiply
    /// the tick rate by the number of restarts.
    #[handler]
    async fn start(&self, ctx: ObjectContext<'_>) -> HandlerResult<bool> {
        let task = ctx.key().to_string();
        // The *adaptive* cadence, not the static one. It sizes the staleness window as well
        // as the log line, and for `repo-index` the two differ by a factor of 720: judged
        // against the daily figure, a timer lost during catch-up would take two days to be
        // recognized as lost rather than four minutes.
        let ops = self.ops.clone();
        let task_for_cadence = task.clone();
        let interval = ctx
            .run(|| {
                let ops = ops.clone();
                let task = task_for_cadence.clone();
                async move { Ok(cadence_now(&ops, &task).as_millis() as u64) }
            })
            .await?;
        let interval = Duration::from_millis(interval);
        let now = ctx.run(|| async { Ok(now_millis()) }).await?;
        let next: Option<u64> = ctx.get(NEXT_TICK_AT).await?;
        let stale = match next {
            None => true,
            Some(at) => now > at + 2 * interval.as_millis() as u64,
        };
        if !stale {
            return Ok(false);
        }
        tracing::info!("scheduler '{task}': every {}s", interval.as_secs());
        ctx.set(NEXT_TICK_AT, now);
        ctx.object_client::<SchedulerClient>(task).tick().send();
        Ok(true)
    }

    #[handler]
    async fn tick(&self, ctx: ObjectContext<'_>) -> HandlerResult<u64> {
        let task = ctx.key().to_string();
        let ops = self.ops.clone();
        let task_for_run = task.clone();

        // Everything a tick does is "look at the store, submit some workflows". Kept
        // in one `ctx.run` because it is cheap and idempotent — the submissions
        // themselves are what carry the exactly-once property.
        let submitted = ctx
            .run(|| {
                let ops = ops.clone();
                let task = task_for_run.clone();
                async move { Ok(dispatch(&ops, &task).await) }
            })
            .await?;

        // Decided *after* the work, so a crawl that just finished enumerating the org
        // drops to the steady cadence on this same tick rather than one cycle later.
        let ops = self.ops.clone();
        let task_for_cadence = task.clone();
        let interval = ctx
            .run(|| {
                let ops = ops.clone();
                let task = task_for_cadence.clone();
                async move { Ok(cadence_now(&ops, &task).as_millis() as u64) }
            })
            .await?;
        let interval = Duration::from_millis(interval);

        let ticks: u64 = ctx.get(TICKS).await?.unwrap_or(0);
        ctx.set(TICKS, ticks + 1);
        let now = ctx.run(|| async { Ok(now_millis()) }).await?;
        ctx.set(NEXT_TICK_AT, now + interval.as_millis() as u64);
        ctx.object_client::<SchedulerClient>(task)
            .tick()
            .send_after(interval);
        Ok(submitted)
    }

    #[handler]
    async fn status(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        Ok(Json(serde_json::json!({
            "task": ctx.key(),
            "ticks": ctx.get::<u64>(TICKS).await?.unwrap_or(0),
            "next_tick_at": ctx.get::<u64>(NEXT_TICK_AT).await?,
        })))
    }
}

/// Submit whatever this task is responsible for. Returns how many workflows were
/// newly started (a refused key isn't counted — that's the free case).
async fn dispatch(ops: &IngestOps, task: &str) -> u64 {
    let mut started = 0u64;
    match task {
        REPO_INDEX => {
            // Bucketed so two ticks in the same window collapse into one crawl — the key
            // does the de-duplication, not a timestamp comparison. The bucket has to be
            // *shorter than the catch-up cadence*, or every catch-up tick would land in the
            // hour-long bucket of the crawl that failed and be refused as a free redo —
            // which is exactly the state that left the index at 2 repos of 164.
            let bucket = if ops.repo_index_looks_complete() {
                now_millis() / 3_600_000
            } else {
                now_millis() / REPO_INDEX_CATCHUP.as_millis() as u64
            };
            let key = format!("{}@{bucket}", ops.org());
            started += submit(ops, "RepoIndex", &key, workflows::rest::RepoIndex::SCOPE).await;
        }
        CONTEXT_REFRESH => {
            for (id, version) in ops.context_sources_due() {
                let key = format!("{id}@{version}");
                started += submit(
                    ops,
                    "ContextIngest",
                    &key,
                    workflows::rest::ContextIngest::SCOPE,
                )
                .await;
            }
        }
        CONTEXTS_DIR => {
            // The managed directory is a filesystem walk, not a workflow: it decides
            // *which* sources exist, and the per-source ingest is what the workflows
            // above do.
            if let Err(e) = ops.sync_contexts_dir().await {
                tracing::warn!("contexts dir sync failed: {e:#}");
            }
        }
        BROWSER_QUEUE => {
            for id in ops.pending_browser_investigations() {
                started += submit(ops, "BrowserRead", &id, scopes::BROWSER).await;
            }
        }
        CODE_INDEX => {
            for repo in ops.indexed_repos() {
                match ops.ingress.start_repo_indexer(&repo).await {
                    Ok(true) => {
                        tracing::info!("index {repo}: armed");
                        started += 1;
                    }
                    Ok(false) => {}
                    // A repo whose indexer can't start (no token, no git) is a warning
                    // once per tick, not a failure of the whole task.
                    Err(e) => tracing::debug!("index {repo}: not armed ({e:#})"),
                }
            }
        }
        TRIAGE_QUEUE => {
            for (issue_key, version) in ops.pending_triage() {
                let key = format!("{issue_key}@{version}");
                started += submit(ops, "IssueTriage", &key, workflows::issue_triage::SCOPE).await;
            }
        }
        other => tracing::warn!("scheduler: unknown task '{other}'"),
    }
    started
}

async fn submit(ops: &IngestOps, workflow: &str, key: &str, scope: &str) -> u64 {
    match ops
        .ingress
        .submit_workflow(workflow, key, Some(scope))
        .await
    {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            tracing::warn!("scheduler: submitting {workflow}/{key} failed: {e:#}");
            0
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
