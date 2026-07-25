//! GitHub watcher — polls the notifications feed with conditional requests
//! (`If-Modified-Since` → `304`), maps each notification to a normalized
//! [`Signal`]. Token comes from the credential store.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, IF_MODIFIED_SINCE, LAST_MODIFIED, USER_AGENT,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;
use url::Url;

use super::{PollBatch, SourceSnapshot, Watcher};
use crate::config::{self, GithubSource};
use crate::signal::{Entity, Severity, Signal, SignalKind, Source, State};

const NOTIFICATIONS_URL: &str = "https://api.github.com/notifications";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 100;

/// How many characters of the triggering comment / body to keep in the signal.
const EXCERPT_CHARS: usize = 600;

pub struct GithubWatcher {
    client: reqwest::Client,
    token: String,
    interval: Duration,
    /// Allowed signal kinds from `cfg.watch`; empty means allow all.
    allowed: Vec<SignalKind>,
    /// Fetch subject + triggering comment to enrich each kept notification.
    enrich: bool,
    /// Subject-title prefixes that mark a notification as ignorable noise.
    ignore_prefixes: Vec<String>,
    /// `Last-Modified` from the previous response, replayed as `If-Modified-Since`.
    last_modified: Mutex<Option<String>>,
    /// Short-TTL cache of the branch→PR lookup used to attribute CI to its PR.
    /// A busy repo fires many CI notifications per branch between polls; without
    /// this each would repeat the same `pulls?head=` call. Keyed by
    /// `"{repo}\0{branch}"`; a cached `None` remembers "no PR for this branch".
    branch_pr_cache: Mutex<HashMap<String, (Option<GhPullRef>, Instant)>>,
    /// Extracted CI log context, keyed by the notification's state key
    /// (`{id}@{updated_at}`). A notification is re-resolved on every poll until
    /// it's read/updated; downloading a concluded run's logs each time would be
    /// wasteful, so cache the bounded excerpt and its browser URL.
    ci_log_cache: Mutex<HashMap<String, Option<CiLog>>>,
}

/// How long a branch→PR resolution stays fresh. Short enough that a newly-opened
/// PR attaches its CI within a poll or two; long enough to collapse a burst of
/// CI notifications for one branch into a single lookup.
const BRANCH_PR_TTL: Duration = Duration::from_secs(300);

impl GithubWatcher {
    pub fn new(cfg: &GithubSource, token: String) -> Result<Self> {
        let interval =
            config::parse_duration(&cfg.poll_interval).unwrap_or(Duration::from_secs(60));
        Ok(Self {
            client: reqwest::Client::builder()
                .build()
                .context("building HTTP client")?,
            token,
            interval,
            allowed: cfg.watch.iter().filter_map(|w| watch_kind(w)).collect(),
            enrich: cfg.enrich,
            ignore_prefixes: cfg.ignore_prefixes.clone(),
            last_modified: Mutex::new(None),
            branch_pr_cache: Mutex::new(HashMap::new()),
            ci_log_cache: Mutex::new(HashMap::new()),
        })
    }

    fn classify(reason: &str) -> (SignalKind, Severity) {
        match reason {
            "review_requested" => (SignalKind::ReviewRequested, Severity::Notice),
            "mention" | "team_mention" => (SignalKind::Mention, Severity::Notice),
            "assign" => (SignalKind::Assigned, Severity::Notice),
            "ci_activity" => (SignalKind::CiFailure, Severity::Warning),
            "comment" | "subscribed" | "state_change" => (SignalKind::ThreadReply, Severity::Info),
            _ => (SignalKind::Other, Severity::Info),
        }
    }

    /// GET a GitHub API resource and deserialize it, returning `None` (with a
    /// debug log) on any transport, status, or parse error. Enrichment is best
    /// effort — a failed fetch degrades to the un-enriched signal, never fails
    /// the poll.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        headers: &HeaderMap,
        url: &str,
    ) -> Option<T> {
        let resp = match self.client.get(url).headers(headers.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                debug!("github: enrichment fetch failed for {url}: {e}");
                return None;
            }
        };
        let resp = match resp.error_for_status() {
            Ok(r) => r,
            Err(e) => {
                debug!("github: enrichment status for {url}: {e}");
                return None;
            }
        };
        match resp.json::<T>().await {
            Ok(v) => Some(v),
            Err(e) => {
                debug!("github: enrichment parse for {url}: {e}");
                None
            }
        }
    }

    /// Pull the subject and the comment that triggered the notification, folding
    /// them into an [`Enrichment`]. Prefers the triggering comment for both the
    /// excerpt and the author; falls back to the subject's own body/author.
    async fn enrich_subject(&self, headers: &HeaderMap, subject: &GhSubject) -> Enrichment {
        let mut enrichment = Enrichment::default();

        if let Some(url) = subject.url.as_deref() {
            if let Some(detail) = self.get_json::<GhSubjectDetail>(headers, url).await {
                enrichment.state = state_label(&detail);
                enrichment.author = detail.user.map(|u| u.login);
                // GitHub's own canonical browser URL for the subject — the most
                // reliable deep link (covers issues, PRs, and discussions, which
                // path-reconstruction from the API URL can't always resolve).
                enrichment.html_url = detail
                    .html_url
                    .map(|u| u.trim().to_owned())
                    .filter(|u| !u.is_empty());
                enrichment.labels = detail.labels.into_iter().map(|l| l.name).collect();
                enrichment.head_sha = detail.head.and_then(|h| h.sha).filter(|s| !s.is_empty());
                enrichment.excerpt = detail
                    .body
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(excerpt);
            }
        }

        // The triggering comment is the most relevant context; when present it
        // overrides the subject body and re-attributes the signal to its author.
        if let Some(url) = subject.latest_comment_url.as_deref() {
            if Some(url) != subject.url.as_deref() {
                if let Some(comment) = self.get_json::<GhComment>(headers, url).await {
                    if let Some(login) = comment.user.map(|u| u.login) {
                        enrichment.author = Some(login);
                    }
                    if let Some(body) = comment
                        .body
                        .as_deref()
                        .map(str::trim)
                        .filter(|b| !b.is_empty())
                    {
                        enrichment.excerpt = Some(excerpt(body));
                    }
                }
            }
        }

        enrichment
    }

    /// Resolve a notification to its real subject: the strong correlation entity
    /// it belongs to, plus the enrichment that describes it. This is where CI
    /// runs get tied back to the PR that triggered them, and where a PR's text
    /// and head commit are pulled in — so the reasoner sees what a notification
    /// is actually about, not just its title.
    async fn resolve_notification(
        &self,
        headers: &HeaderMap,
        n: &GhNotification,
    ) -> (Entity, Enrichment) {
        let repo = &n.repository.full_name;

        // CI check suites have no followable `subject.url`. Parse the branch out
        // of the title and correlate the run to the PR that owns that branch, so
        // CI rolls into the PR's thread. Fall back to a per-branch key (all CI
        // for a branch groups together), then a per-notification key.
        if n.subject.r#type == "CheckSuite" {
            let mut e = Enrichment::default();
            let Some(branch) = ci_branch(&n.subject.title) else {
                return (Entity::new("ci", format!("{repo}:{}", n.id)), e);
            };
            // Follow a matching Actions run for both failed and successful
            // check suites. Failures surface error lines; successful runs keep a
            // bounded tail so the user can see what actually ran.
            if self.enrich {
                if let Some(workflow) = ci_workflow_name(&n.subject.title) {
                    e.ci_log = self
                        .ci_log_cached(
                            headers,
                            repo,
                            &branch,
                            &workflow,
                            ci_failed(&n.subject.title),
                            n,
                        )
                        .await;
                }
            }
            if self.enrich {
                if let Some(pr) = self.pr_for_branch(headers, repo, &branch).await {
                    e.author = pr.user.map(|u| u.login);
                    e.state = pr.state;
                    e.html_url = pr.html_url.filter(|u| !u.trim().is_empty());
                    e.labels = pr.labels.into_iter().map(|l| l.name).collect();
                    e.excerpt = pr
                        .body
                        .as_deref()
                        .map(str::trim)
                        .filter(|b| !b.is_empty())
                        .map(excerpt);
                    e.subject = Some(
                        match pr.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                            Some(t) => format!("CI on PR #{}: {t}", pr.number),
                            None => format!("CI on PR #{}", pr.number),
                        },
                    );
                    // Climb one more level: if the PR closes an issue, the issue
                    // is the controlling identity and the PR rides along as a
                    // secondary entity.
                    e.extra_entities
                        .push(Entity::new("pr", format!("{repo}#{}", pr.number)));
                    if let Some(issue) = linked_issue(pr.body.as_deref(), pr.title.as_deref()) {
                        return (Entity::new("issue", format!("{repo}#{issue}")), e);
                    }
                    return (Entity::new("pr", format!("{repo}#{}", pr.number)), e);
                }
                // No open PR for this branch. On a default branch that usually
                // means the PR already merged — find it through the commit the run
                // was built from, so post-merge CI still attaches to the work it
                // came from instead of piling up as anonymous cards.
                if is_default_branch(&branch) {
                    if let Some(pr) = self.merged_pr_for_run(headers, repo, n).await {
                        e.subject = Some(match pr.title.as_deref().map(str::trim) {
                            Some(t) if !t.is_empty() => {
                                format!("CI on main after PR #{}: {t}", pr.number)
                            }
                            _ => format!("CI on main after PR #{}", pr.number),
                        });
                        e.extra_entities
                            .push(Entity::new("pr", format!("{repo}#{}", pr.number)));
                        if let Some(issue) = linked_issue(pr.body.as_deref(), pr.title.as_deref()) {
                            return (Entity::new("issue", format!("{repo}#{issue}")), e);
                        }
                        return (Entity::new("pr", format!("{repo}#{}", pr.number)), e);
                    }
                }
            }
            e.subject = Some(format!("CI on branch {branch}"));
            return (Entity::new("branch", format!("{repo}@{branch}")), e);
        }

        // Everything else carries a subject URL we can follow directly.
        let mut e = if self.enrich {
            self.enrich_subject(headers, &n.subject).await
        } else {
            Enrichment::default()
        };
        let mut entity = subject_entity(&n.subject.r#type, n.subject.url.as_deref(), repo, &n.id);
        // A PR that closes an issue is *about* that issue. Promote the issue to the
        // controlling identity and keep the PR as a secondary entity, so the PR,
        // its CI, and the issue's own notifications all land on one thread.
        if entity.kind == "pr" {
            if let Some(issue) = linked_issue(e.excerpt.as_deref(), Some(&n.subject.title)) {
                e.extra_entities.push(entity.clone());
                entity = Entity::new("issue", format!("{repo}#{issue}"));
            }
        }
        // For a PR, pull the head commit's summary so the body carries the code
        // change, not just the discussion text.
        if self.enrich && entity.kind == "pr" {
            if let Some(sha) = e.head_sha.take() {
                let url = format!("https://api.github.com/repos/{repo}/commits/{sha}");
                if let Some(commit) = self.get_json::<GhCommit>(headers, &url).await {
                    e.commit = commit
                        .commit
                        .and_then(|c| c.message)
                        .and_then(|m| m.lines().next().map(|l| l.trim().to_owned()))
                        .filter(|s| !s.is_empty());
                }
            }
        }
        (entity, e)
    }

    /// Find the merged PR a default-branch CI run was built from, via the commit
    /// the workflow ran on (`/commits/{sha}/pulls`).
    ///
    /// This closes the last gap in the branch → PR → issue chain. CI on `main` has
    /// no open PR for its branch, so without this every post-merge run becomes an
    /// anonymous card — "workflow run skipped for main branch", over and over, with
    /// nothing linking it to the change that caused it. Going through the commit
    /// recovers the PR that merged, and from there the issue that PR closed.
    ///
    /// Best effort throughout: a run we can't resolve simply falls back to the
    /// branch identity.
    async fn merged_pr_for_run(
        &self,
        headers: &HeaderMap,
        repo: &str,
        n: &GhNotification,
    ) -> Option<GhPullRef> {
        let branch = ci_branch(&n.subject.title)?;
        let workflow = ci_workflow_name(&n.subject.title)?;
        // Locate the run to get its head SHA.
        let mut url =
            Url::parse(&format!("https://api.github.com/repos/{repo}/actions/runs")).ok()?;
        url.query_pairs_mut()
            .append_pair("branch", &branch)
            .append_pair("per_page", "20");
        let runs: GhRunsResponse = self.get_json(headers, url.as_str()).await?;
        let sha = runs
            .workflow_runs
            .iter()
            .find(|r| {
                r.name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&workflow))
            })
            .or_else(|| runs.workflow_runs.first())
            .and_then(|r| r.head_sha.clone())
            .filter(|s| !s.is_empty())?;

        let pulls: Vec<GhPullRef> = self
            .get_json(
                headers,
                &format!("https://api.github.com/repos/{repo}/commits/{sha}/pulls"),
            )
            .await?;
        // A commit can appear in several PRs (backports, chained branches); the
        // merged one is the change that actually landed.
        pulls
            .iter()
            .find(|p| p.merged_at.is_some())
            .or_else(|| pulls.first())
            .cloned()
    }

    /// Find the PR whose head branch is `branch` in `repo` (`owner/name`), so a
    /// CI run can be attributed to the PR that triggered it. Best effort — a
    /// run on a branch with no PR (e.g. `main`) returns `None`.
    async fn pr_for_branch(
        &self,
        headers: &HeaderMap,
        repo: &str,
        branch: &str,
    ) -> Option<GhPullRef> {
        let key = format!("{repo}\0{branch}");
        // Serve a fresh cache hit (including a cached "no PR") without an API call.
        if let Some((pr, at)) = self
            .branch_pr_cache
            .lock()
            .expect("mutex poisoned")
            .get(&key)
        {
            if at.elapsed() < BRANCH_PR_TTL {
                return pr.clone();
            }
        }

        let (owner, _) = repo.split_once('/')?;
        // `head` must be `owner:branch`; `state=all` so a just-merged/closed PR
        // still correlates the CI that was running against it.
        let url = format!(
            "https://api.github.com/repos/{repo}/pulls\
             ?head={owner}:{branch}&state=all&sort=updated&direction=desc&per_page=1"
        );
        // Inspect the status directly (rather than via `get_json`) so we can tell
        // a persistent answer from a transient one. A 200 (including an empty
        // list → "no PR for this branch") and a 404 (no match, or the token lacks
        // pull-request read on this private repo — GitHub masks that as 404) are
        // both cached as negatives/positives, so a burst of CI on one branch is a
        // single lookup and we don't re-hammer a 404 every poll. A 5xx/transport
        // error is left uncached to retry next poll.
        let pr = match self.client.get(&url).headers(headers.clone()).send().await {
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                debug!(
                    "github: no PR for {repo}@{branch} (404 — no match, or token \
                     missing pull_requests:read on a private repo)"
                );
                None
            }
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => resp
                    .json::<Vec<GhPullRef>>()
                    .await
                    .ok()
                    .and_then(|prs| prs.into_iter().next()),
                Err(e) => {
                    debug!("github: pulls status for {repo}@{branch}: {e}");
                    return None;
                }
            },
            Err(e) => {
                debug!("github: pulls fetch for {repo}@{branch}: {e}");
                return None;
            }
        };
        self.branch_pr_cache
            .lock()
            .expect("mutex poisoned")
            .insert(key, (pr.clone(), Instant::now()));
        pr
    }

    /// Cached wrapper over [`fetch_ci_log`], keyed by the notification's state
    /// key so a workflow run's logs are downloaded once, not every poll.
    async fn ci_log_cached(
        &self,
        headers: &HeaderMap,
        repo: &str,
        branch: &str,
        workflow: &str,
        failed: bool,
        n: &GhNotification,
    ) -> Option<CiLog> {
        let key = format!("{}@{}", n.id, n.updated_at);
        if let Some(cached) = self.ci_log_cache.lock().expect("mutex poisoned").get(&key) {
            return cached.clone();
        }
        let log = self
            .fetch_ci_log(headers, repo, branch, workflow, failed)
            .await;
        self.ci_log_cache
            .lock()
            .expect("mutex poisoned")
            .insert(key, log.clone());
        log
    }

    /// Find a matching completed workflow run, download a small number of job
    /// logs, and extract either the failure lines or a concise successful-run
    /// tail. Best effort — needs Actions read on the repo.
    async fn fetch_ci_log(
        &self,
        headers: &HeaderMap,
        repo: &str,
        branch: &str,
        workflow: &str,
        failed: bool,
    ) -> Option<CiLog> {
        let mut url =
            Url::parse(&format!("https://api.github.com/repos/{repo}/actions/runs")).ok()?;
        url.query_pairs_mut()
            .append_pair("branch", branch)
            .append_pair("per_page", "20");
        let runs: GhRunsResponse = self.get_json(headers, url.as_str()).await?;
        // Select the newest matching workflow run with the outcome reported by
        // the notification.
        let run = runs.workflow_runs.into_iter().find(|r| {
            let is_failed = matches!(
                r.conclusion.as_deref(),
                Some("failure") | Some("startup_failure") | Some("timed_out")
            );
            let name_matches = r
                .name
                .as_deref()
                .map(|n| n.eq_ignore_ascii_case(workflow) || n.contains(workflow))
                .unwrap_or(false);
            name_matches && (is_failed == failed)
        })?;

        let jobs_url = format!(
            "https://api.github.com/repos/{repo}/actions/runs/{}/jobs",
            run.id
        );
        let jobs: GhJobsResponse = self.get_json(headers, &jobs_url).await?;
        let mut collected = String::new();
        let wanted_conclusion = if failed { "failure" } else { "success" };
        for job in jobs
            .jobs
            .into_iter()
            .filter(|j| j.conclusion.as_deref() == Some(wanted_conclusion))
            .take(CI_LOG_JOBS)
        {
            let logs_url = format!(
                "https://api.github.com/repos/{repo}/actions/jobs/{}/logs",
                job.id
            );
            let Some(text) = self.fetch_text(headers, &logs_url).await else {
                continue;
            };
            let excerpt = if failed {
                extract_ci_errors(&text).or_else(|| extract_ci_tail(&text))
            } else {
                extract_ci_tail(&text)
            };
            if let Some(excerpt) = excerpt {
                if !collected.is_empty() {
                    collected.push_str("\n\n");
                }
                collected.push_str(&format!(
                    "[{}]\n{excerpt}",
                    job.name.as_deref().unwrap_or("job")
                ));
            }
        }
        (!collected.trim().is_empty()).then_some(CiLog {
            text: collected,
            url: run.html_url,
            failed,
        })
    }

    /// GET a URL and return its body as text, following redirects (Actions job
    /// logs 302 to a signed storage URL). `None` on any transport/status error.
    async fn fetch_text(&self, headers: &HeaderMap, url: &str) -> Option<String> {
        let resp = self.client.get(url).headers(headers.clone()).send().await;
        match resp.and_then(|r| r.error_for_status()) {
            Ok(r) => r.text().await.ok(),
            Err(e) => {
                debug!("github: log fetch for {url}: {e}");
                None
            }
        }
    }
}

/// Extract the branch from a CI check-suite notification title, e.g.
/// "PR Checks (npm) workflow run failed for bh/1.7.2-standard branch" →
/// "bh/1.7.2-standard". Returns `None` if the title isn't in that shape.
fn ci_branch(title: &str) -> Option<String> {
    let stem = title.trim().strip_suffix(" branch")?;
    let (_, branch) = stem.rsplit_once(" for ")?;
    let branch = branch.trim();
    (!branch.is_empty()).then(|| branch.to_owned())
}

/// The workflow name from a CI title: the text before " workflow run", e.g.
/// "PR Checks (npm) workflow run, Attempt #2 failed for …" → "PR Checks (npm)".
fn ci_workflow_name(title: &str) -> Option<String> {
    let name = title.split(" workflow run").next()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Whether a CI title reports a failure (vs. skipped / succeeded).
fn ci_failed(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    t.contains("failed") || t.contains("failure")
}

/// Most Actions log lines have a max we keep, and a char budget — failures
/// surface near the end, so we keep the tail.
const CI_LOG_LINES: usize = 25;
const CI_LOG_CHARS: usize = 2500;
const CI_LOG_JOBS: usize = 3;

/// Strip the leading ISO-8601 timestamp GitHub prefixes each raw log line with
/// ("2026-07-24T19:36:12.3456789Z <content>").
fn strip_ts(line: &str) -> &str {
    match line.split_once(' ') {
        Some((first, rest)) if first.len() >= 20 && first.contains('T') && first.ends_with('Z') => {
            rest
        }
        _ => line,
    }
}

/// GitHub Actions preserves terminal colour/control sequences in downloaded job
/// logs. They are useful in a terminal, but would be displayed literally in the
/// web UI, so remove CSI escape sequences before storing the compact excerpt.
fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\x1b' {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() {
                    let byte = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            continue;
        }
        let ch = text[i..].chars().next().expect("valid UTF-8");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Pull the error-bearing lines out of a raw Actions job log: keep lines that
/// look like errors and return the tail, bounded by line and char count.
fn extract_ci_errors(log: &str) -> Option<String> {
    let mut hits: Vec<String> = Vec::new();
    for line in log.lines() {
        let content = strip_ansi(strip_ts(line));
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        let low = content.to_ascii_lowercase();
        let is_err = low.contains("error ts")
            || low.contains("error:")
            || low.contains(": error")
            || low.contains("cannot find")
            || low.contains("exit code")
            || low.contains("npm err")
            || low.starts_with("error");
        if is_err {
            hits.push(content.to_owned());
        }
    }
    if hits.is_empty() {
        return None;
    }
    // Keep the tail (the failure summary), newest-relevant last, size-bounded.
    let mut out: Vec<String> = Vec::new();
    let mut chars = 0usize;
    for h in hits.iter().rev().take(CI_LOG_LINES) {
        chars += h.len() + 1;
        out.push(h.clone());
        if chars >= CI_LOG_CHARS {
            break;
        }
    }
    out.reverse();
    out.dedup();
    Some(out.join("\n"))
}

/// Keep a compact tail from a successful job log. This makes passing CI useful
/// context too (what completed, its final artifact/test lines) without storing
/// an unbounded raw log in SQLite.
fn extract_ci_tail(log: &str) -> Option<String> {
    let lines: Vec<String> = log
        .lines()
        .map(strip_ts)
        .map(strip_ansi)
        .map(|line| line.trim().to_owned())
        .filter(|line| {
            !line.is_empty() && !line.starts_with("##[group]") && !line.starts_with("##[endgroup]")
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut chars = 0usize;
    for line in lines.iter().rev().take(CI_LOG_LINES) {
        chars += line.len() + 1;
        out.push(line.clone());
        if chars >= CI_LOG_CHARS {
            break;
        }
    }
    out.reverse();
    Some(out.join("\n"))
}

/// Compose the signal body from the repo, notification reason, and any
/// enrichment — a compact header line followed by the excerpt.
fn build_body(repo: &str, reason: &str, enrichment: &Enrichment) -> String {
    let mut header = format!("{repo} · {reason}");
    if let Some(state) = &enrichment.state {
        header.push_str(" · ");
        header.push_str(state);
    }
    if let Some(author) = &enrichment.author {
        header.push_str(" · @");
        header.push_str(author);
    }
    if !enrichment.labels.is_empty() {
        header.push_str("\nlabels: ");
        header.push_str(&enrichment.labels.join(", "));
    }
    // The resolved subject (e.g. the PR a CI run belongs to) and the head-commit
    // summary anchor the body to the real change, not just the notification title.
    if let Some(subject) = &enrichment.subject {
        header.push_str("\n↳ ");
        header.push_str(subject);
    }
    if let Some(commit) = &enrichment.commit {
        header.push_str("\ncommit: ");
        header.push_str(commit);
    }
    // Put the bounded workflow excerpt ahead of the PR body so summaries use the
    // actual build/test outcome rather than just the notification title.
    if let Some(ci_log) = &enrichment.ci_log {
        header.push_str(if ci_log.failed {
            "\n\nCI failure log:\n"
        } else {
            "\n\nCI/CD log tail:\n"
        });
        header.push_str(&ci_log.text);
    }
    match &enrichment.excerpt {
        Some(excerpt) => format!("{header}\n\n{excerpt}"),
        None => header,
    }
}

/// GitHub's notifications API returns an API endpoint in `subject.url`, not a
/// browser URL. Convert the subject types we can deep-link to and otherwise
/// fall back to the repository page.
fn subject_html_url(
    api_url: Option<&str>,
    subject_type: &str,
    repository_html_url: Option<&str>,
) -> Option<String> {
    let api_url = api_url?;
    let parsed = Url::parse(api_url).ok()?;

    if parsed.host_str() == Some("github.com") {
        return Some(api_url.to_owned());
    }
    if parsed.host_str() != Some("api.github.com") {
        return repository_html_url.map(str::to_owned);
    }

    let identifier = parsed.path_segments()?.next_back()?;
    let route = match subject_type {
        "Issue" => "issues",
        "PullRequest" => "pull",
        "Discussion" => "discussions",
        "Commit" => "commit",
        _ => return repository_html_url.map(str::to_owned),
    };
    let repository_html_url = repository_html_url?.trim_end_matches('/');
    Some(format!("{repository_html_url}/{route}/{identifier}"))
}

/// The strong correlation identity for a notification: the specific PR / issue /
/// discussion / commit it concerns, so every notification about the same PR
/// (review request, comments, mentions) rolls into one thread — deduped on the
/// PR number, not the repo. Subjects without a number (CI check suites) fall
/// back to the notification's own thread id, so successive updates of one run
/// roll together while distinct runs stay separate. Deliberately *not* the bare
/// repo: that over-merges every unrelated notification in a busy repo into a
/// single thread (see `correlation::engine::entity_keys`).
fn subject_entity(
    subject_type: &str,
    subject_url: Option<&str>,
    repo: &str,
    notification_id: &str,
) -> Entity {
    let last_segment = subject_url
        .and_then(|u| Url::parse(u).ok())
        .and_then(|u| u.path_segments()?.next_back().map(str::to_owned))
        .filter(|s| !s.is_empty());
    match (subject_type, last_segment) {
        ("PullRequest", Some(n)) => Entity::new("pr", format!("{repo}#{n}")),
        ("Issue", Some(n)) => Entity::new("issue", format!("{repo}#{n}")),
        ("Discussion", Some(n)) => Entity::new("discussion", format!("{repo}#{n}")),
        ("Commit", Some(sha)) => {
            let short = sha.get(..12).unwrap_or(&sha);
            Entity::new("commit", format!("{repo}@{short}"))
        }
        // CI check suites (and any subject with no number): key on the stable
        // notification thread id so one run's updates roll together.
        _ => Entity::new("ci", format!("{repo}:{notification_id}")),
    }
}

/// Default branch names, matching [`crate::correlation::engine`]'s view: CI on one
/// of these has no feature branch to identify it, so the run is attributed through
/// the commit it built instead.
fn is_default_branch(branch: &str) -> bool {
    matches!(
        branch.to_ascii_lowercase().as_str(),
        "main" | "master" | "trunk" | "develop" | "development"
    )
}

/// The issue a pull request closes, from GitHub's own closing keywords.
///
/// This is the branch → PR → **issue** step. Without it a PR, its CI runs, and the
/// issue they are all about sit on three separate threads, and the issue — the one
/// durable statement of what the work is — ends up as the card with none of the
/// activity attached.
///
/// Only GitHub's closing keywords count. A bare `#412` in a PR body is usually a
/// cross-reference ("similar to #412"), and treating that as identity would merge
/// unrelated work.
fn linked_issue(body: Option<&str>, title: Option<&str>) -> Option<u64> {
    const KEYWORDS: &[&str] = &[
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let haystack = format!("{} {}", title.unwrap_or(""), body.unwrap_or("")).to_ascii_lowercase();
    // Scan word-wise so "fixes #412" matches but "prefixes #412" does not.
    let words: Vec<&str> = haystack.split_whitespace().collect();
    for pair in words.windows(2) {
        let keyword = pair[0].trim_matches(|c: char| !c.is_alphanumeric());
        if !KEYWORDS.contains(&keyword) {
            continue;
        }
        let number: String = pair[1]
            .trim_start_matches(|c: char| c != '#')
            .trim_start_matches('#')
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !number.is_empty() {
            if let Ok(n) = number.parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// A subject title is ignorable when it begins with any configured prefix
/// (leading/trailing whitespace trimmed on both sides before comparing).
fn ignored_by_prefix(title: &str, prefixes: &[String]) -> bool {
    let title = title.trim_start();
    prefixes
        .iter()
        .any(|p| !p.trim().is_empty() && title.starts_with(p.trim()))
}

/// Truncate to `EXCERPT_CHARS` on a char boundary, appending an ellipsis when cut.
fn excerpt(text: &str) -> String {
    let text = text.trim();
    let out: String = text.chars().take(EXCERPT_CHARS).collect();
    if text.chars().count() > EXCERPT_CHARS {
        format!("{out}…")
    } else {
        out
    }
}

/// Human label for a subject's lifecycle state (`merged` and `draft` beat the
/// bare `open`/`closed` that both issues and PRs report).
fn state_label(d: &GhSubjectDetail) -> Option<String> {
    if d.merged == Some(true) {
        return Some("merged".into());
    }
    let state = d.state.as_deref()?;
    if d.draft == Some(true) && state == "open" {
        return Some("draft".into());
    }
    Some(state.to_owned())
}

/// Map a `cfg.watch` token to the signal kind it selects.
fn watch_kind(token: &str) -> Option<SignalKind> {
    match token {
        "review_requested" => Some(SignalKind::ReviewRequested),
        "mention" => Some(SignalKind::Mention),
        "ci_failure" => Some(SignalKind::CiFailure),
        "assigned" => Some(SignalKind::Assigned),
        _ => None,
    }
}

#[derive(Deserialize)]
struct GhNotification {
    id: String,
    reason: String,
    updated_at: String,
    subject: GhSubject,
    repository: GhRepo,
}

#[derive(Deserialize)]
struct GhSubject {
    title: String,
    #[serde(default)]
    url: Option<String>,
    /// API URL of the comment that triggered this notification (may equal `url`
    /// when the subject itself is the trigger, or be null for e.g. CI activity).
    #[serde(default)]
    latest_comment_url: Option<String>,
    #[serde(rename = "type", default)]
    r#type: String,
}

#[derive(Deserialize)]
struct GhRepo {
    full_name: String,
    #[serde(default)]
    html_url: Option<String>,
}

/// The issue / pull request / discussion behind a notification's `subject.url`.
#[derive(Deserialize, Default)]
struct GhSubjectDetail {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    /// PR head ref — present only for pull-request subjects.
    #[serde(default)]
    head: Option<GhRef>,
}

/// A single comment behind `subject.latest_comment_url`.
#[derive(Deserialize)]
struct GhComment {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
}

#[derive(Deserialize, Clone)]
struct GhUser {
    login: String,
}

#[derive(Deserialize, Clone)]
struct GhLabel {
    name: String,
}

/// Content pulled from the subject and its triggering comment, folded into the
/// signal so correlation and notifications see real context, not just a title.
#[derive(Default)]
struct Enrichment {
    /// Who to attribute the signal to — comment author, else subject author.
    author: Option<String>,
    /// Lifecycle label: `open` / `closed` / `merged` / `draft`.
    state: Option<String>,
    /// GitHub's canonical browser URL for the subject, when the detail fetch
    /// returned one — preferred over reconstructing the link from the API URL.
    html_url: Option<String>,
    /// The triggering comment's text, falling back to the subject body.
    excerpt: Option<String>,
    labels: Vec<String>,
    /// A one-line description of the resolved subject this notification is really
    /// about (e.g. "CI on PR #1234: Set default to 1.7.2") — added to the body so
    /// a bare "workflow run failed" carries the PR it belongs to.
    subject: Option<String>,
    /// First line of the PR's head-commit message.
    commit: Option<String>,
    /// Head-commit SHA of a PR subject, consumed to fetch [`commit`]; never
    /// rendered.
    head_sha: Option<String>,
    /// A bounded excerpt from the matching workflow run's jobs. Failures keep
    /// error lines; successful runs keep the final useful output.
    ci_log: Option<CiLog>,
    /// Identities resolved *below* the controlling one — the PR a CI run came
    /// from, when the issue that PR closes is what actually owns the thread. They
    /// still correlate (a later notification about the PR itself finds this
    /// signal) without displacing the stronger identity.
    extra_entities: Vec<Entity>,
}

#[derive(Clone)]
struct CiLog {
    text: String,
    url: Option<String>,
    failed: bool,
}

/// A page of Actions workflow runs for a branch.
#[derive(Deserialize)]
struct GhRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<GhRun>,
}

#[derive(Deserialize)]
struct GhRun {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    /// The commit the run was built from — the link to the PR that merged it.
    #[serde(default)]
    head_sha: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

/// The jobs of a workflow run.
#[derive(Deserialize)]
struct GhJobsResponse {
    #[serde(default)]
    jobs: Vec<GhJob>,
}

#[derive(Deserialize)]
struct GhJob {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

/// A PR returned by the `pulls?head=…` lookup used to attribute CI to its PR.
#[derive(Deserialize, Clone)]
struct GhPullRef {
    number: u64,
    #[serde(default)]
    title: Option<String>,
    /// Set when the PR actually landed, which distinguishes the merged PR from
    /// other PRs a commit may appear in.
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    labels: Vec<GhLabel>,
}

/// The `head`/`base` ref of a pull request (we only need the commit SHA).
#[derive(Deserialize, Default)]
struct GhRef {
    #[serde(default)]
    sha: Option<String>,
}

/// A commit fetched to summarize a PR's latest change.
#[derive(Deserialize)]
struct GhCommit {
    #[serde(default)]
    commit: Option<GhCommitDetail>,
}

#[derive(Deserialize)]
struct GhCommitDetail {
    #[serde(default)]
    message: Option<String>,
}

#[async_trait]
impl Watcher for GithubWatcher {
    fn name(&self) -> &'static str {
        "github"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn poll(&self) -> Result<PollBatch> {
        let mut base_headers = HeaderMap::new();
        base_headers.insert(USER_AGENT, HeaderValue::from_static("mugglebot"));
        base_headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        base_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .context("building auth header")?,
        );

        let previous_last_modified = self.last_modified.lock().expect("mutex poisoned").clone();
        let mut new_last_modified = None;
        let mut notifications = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut headers = base_headers.clone();
            if page == 1 {
                if let Some(lm) = &previous_last_modified {
                    if let Ok(v) = HeaderValue::from_str(lm) {
                        headers.insert(IF_MODIFIED_SINCE, v);
                    }
                }
            }
            let resp = self
                .client
                .get(NOTIFICATIONS_URL)
                .headers(headers)
                .query(&[
                    ("all", "false"),
                    ("participating", "false"),
                    ("per_page", "100"),
                ])
                .query(&[("page", page)])
                .send()
                .await
                .context("requesting github notifications")?;

            if page == 1 && resp.status() == reqwest::StatusCode::NOT_MODIFIED {
                debug!("github: 304 not modified");
                return Ok(PollBatch::incremental(vec![]));
            }
            if page == 1 {
                // Commit this cursor only after every page parses successfully.
                new_last_modified = resp
                    .headers()
                    .get(LAST_MODIFIED)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
            }

            let resp = resp
                .error_for_status()
                .context("github notifications status")?;
            let page_notifications: Vec<GhNotification> =
                resp.json().await.context("parsing github notifications")?;
            let page_len = page_notifications.len();
            notifications.extend(page_notifications);
            if page_len < PAGE_SIZE {
                break;
            }
            if page == MAX_PAGES {
                anyhow::bail!("github notifications exceeded {MAX_PAGES} pages");
            }
        }

        if let Some(lm) = new_last_modified {
            *self.last_modified.lock().expect("mutex poisoned") = Some(lm);
        }

        let active_ids = notifications.iter().map(|n| n.id.clone()).collect();
        let now = Utc::now();
        let mut out = Vec::with_capacity(notifications.len());
        for n in notifications {
            let (kind, mut severity) = Self::classify(&n.reason);
            let ci_outcome = (n.subject.r#type == "CheckSuite").then(|| {
                if ci_failed(&n.subject.title) {
                    "failure"
                } else if n.subject.title.to_ascii_lowercase().contains("succeeded") {
                    "success"
                } else {
                    "unknown"
                }
            });
            // GitHub puts both passing and failing workflow runs under the same
            // `ci_activity` notification reason. Keep the legacy kind so the
            // existing `watch = ["ci_failure"]` setting continues to include
            // CI, but do not make a successful run look like a warning.
            if ci_outcome == Some("success") {
                severity = Severity::Info;
            }
            // PRs, issues, and discussions are first-class subjects — always
            // surfaced whatever the notification reason (author, comment,
            // subscribed, review, mention). Everything else (CI check suites,
            // commits, releases) is gated by the reason-kind `watch` filter.
            let always_show = matches!(
                n.subject.r#type.as_str(),
                "PullRequest" | "Issue" | "Discussion"
            );
            if !always_show && !self.allowed.is_empty() && !self.allowed.contains(&kind) {
                continue;
            }
            if ignored_by_prefix(&n.subject.title, &self.ignore_prefixes) {
                debug!(
                    "github: ignoring notification by prefix: {}",
                    n.subject.title
                );
                continue;
            }

            // Resolve each kept notification to its real subject — the PR a CI
            // run belongs to, a PR's text + commit, etc. Only kept notifications
            // pay for the extra API calls; best effort, a failed fetch degrades
            // to the bare notification. (Ignored/filtered noise costs nothing.)
            let (subject_entity, enrichment) = if self.enrich {
                self.resolve_notification(&base_headers, &n).await
            } else {
                (
                    subject_entity(
                        &n.subject.r#type,
                        n.subject.url.as_deref(),
                        &n.repository.full_name,
                        &n.id,
                    ),
                    Enrichment::default(),
                )
            };

            let occurred_at = chrono::DateTime::parse_from_rfc3339(&n.updated_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or(now);
            // Prefer GitHub's own canonical URL from enrichment; otherwise
            // reconstruct the deep link from the API URL; last, the repo page.
            let url = enrichment
                .html_url
                .clone()
                .or_else(|| {
                    subject_html_url(
                        n.subject.url.as_deref(),
                        &n.subject.r#type,
                        n.repository.html_url.as_deref(),
                    )
                })
                .or_else(|| n.repository.html_url.clone());
            let body = build_body(&n.repository.full_name, &n.reason, &enrichment);
            // Primary correlation key is the resolved subject (the PR / issue /
            // branch); repo and person ride along for display and grounding.
            let mut entities = vec![
                subject_entity,
                Entity::new("repo", n.repository.full_name.clone()),
            ];
            // Secondary identities resolved on the way up the chain
            // (branch → PR → issue), so every level still correlates. Deduplicated
            // because several paths contribute and a repeated entity is noise in
            // both the chips and the correlation key set.
            for extra in &enrichment.extra_entities {
                if !entities
                    .iter()
                    .any(|e| e.kind == extra.kind && e.value == extra.value)
                {
                    entities.push(extra.clone());
                }
            }
            if let Some(author) = &enrichment.author {
                entities.push(Entity::new("person", author.clone()));
            }
            let raw = serde_json::json!({
                "reason": n.reason,
                "repository": n.repository.full_name,
                "subject_type": n.subject.r#type,
                "thread_id": n.id,
                "state": enrichment.state,
                "labels": enrichment.labels,
                "ci_log_url": enrichment.ci_log.as_ref().and_then(|log| log.url.clone()),
                "ci_log_kind": enrichment.ci_log.as_ref().map(|log| if log.failed { "failure" } else { "tail" }),
                "ci_outcome": ci_outcome,
            });
            // GitHub reuses a notification's id for the life of a thread; fold
            // updated_at into the dedup key so each state change is a distinct,
            // re-notified signal (AGENTS.md: "once per thread state change").
            let external_id = format!("{}@{}", n.id, n.updated_at);
            out.push(Signal {
                id: Signal::make_id(Source::GitHub, &external_id),
                source: Source::GitHub,
                external_id,
                kind,
                title: n.subject.title,
                body: Some(body),
                url,
                actor: enrichment.author,
                entities,
                severity,
                state: State::Unseen,
                occurred_at,
                ingested_at: now,
                thread: None,
                raw,
                tags: Vec::new(),
            });
        }
        Ok(PollBatch {
            signals: out,
            snapshot: Some(SourceSnapshot {
                source: Source::GitHub,
                active_ids,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_body, excerpt, extract_ci_tail, ignored_by_prefix, is_default_branch, linked_issue,
        state_label, CiLog, Enrichment, GhSubjectDetail,
    };
    use super::{
        ci_branch, ci_failed, ci_workflow_name, extract_ci_errors, subject_entity, subject_html_url,
    };

    #[test]
    fn ci_workflow_name_and_failed() {
        assert_eq!(
            ci_workflow_name("PR Checks (npm) workflow run, Attempt #2 failed for bh/x branch")
                .as_deref(),
            Some("PR Checks (npm)")
        );
        assert!(ci_failed(
            "PR Checks (npm) workflow run, Attempt #2 failed for bh/x branch"
        ));
        assert!(!ci_failed(
            "Claude Code workflow run skipped for main branch"
        ));
    }

    #[test]
    fn extract_ci_errors_pulls_the_real_cause() {
        // Raw Actions log: timestamped lines, mostly noise, errors near the end.
        let log = "\
2026-07-24T19:36:00.1Z Installing dependencies
2026-07-24T19:36:10.2Z added 1200 packages
2026-07-24T19:36:20.3Z > tsc --noEmit
2026-07-24T19:36:21.4Z Error: src/restate-cloud/private/environments.ts(26,73): error TS2307: Cannot find module './restate-version' or its corresponding type declarations.
2026-07-24T19:36:21.5Z Error: src/restate-cloud/private/regions/index.ts(24,73): error TS2307: Cannot find module '../restate-version' or its corresponding type declarations.
2026-07-24T19:36:21.6Z Error: Process completed with exit code 2.";
        let out = extract_ci_errors(log).expect("errors extracted");
        assert!(
            out.contains("Cannot find module './restate-version'"),
            "{out}"
        );
        assert!(out.contains("error TS2307"), "{out}");
        assert!(out.contains("exit code 2"), "{out}");
        // Noise (install lines) is dropped.
        assert!(!out.contains("added 1200 packages"), "{out}");
        // Timestamp prefix is stripped.
        assert!(!out.contains("2026-07-24T19:36"), "{out}");
    }

    #[test]
    fn extract_ci_errors_none_when_clean() {
        let log = "2026-07-24T19:36:00.1Z all good\n2026-07-24T19:36:01.2Z done";
        assert!(extract_ci_errors(log).is_none());
    }

    #[test]
    fn extract_ci_tail_keeps_successful_job_context() {
        let log = "\
2026-07-24T19:36:00.1Z ##[group]Run npm test
2026-07-24T19:36:01.2Z \u{1b}[36;1m42 tests passed\u{1b}[0m
2026-07-24T19:36:02.3Z uploaded artifact dist/app.tgz
2026-07-24T19:36:03.4Z ##[endgroup]";
        let out = extract_ci_tail(log).expect("tail extracted");
        assert!(out.contains("42 tests passed"), "{out}");
        assert!(out.contains("uploaded artifact"), "{out}");
        assert!(!out.contains("##[group]"), "{out}");
        assert!(!out.contains('\u{1b}'), "{out:?}");
    }

    #[test]
    fn ci_branch_parses_title() {
        assert_eq!(
            ci_branch("PR Checks (npm) workflow run failed for bh/1.7.2-standard branch")
                .as_deref(),
            Some("bh/1.7.2-standard")
        );
        assert_eq!(
            ci_branch("Claude Code workflow run skipped for main branch").as_deref(),
            Some("main")
        );
        // Not the CI-title shape → no branch.
        assert_eq!(ci_branch("Some random subject"), None);
        assert_eq!(ci_branch("workflow run failed for  branch"), None);
    }

    #[test]
    fn subject_entity_dedups_prs_on_number() {
        // Every notification about the same PR shares one identity, regardless of
        // which notification thread id delivered it.
        let a = subject_entity(
            "PullRequest",
            Some("https://api.github.com/repos/octo/repo/pulls/17"),
            "octo/repo",
            "111",
        );
        let b = subject_entity(
            "PullRequest",
            Some("https://api.github.com/repos/octo/repo/pulls/17"),
            "octo/repo",
            "222",
        );
        assert_eq!(a.kind, "pr");
        assert_eq!(a.value, "octo/repo#17");
        assert_eq!(a.value, b.value, "same PR → same correlation identity");

        // A different PR in the same repo is a different identity.
        let c = subject_entity(
            "PullRequest",
            Some("https://api.github.com/repos/octo/repo/pulls/18"),
            "octo/repo",
            "333",
        );
        assert_ne!(a.value, c.value);
    }

    #[test]
    fn subject_entity_ci_keys_on_notification_thread() {
        // CI check suites carry no subject number, so they key on the stable
        // notification id — distinct suites never collapse together.
        let a = subject_entity("CheckSuite", None, "octo/repo", "111");
        let b = subject_entity("CheckSuite", None, "octo/repo", "222");
        assert_eq!(a.kind, "ci");
        assert_eq!(a.value, "octo/repo:111");
        assert_ne!(a.value, b.value, "distinct check suites stay separate");
    }

    #[test]
    fn closing_keywords_link_a_pr_to_its_issue() {
        // The branch → PR → issue step: only GitHub's closing keywords count.
        assert_eq!(linked_issue(Some("Fixes #412"), None), Some(412));
        assert_eq!(linked_issue(Some("this closes #7."), None), Some(7));
        assert_eq!(
            linked_issue(None, Some("Resolves #99: bound the pool")),
            Some(99)
        );
        assert_eq!(
            linked_issue(Some("blah\n\nCloses #1234\n\nmore"), None),
            Some(1234)
        );
    }

    /// A bare cross-reference is not a claim of ownership. Treating "similar to
    /// #412" as identity would merge unrelated work onto one thread.
    #[test]
    fn a_bare_reference_is_not_an_issue_link() {
        assert_eq!(linked_issue(Some("similar to #412"), None), None);
        assert_eq!(linked_issue(Some("see #412 for context"), None), None);
        assert_eq!(linked_issue(Some("no references at all"), None), None);
        // …and a word merely ending in a keyword must not match.
        assert_eq!(linked_issue(Some("prefixes #412 nicely"), None), None);
    }

    #[test]
    fn default_branches_are_recognized() {
        assert!(is_default_branch("main"));
        assert!(is_default_branch("MASTER"));
        assert!(is_default_branch("develop"));
        assert!(!is_default_branch("fix/pool-leak"));
        assert!(!is_default_branch("maintenance"));
    }

    #[test]
    fn ignores_configured_prefix() {
        let prefixes = vec!["CLA Assistant workflow run".to_string()];
        assert!(ignored_by_prefix(
            "CLA Assistant workflow run #42 completed",
            &prefixes
        ));
        // Leading whitespace on the title doesn't smuggle noise past the filter.
        assert!(ignored_by_prefix(
            "   CLA Assistant workflow run",
            &prefixes
        ));
        assert!(!ignored_by_prefix("Fix the login bug", &prefixes));
    }

    #[test]
    fn empty_prefix_never_matches() {
        assert!(!ignored_by_prefix("anything", &["".to_string()]));
        assert!(!ignored_by_prefix("anything", &["   ".to_string()]));
    }

    #[test]
    fn excerpt_truncates_on_char_boundary() {
        let long = "x".repeat(super::EXCERPT_CHARS + 50);
        let out = excerpt(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), super::EXCERPT_CHARS + 1);
        assert_eq!(excerpt("  short  "), "short");
    }

    #[test]
    fn state_label_prefers_merged_and_draft() {
        let merged = GhSubjectDetail {
            state: Some("closed".into()),
            merged: Some(true),
            ..Default::default()
        };
        assert_eq!(state_label(&merged).as_deref(), Some("merged"));

        let draft = GhSubjectDetail {
            state: Some("open".into()),
            draft: Some(true),
            ..Default::default()
        };
        assert_eq!(state_label(&draft).as_deref(), Some("draft"));

        let open = GhSubjectDetail {
            state: Some("open".into()),
            ..Default::default()
        };
        assert_eq!(state_label(&open).as_deref(), Some("open"));
    }

    #[test]
    fn build_body_composes_header_and_excerpt() {
        let enrichment = Enrichment {
            author: Some("octocat".into()),
            state: Some("open".into()),
            excerpt: Some("Please take a look".into()),
            labels: vec!["bug".into(), "p1".into()],
            ..Default::default()
        };
        let body = build_body("octo/repo", "review_requested", &enrichment);
        assert_eq!(
            body,
            "octo/repo · review_requested · open · @octocat\nlabels: bug, p1\n\nPlease take a look"
        );
    }

    #[test]
    fn build_body_labels_successful_ci_log_tail() {
        let enrichment = Enrichment {
            ci_log: Some(CiLog {
                text: "42 tests passed".into(),
                url: Some("https://github.com/octo/repo/actions/runs/1".into()),
                failed: false,
            }),
            ..Default::default()
        };
        let body = build_body("octo/repo", "ci_activity", &enrichment);
        assert!(body.contains("CI/CD log tail:\n42 tests passed"), "{body}");
    }

    #[test]
    fn build_body_without_enrichment_is_the_bare_header() {
        let body = build_body("octo/repo", "mention", &Enrichment::default());
        assert_eq!(body, "octo/repo · mention");
    }

    #[test]
    fn converts_issue_api_url_to_browser_url() {
        assert_eq!(
            subject_html_url(
                Some("https://api.github.com/repos/octo-org/octo-repo/issues/42"),
                "Issue",
                Some("https://github.com/octo-org/octo-repo"),
            ),
            Some("https://github.com/octo-org/octo-repo/issues/42".into())
        );
    }

    #[test]
    fn converts_pull_request_api_url_to_browser_url() {
        assert_eq!(
            subject_html_url(
                Some("https://api.github.com/repos/octo-org/octo-repo/pulls/17"),
                "PullRequest",
                Some("https://github.com/octo-org/octo-repo"),
            ),
            Some("https://github.com/octo-org/octo-repo/pull/17".into())
        );
    }

    #[test]
    fn preserves_existing_browser_url() {
        assert_eq!(
            subject_html_url(
                Some("https://github.com/octo-org/octo-repo/issues/42"),
                "Issue",
                Some("https://github.com/octo-org/octo-repo"),
            ),
            Some("https://github.com/octo-org/octo-repo/issues/42".into())
        );
    }
}
