//! A pull request's diff, summarized — and stored on the pull request's own object.
//!
//! This used to be fetched on demand and never kept: a diff is one API call plus a local
//! model pass, and eagerly diffing every PR across 147 repositories would spend the GitHub
//! budget the watchers depend on. That reasoning was right about *what not to diff* and
//! wrong about *what not to keep*. The board holds only pull requests the operator is
//! actually in — tens, not thousands — and for those the diff is read repeatedly: once
//! from the card, again from the issue it attempts, again after clicking in. Re-deriving it
//! each time paid the same API call and the same model pass for an answer that had not
//! changed.
//!
//! So the report lives in the `PullRequest` object's state, keyed by the watermark it was
//! built from. Same watermark → the stored report, no call to anything. New activity on the
//! PR → a new watermark → one refresh, submitted as a workflow so a GitHub 403 half-way
//! through resumes rather than restarting.
//!
//! # What is stored, and what is not
//!
//! Patches are the bulk of a diff and the part with no upper bound: sixty files at four
//! thousand characters each is a quarter of a megabyte in object state *and* in the
//! invocation journal that wrote it. So the stored report keeps every file's path and
//! counts — which is what the pane shows collapsed, and what makes the totals honest — and
//! keeps patches only up to [`PERSIST_PATCH_BUDGET`]. A file whose patch was dropped says
//! so, because a reader who cannot tell "no textual hunk" from "we didn't keep it" is being
//! misled about the change.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::debug;

use crate::github::{GithubClient, PullFile};
use crate::reasoner::{CompletionRequest, Reasoner};

/// Pull requests diffed in one pane request. An issue with a dozen attempts is real, and a
/// dozen diffs is a dozen API calls plus a dozen model passes for one click.
pub const MAX_DIFF_PRS: usize = 5;

/// Files fetched per diff. Beyond this the report says it is truncated rather than
/// pretending the change is smaller than it is.
pub const MAX_DIFF_FILES: usize = 60;

/// Patch characters kept per file. A generated lockfile is thousands of lines of nothing,
/// and one such file would otherwise crowd out every real change in the summary.
pub const MAX_PATCH_CHARS: usize = 4_000;

/// The whole diff handed to the model. Bounded independently of the per-file cap, because
/// sixty files at four thousand characters would be a quarter of a million.
pub const MAX_DIFF_PROMPT_CHARS: usize = 24_000;

/// Total patch characters kept in object state, across all files of one PR.
///
/// State is replicated and every write goes through the invocation journal, so this is the
/// number that decides what a warm board costs the cluster. Sized to hold the patches of an
/// ordinary review in full and to degrade to path-and-counts on a vendored-dependency bump,
/// which is the case where the patches are worth the least anyway.
pub const PERSIST_PATCH_BUDGET: usize = 64_000;

/// One file in a stored diff.
///
/// Mirrors [`PullFile`] plus the one fact the wire type has no reason to carry: whether the
/// patch is absent because GitHub had none (binary) or because we chose not to keep it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
    /// The patch existed but was dropped to stay inside [`PERSIST_PATCH_BUDGET`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub patch_omitted: bool,
}

impl From<PullFile> for DiffFile {
    fn from(f: PullFile) -> Self {
        Self {
            path: f.path,
            additions: f.additions,
            deletions: f.deletions,
            patch: f.patch,
            patch_omitted: false,
        }
    }
}

/// One pull request's diff, as the pane renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffReport {
    pub repo: String,
    pub number: i64,
    pub files: Vec<DiffFile>,
    pub file_count: usize,
    pub additions: u64,
    pub deletions: u64,
    /// What the change does, from the patches. `None` when the local model was unavailable
    /// — a diff pane with no summary is still a diff pane.
    pub summary: Option<String>,
    /// More files than were fetched. Said explicitly, because a truncated diff that looks
    /// complete is how a reader concludes a change is smaller than it is.
    pub truncated: bool,
    /// Why this PR has no diff, when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A stored report, with what it was built from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDiff {
    /// The subject watermark this was read at — the newest signal id on the PR. The
    /// freshness token, and free: it needs no extra API call to compute, unlike a head sha.
    pub watermark: String,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
    pub report: DiffReport,
}

/// Fetch and summarize one PR's diff.
pub struct DiffReader {
    pub github: Option<GithubClient>,
    /// Local by policy: reading a diff is exactly the work that shouldn't leave the machine.
    pub reasoner: Arc<dyn Reasoner>,
}

impl DiffReader {
    pub fn new(token: Option<String>, reasoner: Arc<dyn Reasoner>) -> Result<Self> {
        let github = match token {
            Some(t) => Some(GithubClient::new(t)?),
            None => None,
        };
        Ok(Self { github, reasoner })
    }

    /// Read `repo#number`'s diff and summarize it.
    ///
    /// An unreadable PR comes back as a report carrying its error rather than as an `Err`:
    /// one broken PR must not empty the pane for the others attempting the same issue.
    pub async fn read(&self, repo: &str, number: i64) -> DiffReport {
        let empty = |error: Option<String>| DiffReport {
            repo: repo.to_string(),
            number,
            files: Vec::new(),
            file_count: 0,
            additions: 0,
            deletions: 0,
            summary: None,
            truncated: false,
            error,
        };
        let Some(gh) = &self.github else {
            return empty(Some("reading a diff needs a stored GitHub token".into()));
        };
        let files = match gh
            .pull_files(repo, number as u64, MAX_DIFF_FILES, MAX_PATCH_CHARS)
            .await
        {
            Ok(f) => f,
            Err(e) => return empty(Some(format!("{e:#}"))),
        };
        let summary = self.summarize(repo, number, &files).await;
        let additions = files.iter().map(|f| f.additions).sum();
        let deletions = files.iter().map(|f| f.deletions).sum();
        DiffReport {
            repo: repo.to_string(),
            number,
            file_count: files.len(),
            truncated: files.len() >= MAX_DIFF_FILES,
            files: files.into_iter().map(DiffFile::from).collect(),
            additions,
            deletions,
            summary,
            error: None,
        }
    }

    /// One paragraph on what a diff does, from the patches themselves.
    ///
    /// Best-effort: a pane that fails to open because Ollama is down is nothing, whereas a
    /// pane with a file list and no summary is still the diff.
    async fn summarize(&self, repo: &str, number: i64, files: &[PullFile]) -> Option<String> {
        if files.is_empty() {
            return None;
        }
        let system = "You summarize a pull request's diff for an engineer deciding whether to \
             review it. Two or three sentences, behavioural: what the change makes the system do \
             differently, and which part of it carries the risk. Name the mechanism only where the \
             diff shows it. If the change is mechanical — a version bump, a rename, generated code \
             — say so plainly; \"no behavioural change\" is a useful answer. Do not restate the \
             file list, which the reader can already see.";
        let mut body = String::new();
        for f in files {
            body.push_str(&format!(
                "--- {} (+{} -{})\n",
                f.path, f.additions, f.deletions
            ));
            if let Some(patch) = &f.patch {
                body.push_str(patch);
                body.push('\n');
            }
        }
        let mut req = CompletionRequest::single(format!(
            "Pull request {repo}#{number}\n\n{}\n\n=== YOUR TASK ===\nSummarize what this diff does.",
            crate::tools::truncate_for_prompt(&body, MAX_DIFF_PROMPT_CHARS)
        ));
        req.system = Some(system.to_string());
        req.max_tokens = 320;
        match self.reasoner.complete(&req).await {
            Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
            Ok(_) => None,
            Err(e) => {
                debug!("diff summary unavailable for {repo}#{number}: {e:#}");
                None
            }
        }
    }
}

/// Trim a report's patches to what is worth replicating.
///
/// Files keep their order — the API returns them in the PR's own order, which is roughly
/// significance-first for a hand-written change — so the budget is spent on the files a
/// reviewer opens first rather than on whatever sorts last.
pub fn trim_for_state(mut report: DiffReport) -> DiffReport {
    let mut spent = 0usize;
    for f in report.files.iter_mut() {
        let Some(patch) = &f.patch else { continue };
        if spent + patch.len() <= PERSIST_PATCH_BUDGET {
            spent += patch.len();
        } else {
            f.patch = None;
            f.patch_omitted = true;
        }
    }
    report
}

/// `owner/repo!987` → `("owner/repo", 987)`.
pub fn parse_pr_key(key: &str) -> Option<(String, i64)> {
    let (repo, number) = key.rsplit_once('!')?;
    Some((repo.to_string(), number.parse().ok()?))
}

/// `("owner/repo", 987)` → `owner/repo!987`, the `PullRequest` object key.
pub fn pr_key(repo: &str, number: i64) -> String {
    format!("{repo}!{number}")
}

/// Read one PR object's stored diff, or `None` when it has never been read.
pub async fn stored(
    ingress: &crate::restate::ingress::Ingress,
    repo: &str,
    number: i64,
) -> Result<Option<StoredDiff>> {
    let body = ingress
        .call_object("PullRequest", &pr_key(repo, number), "diff")
        .await
        .context("reading the stored diff")?;
    // The handler answers `null` for "never read", which is not an error and must not be
    // logged as one: it is the ordinary state of a PR nobody has opened yet.
    Ok(serde_json::from_str::<Option<StoredDiff>>(&body).unwrap_or(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, patch: Option<&str>) -> DiffFile {
        DiffFile {
            path: path.into(),
            additions: 1,
            deletions: 0,
            patch: patch.map(str::to_string),
            patch_omitted: false,
        }
    }

    fn report(files: Vec<DiffFile>) -> DiffReport {
        DiffReport {
            repo: "o/r".into(),
            number: 1,
            file_count: files.len(),
            files,
            additions: 0,
            deletions: 0,
            summary: None,
            truncated: false,
            error: None,
        }
    }

    #[test]
    fn trimming_keeps_the_first_files_whole_and_marks_the_rest() {
        let big = "x".repeat(PERSIST_PATCH_BUDGET - 10);
        let out = trim_for_state(report(vec![
            file("a.rs", Some(&big)),
            file("b.rs", Some("+one line")),
            file("vendor.lock", Some(&"y".repeat(5_000))),
            // A binary file had no patch to begin with, which is a different fact from one
            // we dropped — the pane says "—" for the first and "not stored" for the second.
            file("logo.png", None),
        ]));
        assert!(
            out.files[0].patch.is_some(),
            "the first file survives whole"
        );
        assert!(!out.files[0].patch_omitted);
        assert!(
            out.files[1].patch.is_some(),
            "a small patch still fits inside the budget"
        );
        assert!(
            out.files[2].patch.is_none() && out.files[2].patch_omitted,
            "the file that would blow the budget is marked, not silently emptied"
        );
        assert!(
            out.files[3].patch.is_none() && !out.files[3].patch_omitted,
            "a binary file was never omitted by us"
        );
        // Every file keeps its path and counts either way: the collapsed pane and the
        // totals stay correct however much patch text was dropped.
        assert_eq!(out.files.len(), 4);
        assert_eq!(out.file_count, 4);
    }

    /// The PR key shapes that reach the diff pane, and the ones that must not be mistaken
    /// for it.
    #[test]
    fn pr_keys_round_trip() {
        assert_eq!(
            parse_pr_key("restatedev/restate-cloud!1235"),
            Some(("restatedev/restate-cloud".to_string(), 1235))
        );
        // `rsplit_once` on purpose: a repo name may legitimately contain `!`, and the number
        // is always last.
        assert_eq!(
            parse_pr_key("o/we!rd!12"),
            Some(("o/we!rd".to_string(), 12))
        );
        // Neither a Slack thread nor a malformed key is a pull request — reading a diff for
        // one would mean fetching a PR that does not exist.
        assert_eq!(parse_pr_key("chan/1699999.123"), None);
        assert_eq!(parse_pr_key("o/r!"), None);
        assert_eq!(parse_pr_key("o/r!notanumber"), None);
        assert_eq!(parse_pr_key(""), None);
        assert_eq!(
            pr_key("restatedev/restate-cloud", 1235),
            "restatedev/restate-cloud!1235"
        );
        // An issue key is not a PR key. `#` and `!` are deliberately different: issue and
        // PR numbering are independent, so `o/r#5` and `o/r!5` are different things.
        assert_eq!(parse_pr_key("o/r#5"), None);
    }
}
