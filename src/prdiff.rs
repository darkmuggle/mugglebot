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

/// Patch characters per findings pass.
///
/// One focused prompt per batch is the point: a small local model asked to review eighteen
/// files at once returns a shrug, and the same model asked about two files returns specifics.
/// Sized so a batch is a few minutes of a human's attention, which is about the span the model
/// holds together.
pub const REVIEW_BATCH_CHARS: usize = 8_000;

/// Findings passes per pull request.
///
/// A ceiling on what one review costs the single on-device lane. A change bigger than this is
/// reviewed in its first four batches and says so — better than an unbounded queue of model
/// calls behind every other pass waiting to run.
pub const MAX_REVIEW_BATCHES: usize = 4;

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
    /// The head commit this diff is of.
    ///
    /// The freshness token that actually corresponds to the thing being cached. `watermark`
    /// records what *triggered* the read, which is not the same question: a pull request
    /// pushed to without notifying anybody keeps its watermark and silently keeps a diff of
    /// the previous commit. Compared against the live head by the push sweep.
    ///
    /// `Option`, and defaulted, because diffs stored before this existed have no sha — those
    /// read as "unknown", which the sweep treats as *stale* rather than current. Assuming
    /// current would leave every pre-existing diff permanently unrefreshed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
}

/// A stored report, with what it was built from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDiff {
    /// What triggered this read — the newest signal id on the PR, or a synthetic token for
    /// an explicit re-read. Free to compute, which is why it is here.
    ///
    /// **Not** the freshness token any more. It was, and the consequence was the bug this
    /// field's doc used to describe as a saving: activity is a proxy for "the diff changed",
    /// and a push to your own branch produces no activity. `report.head_sha` is the real
    /// answer; this stays for provenance and for the workflow key.
    pub watermark: String,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
    pub report: DiffReport,
}

/// What the review advises doing with the pull request.
///
/// Deliberately the three actions a reviewer actually has on GitHub, rather than a score. A
/// number invites being read as a grade on the author; "approve" or "request changes" is a
/// decision about the code, which is the only thing this is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recommendation {
    /// Good as it stands. Said plainly when it is true — a review that never approves is a
    /// review nobody believes, and "here are some observations" on a clean change is noise
    /// dressed as diligence.
    Approve,
    /// Worth saying something, nothing blocking.
    Comment,
    /// Something here should change before this lands.
    RequestChanges,
}

impl Recommendation {
    fn parse(raw: &str) -> Self {
        match raw
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "approve" | "approved" | "lgtm" => Self::Approve,
            "request_changes" | "changes_requested" | "block" => Self::RequestChanges,
            _ => Self::Comment,
        }
    }
}

/// How much one inline note matters.
/// Ordered by declaration, most severe first, so `<` reads as "more severe than" — used when
/// two findings land on the same line and only one can stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// This is wrong, or will break something. The only severity that justifies
    /// `request_changes` on its own.
    Blocker,
    /// A real concern worth answering before merge, but arguable.
    Concern,
    /// Style, naming, a missing test for an edge case. Cheap to fix, cheap to decline.
    Nit,
    /// Worth pointing at because it is *right* — the case a reviewer usually skips, which is
    /// why an approving review reads as content-free when it should read as specific.
    Praise,
}

impl Severity {
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "blocker" | "blocking" | "bug" | "error" => Self::Blocker,
            "concern" | "question" | "major" => Self::Concern,
            "praise" | "good" | "nice" => Self::Praise,
            _ => Self::Nit,
        }
    }
}

/// One note against a specific place in the diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    /// The file it is about. Always set; a note that names no file is dropped, because an
    /// unanchored remark is what a summary is for.
    pub path: String,
    pub severity: Severity,
    pub note: String,
    /// The line the model quoted, verbatim from the patch. This is the anchor that actually
    /// works: models are unreliable at counting positions in a hunk and reliable at copying
    /// a line they are looking at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// The new-file line number, when the model gave one. A fallback for the anchor, not a
    /// substitute — see [`anchor_index`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Resolved index into the file's patch lines, or `None` when neither the anchor nor the
    /// line number matched anything. Resolved here rather than in the UI so the matching is
    /// testable, and so an unresolvable note degrades to a file-level one instead of being
    /// silently attached to the wrong line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_index: Option<usize>,
}

/// A code review of one pull request: what to do about it, why, and where.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub recommendation: Recommendation,
    /// The general recommendation in prose — what a reviewer would write in the box above the
    /// Approve button.
    pub rationale: String,
    pub comments: Vec<ReviewComment>,
    /// Which tier produced it, so an operator can tell a local read from an escalated one.
    pub produced_by: String,
}

/// A stored review, with what it was built from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredReview {
    pub watermark: String,
    pub reviewed_at: chrono::DateTime<chrono::Utc>,
    pub review: Review,
}

/// Fetch and summarize one PR's diff.
pub struct DiffReader {
    pub github: Option<GithubClient>,
    /// Reads and reviews the diff. The `[reasoner] code` tier — off the machine by default,
    /// because a 33B local coder reviewing a refactor produced four copies of one sentence,
    /// each anchored to a line the patch deleted.
    pub reasoner: Arc<dyn Reasoner>,
    /// What the pane reports as having produced the review. Carried rather than assumed —
    /// see [`DiffReader::review`].
    tier: String,
}

impl DiffReader {
    pub fn new(
        token: Option<String>,
        reasoner: Arc<dyn Reasoner>,
        tier: impl Into<String>,
    ) -> Result<Self> {
        let github = match token {
            Some(t) => Some(GithubClient::new(t)?),
            None => None,
        };
        Ok(Self {
            github,
            reasoner,
            tier: tier.into(),
        })
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
            head_sha: None,
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
        // One extra call, to record *which commit* this diff is of. Worth it against the
        // rest of a read — a files call plus up to five model passes — and it is what lets a
        // push that notified nobody be noticed at all. Best-effort: a diff whose sha could
        // not be read is still a diff, and reads as stale rather than as current.
        let head_sha = match gh.pull_head_sha(repo, number as u64).await {
            Ok(sha) => sha,
            Err(e) => {
                debug!("head sha for {repo}#{number} unavailable: {e:#}");
                None
            }
        };
        let summary = self.summarize(repo, number, &files).await;
        let additions = files.iter().map(|f| f.additions).sum();
        let deletions = files.iter().map(|f| f.deletions).sum();
        DiffReport {
            repo: repo.to_string(),
            number,
            head_sha,
            file_count: files.len(),
            truncated: files.len() >= MAX_DIFF_FILES,
            files: files.into_iter().map(DiffFile::from).collect(),
            additions,
            deletions,
            summary,
            error: None,
        }
    }

    /// Review the change: approve it, or say what should change and where.
    ///
    /// This is deliberately more than the diff summary above it. A summary explains; a review
    /// takes a position — and the position that was missing is **approval**. A tool that only
    /// ever explains leaves the reader to decide whether the change is good, which is the
    /// judgment they wanted help with.
    ///
    /// It reviews the code and says nothing about who wrote it. Most pull requests on this
    /// board are the operator's own, and a review that goes easy on those is worthless
    /// precisely where it is read most.
    ///
    /// # Two stages, and why
    ///
    /// Findings first, verdict second. Asked for both at once, the on-device model reliably
    /// returned `approve` with an empty comment list and a rationale that restated the title —
    /// "a good improvement to the system" on an eighteen-file feature. That is the explainer
    /// problem wearing a verdict, and it happens because one prompt asks a small model to read
    /// a large diff, decide, and justify, all in one pass.
    ///
    /// So: each batch of files is reviewed on its own for *findings only*, with no verdict to
    /// reach for, and a final pass decides the recommendation **from the findings** rather than
    /// from the diff. Small focused prompts are what a local model is good at, and the verdict
    /// then has to follow from something specific.
    ///
    /// Best-effort throughout: a diff pane with no review is still a diff pane, and a model
    /// that returns prose produces no review rather than a fabricated verdict.
    /// Review with this reader's own tier — the automatic path.
    pub async fn review(&self, repo: &str, number: i64, report: &DiffReport) -> Option<Review> {
        self.review_with(repo, number, report, &*self.reasoner, &self.tier)
            .await
    }

    /// Review with a model the operator named.
    ///
    /// The reasoner is a parameter rather than a field because "review this again on a
    /// different model" is a question about *this* review, not a reconfiguration of the
    /// reader. `tier` is what the pane will report as having produced it, so it has to name
    /// the model that actually ran — the caption is the only way to tell two reviews of the
    /// same diff apart.
    pub async fn review_with(
        &self,
        repo: &str,
        number: i64,
        report: &DiffReport,
        reasoner: &dyn Reasoner,
        tier: &str,
    ) -> Option<Review> {
        let batches = batch_files(&report.files, REVIEW_BATCH_CHARS, MAX_REVIEW_BATCHES);
        if batches.is_empty() {
            return None;
        }
        let batched = batches.len();
        let mut comments: Vec<ReviewComment> = Vec::new();
        for batch in &batches {
            comments.extend(self.findings(repo, number, batch, report, reasoner).await);
        }
        let (recommendation, rationale) = self
            .decide(repo, number, report, &comments, reasoner)
            .await?;
        debug!(
            "reviewed {repo}#{number}: {recommendation:?} from {} finding(s) over {batched} batch(es)",
            comments.len()
        );
        Some(Review {
            recommendation,
            rationale,
            comments,
            // The tier that answered. It used to be hardcoded "local" on the reasoning that
            // this reader held the on-device tier by policy — true when written, false the
            // moment review moved to the `code` tier, and the pane went on captioning a
            // Sonnet review "reviewed by local".
            produced_by: tier.to_string(),
        })
    }

    /// Stage one: concrete findings for one batch of files, with no verdict to reach for.
    async fn findings(
        &self,
        repo: &str,
        number: i64,
        batch: &[&DiffFile],
        report: &DiffReport,
        reasoner: &dyn Reasoner,
    ) -> Vec<ReviewComment> {
        let system = "You are reviewing part of a pull request as a senior engineer on this \
             codebase. Report findings only — you are NOT deciding whether to approve, and you \
             must not comment on the change as a whole. Reply with ONLY a JSON array:\n\
             [{\"path\":\"<exact path from the diff>\", \
             \"anchor\":\"<the line you are commenting on, copied VERBATIM from the patch, \
             including its leading + or space>\", \
             \"severity\":\"blocker|concern|nit|praise\", \
             \"note\":\"<one or two sentences: what is wrong and what to do instead. For \
             praise, what is right and why it matters>\"}]\n\
             Rules:\n\
             - Judge the code. Say nothing about the author, their experience, or their \
             intentions. You do not know who wrote this and it does not matter.\n\
             - Every finding must quote a line that is really in the patch below, copied \
             exactly. If you cannot quote it, do not raise it.\n\
             - `blocker` is for something that is wrong: a bug, an unhandled edge case, a \
             security or data-loss hazard, a change that contradicts its own description. \
             `concern` is arguable but worth answering. `nit` is style, naming, or a missing \
             test. `praise` is for a specific choice that is notably right — not for the change \
             existing.\n\
             - Look for: off-by-one and boundary errors, unwrapped errors and swallowed \
             failures, a default that changes existing behaviour, a config value read but never \
             validated, retries without a ceiling, a lock or await held across a call, a \
             comparison that is inverted, dead or duplicated logic, and tests that assert the \
             mock rather than the behaviour.\n\
             - `[]` is a correct and common answer. Do not invent findings to look thorough, \
             and do not restate what the code obviously does.\n\
             - You are reading hunks only, capped per file. Do not comment on what is not \
             shown, and do not ask for context you were not given.\n\
             - NEVER say something is unused, not used, never called, not imported, not \
             exported, undefined, untested, no longer needed, or should be deleted — in this \
             file or anywhere else. You are looking at a few hunks of a large repository. You \
             cannot see the callers, the exports, the tests, or the rest of the file, so you \
             cannot know any of it, and rephrasing the claim as \"in this file\" does not make \
             it visible. Findings of this kind are discarded before anyone reads them.\n\
             - Report only what the lines in front of you demonstrate: a wrong value, an \
             inverted condition, an unhandled case in code that is shown, a type that does not \
             match its use in the same hunk.\n\
             - Do not appeal to general practice. \"Not recommended\", \"not a best \
             practice\", \"consider a different approach\" and \"this could cause issues\" \
             are not findings — they are true of every choice and specific to none. If you \
             cannot say what goes wrong, in these lines, do not raise it.\n\
             - **Anchor on a line the change ADDS (`+`) or leaves as context (space). Never \
             anchor on a `-` line.** A `-` line is code this pull request is deleting. \
             Criticizing it is arguing for the state the author is moving away from, and \
             asking them to restructure something they just removed is worse than useless. \
             If a deletion looks wrong, the finding is about what replaced it — anchor \
             there.\n\
             - Do not anchor on a `@@ ... @@` hunk header. It is positional bookkeeping, \
             not code.\n\
             - Two findings that make the same point are one finding. If you write the same \
             sentence about several places, you have found a pattern you cannot see the \
             whole of from these hunks — say it once, about the clearest instance, or not at \
             all.";
        let mut body = String::new();
        for f in batch {
            body.push_str(&format!(
                "--- {} (+{} -{})\n{}\n",
                f.path,
                f.additions,
                f.deletions,
                f.patch.as_deref().unwrap_or("")
            ));
        }
        // What the change is, and how big it is, before the hunks.
        //
        // Without this the pass was reviewing two files out of eighteen with no idea what the
        // pull request was doing — and it showed. Asked to review a refactor that moved each
        // controller into its own subtree, it read the deletions as proposals and filed four
        // blockers demanding the author restructure code they were in the middle of deleting.
        // A reviewer who cannot see that a change is a move cannot review a move.
        //
        // Both parts are free: the summary is already computed for the pane before `review`
        // runs, and the file list is in the report. Neither costs an API call.
        let intent = report.summary.as_deref().unwrap_or("(not summarized)");
        let shape = {
            let mut s = String::new();
            for f in &report.files {
                s.push_str(&format!(
                    "  {} (+{} -{})\n",
                    f.path, f.additions, f.deletions
                ));
            }
            s
        };
        let mut req = CompletionRequest::single(format!(
            "=== PULL REQUEST {repo}#{number} ===\n\
             What the change does, overall:\n{intent}\n\n\
             Every file it touches ({} file(s), +{} -{}){}:\n{shape}\n\
             === THE PART YOU ARE REVIEWING ({} of these files) ===\n{}\n\
             === YOUR TASK ===\nReport findings on the files in this part only. Judge them as \
             pieces of the change described above — if a line is being deleted or moved as \
             part of that, that is the change working as intended, not a defect. Reply with \
             the JSON array only.",
            report.file_count,
            report.additions,
            report.deletions,
            if report.truncated {
                " — list truncated"
            } else {
                ""
            },
            batch.len(),
            crate::tools::truncate_for_prompt(&body, MAX_DIFF_PROMPT_CHARS)
        ));
        req.system = Some(system.to_string());
        req.max_tokens = 900;
        let raw = match reasoner.complete(&req).await {
            Ok(t) => t,
            Err(e) => {
                debug!("findings unavailable for {repo}#{number}: {e:#}");
                return Vec::new();
            }
        };
        match crate::reasoner::extract_json(&raw) {
            // Tolerant of the two shapes a model returns: the array asked for, or an object
            // wrapping it. Rejecting the second would throw away good findings over packaging.
            Some(v) => {
                let array = v
                    .get("comments")
                    .or_else(|| v.get("findings"))
                    .cloned()
                    .unwrap_or(v);
                parse_comments(&array, report)
            }
            None => {
                debug!("findings for {repo}#{number} were not JSON");
                Vec::new()
            }
        }
    }

    /// Stage two: the verdict, decided from the findings rather than from the diff.
    async fn decide(
        &self,
        repo: &str,
        number: i64,
        report: &DiffReport,
        comments: &[ReviewComment],
        reasoner: &dyn Reasoner,
    ) -> Option<(Recommendation, String)> {
        let system = "You are the reviewer of record on a pull request, writing the note that \
             goes above the Approve button. Reply with ONLY JSON:\n\
             {\"recommendation\":\"approve|comment|request_changes\", \
             \"rationale\":\"<2-4 sentences>\"}\n\
             Rules:\n\
             - Decide from the FINDINGS. `request_changes` if any finding is a blocker; \
             `comment` if there are concerns worth answering; otherwise `approve`. Nits and \
             praise never block approval.\n\
             - No findings means the change is clean: `approve`, and say so directly.\n\
             - The rationale must be about THIS change: name the mechanism it changes, and say \
             what you checked. Banned as empty: \"good improvement\", \"improves the \
             system\", \"looks good\", \"well written\", \"follows best practices\", \
             and any sentence that would be true of any pull request.\n\
             - If you are requesting changes, lead with the blocker in one clause so it is \
             readable without scrolling.\n\
             - Judge the code, never the author.";
        let findings = if comments.is_empty() {
            "(none — the per-file passes found nothing to raise)".to_string()
        } else {
            comments
                .iter()
                .map(|c| {
                    format!(
                        "- [{:?}] {}: {}",
                        c.severity,
                        c.path,
                        c.note.replace('\n', " ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let files = report
            .files
            .iter()
            .map(|f| format!("{} (+{} -{})", f.path, f.additions, f.deletions))
            .collect::<Vec<_>>()
            .join("\n");
        let mut req = CompletionRequest::single(format!(
            "=== PULL REQUEST {repo}#{number} ===\n{} file(s), +{} -{}\n\n\
             === WHAT IT DOES ===\n{}\n\n=== FILES ===\n{files}\n\n\
             === FINDINGS FROM THE PER-FILE REVIEW ===\n{findings}\n\n\
             === YOUR TASK ===\nDecide the recommendation and write the rationale. Reply with \
             the JSON object only.",
            report.file_count,
            report.additions,
            report.deletions,
            report
                .summary
                .as_deref()
                .unwrap_or("(no summary available)"),
        ));
        req.system = Some(system.to_string());
        req.max_tokens = 400;
        let raw = match reasoner.complete(&req).await {
            Ok(t) => t,
            Err(e) => {
                debug!("review verdict unavailable for {repo}#{number}: {e:#}");
                return None;
            }
        };
        let v = crate::reasoner::extract_json(&raw)?;
        let rationale = v
            .get("rationale")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // A verdict with no reasoning is not a review. Better to show the findings under a
        // missing verdict than a bare pill the reader cannot weigh.
        if rationale.is_empty() {
            return None;
        }
        let stated = Recommendation::parse(
            v.get("recommendation")
                .and_then(|r| r.as_str())
                .unwrap_or("comment"),
        );
        Some((reconcile(stated, comments), rationale))
    }

    /// One paragraph on what a diff does, from the patches themselves.    /// One paragraph on what a diff does, from the patches themselves.
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

/// Reconcile the model's stated verdict with its own findings.
///
/// The findings are the evidence, so they win. Both directions matter:
///
/// - A **blocker** means something is wrong, so the verdict is `request_changes` however the
///   model labelled it. An approval sitting on top of "this will panic" is worse than either
///   half alone.
/// - **No blocker** means nothing was found to be wrong, so `request_changes` is demoted to
///   `comment`. Observed on a real pull request that enabled Linkerd HA correctly: two
///   `concern` findings, no blocker, and `request_changes` — resting on "not recommended in a
///   production environment", which is an appeal to a norm rather than to anything in the diff.
///   A verdict that blocks a merge has to rest on a finding that says something *is* wrong, or
///   it is the generic-advice failure again with a stronger word on it.
///
/// Nits and praise never block and never prevent an approval.
pub fn reconcile(stated: Recommendation, comments: &[ReviewComment]) -> Recommendation {
    if comments.iter().any(|c| c.severity == Severity::Blocker) {
        return Recommendation::RequestChanges;
    }
    match stated {
        Recommendation::RequestChanges => Recommendation::Comment,
        other => other,
    }
}

/// Group files into review batches, largest-first, stopping at `max_batches`.
///
/// Largest first because that is where the risk is: if the cap truncates a review, the files
/// it dropped should be the one-line ones. A file whose patch was not kept is skipped
/// entirely — there is nothing to review.
pub fn batch_files(
    files: &[DiffFile],
    batch_chars: usize,
    max_batches: usize,
) -> Vec<Vec<&DiffFile>> {
    let mut patched: Vec<&DiffFile> = files.iter().filter(|f| f.patch.is_some()).collect();
    patched.sort_by_key(|f| std::cmp::Reverse(f.patch.as_deref().map_or(0, str::len)));

    let mut out: Vec<Vec<&DiffFile>> = Vec::new();
    let mut current: Vec<&DiffFile> = Vec::new();
    let mut spent = 0usize;
    for f in patched {
        let len = f.patch.as_deref().map_or(0, str::len);
        // A single file over the budget still gets its own batch: it is the most interesting
        // file in the change, and skipping it to respect a byte count would be backwards.
        if !current.is_empty() && spent + len > batch_chars {
            out.push(std::mem::take(&mut current));
            spent = 0;
            if out.len() >= max_batches {
                return out;
            }
        }
        current.push(f);
        spent += len;
    }
    if !current.is_empty() && out.len() < max_batches {
        out.push(current);
    }
    out
}

/// A stored diff rendered for a model prompt: the summary, the file list, then the patches.
///
/// Lives here rather than at the caller because the shape it walks is here, and because
/// `patch_omitted` has to be reported rather than rendered as an empty patch — a file whose
/// patch was dropped to stay inside [`PERSIST_PATCH_BUDGET`] and a binary file with no patch
/// look identical otherwise, and one of those is about the change.
///
/// Largest patch first, matching [`batch_files`]: a render that has to be truncated should
/// lose the one-line files rather than the seven-thousand-character one.
pub fn render_for_prompt(report: &DiffReport) -> String {
    let mut out = format!(
        "{} PR #{}: {} file(s), +{} -{}{}\n",
        report.repo,
        report.number,
        report.file_count,
        report.additions,
        report.deletions,
        if report.truncated {
            " (file list truncated)"
        } else {
            ""
        }
    );
    if let Some(summary) = report.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("\nWhat it does: {summary}\n"));
    }
    if let Some(error) = report.error.as_deref() {
        out.push_str(&format!("\nThe diff could not be read: {error}\n"));
        return out;
    }

    out.push_str("\nFiles:\n");
    for f in &report.files {
        out.push_str(&format!("- {} (+{} -{})", f.path, f.additions, f.deletions));
        if f.patch_omitted {
            out.push_str(" [patch not stored]");
        } else if f.patch.is_none() {
            out.push_str(" [binary or no patch]");
        }
        out.push('\n');
    }

    let mut with_patches: Vec<&DiffFile> =
        report.files.iter().filter(|f| f.patch.is_some()).collect();
    with_patches.sort_by_key(|f| std::cmp::Reverse(f.patch.as_deref().map_or(0, str::len)));
    for f in with_patches {
        out.push_str(&format!(
            "\n--- {} ---\n{}\n",
            f.path,
            f.patch.as_deref().unwrap_or("")
        ));
    }
    out
}

/// Claims a diff cannot support, matched on the phrasing models use to make them.
///
/// A review of a few hunks cannot know whether a symbol is used elsewhere, exported, tested,
/// or safe to delete — and the local model asserts exactly those things, confidently, as
/// blockers. Observed on a real pull request: five "not used anywhere in the codebase"
/// blockers, every one of them wrong, about symbols the diff had just introduced and the rest
/// of the repository uses.
///
/// Filtered rather than trusted, on the same principle as `explain::verify` removing claims the
/// dossier cannot support: the deterministic check is a guarantee where a sterner prompt is only
/// a better guess. A false blocker does more damage than a missed nit, because it is the one
/// finding a reader acts on.
const UNVERIFIABLE: &[&str] = &[
    // Claims of absence about the wider codebase. Matched on the *shape* rather than exact
    // phrasings, because the first version of this list used phrasings and the model simply
    // rephrased around it: "not used anywhere" became "not used in the file", and the same
    // five wrong blockers came back.
    "not used",
    "unused",
    "never used",
    "not called",
    "never called",
    "not referenced",
    "never referenced",
    "not imported",
    "not exported",
    "not defined",
    "does not exist",
    "doesn't exist",
    "no longer needed",
    "no longer used",
    "should be removed as",
    "no tests",
    "not tested",
    "missing from the codebase",
];

/// Is the diff we hold still a diff of this pull request?
///
/// The whole freshness question in one place, because it used to be answered implicitly by a
/// workflow key built from notification activity — and a push to your own branch produces no
/// activity, so the first evaluation was the only one a pull request ever got.
///
/// `None` for the stored sha means the diff predates the field being recorded. Treated as
/// **stale**: assuming it were current would freeze exactly the rows that already carry the
/// bug. One redundant re-diff per pre-existing PR is the cost, once.
pub fn diff_is_current(stored: Option<&str>, head: &str) -> bool {
    stored.is_some_and(|s| s == head)
}

/// The marker that carries an operator-chosen model inside a workflow key.
///
/// Re-dispatching a review has to reach a workflow, and `submit_workflow` carries no body —
/// only a key. So the model rides in the key, which is also where it *belongs*: the key is
/// the idempotency identity, and "review this diff with Opus" is genuinely different work
/// from "review this diff with Sonnet". Same PR, same watermark, same model → a refused
/// redo, free, exactly as pressing the button twice should be. Different model → new work.
///
/// `#model:` rather than a bare separator because the key already contains `@`, `/`, `!` and
/// `#` in the pr key and the watermark, and this must not collide with any of them.
const MODEL_MARKER: &str = "#model:";

/// Decorate a workflow key's version with the model to review on.
pub fn with_model(version: &str, provider: &str, model: &str) -> String {
    format!("{version}{MODEL_MARKER}{provider}/{model}")
}

/// Split a decorated version back into `(watermark, Some((provider, model)))`.
///
/// The watermark comes back clean, which matters because it is stored on the pull request's
/// object as the freshness token: leaving the decoration on it would make every re-dispatched
/// review look like a different watermark and defeat the caching it exists for.
pub fn split_model(version: &str) -> (&str, Option<(&str, &str)>) {
    match version.split_once(MODEL_MARKER) {
        Some((watermark, spec)) => {
            // A forced redo appends `#r{timestamp}` *after* the spec to break the key's
            // idempotency. Cut it off: model names never contain `#`, and leaving it on made
            // the requested model `claude-opus-4-8#r1785…`, which builds a reasoner pointed at
            // a model that does not exist.
            let spec = spec.split_once('#').map_or(spec, |(head, _)| head);
            match spec.split_once('/') {
                // A provider with no model, or a model with no provider, is not enough to build a
                // reasoner — fall back to the default tier rather than guessing one half.
                Some((p, m)) if !p.is_empty() && !m.is_empty() => (watermark, Some((p, m))),
                _ => (watermark, None),
            }
        }
        None => (version, None),
    }
}

/// Whether an anchor is a quoted hunk header rather than a quoted line of code.
///
/// Three shapes, all seen in real output: the header itself (`@@ -10,6 +10,9 @@ fn serve()`),
/// the header with a diff marker glued on (`+57,18  @@ impl ApiDiscovery {`), and a bare
/// fragment of one (`-4`). None of them is code, and all of them read as code to a reviewer
/// skimming the pane.
fn looks_like_hunk_header(anchor: &str) -> bool {
    let a = anchor.trim();
    if a.contains("@@") {
        return true;
    }
    // `-4`, `+57,18` — a marker followed by nothing but line numbers.
    let rest = a.trim_start_matches(['+', '-', ' ']).trim();
    !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == ' ')
}

/// Whether two notes are making the same point.
///
/// Word overlap, not a prefix or an exact match, because the repeats seen in practice are
/// neither verbatim nor front-loaded: the four bogus blockers on `restate-cloud!1255` all
/// followed one template — *"The use of X implies that Y, which could lead to conflicts or
/// unexpected behavior. It's recommended to refactor the code so that each Z has its own
/// W."* — and diverged at the **fourth** word. Comparing leading words misses that
/// entirely; comparing whole strings misses it too.
///
/// The threshold is measured against those four notes rather than guessed. Jaccard over
/// their word sets: the genuine duplicate pair scores **0.84**, and every other pairing
/// among them scores **0.45–0.59**. So 0.75 separates "the same claim reworded" from "the
/// same template, different claim" with room on both sides. The looser pairs are left
/// alone deliberately — they are killed by the deleted-line rule, and collapsing findings
/// that merely *sound* alike would hide real ones.
const SAME_POINT_JACCARD: f32 = 0.75;

/// Below this many words, overlap is noise — two terse notes can share most of their words
/// and mean different things. Short notes fall back to matching exactly.
const SAME_POINT_MIN_WORDS: usize = 6;

fn same_point(a: &str, b: &str) -> bool {
    fn words(s: &str) -> std::collections::BTreeSet<String> {
        s.to_ascii_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '\'')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return false;
    }
    if wa.len() < SAME_POINT_MIN_WORDS || wb.len() < SAME_POINT_MIN_WORDS {
        return wa == wb;
    }
    let shared = wa.intersection(&wb).count() as f32;
    let union = wa.union(&wb).count() as f32;
    shared / union >= SAME_POINT_JACCARD
}

/// Whether a note asserts something the diff alone cannot show.
pub fn unverifiable(note: &str) -> bool {
    let lower = note.to_ascii_lowercase();
    UNVERIFIABLE.iter().any(|p| lower.contains(p))
}

/// Shape a model's findings array, dropping what cannot be trusted.
///
/// Every rule here exists because the alternative is a note that reads as authoritative and
/// points at the wrong thing: a finding on a file that is not in the diff, or a quoted line
/// that appears nowhere, is worse than no finding at all.
pub fn parse_comments(v: &serde_json::Value, report: &DiffReport) -> Vec<ReviewComment> {
    let mut comments = Vec::new();
    let items = match v.as_array() {
        Some(a) => a.as_slice(),
        None => std::slice::from_ref(v),
    };
    for c in items {
        let Some(path) = c.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        let note = c.get("note").and_then(|n| n.as_str()).unwrap_or("").trim();
        if note.is_empty() {
            continue;
        }
        // A path the diff does not contain means the note is about a file the model imagined.
        let Some(file) = report.files.iter().find(|f| f.path == path) else {
            continue;
        };
        let anchor = c
            .get("anchor")
            .and_then(|a| a.as_str())
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string);
        let line = c.get("line").and_then(|l| l.as_u64());
        let patch_index = file
            .patch
            .as_deref()
            .and_then(|patch| anchor_index(patch, anchor.as_deref(), line));
        // A claim about code the diff does not contain is dropped, however confidently it was
        // stated. See [`UNVERIFIABLE`].
        if unverifiable(note) {
            debug!("dropping an unverifiable finding on {path}: {note}");
            continue;
        }
        // What the model *quoted* is evidence in its own right, and has to be judged before
        // where the quote resolved to.
        //
        // Checking only the resolved line was not enough — proven by re-reviewing the same
        // diff on the local model once re-dispatch existed. Two findings anchored
        // `-pub struct HealthTarget {` came straight through: on a refactor that *moves*
        // code, that text exists as both a `-` and a `+` line, the anchor search returns
        // whichever comes first, and landing on the `+` copy made a note about a deletion
        // look reviewable. The model's own `-` prefix says which one it was reading.
        //
        // Three more were anchored `+57,18  @@ impl ApiDiscovery {` — a hunk header with a
        // `+` stuck on the front, which resolves to nothing and so survived as a file-level
        // note. A quoted hunk header is not a quotation of code however it is prefixed.
        if let Some(anchor) = anchor.as_deref() {
            if anchor.starts_with('-') && !looks_like_hunk_header(anchor) {
                debug!("dropping a finding quoting a deleted line in {path}: {note}");
                continue;
            }
            if looks_like_hunk_header(anchor) {
                debug!("dropping a finding quoting a hunk header in {path}: {note}");
                continue;
            }
        }
        // Where the anchor landed decides whether the finding is reviewable at all. The
        // prompt asks for all of this; the prompt was also being ignored, and a rule that
        // isn't enforced is a comment.
        match patch_index.and_then(|i| file.patch.as_deref()?.lines().nth(i)) {
            // A finding about a line the pull request *deletes*. This was the single biggest
            // source of nonsense: on a refactor that moved code between modules, every
            // "blocker" was anchored to a `-` line and demanded the author restructure
            // something they were removing. There is no useful version of this note.
            Some(l) if l.starts_with('-') => {
                debug!("dropping a finding on a deleted line in {path}: {note}");
                continue;
            }
            // A hunk header is positional bookkeeping. It matches an anchor search happily,
            // which is how `@@ -23,10 +23,10 @@ struct Cli {}` ended up quoted as code.
            Some(l) if l.starts_with("@@") => {
                debug!("dropping a finding anchored to a hunk header in {path}: {note}");
                continue;
            }
            // Anchored to an added or context line: reviewable, keep it.
            //
            // An anchor that did not resolve is deliberately *not* dropped here. It degrades
            // to a file-level note, which is the existing behaviour and was chosen for a
            // reason that still holds: a model that shortened or slightly misquoted a long
            // line has still found something real, and a note pinned to a guess would be
            // worse than one pinned to nothing. The two useless unanchored notes seen in
            // practice were useless because they were content-free nits, not because they
            // floated.
            Some(_) | None => {}
        }
        let severity = Severity::parse(c.get("severity").and_then(|s| s.as_str()).unwrap_or("nit"));
        // One finding per place. Two notes on the same line are almost always the model saying
        // the same thing twice in different words — which reads as two problems — and the more
        // severe of the two is the one worth keeping.
        // The same point made about several places is one finding. Anchoring dedup on
        // path+line only caught verbatim repeats in one file; the failure in practice was one
        // sentence reproduced across three files with four different anchors, which reads as
        // four independent problems and inflated the verdict to `request_changes`. Compared on
        // the note's own words, so it catches the repeat wherever it landed.
        if let Some(existing) = comments
            .iter()
            .find(|e: &&ReviewComment| same_point(&e.note, note))
        {
            debug!(
                "dropping a finding that repeats one already made on {}: {note}",
                existing.path
            );
            continue;
        }
        if let Some(existing) = comments
            .iter_mut()
            .find(|e: &&mut ReviewComment| e.path == path && e.patch_index == patch_index)
        {
            if severity < existing.severity {
                existing.severity = severity;
                existing.note = note.to_string();
            }
            continue;
        }
        comments.push(ReviewComment {
            path: path.to_string(),
            severity,
            note: note.to_string(),
            anchor,
            line,
            patch_index,
        });
    }
    comments
}

/// Shape a model's review JSON, dropping what cannot be trusted.
///
/// Every rule here exists because the alternative is a review that reads as authoritative and
/// points at the wrong thing: a note on a file that is not in the diff, or a quoted line that
/// appears nowhere, is worse than no note at all.
pub fn parse_review(v: &serde_json::Value, report: &DiffReport, produced_by: &str) -> Review {
    let recommendation = Recommendation::parse(
        v.get("recommendation")
            .and_then(|r| r.as_str())
            .unwrap_or("comment"),
    );
    let rationale = v
        .get("rationale")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let comments = v
        .get("comments")
        .map(|c| parse_comments(c, report))
        .unwrap_or_default();
    Review {
        // Same invariant as the two-stage path: whichever way the verdict arrives, it may not
        // contradict the findings it came with.
        recommendation: reconcile(recommendation, &comments),
        rationale,
        comments,
        produced_by: produced_by.to_string(),
    }
}

/// Resolve a comment to a line of the patch.
///
/// The quoted anchor is tried first and the line number second, because that is the order of
/// reliability: a model copying a line it is looking at is usually exact, and the same model
/// counting positions inside a hunk is often off by a few. Getting this backwards attaches
/// confident-looking notes to the wrong line, which is the one failure mode worse than
/// attaching them to none.
///
/// Returns `None` when neither matches — the caller renders the note at file level rather than
/// guessing.
pub fn anchor_index(patch: &str, anchor: Option<&str>, line: Option<u64>) -> Option<usize> {
    let lines: Vec<&str> = patch.lines().collect();
    if let Some(anchor) = anchor.map(str::trim).filter(|a| !a.is_empty()) {
        // Compared on the content, not the raw line: the model is asked to keep the leading
        // `+`, and drops it often enough that requiring it would throw away good anchors.
        let want = anchor.trim_start_matches(['+', '-', ' ']).trim();
        if !want.is_empty() {
            // A moved line exists twice — once as `-`, once as `+` — so content alone cannot
            // say which copy was quoted, and `position` returns whichever comes first. When
            // the anchor carries its marker, match the marker too: quoting the `+` copy of a
            // moved line is a finding about where it landed, and resolving that onto the `-`
            // copy made it look like a note about the removal and got it dropped.
            let marker = anchor.chars().next().filter(|c| *c == '+' || *c == '-');
            if let Some(marker) = marker {
                if let Some(i) = lines.iter().position(|l| {
                    l.starts_with(marker) && l.trim_start_matches(['+', '-', ' ']).trim() == want
                }) {
                    return Some(i);
                }
            }
            if let Some(i) = lines.iter().position(|l| {
                let content = l.trim_start_matches(['+', '-', ' ']).trim();
                content == want
            }) {
                return Some(i);
            }
            // A substring match, for a model that quoted part of a long line. Only when it is
            // unambiguous: two candidates mean we do not know which one it meant.
            let hits: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains(want))
                .map(|(i, _)| i)
                .collect();
            if let [only] = hits[..] {
                return Some(only);
            }
        }
    }
    let target = line?;
    // Walk the hunks counting new-file line numbers. A `-` line exists only in the old file,
    // so it does not advance the counter; everything else does.
    let mut new_line = 0u64;
    for (i, l) in lines.iter().enumerate() {
        if let Some(start) = hunk_new_start(l) {
            new_line = start;
            continue;
        }
        if new_line == 0 {
            continue;
        }
        if l.starts_with('-') {
            continue;
        }
        if new_line == target {
            return Some(i);
        }
        new_line += 1;
    }
    None
}

/// The new-file starting line of a hunk header: `@@ -12,7 +34,9 @@` → `34`.
fn hunk_new_start(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("@@ ")?;
    let plus = rest.split('+').nth(1)?;
    let digits: String = plus.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
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

/// Read one PR object's stored review, or `None` when it has not been reviewed.
pub async fn stored_review(
    ingress: &crate::restate::ingress::Ingress,
    repo: &str,
    number: i64,
) -> Result<Option<StoredReview>> {
    let body = ingress
        .call_object("PullRequest", &pr_key(repo, number), "review")
        .await
        .context("reading the stored review")?;
    Ok(serde_json::from_str::<Option<StoredReview>>(&body).unwrap_or(None))
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
            head_sha: Some("deadbeef".into()),
        }
    }

    const PATCH: &str = "@@ -10,6 +10,9 @@ fn serve() {\n     let cfg = load();\n-    let n = 1;\n+    let n = cfg.retries;\n+    if n > 100 {\n+        panic!(\"too many\");\n+    }\n     start(n);";

    #[test]
    fn a_quoted_line_anchors_the_comment() {
        // The reliable path: the model copies a line it is looking at, leading `+` and all.
        let i = anchor_index(PATCH, Some("+    if n > 100 {"), None).expect("anchored");
        assert_eq!(PATCH.lines().nth(i).unwrap(), "+    if n > 100 {");

        // The same line without its marker still anchors — models drop it often enough that
        // requiring it would throw away good anchors for no benefit.
        assert_eq!(anchor_index(PATCH, Some("if n > 100 {"), None), Some(i));

        // A line the patch does not contain resolves to nothing rather than to something
        // nearby: a confident note on the wrong line is worse than a note with no line.
        assert_eq!(anchor_index(PATCH, Some("let x = 9;"), None), None);
    }

    #[test]
    fn a_line_number_is_the_fallback_and_the_anchor_wins() {
        // New-file numbering: the hunk starts at 10, and the `-` line does not advance it.
        // 10 = " let cfg = load();", 11 = "+    let n = cfg.retries;", 12 = "+    if n > 100 {"
        let i = anchor_index(PATCH, None, Some(12)).expect("resolved by line");
        assert_eq!(PATCH.lines().nth(i).unwrap(), "+    if n > 100 {");

        // A number past the end of the hunk resolves to nothing.
        assert_eq!(anchor_index(PATCH, None, Some(999)), None);

        // When both are given and they disagree, the quoted line wins — it is the more
        // reliable of the two, which is the whole reason it is asked for.
        let by_anchor = anchor_index(PATCH, Some("panic!"), Some(11)).expect("anchored");
        assert!(PATCH.lines().nth(by_anchor).unwrap().contains("panic!"));
    }

    /// The review that prompted this, reduced to its shape.
    ///
    /// Taken from `restatedev/restate-cloud!1255` — a refactor moving each controller into
    /// its own subtree. The local model returned four "blockers", each anchored to a line
    /// the patch **deleted**, each a near-copy of one sentence, and the verdict came back
    /// `request_changes` on the strength of them. Every rule asserted here exists to kill
    /// one part of that.
    #[test]
    fn findings_about_deleted_code_and_repeated_findings_are_dropped() {
        let files = vec![DiffFile {
            path: "src/serve.rs".into(),
            additions: 4,
            deletions: 1,
            patch: Some(PATCH.to_string()),
            patch_omitted: false,
        }];
        let r = report(files);
        let raw = serde_json::json!([
            // Anchored to a `-` line: the author is deleting this. Arguing for it is arguing
            // against the change — this was every one of the four live "blockers".
            {"path": "src/serve.rs", "anchor": "-    let n = 1;", "severity": "blocker",
             "note": "The use of a literal implies the value is fixed, which could lead to unexpected behaviour. It is recommended to refactor this."},
            // A hunk header quoted as if it were code.
            {"path": "src/serve.rs", "anchor": "@@ -10,6 +10,9 @@ fn serve() {",
             "severity": "nit", "note": "This function signature is unclear."},
            // Anchored to an added line, so reviewable. Verbatim from the live review.
            {"path": "src/serve.rs", "anchor": "+        panic!(\"too many\");",
             "severity": "blocker",
             "note": "The use of REGISTRY implies that the state struct is being used by multiple controllers, which could lead to conflicts or unexpected behavior. It's recommended to refactor the code so that each controller has its own SharedState."},
            // The same claim with the subject swapped (0.84), on a different anchor — also
            // verbatim, and the repeat that per-line dedup could never catch.
            {"path": "src/serve.rs", "anchor": "+    if n > 100 {", "severity": "blocker",
             "note": "The use of a HashMap with String keys implies that the state struct is being used by multiple controllers, which could lead to conflicts or unexpected behavior. It's recommended to refactor the code so that each controller has its own SharedState."}
        ]);
        let comments = parse_comments(&raw, &r);
        assert_eq!(
            comments.len(),
            1,
            "two unreviewable and one repeat should go: {comments:?}"
        );
        assert!(comments[0].note.starts_with("The use of REGISTRY"));
        assert_eq!(
            comments[0].anchor.as_deref(),
            Some("+        panic!(\"too many\");")
        );
    }

    /// The two notes below are verbatim from `restate-cloud!1255` — the pair that scored
    /// 0.84, differing only in the subject of the sentence. Pinned as the calibration case
    /// for [`SAME_POINT_JACCARD`], since the threshold was chosen from these measurements
    /// and a change to either should have to face them.
    /// The anchors that survived the first version of these guards, verbatim from a
    /// re-review of `restate-cloud!1255` on the local model. Each one is why the check moved
    /// from "where did the anchor resolve" to "what did the model quote".
    #[test]
    fn a_quoted_hunk_header_or_deletion_is_recognised_from_the_anchor_alone() {
        // Hunk headers, including the two mangled forms the model actually produced.
        assert!(looks_like_hunk_header("@@ -10,6 +10,9 @@ fn serve() {"));
        assert!(looks_like_hunk_header("+57,18  @@ impl ApiDiscovery  {"));
        assert!(looks_like_hunk_header("-23,10 +23,10 @@ struct Cli  { }"));
        assert!(looks_like_hunk_header("-4"));
        assert!(looks_like_hunk_header("+62,18"));

        // Real code is not a hunk header, whatever its marker — including a deletion, which
        // is caught by the separate `-` rule rather than by this one.
        assert!(!looks_like_hunk_header("-pub struct HealthTarget  {"));
        assert!(!looks_like_hunk_header("+    if n > 100 {"));
        assert!(!looks_like_hunk_header("     let cfg = load();"));
        // A line that is only digits is indistinguishable from a header fragment and is not
        // worth anchoring to either way.
        assert!(looks_like_hunk_header("+    42"));
    }

    /// A moved line exists as both `-` and `+`. The anchor search returns whichever comes
    /// first, so resolving alone cannot tell a note about the removal from a note about the
    /// addition — the model's own prefix can, and this is the case that proved it.
    #[test]
    fn a_moved_line_quoted_as_a_deletion_is_still_dropped() {
        // `pub struct HealthTarget {` appears as a deletion *and* an addition, in that order.
        const MOVED: &str = "@@ -1,3 +1,4 @@\n-pub struct HealthTarget {\n     pub name: String,\n+pub struct HealthTarget {";
        let files = vec![DiffFile {
            path: "src/health.rs".into(),
            additions: 1,
            deletions: 1,
            patch: Some(MOVED.to_string()),
            patch_omitted: false,
        }];
        let r = report(files);
        let raw = serde_json::json!([
            {"path": "src/health.rs", "anchor": "-pub struct HealthTarget {",
             "severity": "blocker", "note": "This struct should not be declared here at all."}
        ]);
        assert!(
            parse_comments(&raw, &r).is_empty(),
            "quoting the `-` copy of a moved line is a note about the removal"
        );

        // The same line quoted as the addition is a real finding about where it landed.
        let raw = serde_json::json!([
            {"path": "src/health.rs", "anchor": "+pub struct HealthTarget {",
             "severity": "concern", "note": "The moved struct no longer derives Default, which the caller below relies on."}
        ]);
        assert_eq!(parse_comments(&raw, &r).len(), 1);
    }

    #[test]
    fn the_same_claim_reworded_is_one_finding() {
        let registry = "The use of REGISTRY implies that the state struct is being used by \
             multiple controllers, which could lead to conflicts or unexpected behavior. It's \
             recommended to refactor the code so that each controller has its own SharedState.";
        let hashmap = "The use of a HashMap with String keys implies that the state struct is \
             being used by multiple controllers, which could lead to conflicts or unexpected \
             behavior. It's recommended to refactor the code so that each controller has its \
             own SharedState.";
        assert!(same_point(registry, hashmap), "0.84 — the same claim");

        // Same template, different claim (0.46). Left alone on purpose: collapsing findings
        // that merely sound alike would hide real ones.
        let array = "The use of a hardcoded array implies that the controller is only handling \
             one component, which could lead to unexpected behavior if more components are \
             added in the future. It's recommended to refactor the code so that each component \
             has its own renderer.";
        assert!(!same_point(registry, array), "0.46 — a different claim");

        // Genuinely different findings are not collapsed.
        assert!(!same_point(
            "Clamp instead of panicking on this config value at startup.",
            "This retry loop has no ceiling and will spin forever on a 500.",
        ));
        // Short notes match only exactly, and an empty note matches nothing.
        assert!(same_point("Missing a period.", "Missing a period."));
        assert!(!same_point("Missing a period.", "Missing a comma."));
        assert!(!same_point("", ""));
    }

    #[test]
    fn a_review_drops_what_it_cannot_stand_behind() {
        let files = vec![DiffFile {
            path: "src/serve.rs".into(),
            additions: 4,
            deletions: 1,
            patch: Some(PATCH.to_string()),
            patch_omitted: false,
        }];
        let r = report(files);
        let raw = serde_json::json!({
            "recommendation": "request_changes",
            "rationale": "The retry ceiling panics instead of clamping.",
            "comments": [
                // Anchored and real: kept, with its resolved patch line.
                {"path": "src/serve.rs", "anchor": "+        panic!(\"too many\");",
                 "severity": "blocker", "note": "Clamp instead of panicking on config."},
                // A file that is not in the diff: the model imagined it.
                {"path": "src/other.rs", "anchor": "+ x", "severity": "nit", "note": "no"},
                // A real file, a line that does not exist: kept as a file-level note rather
                // than pinned to a guess.
                {"path": "src/serve.rs", "anchor": "+ nonexistent line", "severity": "nit",
                 "note": "Name this constant."},
                // No note is no comment.
                {"path": "src/serve.rs", "anchor": "+    if n > 100 {", "severity": "nit", "note": "  "}
            ]
        });
        let review = parse_review(&raw, &r, "local");
        assert_eq!(review.recommendation, Recommendation::RequestChanges);
        assert_eq!(review.comments.len(), 2, "{:?}", review.comments);
        assert_eq!(review.comments[0].severity, Severity::Blocker);
        assert!(review.comments[0].patch_index.is_some());
        assert_eq!(
            review.comments[1].patch_index, None,
            "an unresolvable anchor degrades to a file-level note"
        );

        // An unknown recommendation is `comment`, never silently an approval: guessing
        // "approve" from a malformed answer is the one wrong direction to fail in.
        let vague = serde_json::json!({ "recommendation": "looks fine to me?", "rationale": "" });
        assert_eq!(
            parse_review(&vague, &r, "local").recommendation,
            Recommendation::Comment
        );
        assert_eq!(
            parse_review(&serde_json::json!({"recommendation": "LGTM"}), &r, "local")
                .recommendation,
            Recommendation::Approve,
            "the shapes a model actually returns still resolve"
        );
    }

    #[test]
    fn batches_are_largest_first_and_bounded() {
        let f = |path: &str, n: usize| DiffFile {
            path: path.into(),
            additions: n as u64,
            deletions: 0,
            patch: Some("x".repeat(n)),
            patch_omitted: false,
        };
        let files = vec![
            f("small.rs", 100),
            f("huge.rs", 7_000),
            f("mid.rs", 3_000),
            DiffFile {
                path: "vendor.lock".into(),
                additions: 900,
                deletions: 0,
                patch: None,
                patch_omitted: true,
            },
        ];
        let batches = batch_files(&files, 5_000, 4);
        // Largest first, because a truncated review should drop the one-line files, not the
        // seven-thousand-character one.
        assert_eq!(batches[0][0].path, "huge.rs");
        // A file over the budget on its own still gets reviewed rather than being skipped to
        // respect a byte count.
        assert_eq!(batches[0].len(), 1);
        assert_eq!(
            batches[1]
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["mid.rs", "small.rs"]
        );
        // A file whose patch was not kept has nothing to review.
        assert!(!batches.iter().flatten().any(|f| f.path == "vendor.lock"));

        // The cap is a ceiling on what one review costs the single on-device lane.
        let many: Vec<DiffFile> = (0..20).map(|i| f(&format!("f{i}.rs"), 4_000)).collect();
        assert_eq!(batch_files(&many, 5_000, 4).len(), 4);
        // Nothing reviewable is no batches, so no model call happens at all.
        assert!(batch_files(&[], 5_000, 4).is_empty());
    }

    /// The verdict has to follow the findings, in both directions.
    #[test]
    fn the_verdict_cannot_contradict_the_findings() {
        let r = report(vec![DiffFile {
            path: "src/serve.rs".into(),
            additions: 4,
            deletions: 1,
            patch: Some(PATCH.to_string()),
            patch_omitted: false,
        }]);
        let with = |severity: &str| {
            parse_comments(
                &serde_json::json!([{
                    "path": "src/serve.rs", "anchor": "+    if n > 100 {",
                    "severity": severity, "note": "Something specific about this line."
                }]),
                &r,
            )
        };
        // `decide` owns the reconciliation, but the invariant it enforces is this pairing, and
        // it is worth pinning without a model in the loop: request_changes needs a blocker.
        assert_eq!(
            reconcile(Recommendation::RequestChanges, &with("concern")),
            Recommendation::Comment
        );
        assert_eq!(
            reconcile(Recommendation::RequestChanges, &with("blocker")),
            Recommendation::RequestChanges
        );
        assert_eq!(
            reconcile(Recommendation::Approve, &with("blocker")),
            Recommendation::RequestChanges
        );
        // Nits and praise never block, and never stop an approval either.
        assert_eq!(
            reconcile(Recommendation::Approve, &with("nit")),
            Recommendation::Approve
        );
        assert_eq!(
            reconcile(Recommendation::Approve, &[]),
            Recommendation::Approve
        );
        // A blocker decides it whatever the model said, including up from `comment`: it found
        // something wrong, and "here are some thoughts" is not what that means.
        assert_eq!(
            reconcile(Recommendation::Comment, &with("blocker")),
            Recommendation::RequestChanges
        );
    }

    #[test]
    fn claims_the_diff_cannot_support_are_dropped() {
        // Every one of these came back as a *blocker* from the local model on a real pull
        // request, about symbols the diff had just introduced. A review of a few hunks cannot
        // see the rest of the repository, so it cannot know any of it.
        assert!(unverifiable(
            "The import for `foo` is not used anywhere in the code."
        ));
        assert!(unverifiable(
            "getRestateEnvironment is never called. It should be removed as it is unnecessary."
        ));
        assert!(unverifiable(
            "This function is not exported from the module."
        ));
        assert!(unverifiable("There are no tests covering this branch."));

        // What the lines themselves show is kept, including notes whose wording brushes past
        // the same words.
        assert!(!unverifiable(
            "This sets defaultProbeScrapeInterval to a string; the field is a number."
        ));
        assert!(!unverifiable(
            "The retry loop has no ceiling — it will spin forever."
        ));
        assert!(!unverifiable("`used` is misspelled in this message."));
    }

    #[test]
    fn findings_are_deduped_across_batches() {
        let r = report(vec![DiffFile {
            path: "src/serve.rs".into(),
            additions: 4,
            deletions: 1,
            patch: Some(PATCH.to_string()),
            patch_omitted: false,
        }]);
        // The same finding twice — overlapping batches, or a model repeating itself — is one
        // finding, not two identical notes on the same line.
        let raw = serde_json::json!([
            {"path": "src/serve.rs", "anchor": "+    if n > 100 {", "severity": "concern", "note": "Clamp it."},
            {"path": "src/serve.rs", "anchor": "+    if n > 100 {", "severity": "concern", "note": "Clamp it."},
            {"path": "src/serve.rs", "anchor": "+        panic!(\"too many\");", "severity": "blocker", "note": "Do not panic."}
        ]);
        let out = parse_comments(&raw, &r);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].severity, Severity::Blocker);

        // Two findings on the *same* line collapse to the more severe one: that is the model
        // rephrasing itself, and showing both reads as two problems.
        let same_line = serde_json::json!([
            {"path": "src/serve.rs", "anchor": "+    if n > 100 {", "severity": "nit", "note": "Name the ceiling."},
            {"path": "src/serve.rs", "anchor": "if n > 100 {", "severity": "blocker", "note": "Off by one: n == 100 is allowed."}
        ]);
        let out = parse_comments(&same_line, &r);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, Severity::Blocker);
        assert!(out[0].note.starts_with("Off by one"));

        // An unverifiable finding is dropped even when it is the only one, so nothing takes its
        // place and the review comes back clean rather than confidently wrong.
        let bogus = serde_json::json!([
            {"path": "src/serve.rs", "anchor": "+    let n = cfg.retries;", "severity": "blocker",
             "note": "cfg.retries is not used anywhere else and should be removed."}
        ]);
        assert!(parse_comments(&bogus, &r).is_empty());
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
    /// The model rides in the workflow key, so the codec has to survive the keys that
    /// actually occur — and a GitHub watermark is `github/24818841558@2026-07-29T19:22:36Z`,
    /// which already contains `/` and `@`.
    /// The push that nobody is told about.
    ///
    /// A pull request's diff was cached against the newest *notification* on it, so a push to
    /// your own branch — which notifies no one — left the diff and its review frozen at the
    /// first commit while the pane went on presenting them as current. The head sha is the
    /// only token that moves when the diff moves.
    #[test]
    fn a_diff_is_current_only_for_the_commit_it_was_read_at() {
        assert!(diff_is_current(Some("1ba62cdc"), "1ba62cdc"));
        // Pushed since: the stored diff is of the previous commit.
        assert!(!diff_is_current(Some("1ba62cdc"), "9f0e1a22"));

        // No recorded sha — a diff stored before the field existed. Stale, deliberately:
        // calling it current would leave every pre-existing row frozen forever, which is the
        // bug rather than a saving.
        assert!(!diff_is_current(None, "1ba62cdc"));
    }

    #[test]
    fn a_model_rides_in_the_key_without_disturbing_the_watermark() {
        let wm = "github/24818841558@2026-07-29T19:22:36Z";
        let version = with_model(wm, "claude", "claude-opus-5");
        // The watermark comes back byte-identical. This is the part that matters: it is the
        // freshness token stored on the pull request's object, and a decorated one would make
        // every re-dispatch look like new activity.
        assert_eq!(
            split_model(&version),
            (wm, Some(("claude", "claude-opus-5")))
        );

        // An undecorated key is the automatic path, and must stay untouched.
        assert_eq!(split_model(wm), (wm, None));

        // A model name containing a slash still resolves — `split_once` takes the first, so
        // provider and model separate at the right place.
        let ollama = with_model(wm, "ollama_local", "deepseek-coder:33b");
        assert_eq!(
            split_model(&ollama),
            (wm, Some(("ollama_local", "deepseek-coder:33b")))
        );

        // Half a spec is not enough to build a reasoner. Falling back to the default tier is
        // right; guessing the missing half is not.
        assert_eq!(split_model(&format!("{wm}#model:claude")), (wm, None));
        assert_eq!(split_model(&format!("{wm}#model:/opus")), (wm, None));
        assert_eq!(split_model(&format!("{wm}#model:claude/")), (wm, None));

        // A forced redo breaks the key without corrupting the model name — the suffix lands
        // after the spec and has to be cut off, or the reasoner is built for a model called
        // `claude-opus-5#r1785...`.
        let forced = format!("{}#r1785000000", with_model(wm, "claude", "claude-opus-5"));
        assert_eq!(
            split_model(&forced),
            (wm, Some(("claude", "claude-opus-5")))
        );

        // Different models are different keys — which is what makes re-dispatch on one model
        // free and re-dispatch on another real work.
        assert_ne!(
            with_model(wm, "claude", "claude-opus-5"),
            with_model(wm, "claude", "claude-sonnet-5")
        );
    }

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
