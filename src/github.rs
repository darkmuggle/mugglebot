//! A small authenticated GitHub REST client for the **investigation** path —
//! separate from [`crate::watchers::github`], which polls the notifications feed.
//!
//! The watcher answers "what happened to me?"; this answers "what in the code
//! could explain this symptom?" — listing an org's repositories, reading their
//! READMEs (conditionally, on ETag), searching issues/PRs, walking a commit log,
//! and searching code. Everything here is read-only.
//!
//! Every call is best-effort in spirit but returns `Result`, so the caller
//! decides whether a miss degrades the investigation or fails it. Rate limits are
//! surfaced as errors rather than retried in place: the investigator runs off the
//! poll path and the caches (see [`crate::store`]) keep repeat work off the wire.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, IF_NONE_MATCH, USER_AGENT};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::Duration;
use tracing::debug;

use crate::store::CommitEntry;

const API: &str = "https://api.github.com";
/// GitHub caps `per_page` at 100 for list endpoints.
const PAGE_SIZE: usize = 100;
/// Stop paging an org listing here — an org with more repos than this has bigger
/// routing problems than the index can solve.
const MAX_REPO_PAGES: usize = 10;
/// Characters kept from a single comment. Long enough for a real explanation,
/// short enough that one essay can't crowd out the rest of the discussion.
const COMMENT_CHARS: usize = 1_200;

/// README bytes kept for summarization. A long monorepo README is mostly badges
/// and tables of contents past this point.
const README_CHARS: usize = 12_000;

pub struct GithubClient {
    client: reqwest::Client,
    token: String,
}

/// One repository as the org listing returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoMeta {
    pub full_name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    pub language: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub fork: bool,
    pub pushed_at: Option<String>,
}

/// An issue or pull request matched by a symptom search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IssueHit {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub state: String,
    /// `issue` or `pull_request` — a PR is an issue with a `pull_request` key.
    pub kind: String,
    pub url: String,
    pub body: Option<String>,
    pub labels: Vec<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub closed_at: Option<String>,
}

/// An open pull request — a candidate fix somebody may already have written.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PullRequest {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub state: String,
    pub draft: bool,
    pub body: Option<String>,
    pub labels: Vec<String>,
    pub head_ref: Option<String>,
    pub updated_at: Option<String>,
}

/// A comment on an issue or pull request.
///
/// This is where the real content of an issue usually lives. A title says what
/// broke; the discussion says what was tried, what was ruled out, what a maintainer
/// decided, and — on a PR — what a reviewer is blocking on. Reasoning from the body
/// alone reliably misses the answer someone already wrote down.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Comment {
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub body: String,
    /// `discussion` (issue/PR conversation), `review` (a review's summary), or
    /// `review_comment` (inline, on a specific line).
    pub kind: String,
    /// For an inline review comment, the file it's attached to.
    pub path: Option<String>,
    /// For a review: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`.
    pub state: Option<String>,
}

impl Comment {
    /// Is this a reviewer blocking the change? Those must never be dropped by
    /// selection — "this doesn't handle the retry case" is the single most useful
    /// sentence on a PR.
    pub fn is_blocking(&self) -> bool {
        self.state
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("CHANGES_REQUESTED"))
    }

    /// One line for a prompt, attributed and labeled.
    pub fn render(&self) -> String {
        let who = self.author.as_deref().unwrap_or("unknown");
        let when = self
            .created_at
            .as_deref()
            .map(|t| t.get(..10).unwrap_or(t).to_string())
            .unwrap_or_default();
        let mut head = format!("[{}] {who}", self.kind);
        if let Some(state) = &self.state {
            head.push_str(&format!(" ({state})"));
        }
        if let Some(path) = &self.path {
            head.push_str(&format!(" on {path}"));
        }
        if !when.is_empty() {
            head.push_str(&format!(" {when}"));
        }
        format!("{head}:\n{}\n", self.body.trim())
    }
}

/// One file a pull request touches, with its diff.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PullFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    /// The unified diff hunk, truncated. Absent for binary files.
    pub patch: Option<String>,
}

/// A code-search match: the fallback when no issue, PR, or commit explains the
/// symptom and the question becomes "which code does this?"
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeHit {
    pub repo: String,
    pub path: String,
    pub url: String,
    /// Matching line fragments, when the API returns text matches.
    pub fragments: Vec<String>,
}

impl GithubClient {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .context("building GitHub HTTP client")?,
            token,
        })
    }

    fn headers(&self, accept: &str) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(USER_AGENT, HeaderValue::from_static("mugglebot"));
        h.insert(ACCEPT, HeaderValue::from_str(accept)?);
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .context("GitHub token is not a valid header value")?,
        );
        Ok(h)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .client
            .get(url)
            .headers(self.headers("application/vnd.github+json")?)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            // The message body carries the useful part of a 403 (rate limit vs.
            // SAML vs. missing scope), so surface it instead of the bare code.
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url} → {status}: {}", first_line(&body));
        }
        resp.json::<T>()
            .await
            .with_context(|| format!("decoding {url}"))
    }

    /// Every non-fork repository in an org, newest-pushed first.
    pub async fn list_org_repos(&self, org: &str) -> Result<Vec<RepoMeta>> {
        let mut out = Vec::new();
        for page in 1..=MAX_REPO_PAGES {
            let url =
                format!("{API}/orgs/{org}/repos?per_page={PAGE_SIZE}&page={page}&sort=pushed");
            let batch: Vec<RepoMeta> = self.get_json(&url).await?;
            let n = batch.len();
            out.extend(batch.into_iter().filter(|r| !r.fork));
            if n < PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }

    /// A repository's README as raw text. Returns `Ok(None)` when the ETag still
    /// matches (`304`) or the repo has no README at all (`404`) — both mean
    /// "nothing new to summarize", which the caller distinguishes by the ETag it
    /// passed in.
    pub async fn readme(
        &self,
        full_name: &str,
        etag: Option<&str>,
    ) -> Result<Option<(String, Option<String>)>> {
        let url = format!("{API}/repos/{full_name}/readme");
        let mut headers = self.headers("application/vnd.github.raw")?;
        if let Some(tag) = etag {
            if let Ok(v) = HeaderValue::from_str(tag) {
                headers.insert(IF_NONE_MATCH, v);
            }
        }
        let resp = self
            .client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        match resp.status() {
            StatusCode::NOT_MODIFIED => Ok(None),
            StatusCode::NOT_FOUND => {
                debug!("github: {full_name} has no README");
                Ok(None)
            }
            s if s.is_success() => {
                let new_etag = resp
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let text = resp.text().await.unwrap_or_default();
                Ok(Some((truncate(&text, README_CHARS), new_etag)))
            }
            s => bail!("GET {url} → {s}"),
        }
    }

    /// Every open issue currently assigned to the authenticated user, across all
    /// repositories they can see.
    ///
    /// This is `/issues`, not `/notifications`: assignment is a standing state, so
    /// an issue assigned weeks ago with no recent activity produces no notification
    /// but still belongs on the board. Pull requests are filtered out — an assigned
    /// PR is review work, which the notification feed already surfaces.
    pub async fn assigned_issues(&self, limit: usize) -> Result<Vec<IssueHit>> {
        let mut out = Vec::new();
        for page in 1..=5 {
            let url =
                format!("{API}/issues?filter=assigned&state=open&per_page={PAGE_SIZE}&page={page}");
            let batch: Vec<GhIssue> = self.get_json(&url).await?;
            let n = batch.len();
            out.extend(
                batch
                    .into_iter()
                    .filter(|i| i.pull_request.is_none())
                    .map(IssueHit::from),
            );
            if n < PAGE_SIZE || out.len() >= limit {
                break;
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// The default branch and clone URL for a repository.
    pub async fn repo_checkout_info(&self, full_name: &str) -> Result<(String, u64)> {
        let detail: GhRepoDetail = self.get_json(&format!("{API}/repos/{full_name}")).await?;
        Ok((detail.default_branch, detail.size))
    }

    /// Search issues and pull requests. `query` is a GitHub search expression;
    /// repo scoping (`repo:owner/name`) is the caller's job so a single search
    /// can span the routed repos.
    pub async fn search_issues(&self, query: &str, limit: usize) -> Result<Vec<IssueHit>> {
        let url = format!(
            "{API}/search/issues?q={}&per_page={}&sort=updated&order=desc",
            urlencode(query),
            limit.min(PAGE_SIZE)
        );
        let resp: GhSearch<GhIssue> = self.get_json(&url).await?;
        Ok(resp.items.into_iter().map(IssueHit::from).collect())
    }

    /// Commits on the default branch between `since` and now, newest first.
    /// `files` is left empty here — populating it costs one API call per commit,
    /// so [`Self::commit_files`] fills it in only for ranked candidates.
    pub async fn commits(
        &self,
        full_name: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<CommitEntry>> {
        let mut out = Vec::new();
        let mut page = 1usize;
        while out.len() < limit {
            let url = format!(
                "{API}/repos/{full_name}/commits?since={}&per_page={PAGE_SIZE}&page={page}",
                since.to_rfc3339()
            );
            let batch: Vec<GhCommit> = self.get_json(&url).await?;
            let n = batch.len();
            for c in batch {
                out.push(c.into_entry(full_name));
            }
            if n < PAGE_SIZE {
                break;
            }
            page += 1;
        }
        out.truncate(limit);
        Ok(out)
    }

    /// The conversation on an issue or pull request, oldest first.
    pub async fn issue_comments(&self, full_name: &str, number: u64) -> Result<Vec<Comment>> {
        let url = format!("{API}/repos/{full_name}/issues/{number}/comments?per_page={PAGE_SIZE}");
        let raw: Vec<GhIssueComment> = self.get_json(&url).await?;
        Ok(raw
            .into_iter()
            .map(|c| Comment {
                author: c.user.map(|u| u.login),
                created_at: c.created_at,
                body: truncate(c.body.unwrap_or_default().trim(), COMMENT_CHARS),
                kind: "discussion".into(),
                path: None,
                state: None,
            })
            .filter(|c| !c.body.is_empty())
            .collect())
    }

    /// A pull request's reviews and inline review comments.
    ///
    /// Separate from [`Self::issue_comments`] because GitHub keeps them on different
    /// endpoints, and because they answer a different question: the conversation says
    /// what the change is *for*, the reviews say whether it's *right*. Both matter
    /// when judging whether a PR actually fixes something.
    pub async fn pull_reviews(&self, full_name: &str, number: u64) -> Result<Vec<Comment>> {
        let mut out = Vec::new();
        let reviews: Vec<GhReview> = self
            .get_json(&format!(
                "{API}/repos/{full_name}/pulls/{number}/reviews?per_page={PAGE_SIZE}"
            ))
            .await
            .unwrap_or_default();
        for r in reviews {
            let body = truncate(r.body.unwrap_or_default().trim(), COMMENT_CHARS);
            // An approval with no text is still meaningful — it says the change was
            // accepted — so keep it even when the body is empty.
            let state = r.state.filter(|s| !s.is_empty());
            if body.is_empty() && state.is_none() {
                continue;
            }
            out.push(Comment {
                author: r.user.map(|u| u.login),
                created_at: r.submitted_at,
                body,
                kind: "review".into(),
                path: None,
                state,
            });
        }
        let inline: Vec<GhReviewComment> = self
            .get_json(&format!(
                "{API}/repos/{full_name}/pulls/{number}/comments?per_page={PAGE_SIZE}"
            ))
            .await
            .unwrap_or_default();
        for c in inline {
            let body = truncate(c.body.unwrap_or_default().trim(), COMMENT_CHARS);
            if body.is_empty() {
                continue;
            }
            out.push(Comment {
                author: c.user.map(|u| u.login),
                created_at: c.created_at,
                body,
                kind: "review_comment".into(),
                path: c.path,
                state: None,
            });
        }
        Ok(out)
    }

    /// Open pull requests in a repository, most-recently-updated first.
    ///
    /// The point of looking is that somebody else may already be fixing the thing
    /// you're about to start on — so this deliberately does not filter by author.
    pub async fn open_pulls(&self, full_name: &str, limit: usize) -> Result<Vec<PullRequest>> {
        let url = format!(
            "{API}/repos/{full_name}/pulls?state=open&per_page={}&sort=updated&direction=desc",
            limit.min(PAGE_SIZE)
        );
        let pulls: Vec<GhPull> = self.get_json(&url).await?;
        Ok(pulls
            .into_iter()
            .map(|p| PullRequest::from_wire(p, full_name))
            .collect())
    }

    /// The files a pull request touches, with their diffs.
    ///
    /// The patch is what makes a critique possible: a PR title claims intent, the
    /// diff shows what it actually does. Truncated per file so one large change
    /// can't consume the whole context window.
    pub async fn pull_files(
        &self,
        full_name: &str,
        number: u64,
        max_files: usize,
        max_patch_chars: usize,
    ) -> Result<Vec<PullFile>> {
        let url = format!(
            "{API}/repos/{full_name}/pulls/{number}/files?per_page={}",
            max_files.min(PAGE_SIZE)
        );
        let files: Vec<GhPullFile> = self.get_json(&url).await?;
        Ok(files
            .into_iter()
            .take(max_files)
            .map(|f| PullFile {
                path: f.filename,
                additions: f.additions,
                deletions: f.deletions,
                patch: f.patch.map(|p| truncate(&p, max_patch_chars)),
            })
            .collect())
    }

    /// The paths one commit touched — the strongest signal that a commit relates
    /// to a symptom naming a component.
    pub async fn commit_files(&self, full_name: &str, sha: &str) -> Result<Vec<String>> {
        let url = format!("{API}/repos/{full_name}/commits/{sha}");
        let detail: GhCommitDetail = self.get_json(&url).await?;
        Ok(detail.files.into_iter().map(|f| f.filename).collect())
    }

    /// Search code across the given repos. Used only as the last fallback, when
    /// no issue, PR, or commit explains the symptom.
    pub async fn search_code(
        &self,
        terms: &str,
        repos: &[String],
        limit: usize,
    ) -> Result<Vec<CodeHit>> {
        if repos.is_empty() || terms.trim().is_empty() {
            return Ok(Vec::new());
        }
        let scope = repos
            .iter()
            .map(|r| format!("repo:{r}"))
            .collect::<Vec<_>>()
            .join(" ");
        let url = format!(
            "{API}/search/code?q={}&per_page={}",
            urlencode(&format!("{terms} {scope}")),
            limit.min(30)
        );
        // Text-match fragments need their own media type.
        let resp = self
            .client
            .get(&url)
            .headers(self.headers("application/vnd.github.text-match+json")?)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("code search → {status}: {}", first_line(&body));
        }
        let found: GhSearch<GhCode> = resp.json().await.context("decoding code search")?;
        Ok(found
            .items
            .into_iter()
            .map(|c| CodeHit {
                repo: c.repository.full_name,
                path: c.path,
                url: c.html_url,
                fragments: c
                    .text_matches
                    .into_iter()
                    .filter_map(|m| m.fragment)
                    .map(|f| truncate(f.trim(), 300))
                    .collect(),
            })
            .collect())
    }
}

// ---- wire types --------------------------------------------------------------

#[derive(Deserialize)]
struct GhSearch<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    created_at: Option<String>,
    updated_at: Option<String>,
    closed_at: Option<String>,
    /// Present only on pull requests.
    pull_request: Option<serde_json::Value>,
    /// The search API returns the repository only via the issue's own URL.
    repository_url: Option<String>,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

impl From<GhIssue> for IssueHit {
    fn from(i: GhIssue) -> Self {
        IssueHit {
            repo: i
                .repository_url
                .as_deref()
                .and_then(repo_from_api_url)
                .or_else(|| repo_from_html_url(&i.html_url))
                .unwrap_or_default(),
            number: i.number,
            title: i.title,
            state: i.state,
            kind: if i.pull_request.is_some() {
                "pull_request".into()
            } else {
                "issue".into()
            },
            url: i.html_url,
            body: i.body.map(|b| truncate(&b, 1_200)),
            labels: i.labels.into_iter().map(|l| l.name).collect(),
            created_at: i.created_at,
            updated_at: i.updated_at,
            closed_at: i.closed_at,
        }
    }
}

#[derive(Deserialize)]
struct GhIssueComment {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct GhReview {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
struct GhReviewComment {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct GhPull {
    number: u64,
    title: String,
    html_url: String,
    state: String,
    #[serde(default)]
    draft: bool,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    user: Option<GhUser>,
    head: Option<GhRef>,
    updated_at: Option<String>,
}

impl PullRequest {
    fn from_wire(p: GhPull, repo: &str) -> Self {
        PullRequest {
            repo: repo.to_string(),
            number: p.number,
            title: p.title,
            url: p.html_url,
            author: p.user.map(|u| u.login),
            state: p.state,
            draft: p.draft,
            body: p.body.map(|b| truncate(&b, 1_500)),
            labels: p.labels.into_iter().map(|l| l.name).collect(),
            head_ref: p.head.map(|h| h.label),
            updated_at: p.updated_at,
        }
    }
}

/// A PR's head ref. `label` is `owner:branch`, which is the useful form.
#[derive(Deserialize)]
struct GhRef {
    #[serde(default)]
    label: String,
}

#[derive(Deserialize)]
struct GhPullFile {
    filename: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    patch: Option<String>,
}

#[derive(Deserialize)]
struct GhRepoDetail {
    #[serde(default = "default_branch")]
    default_branch: String,
    /// Repository size in KB, as GitHub reports it.
    #[serde(default)]
    size: u64,
}

fn default_branch() -> String {
    "main".into()
}

#[derive(Deserialize)]
struct GhCommit {
    sha: String,
    html_url: Option<String>,
    commit: GhCommitMeta,
    author: Option<GhUser>,
}

#[derive(Deserialize)]
struct GhCommitMeta {
    message: String,
    author: Option<GhCommitAuthor>,
}

#[derive(Deserialize)]
struct GhCommitAuthor {
    name: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct GhCommitDetail {
    #[serde(default)]
    files: Vec<GhFile>,
}

#[derive(Deserialize)]
struct GhFile {
    filename: String,
}

#[derive(Deserialize)]
struct GhCode {
    path: String,
    html_url: String,
    repository: GhCodeRepo,
    #[serde(default)]
    text_matches: Vec<GhTextMatch>,
}

#[derive(Deserialize)]
struct GhCodeRepo {
    full_name: String,
}

#[derive(Deserialize)]
struct GhTextMatch {
    fragment: Option<String>,
}

impl GhCommit {
    fn into_entry(self, full_name: &str) -> CommitEntry {
        let committed_at = self
            .commit
            .author
            .as_ref()
            .and_then(|a| a.date.as_deref())
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        CommitEntry {
            full_name: full_name.to_string(),
            author: self
                .author
                .map(|a| a.login)
                .or_else(|| self.commit.author.and_then(|a| a.name)),
            committed_at,
            message: truncate(&self.commit.message, 1_000),
            url: self.html_url,
            sha: self.sha,
            files: Vec::new(),
        }
    }
}

// ---- helpers -----------------------------------------------------------------

/// `https://api.github.com/repos/owner/name` → `owner/name`.
fn repo_from_api_url(url: &str) -> Option<String> {
    let rest = url.split("/repos/").nth(1)?;
    let mut parts = rest.split('/');
    Some(format!("{}/{}", parts.next()?, parts.next()?))
}

/// `https://github.com/owner/name/issues/7` → `owner/name`.
fn repo_from_html_url(url: &str) -> Option<String> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    Some(format!("{}/{}", parts.next()?, parts.next()?))
}

/// Percent-encode a search expression. GitHub's search syntax needs `:` and `/`
/// preserved inside qualifiers, so this only escapes what actually breaks a query
/// string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn first_line(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| truncate(body.trim(), 200))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_parsed_from_either_url_shape() {
        assert_eq!(
            repo_from_api_url("https://api.github.com/repos/restatedev/restate").as_deref(),
            Some("restatedev/restate")
        );
        assert_eq!(
            repo_from_html_url("https://github.com/restatedev/restate-cloud/issues/7").as_deref(),
            Some("restatedev/restate-cloud")
        );
    }

    #[test]
    fn search_qualifiers_survive_encoding() {
        // `repo:owner/name` must stay intact or the search silently widens.
        let q = urlencode("pool exhausted repo:restatedev/restate is:issue");
        assert!(q.contains("repo:restatedev/restate"));
        assert!(q.contains("is:issue"));
        assert!(!q.contains(' '));
    }

    #[test]
    fn encodes_characters_that_break_a_query_string() {
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("\"quoted phrase\""), "%22quoted+phrase%22");
    }

    #[test]
    fn issue_hit_marks_pull_requests() {
        let raw = serde_json::json!({
            "number": 42, "title": "fix pool", "state": "closed",
            "html_url": "https://github.com/restatedev/restate/pull/42",
            "body": "b", "labels": [{"name": "bug"}],
            "pull_request": {"url": "x"},
            "repository_url": "https://api.github.com/repos/restatedev/restate"
        });
        let hit = IssueHit::from(serde_json::from_value::<GhIssue>(raw).unwrap());
        assert_eq!(hit.kind, "pull_request");
        assert_eq!(hit.repo, "restatedev/restate");
        assert_eq!(hit.labels, vec!["bug"]);
    }

    #[test]
    fn error_body_reduces_to_the_github_message() {
        let body = r#"{"message":"API rate limit exceeded","documentation_url":"…"}"#;
        assert_eq!(first_line(body), "API rate limit exceeded");
    }

    #[test]
    fn commit_entry_prefers_login_over_commit_author_name() {
        let raw = serde_json::json!({
            "sha": "abc123def456",
            "html_url": "https://github.com/restatedev/restate/commit/abc123def456",
            "commit": {"message": "fix: bound the pool\n\nlong body", "author": {"name": "Full Name", "date": "2026-07-01T10:00:00Z"}},
            "author": {"login": "octocat"}
        });
        let c = serde_json::from_value::<GhCommit>(raw)
            .unwrap()
            .into_entry("restatedev/restate");
        assert_eq!(c.author.as_deref(), Some("octocat"));
        assert_eq!(c.subject(), "fix: bound the pool");
        assert_eq!(c.short_sha(), "abc123de");
    }
}
