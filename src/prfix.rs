//! "Is somebody already fixing this?" — open PRs as candidate fixes.
//!
//! Before starting on an assigned issue, the single most valuable thing to know is
//! that a pull request already exists for it — quite possibly written by someone
//! else, which is precisely the case you won't find by reading your own
//! notifications. Duplicated work is expensive and invisible until the second PR
//! opens.
//!
//! So for each triaged issue, MuggleBot scans the repo's open PRs, shortlists the
//! plausible ones, and for each candidate answers three questions in order:
//!
//! 1. **What does it actually implement?** — read from the diff, not the title. A
//!    PR description states intent; the patch states behavior.
//! 2. **Does it really fix the issue?** — a critique, including what it misses.
//!    This is the part that saves you: "closes #412" in a description is a claim,
//!    not a verification.
//! 3. **What else does it resolve?** — a patch that touches the mechanism behind
//!    several issues closes more than the one it names.
//!
//! # Escalation
//!
//! The analysis runs on the **local** coder model, which is the right tool for
//! reading a diff. If it fails, or returns something unusable, the same question
//! goes to the small cloud model, then to the routed tier. Escalation is on
//! *failure*, not on preference — and whichever tier answered is recorded on the
//! result, so a verdict can always be attributed.
//!
//! Nothing here comments on, approves, or merges a pull request.

use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::comments::CommentJudge;
use crate::correlation::{Analyst, RelationKind};
use crate::crossref;
use crate::github::{GithubClient, PullFile, PullRequest};
use crate::reasoner::{self, CompletionRequest, Reasoner};
pub use crate::store::PrFix;
use crate::store::{IssueTriage, Store};

/// What a PR is being judged *against*.
///
/// Two callers need this: assigned-issue triage (the issue you own) and root-cause
/// investigation (a subject that correlated up to an issue or PR). Naming the subject
/// explicitly, rather than taking a triage row, is what lets an investigated subject
/// have its associated PRs judged too — the association is worthless if nothing
/// reads it.
#[derive(Debug, Clone)]
pub struct Subject {
    /// Storage key: `owner/repo#number` for an issue, else the subject id.
    pub key: String,
    pub repo: String,
    pub number: i64,
    pub title: String,
}

impl From<&IssueTriage> for Subject {
    fn from(t: &IssueTriage) -> Self {
        Subject {
            key: t.issue_key.clone(),
            repo: t.repo.clone(),
            number: t.number,
            title: t.title.clone(),
        }
    }
}

/// What the discussion on a PR amounts to: what people said, and what they pointed at.
///
/// Two fields rather than one blob because they are different kinds of evidence and the judge
/// is told to weigh them differently — a reviewer's objection is an opinion about this diff, a
/// commit message followed out of the discussion is a fact about another one.
#[derive(Debug, Default)]
struct Discussion {
    /// The meritorious reviews and comments, rendered.
    reviews: String,
    /// Commits the discussion said the fix lives in, read out of the code index. `None` when
    /// nobody pointed anywhere.
    referenced: Option<String>,
}

/// Open PRs pulled per repo before shortlisting.
const PR_SCAN_LIMIT: usize = 50;
/// Candidates that get the full read-the-diff analysis. Each is several model
/// calls, so this is the cost ceiling per issue.
const MAX_ANALYZED: usize = 3;
/// Files of a PR's diff shown to the model.
const MAX_PR_FILES: usize = 10;
/// Characters of patch kept per file.
const MAX_PATCH_CHARS: usize = 2_000;
/// Characters of judged review/discussion text folded into a critique.
const REVIEW_CHARS: usize = 4_000;

pub struct PrFixFinder {
    store: Arc<Store>,
    /// Local coder model — reads diffs. The only model that judges a PR.
    coder: Arc<dyn Reasoner>,
    /// A second attempt when the first returns nothing parseable, at a higher
    /// temperature. Same model: this used to be an escalation ladder to a cloud tier, and
    /// what it was actually recovering from was a model that had answered in prose, which
    /// a retry fixes as well as a metered call did.
    retry: Arc<dyn Reasoner>,
    /// Scores the PR's reviews so the critique accounts for what reviewers said.
    comments: CommentJudge,
    /// Used to lump issues that one PR resolves into a single subject.
    analyst: Option<Arc<Analyst>>,
}

impl PrFixFinder {
    pub fn new(store: Arc<Store>, coder: Arc<dyn Reasoner>, retry: Arc<dyn Reasoner>) -> Self {
        Self {
            store,
            comments: CommentJudge::new(coder.clone()),
            coder,
            retry,
            analyst: None,
        }
    }

    /// Attach the analyst so a PR that fixes several issues can collapse them onto
    /// one subject. Optional because the investigator constructs a finder before the
    /// analyst exists.
    pub fn with_analyst(mut self, analyst: Arc<Analyst>) -> Self {
        self.analyst = Some(analyst);
        self
    }

    /// Collapse the issues one PR fixes into a single subject.
    ///
    /// If a patch resolves #1204, #1178 and #660, those are one piece of work with
    /// three issue numbers. Leaving them as three cards means reading the same
    /// analysis three times and closing three things by hand — so they're merged
    /// once the PR has been judged to fix more than one.
    ///
    /// Only a `fixes` verdict lumps. `partial` or `related` means the issues are
    /// connected but not the same work, and merging those would hide a real
    /// distinction.
    async fn lump_issues_fixed_together(&self, fix: &PrFix) {
        if fix.verdict != "fixes" {
            return;
        }
        let Some(analyst) = &self.analyst else { return };
        let issues = match self.store.issues_fixed_by_pr(&fix.pr_repo, fix.pr_number) {
            Ok(issues) if issues.len() > 1 => issues,
            Ok(_) => return,
            Err(e) => {
                debug!(
                    "pr-fix: looking up issues fixed by {}: {e:#}",
                    fix.reference()
                );
                return;
            }
        };
        // Subject ids for those issues, deduplicated.
        let mut subjects: Vec<String> = Vec::new();
        for key in &issues {
            if let Ok(Some(tid)) = self.store.subject_for_issue(key) {
                if !subjects.contains(&tid.to_string()) {
                    subjects.push(tid);
                }
            }
        }
        if subjects.len() < 2 {
            return;
        }
        let (keep, rest) = subjects.split_first().expect("checked len >= 2");
        for other in rest {
            match analyst.relate(keep, other, RelationKind::Same).await {
                Ok(_) => debug!(
                    "pr-fix: lumped {other} into {keep} — {} fixes {} issues",
                    fix.reference(),
                    issues.len()
                ),
                Err(e) => debug!("pr-fix: lumping {other} into {keep} failed: {e:#}"),
            }
        }
    }

    /// Find and analyze open PRs that might fix `issue`. Persists what it finds and
    /// returns the candidates it kept.
    ///
    /// `fresh` forces model calls past the completion cache (a deliberate re-run).
    pub async fn find(
        &self,
        gh: &GithubClient,
        issue: &Subject,
        issue_body: &str,
        other_open_issues: &[String],
        fresh: bool,
    ) -> Result<Vec<PrFix>> {
        let pulls = gh.open_pulls(&issue.repo, PR_SCAN_LIMIT).await?;
        if pulls.is_empty() {
            debug!("pr-fix: no open PRs in {}", issue.repo);
            return Ok(Vec::new());
        }
        debug!(
            "pr-fix: {} open PR(s) in {} to consider for #{}",
            pulls.len(),
            issue.repo,
            issue.number
        );

        // Shortlist first — reading every diff would cost a model call per PR.
        let shortlist = self
            .shortlist(issue, issue_body, &pulls, fresh)
            .await
            .into_iter()
            .take(MAX_ANALYZED)
            .collect::<Vec<_>>();
        if shortlist.is_empty() {
            return Ok(Vec::new());
        }

        let mut kept = Vec::new();
        for pr in shortlist {
            let files = match gh
                .pull_files(&issue.repo, pr.number, MAX_PR_FILES, MAX_PATCH_CHARS)
                .await
            {
                Ok(files) => files,
                Err(e) => {
                    debug!(
                        "pr-fix: diff for {}#{} failed: {e:#}",
                        issue.repo, pr.number
                    );
                    Vec::new()
                }
            };
            let discussion = self.discussion(gh, issue, &pr, fresh).await;
            match self
                .analyze(
                    issue,
                    issue_body,
                    &pr,
                    &files,
                    &discussion,
                    other_open_issues,
                    fresh,
                )
                .await
            {
                Some(fix) => {
                    // An `unrelated` verdict is the shortlist being wrong, which is
                    // fine — it just isn't worth showing.
                    if fix.verdict != "unrelated" {
                        if let Err(e) = self.store.put_pr_fix(&fix) {
                            warn!("pr-fix: storing {} failed: {e:#}", fix.reference());
                        }
                        // Stored first: lumping reads back every issue this PR fixes,
                        // including this one.
                        self.lump_issues_fixed_together(&fix).await;
                        kept.push(fix);
                    }
                }
                None => debug!(
                    "pr-fix: no usable analysis for {}#{}",
                    issue.repo, pr.number
                ),
            }
        }
        Ok(kept)
    }

    /// Judge one specific pull request against a subject, by number.
    ///
    /// The association path: correlation has already tied this PR to the issue (via
    /// a branch, a CI run, or a closing keyword), so there is nothing to shortlist —
    /// the question is only whether the PR actually fixes it. Fetches the diff and
    /// runs the same implementation → critique → also-fixes read.
    #[allow(clippy::too_many_arguments)]
    pub async fn judge_known_pr(
        &self,
        gh: &GithubClient,
        subject: &Subject,
        pr_repo: &str,
        pr_number: u64,
        subject_body: &str,
        other_open_issues: &[String],
        fresh: bool,
    ) -> Option<PrFix> {
        let pulls = gh.open_pulls(pr_repo, PR_SCAN_LIMIT).await.ok()?;
        let pr = pulls.into_iter().find(|p| p.number == pr_number)?;
        let files = gh
            .pull_files(pr_repo, pr_number, MAX_PR_FILES, MAX_PATCH_CHARS)
            .await
            .unwrap_or_default();
        let discussion = self.discussion(gh, subject, &pr, fresh).await;
        let fix = self
            .analyze(
                subject,
                subject_body,
                &pr,
                &files,
                &discussion,
                other_open_issues,
                fresh,
            )
            .await?;
        if let Err(e) = self.store.put_pr_fix(&fix) {
            warn!("pr-fix: storing {} failed: {e:#}", fix.reference());
        }
        self.lump_issues_fixed_together(&fix).await;
        Some(fix)
    }

    /// The PR's reviews and inline comments, scored on merit — plus whatever they point at.
    ///
    /// This is the highest-value input to a critique: a reviewer who read the change
    /// and requested changes has already found what's wrong with it, and a model
    /// second-guessing that is worse than a model deferring to it.
    ///
    /// And when a reviewer says the fix is somewhere else — another commit, or a PR in another
    /// repo — that reference is followed into the code index, because judging this diff on its
    /// own against an issue that has already been fixed elsewhere is the confident wrong answer
    /// this pass exists to avoid.
    async fn discussion(
        &self,
        gh: &GithubClient,
        subject: &Subject,
        pr: &PullRequest,
        fresh: bool,
    ) -> Discussion {
        let mut all = gh
            .pull_reviews(&pr.repo, pr.number)
            .await
            .unwrap_or_default();
        // A PR's conversation lives on the issues endpoint; its reviews on the pulls
        // one. Both are part of "what people said about this change".
        if let Ok(discussion) = gh.issue_comments(&pr.repo, pr.number).await {
            all.extend(discussion);
        }
        // The description is scanned for references alongside the comments: "the real fix is in
        // X" is as often written by the author as by a reviewer.
        let mut pointing = pr.body.clone().unwrap_or_default();

        let reviews = if all.is_empty() {
            "(no reviews or comments)".to_string()
        } else {
            let total = all.len();
            let context = format!(
                "PR #{}: {} — judged against {}",
                pr.number, pr.title, subject.title
            );
            let judged = self
                .comments
                .select(&all, &context, REVIEW_CHARS, fresh)
                .await;
            debug!(
                "pr-fix: {}#{}: {} of {total} review comment(s) judged substantive",
                pr.repo,
                pr.number,
                judged.len()
            );
            // Only the meritorious comments are followed. A "+1" that happens to contain a sha
            // is not someone telling you where the fix is.
            for j in &judged {
                pointing.push('\n');
                pointing.push_str(&j.comment.body);
            }
            crate::comments::render(&judged, total)
        };

        Discussion {
            reviews,
            referenced: self.follow_references(subject, pr, &pointing),
        }
    }

    /// Resolve the fix-is-elsewhere references in some text against the code index.
    fn follow_references(
        &self,
        subject: &Subject,
        pr: &PullRequest,
        text: &str,
    ) -> Option<String> {
        // This conversation's own numbers: a PR saying "fixes #412" about the issue under
        // judgment is the ordinary case, not a claim that the work happened elsewhere.
        let mut skip = vec![pr.number];
        if subject.repo == pr.repo {
            skip.push(subject.number as u64);
        }
        let refs = crossref::extract(text, &pr.repo, &skip);
        if refs.is_empty() {
            return None;
        }
        let resolved = crossref::resolve(&self.store, &refs);
        debug!(
            "pr-fix: {}#{}: following {} fix-is-elsewhere reference(s); {} found in the index",
            pr.repo,
            pr.number,
            resolved.len(),
            resolved.iter().filter(|r| r.commit.is_some()).count(),
        );
        crossref::render(&resolved)
    }

    /// Narrow open PRs to the ones plausibly about this issue, on the local model.
    ///
    /// Falls back to a deterministic scan — an explicit issue reference in the PR
    /// body, or overlapping identifiers — so the pass still works with no model.
    async fn shortlist(
        &self,
        issue: &Subject,
        issue_body: &str,
        pulls: &[PullRequest],
        fresh: bool,
    ) -> Vec<PullRequest> {
        // A PR that literally says "fixes #412" is a certainty, not a candidate;
        // take those first and never let a model's opinion drop them.
        let explicit: Vec<PullRequest> = pulls
            .iter()
            .filter(|p| mentions_issue(p, issue.number))
            .cloned()
            .collect();

        let mut catalog = String::new();
        for (i, p) in pulls.iter().enumerate() {
            catalog.push_str(&format!(
                "{i}. #{} {}{} — by {}{}\n",
                p.number,
                p.title,
                if p.draft { " [draft]" } else { "" },
                p.author.as_deref().unwrap_or("unknown"),
                p.head_ref
                    .as_deref()
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default(),
            ));
        }
        let system = "You are checking whether an issue is already being fixed. Given an issue and a \
             numbered list of that repository's OPEN pull requests, reply with ONLY a JSON array of \
             the indices that could plausibly be addressing this issue — most likely first. Judge by \
             whether the PR appears to touch the same component or behavior. Return [] if none \
             plausibly relate. Be selective: a wrong guess costs a wasted deep read.";
        let prompt = format!(
            "Issue #{} in {}: {}\n\n{}\n\nOpen pull requests:\n{catalog}",
            issue.number,
            issue.repo,
            issue.title,
            truncate(issue_body, 1_500),
        );
        let mut req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(200);
        req.no_cache = fresh;

        let picked: Vec<usize> = match self.coder.complete(&req).await {
            Ok(raw) => reasoner::extract_json(&raw)
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|n| n.as_u64())
                            .map(|n| n as usize)
                            .filter(|n| *n < pulls.len())
                            .collect()
                    })
                })
                .unwrap_or_default(),
            Err(e) => {
                debug!("pr-fix: local shortlisting failed: {e:#}");
                Vec::new()
            }
        };

        // Explicit references first, then the model's picks, deduplicated.
        let mut out = explicit;
        for i in picked {
            let pr = &pulls[i];
            if !out.iter().any(|p| p.number == pr.number) {
                out.push(pr.clone());
            }
        }
        if out.is_empty() {
            // No model, no explicit reference: fall back to identifier overlap.
            let terms = crate::triage::identifiers(&format!("{} {}", issue.title, issue_body));
            out = pulls
                .iter()
                .filter(|p| {
                    let text = format!("{} {}", p.title, p.body.as_deref().unwrap_or(""))
                        .to_ascii_lowercase();
                    terms.iter().filter(|t| text.contains(t.as_str())).count() >= 2
                })
                .cloned()
                .collect();
        }
        out
    }

    /// The three-part read of one candidate PR: implementation, critique, and what
    /// else it resolves. Tries local first, then escalates on failure.
    #[allow(clippy::too_many_arguments)]
    async fn analyze(
        &self,
        issue: &Subject,
        issue_body: &str,
        pr: &PullRequest,
        files: &[PullFile],
        discussion: &Discussion,
        other_open_issues: &[String],
        fresh: bool,
    ) -> Option<PrFix> {
        let diff = render_diff(files);
        let others = if other_open_issues.is_empty() {
            "(none provided)".to_string()
        } else {
            other_open_issues.join("\n")
        };
        let system = "You are reviewing whether an open pull request already fixes an issue, for an \
             engineer deciding whether to start work. Read the DIFF, not the description — a PR \
             description states intent, the patch states behavior. Reply with ONLY JSON:\n\
             {\"verdict\":\"fixes|partial|related|unrelated\", \"confidence\":0.0-1.0, \
             \"implementation\":\"<2-4 sentences: what the patch actually changes, mechanically>\", \
             \"critique\":\"<2-4 sentences: does this genuinely fix the issue? what does it miss, \
             get wrong, or leave untested? be specific and skeptical — a description claiming to \
             close the issue is a claim, not a verification. If a reviewer has already raised \
             something, say so and defer to it: a human who read this change and objected is better \
             evidence than your own reading>\", \
             \"conversation\":\"<1-3 sentences summarizing what REVIEWERS said, or \\\"\\\" if \
             there is no discussion. Report their positions, not your own: who is blocking on what, \
             what was asked and answered, what is still open. If a reviewer objected, lead with the \
             objection>\", \
             \"also_fixes\":[{\"reference\":\"<verbatim entry from the other-issues list>\", \
             \"why\":\"<one sentence naming the specific change in THIS diff that resolves that \
             other issue>\"}]}\n\
             Rules: `fixes` only if the diff plainly resolves the issue's mechanism; `partial` if it \
             addresses part of it or papers over it; `related` if it touches the same code without \
             fixing it; `unrelated` otherwise. Do not invent file names or behavior not in the diff.\n\
             If a WORK REFERENCED ELSEWHERE section is present, the discussion claimed the fix lives \
             in another commit or another repository, and those commits were read out of the code \
             index for you. Weigh them: a fix that already landed elsewhere is not delivered by THIS \
             diff, so say so in the critique, name the commit, and judge this PR on what it changes \
             rather than on the issue being resolved — `fixes` requires the resolving change to be in \
             the diff above. A reference the index could not resolve means work that may exist and \
             cannot be read: say that it is unverified rather than assuming either way.\n\
             `also_fixes` is almost always EMPTY — most patches fix exactly one thing. Include an \
             entry only if you can point to the specific hunk that resolves that other issue too. \
             Never copy the other-issues list back; unrelated work in the same repository is not \
             \"also fixed\" just because it is listed.";
        // Only present when the discussion actually pointed somewhere. An empty section under a
        // header reads as "nothing was referenced", which is a claim we would be making up.
        let referenced = discussion
            .referenced
            .as_deref()
            .map(|r| {
                format!(
                    "=== WORK REFERENCED ELSEWHERE (followed out of the discussion, read from \
                     the code index) ===\n{r}\n\n"
                )
            })
            .unwrap_or_default();
        let reviews = &discussion.reviews;
        let prompt = format!(
            "=== ISSUE #{} in {} ===\n{}\n\n{}\n\n\
             === CANDIDATE PULL REQUEST #{} ===\nTitle: {}\nAuthor: {}\nState: {}{}\n\
             Description: {}\n\n=== DIFF ===\n{diff}\n\n\
             === REVIEWS AND DISCUSSION ON THIS PR ===\n{reviews}\n\n\
             {referenced}\
             === OTHER OPEN ISSUES (for also_fixes) ===\n{others}\n\n\
             === YOUR TASK ===\nJudge PR #{} against issue #{}: what it implements, whether it \
             really fixes the issue, and what else it resolves. Reply with the JSON object only.",
            issue.number,
            issue.repo,
            issue.title,
            truncate(issue_body, 1_500),
            pr.number,
            pr.title,
            pr.author.as_deref().unwrap_or("unknown"),
            pr.state,
            if pr.draft { " (draft)" } else { "" },
            truncate(pr.body.as_deref().unwrap_or("(none)"), 800),
            pr.number,
            issue.number,
        );

        // One local attempt, then one retry — both on the local model. What the old
        // three-rung cloud ladder recovered from was an answer in prose rather than JSON,
        // and a second ask fixes that.
        for (tier, reasoner) in [("local", &self.coder), ("local-retry", &self.retry)] {
            let mut req = CompletionRequest::single(prompt.clone())
                .with_system(system)
                .max_tokens(900);
            req.no_cache = fresh;
            let raw = match reasoner.complete(&req).await {
                Ok(raw) => raw,
                Err(e) => {
                    debug!(
                        "pr-fix: {tier} failed on {}#{}: {e:#}",
                        issue.repo, pr.number
                    );
                    continue;
                }
            };
            if let Some(fix) = self.parse(issue, pr, files, other_open_issues, tier, &raw) {
                if tier != "local" {
                    debug!(
                        "pr-fix: {}#{} analyzed by {tier} after local came up short",
                        issue.repo, pr.number
                    );
                }
                return Some(fix);
            }
            debug!(
                "pr-fix: {tier} returned unusable JSON for {}#{}",
                issue.repo, pr.number
            );
        }
        None
    }

    /// Shape a model response into a [`PrFix`], or `None` if it isn't usable.
    fn parse(
        &self,
        issue: &Subject,
        pr: &PullRequest,
        files: &[PullFile],
        other_open_issues: &[String],
        tier: &str,
        raw: &str,
    ) -> Option<PrFix> {
        let v = reasoner::extract_json(raw)?;
        let verdict = v
            .get("verdict")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| matches!(s.as_str(), "fixes" | "partial" | "related" | "unrelated"))?;
        let implementation = v
            .get("implementation")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // An analysis with no reasoning behind it is not an analysis. Requiring the
        // critique is what stops a bare "verdict: fixes" from reaching the board.
        let critique = v
            .get("critique")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        // What reviewers actually said. Distinct from the critique, which is
        // MuggleBot's own reading of the diff — and deliberately shown separately on
        // the board, because "a reviewer already objected to this" is much better
        // evidence than a model's opinion of the same change.
        let conversation = v
            .get("conversation")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let also_fixes = parse_also_fixes(&v, other_open_issues);
        let now = Utc::now().to_rfc3339();
        Some(PrFix {
            issue_key: issue.key.clone(),
            pr_repo: pr.repo.clone(),
            pr_number: pr.number as i64,
            pr_title: pr.title.clone(),
            pr_url: Some(pr.url.clone()),
            pr_author: pr.author.clone(),
            // `merged` is its own state here, not GitHub's `closed`: a change that shipped
            // and a change that was abandoned read as the same string upstream.
            pr_state: Some(if pr.draft {
                "draft".to_string()
            } else if pr.merged_at.is_some() {
                "merged".to_string()
            } else {
                pr.state.clone()
            }),
            files: files.iter().map(|f| f.path.clone()).collect(),
            verdict,
            confidence: v
                .get("confidence")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0),
            implementation,
            critique: Some(critique),
            conversation,
            also_fixes,
            analyzed_by: Some(tier.to_string()),
            created_at: now.clone(),
            updated_at: now,
        })
    }
}

// ---- helpers -----------------------------------------------------------------

/// Entries kept in `also_fixes`. A patch fixing more than a couple of unrelated
/// issues is implausible on its face.
const MAX_ALSO_FIXES: usize = 3;
/// A justification shorter than this is a restatement, not a reason.
const MIN_WHY_CHARS: usize = 20;

/// Extract `also_fixes`, rejecting the two ways a model gets this wrong.
///
/// **Invention** — an issue reference that was never offered — is filtered out
/// per entry. **Echoing** is the subtler failure and the one seen in practice: asked
/// which other issues a patch also resolves, a model hands the whole candidate list
/// back. Requiring a per-entry justification that names the responsible hunk raises
/// the cost of echoing, and discarding the field outright when it claims most of the
/// offered list catches what survives that. Both fail closed: `also_fixes` is a
/// bonus claim, and a wrong one sends someone to close an issue that isn't fixed.
fn parse_also_fixes(v: &serde_json::Value, offered: &[String]) -> Vec<String> {
    let Some(items) = v.get("also_fixes").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        // Accept both the object form and a bare string, since models drift.
        let (reference, why) = match item {
            serde_json::Value::Object(_) => (
                item.get("reference").and_then(|r| r.as_str()).unwrap_or(""),
                item.get("why").and_then(|w| w.as_str()).unwrap_or(""),
            ),
            serde_json::Value::String(s) => (s.as_str(), ""),
            _ => continue,
        };
        let reference = reference.trim();
        let why = why.trim();
        // Must name an issue we actually offered…
        if reference.is_empty() || !offered.iter().any(|o| o.contains(reference)) {
            continue;
        }
        // …and must say why, specifically.
        if why.chars().count() < MIN_WHY_CHARS {
            continue;
        }
        if !out.iter().any(|e: &String| e.starts_with(reference)) {
            out.push(format!("{reference} — {why}"));
        }
    }
    // Echo guard: claiming most of the candidate list is the list being parroted
    // back, not a patch with unusual reach.
    if !offered.is_empty() && out.len() * 2 > offered.len() {
        debug!(
            "pr-fix: discarding also_fixes — {} of {} offered issues claimed, which reads as the \
             candidate list echoed back",
            out.len(),
            offered.len()
        );
        return Vec::new();
    }
    out.truncate(MAX_ALSO_FIXES);
    out
}

/// Does this PR explicitly reference the issue? Matches the closing keywords
/// GitHub itself recognizes, plus a bare `#N`.
fn mentions_issue(pr: &PullRequest, number: i64) -> bool {
    let needle = format!("#{number}");
    let haystack = format!(
        "{} {} {}",
        pr.title,
        pr.body.as_deref().unwrap_or(""),
        pr.head_ref.as_deref().unwrap_or("")
    );
    // Guard against `#41` matching inside `#412`.
    haystack.match_indices(&needle).any(|(i, _)| {
        haystack[i + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit())
    })
}

/// Lay a PR's diff out for the model.
fn render_diff(files: &[PullFile]) -> String {
    if files.is_empty() {
        return "(the diff could not be read; judge from the description alone and say so)".into();
    }
    let mut out = String::new();
    for f in files {
        out.push_str(&format!(
            "\n--- {} (+{} -{}) ---\n",
            f.path, f.additions, f.deletions
        ));
        match f.patch.as_deref() {
            Some(patch) => out.push_str(patch),
            None => out.push_str("(binary or too large to show)"),
        }
        out.push('\n');
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, title: &str, body: &str) -> PullRequest {
        PullRequest {
            repo: "restatedev/restate".into(),
            number,
            title: title.into(),
            url: format!("https://github.com/restatedev/restate/pull/{number}"),
            author: Some("octocat".into()),
            state: "open".into(),
            draft: false,
            body: Some(body.into()),
            labels: vec![],
            head_ref: Some("octocat:fix-pool".into()),
            updated_at: None,
            merged_at: None,
        }
    }

    #[test]
    fn closing_keywords_and_bare_references_are_detected() {
        assert!(mentions_issue(&pr(1, "Fix pool", "Fixes #412"), 412));
        assert!(mentions_issue(&pr(1, "Closes #412", ""), 412));
        assert!(mentions_issue(
            &pr(1, "Fix", "related to #412 somehow"),
            412
        ));
    }

    /// `#41` must not match inside `#412`, or an unrelated PR looks like a certain
    /// fix and gets promoted past the model's judgment.
    #[test]
    fn a_shorter_number_does_not_match_a_longer_one() {
        assert!(!mentions_issue(&pr(1, "Fix", "Fixes #412"), 41));
        assert!(!mentions_issue(&pr(1, "Fix", "Fixes #4120"), 412));
        assert!(mentions_issue(&pr(1, "Fix", "Fixes #412."), 412));
    }

    #[test]
    fn unrelated_prs_are_not_flagged() {
        assert!(!mentions_issue(&pr(1, "Bump serde", "Routine update"), 412));
    }

    fn offered(n: usize) -> Vec<String> {
        (1..=n)
            .map(|i| format!("restatedev/restate#{i} — some issue"))
            .collect()
    }

    #[test]
    fn also_fixes_requires_a_real_justification() {
        let offered = offered(6);
        let v = serde_json::json!({ "also_fixes": [
            { "reference": "restatedev/restate#1", "why": "the same pool-bounding hunk covers it" },
            { "reference": "restatedev/restate#2", "why": "yes" },
        ]});
        let kept = parse_also_fixes(&v, &offered);
        assert_eq!(kept.len(), 1, "a two-word 'why' is not a reason: {kept:?}");
        assert!(kept[0].starts_with("restatedev/restate#1"));
        assert!(
            kept[0].contains("pool-bounding"),
            "the reason is carried through"
        );
    }

    /// The failure seen in practice: asked what else a patch fixes, the model hands
    /// back the whole candidate list.
    #[test]
    fn echoing_the_candidate_list_is_discarded() {
        let offered = offered(5);
        let items: Vec<_> = (1..=5)
            .map(|i| {
                serde_json::json!({
                    "reference": format!("restatedev/restate#{i}"),
                    "why": "this patch also resolves that issue somehow",
                })
            })
            .collect();
        let v = serde_json::json!({ "also_fixes": items });
        assert!(
            parse_also_fixes(&v, &offered).is_empty(),
            "claiming most of the offered list must be rejected wholesale"
        );
    }

    #[test]
    fn a_plausible_single_extra_fix_survives() {
        let offered = offered(6);
        let v = serde_json::json!({ "also_fixes": [
            { "reference": "restatedev/restate#3",
              "why": "the retry cap added in workos-api.ts covers that report too" },
        ]});
        assert_eq!(parse_also_fixes(&v, &offered).len(), 1);
    }

    #[test]
    fn invented_issue_references_are_dropped() {
        let v = serde_json::json!({ "also_fixes": [
            { "reference": "restatedev/restate#9999",
              "why": "a confident sentence about an issue nobody mentioned" },
        ]});
        assert!(parse_also_fixes(&v, &offered(6)).is_empty());
    }

    #[test]
    fn a_missing_also_fixes_field_is_simply_empty() {
        assert!(parse_also_fixes(&serde_json::json!({}), &offered(6)).is_empty());
    }

    #[test]
    fn a_missing_diff_is_stated_rather_than_hidden() {
        let rendered = render_diff(&[]);
        assert!(rendered.contains("could not be read"));
        assert!(rendered.contains("say so"), "the model must disclose it");
    }

    #[test]
    fn diff_rendering_labels_files_with_their_churn() {
        let out = render_diff(&[PullFile {
            path: "src/pool.rs".into(),
            additions: 12,
            deletions: 3,
            patch: Some("@@ -1 +1 @@\n-old\n+new".into()),
        }]);
        assert!(out.contains("--- src/pool.rs (+12 -3) ---"));
        assert!(out.contains("+new"));
    }

    #[test]
    fn binary_files_are_marked_not_dropped() {
        let out = render_diff(&[PullFile {
            path: "logo.png".into(),
            additions: 0,
            deletions: 0,
            patch: None,
        }]);
        assert!(out.contains("logo.png"));
        assert!(out.contains("binary"));
    }
}
