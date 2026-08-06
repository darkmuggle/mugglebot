//! The remaining workflows: `BrowserRead`, `PrCritique`, `RepoIndex`, `Merge`,
//! `ContextIngest`.
//!
//! Each replaces a hand-rolled loop, and each replacement removes a specific piece of
//! machinery rather than just relocating it:
//!
//! | Workflow | Replaces | What goes away |
//! |---|---|---|
//! | `BrowserRead` | a claim-a-row worker over `browser_investigations` | the `status` column as a queue, and a job left `running` when the daemon died inside it |
//! | `PrCritique` | an inline call during triage | re-judging a PR whose diff hasn't moved |
//! | `RepoIndex` | a `tokio` interval loop | a refresh silently skipped because the process restarted mid-cycle |
//! | `ContextIngest` | two interval loops (URL refresh, directory sync) | re-summarizing a page whose ETag hasn't changed |
//! | `Merge` | the Analyst's inline auto-merge | a half-applied merge — signals moved, edges not rewritten |
//!
//! The keys are the interesting part in every case: `{investigation-id}`,
//! `{pr}@{sha}`, `{org}@{bucket}`, `{context-id}@{etag|mtime}`, `{a}+{b}`. A
//! redundant submission is a refused key rather than repeated work.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::{split_versioned, WorkflowOps};
use crate::restate::scopes;

// ---- BrowserRead -------------------------------------------------------------

/// Read the dashboard behind an alert link, in the operator's authenticated Chrome.
///
/// Keyed by investigation id. Runs in the `browser` scope with concurrency 1, because
/// there is one Chrome — which is what the claim-a-row worker loop was for. The
/// difference is that a crashed investigation releases its slot, where a claimed row
/// stayed claimed forever.
pub struct BrowserRead {
    ops: Arc<WorkflowOps>,
}

impl BrowserRead {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
    pub const SCOPE: &'static str = scopes::BROWSER;
}

#[restate_sdk::workflow]
impl BrowserRead {
    /// Capped retries. A permanently unreachable link must stop consuming the single
    /// browser slot rather than being retried forever — which is what Restate does by
    /// default, and what the old worker's `max_attempts` column existed to prevent.
    ///
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler(invocation_retry_policy(max_attempts = 4))]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("BrowserRead", &key, self.read(ctx)).await
    }
}

impl BrowserRead {
    async fn read(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let id = ctx.key().to_string();
        let ops = self.ops.clone();
        let value = ctx
            .run(|| {
                let ops = ops.clone();
                let id = id.clone();
                async move {
                    let out = ops.browser_read(&id).await.map_err(browser_classify)?;
                    Ok(Json(out))
                }
            })
            .await?
            .into_inner();
        Ok(Json(value))
    }
}

// ---- ThreadAnalyse -----------------------------------------------------------

/// Read one Slack thread with two models and record what they said.
///
/// Keyed `{analysis id}` — the store row is already unique per `(channel, thread_ts)`, so the
/// duplicate-suppression the key would give is where it belongs: in the table. Re-pasting a
/// link whose thread has grown updates that row's reply count and re-queues it, which is the
/// one case where re-running *should* cost something.
pub struct ThreadAnalyse {
    ops: Arc<WorkflowOps>,
}

impl ThreadAnalyse {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
    pub const SCOPE: &'static str = scopes::THREAD;
}

#[restate_sdk::workflow]
impl ThreadAnalyse {
    /// Two attempts, not four. Each retry is two more metered model calls on a question the
    /// operator asked once, and the failures worth retrying here (a Slack blip) recover on
    /// the second go or not at all.
    #[handler(invocation_retry_policy(max_attempts = 2))]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("ThreadAnalyse", &key, self.read(ctx)).await
    }
}

impl ThreadAnalyse {
    async fn read(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let id = ctx.key().to_string();
        let ops = self.ops.clone();
        let value = ctx
            .run(|| {
                let ops = ops.clone();
                let id = id.clone();
                async move {
                    let out = ops.thread_analyse(&id).await.map_err(thread_classify)?;
                    Ok(Json(out))
                }
            })
            .await?
            .into_inner();
        Ok(Json(value))
    }
}

/// A channel the token cannot see, a thread that does not exist, or a missing scope will not
/// start working on a retry — and the analyser has already written the reason onto the row,
/// so the operator can read it. A model timing out is worth one more go.
fn thread_classify(e: anyhow::Error) -> HandlerError {
    let msg = format!("{e:#}");
    let lower = msg.to_ascii_lowercase();
    let terminal = [
        "no such thread analysis",
        "not_in_channel",
        "channel_not_found",
        "thread_not_found",
        "message_not_found",
        "missing_scope",
        "no slack token",
        "no models configured",
        "does not look like a slack link",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if terminal {
        HandlerError::from(TerminalError::new(msg))
    } else {
        HandlerError::from(anyhow::anyhow!(msg))
    }
}

// ---- GrafanaRead -------------------------------------------------------------

/// Read the *numbers* behind an alert over Grafana's HTTP API.
///
/// Keyed by investigation id, like [`BrowserRead`], and sharing its queue — but in the
/// `grafana` scope rather than `browser`, because the concurrency-1 limit there exists for
/// the single Chrome and has nothing to say about an HTTP read.
///
/// This workflow does not fail when Grafana has nothing to show. That case escalates the
/// same investigation to the browser tier and returns successfully, so an alert whose panel
/// the token cannot query ends up read rather than retried four times and abandoned.
pub struct GrafanaRead {
    ops: Arc<WorkflowOps>,
}

impl GrafanaRead {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
    pub const SCOPE: &'static str = scopes::GRAFANA;
}

#[restate_sdk::workflow]
impl GrafanaRead {
    #[handler(invocation_retry_policy(max_attempts = 4))]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("GrafanaRead", &key, self.read(ctx)).await
    }
}

impl GrafanaRead {
    async fn read(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let id = ctx.key().to_string();
        let ops = self.ops.clone();
        let value = ctx
            .run(|| {
                let ops = ops.clone();
                let id = id.clone();
                async move {
                    let out = ops.grafana_read(&id).await.map_err(grafana_classify)?;
                    Ok(Json(out))
                }
            })
            .await?
            .into_inner();
        Ok(Json(value))
    }
}

/// A 5xx, a timeout, or a rate limit is **transient**: Grafana Cloud has bad minutes and
/// retrying is exactly right. A 401/403/404 is terminal — a revoked token, a datasource the
/// Viewer cannot see, or a rule that has since been deleted will not start working on the
/// second attempt, and the operator needs to see the reason rather than four copies of it.
///
/// Note what is *absent*: "no series in the window" is not classified here at all, because
/// it never reaches this function. That is an outcome, and it escalates to the browser.
fn grafana_classify(e: anyhow::Error) -> HandlerError {
    let msg = format!("{e:#}");
    let lower = msg.to_ascii_lowercase();
    let terminal = [
        "grafana 401",
        "grafana 403",
        "grafana 404",
        "no such investigation",
        "has no parsed grafana links",
        "is not configured",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if terminal {
        HandlerError::from(TerminalError::new(msg))
    } else {
        HandlerError::from(anyhow::anyhow!(msg))
    }
}

/// No Chrome on the port, no `npx`, or an unparseable response is **transient** — the
/// operator may simply not have started Chrome with `--remote-debugging-port` yet, and
/// retrying is exactly right. A 404 or an auth wall is terminal: that link will never
/// load, and a permanently unreachable page must stop consuming the single browser
/// slot rather than spinning forever.
fn browser_classify(e: anyhow::Error) -> HandlerError {
    let msg = format!("{e:#}");
    let lower = msg.to_ascii_lowercase();
    let terminal = [
        "404",
        "not found",
        "403",
        "unauthorized",
        "no such investigation",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if terminal {
        TerminalError::new(msg).into()
    } else {
        HandlerError::from(anyhow::anyhow!(msg))
    }
}

// ---- PrCritique --------------------------------------------------------------

/// Judge whether an open PR actually fixes an assigned issue.
///
/// Keyed `{owner}/{repo}!{n}@{sha}`: the same diff is the same judgment, so a re-run
/// on an unchanged PR is a refused key rather than another read of the diff and its
/// reviews.
pub struct PrCritique {
    ops: Arc<WorkflowOps>,
}

impl PrCritique {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
    /// This is triage's step 5 run on its own, so it runs on the same `[reasoner] triage`
    /// tier — off the machine by default, and therefore not in the one-GPU queue.
    pub const SCOPE: &'static str = scopes::CLOUD_LLM;
}

#[restate_sdk::workflow]
impl PrCritique {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("PrCritique", &key, self.critique(ctx)).await
    }
}

impl PrCritique {
    async fn critique(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let (pr, sha) = split_versioned(ctx.key());
        let (pr, sha) = (pr.to_string(), sha.to_string());
        let ops = self.ops.clone();
        let value = ctx
            .run(|| {
                let ops = ops.clone();
                let (pr, sha) = (pr.clone(), sha.clone());
                async move {
                    let out = ops
                        .pr_critique(&pr, &sha)
                        .await
                        .map_err(|e| HandlerError::from(TerminalError::new(format!("{e:#}"))))?;
                    Ok(Json(out))
                }
            })
            .await?
            .into_inner();
        Ok(Json(value))
    }
}

// ---- RepoIndex ---------------------------------------------------------------

/// Re-read the watched org's repositories and distil an index card per repo from its
/// **code** — the routing table for symptom → repo.
///
/// Keyed `{org}@{bucket}` where the bucket is a coarse time slice, so two refreshes in
/// the same window collapse. Runs in the `github` scope: indexing an org is otherwise
/// a self-inflicted rate limit.
pub struct RepoIndex {
    ops: Arc<WorkflowOps>,
}

impl RepoIndex {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
    /// Its own scope at concurrency 1, not `github` at 4: the crawl's hazard is two of
    /// *itself* running, which a shared API-burst scope does not express.
    pub const SCOPE: &'static str = scopes::REPO_INDEX;
}

#[restate_sdk::workflow]
impl RepoIndex {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<u64> {
        let key = ctx.key().to_string();
        super::tracked("RepoIndex", &key, self.sync(ctx)).await
    }
}

impl RepoIndex {
    async fn sync(&self, ctx: WorkflowContext<'_>) -> HandlerResult<u64> {
        let (org, bucket) = split_versioned(ctx.key());
        let (org, bucket) = (org.to_string(), bucket.to_string());
        let ops = self.ops.clone();
        let summarized = ctx
            .run(|| {
                let ops = ops.clone();
                async move {
                    ops.repos
                        .sync()
                        .await
                        .map(|n| n as u64)
                        .map_err(|e| HandlerError::from(anyhow::anyhow!("{e:#}")))
                }
            })
            .await?;
        tracing::info!("repo index for {org} ({bucket}): {summarized} card(s) rebuilt");
        Ok(summarized)
    }
}

// ---- PrDiff ------------------------------------------------------------------

/// Read a pull request's diff, summarize it, **review it**, and store both on the pull request.
///
/// Keyed `{owner}/{repo}!{n}@{watermark}`: the same activity is the same diff, so a
/// re-submission for a PR nothing has happened to is a refused key rather than another API
/// call and another model pass. That refusal is what makes it safe to submit this from every
/// analysis pass — see [`crate::prdiff`] for why the report is kept at all.
///
/// The result goes to the object rather than back to the caller. Nothing awaits this: the
/// pane reads the object's state, and a diff that is not there yet is fetched inline once.
///
/// Two steps, two state keys, and they fail independently — see the handler.
pub struct PrDiff {
    ops: Arc<WorkflowOps>,
}

impl PrDiff {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
    /// The local model does the summarizing, so this belongs in the same queue as every
    /// other on-device pass — one GPU, one lane.
    pub const SCOPE: &'static str = scopes::LOCAL_LLM;
}

#[restate_sdk::workflow]
impl PrDiff {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<bool> {
        let key = ctx.key().to_string();
        super::tracked("PrDiff", &key, self.read(ctx)).await
    }
}

impl PrDiff {
    async fn read(&self, ctx: WorkflowContext<'_>) -> HandlerResult<bool> {
        let (pr, version) = split_versioned(ctx.key());
        // An operator-chosen model rides in the key — see `prdiff::split_model` for why there
        // and not in a body. The watermark comes back without it, so re-dispatching a review
        // does not look like new activity on the pull request.
        let (watermark, model) = crate::prdiff::split_model(version);
        let model = model.map(|(p, m)| (p.to_string(), m.to_string()));
        let (pr, watermark) = (pr.to_string(), watermark.to_string());
        let Some((repo, number)) = crate::prdiff::parse_pr_key(&pr) else {
            return Err(TerminalError::new(format!("{pr} does not name a pull request")).into());
        };

        let ops = self.ops.clone();
        let stored = ctx
            .run(|| {
                let ops = ops.clone();
                let (repo, watermark) = (repo.clone(), watermark.clone());
                async move {
                    let report = ops.diff_reader().read(&repo, number).await;
                    // A PR whose diff could not be read is stored *with* its error rather
                    // than left absent: absent means "never looked", and the pane would
                    // fetch it again on every open, paying the same failing call each time.
                    Ok(Json(crate::prdiff::StoredDiff {
                        watermark,
                        fetched_at: chrono::Utc::now(),
                        report: crate::prdiff::trim_for_state(report),
                    }))
                }
            })
            .await?
            .into_inner();

        let ok = stored.report.error.is_none();
        let client = ctx
            .object_client::<crate::restate::objects::pull_request::PullRequestClient>(pr.clone());
        client.put_diff(Json(stored.clone())).send();
        tracing::info!("diff for {pr} (watermark {watermark}): stored, ok={ok}");

        // Step 2: review it. A separate journalled step, so a model that returns prose costs
        // the review and not the diff that was already fetched and stored — and a retry does
        // not re-read GitHub.
        //
        // Same workflow rather than a second one because the patches are already in hand here:
        // splitting it would mean either passing the diff through another invocation's journal
        // or reading it back out of state to review it.
        if ok {
            let ops = self.ops.clone();
            let (repo_for_review, watermark_for_review) = (repo.clone(), watermark.clone());
            let report = stored.report.clone();
            let reviewed = ctx
                .run(|| {
                    let ops = ops.clone();
                    let (repo, watermark) = (repo_for_review.clone(), watermark_for_review.clone());
                    let report = report.clone();
                    let model = model.clone();
                    async move {
                        let reader = ops.diff_reader();
                        let review = match &model {
                            // The operator named a model. Built here rather than held on the
                            // reader: this is one question about one review, not a change to
                            // how the daemon is configured.
                            Some((provider, model)) => {
                                let reasoner = ops.reasoner_for(provider, model);
                                reader
                                    .review_with(&repo, number, &report, &*reasoner, model)
                                    .await
                            }
                            None => reader.review(&repo, number, &report).await,
                        };
                        Ok(Json(review.map(|review| crate::prdiff::StoredReview {
                            watermark,
                            reviewed_at: chrono::Utc::now(),
                            review,
                        })))
                    }
                })
                .await?
                .into_inner();
            match reviewed {
                Some(review) => {
                    tracing::info!(
                        "review for {pr}: {:?}, {} inline comment(s)",
                        review.review.recommendation,
                        review.review.comments.len()
                    );
                    client.put_review(Json(review)).send();
                }
                // Nothing stored, so the pane keeps offering the review rather than showing an
                // empty one that looks like "no comments".
                None => tracing::debug!("no review for {pr}: the model did not return JSON"),
            }
        }
        Ok(ok)
    }
}

// ---- ContextIngest -----------------------------------------------------------

/// Fetch → normalize → summarize → embed → store, for one context source.
///
/// Keyed `{context-id}@{etag|mtime}`: an unchanged source is a refused key, which is
/// the whole point — the old interval loop re-checked every source on a timer and
/// relied on conditional requests to avoid re-summarizing. Now "nothing changed" costs
/// nothing at all.
pub struct ContextIngest {
    ops: Arc<WorkflowOps>,
}

impl ContextIngest {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }
    pub const SCOPE: &'static str = scopes::LOCAL_LLM;
}

#[restate_sdk::workflow]
impl ContextIngest {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<bool> {
        let key = ctx.key().to_string();
        super::tracked("ContextIngest", &key, self.ingest(ctx)).await
    }
}

impl ContextIngest {
    async fn ingest(&self, ctx: WorkflowContext<'_>) -> HandlerResult<bool> {
        let (id, version) = split_versioned(ctx.key());
        let (id, version) = (id.to_string(), version.to_string());
        let ops = self.ops.clone();
        let changed = ctx
            .run(|| {
                let ops = ops.clone();
                let id = id.clone();
                async move {
                    ops.context_refresh(&id)
                        .await
                        .map_err(|e| HandlerError::from(anyhow::anyhow!("{e:#}")))
                }
            })
            .await?;
        tracing::debug!("context {id} @{version}: changed = {changed}");
        Ok(changed)
    }
}

// ---- Merge -------------------------------------------------------------------

/// Collapse two subjects into one.
///
/// Keyed `{a}+{b}`. Multi-step and it must be exactly-once: re-pointing the signals,
/// rewriting the relation edges, carrying the artifacts, and forwarding future
/// activity are four writes that used to happen inline, so a failure between them left
/// a half-merged pair — signals moved to a subject whose edges still pointed at the
/// old one. Journalled, a retry finishes the merge instead of starting a second.
pub struct Merge {
    ops: Arc<WorkflowOps>,
}

impl Merge {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }

    /// `{keep}+{drop}`. Deliberately not sorted: which subject survives is a decision
    /// (the canonical one), not an implementation detail, so it has to be recoverable
    /// from the key.
    pub fn key(keep: &str, drop: &str) -> String {
        format!("{keep}+{drop}")
    }
}

#[restate_sdk::workflow]
impl Merge {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<String> {
        let key = ctx.key().to_string();
        super::tracked("Merge", &key, self.merge(ctx)).await
    }
}

impl Merge {
    async fn merge(&self, ctx: WorkflowContext<'_>) -> HandlerResult<String> {
        let raw = ctx.key().to_string();
        let Some((keep, drop)) = raw.split_once('+') else {
            return Err(TerminalError::new(format!(
                "'{raw}' is not a merge key (expected keep+drop)"
            ))
            .into());
        };
        let (keep, drop) = (keep.to_string(), drop.to_string());
        let ops = self.ops.clone();
        let canonical = ctx
            .run(|| {
                let ops = ops.clone();
                let (keep, drop) = (keep.clone(), drop.clone());
                async move {
                    // A merge that fails is a data problem, not a flake: retrying it
                    // forever would keep re-attempting the same impossible rewrite.
                    ops.merge(&keep, &drop)
                        .await
                        .map_err(|e| HandlerError::from(TerminalError::new(format!("{e:#}"))))
                }
            })
            .await?;
        Ok(canonical)
    }
}
