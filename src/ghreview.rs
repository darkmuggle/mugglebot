//! Reviewing a GitHub **branch** — a generic model read, plus personas, inline on the diff.
//!
//! The existing review path is keyed on a pull request: a PR arrives as a notification, gets a
//! subject, and one review is attached to it. This is the other direction. You point at a
//! branch — often before there is a PR, which is when a review is worth most — and ask several
//! reviewers at once.
//!
//! # Three dots, not two
//!
//! A branch is compared `base...head`, against the **merge base**. Two dots would fold in
//! everything that landed on the base branch since this one left it, presenting other people's
//! commits as part of the change under review. On a busy repo that is most of the diff.
//!
//! # One comment type, two very different producers
//!
//! A generic review and a persona review are not the same claim. The first says *"this is
//! wrong"*; the second says *"Pavel would say this is wrong"*, which is a prediction about a
//! colleague and has to stay labelled as one everywhere — the invariant
//! [`crate::persona`] already holds.
//!
//! They are nevertheless rendered through one [`Placed`] type, because the alternative is two
//! inline-comment renderers that drift apart, and the *placement* problem is identical. What
//! keeps them distinct is [`Placed::by`], carried into every surface.
//!
//! # Placement reuses the matcher that already works
//!
//! Both producers are asked to quote the line they are talking about, and both are resolved by
//! [`crate::prdiff::anchor_index`] — the tested matcher, including the moved-line case where a
//! line exists twice in one patch and only its `+`/`-` marker says which copy was meant.
//!
//! A persona's `PredictedPoint::line` is *already* a copied line rather than a number, so it
//! needs no new anchoring rule; it needed a name that says so, and the field's own doc comment
//! says the same. A point that resolves to nothing becomes a file-level comment rather than
//! being attached to whatever line happened to match, which is what makes an unresolvable
//! anchor visible instead of wrong.
//!
//! # Dismissal is a flag, never a delete
//!
//! Dismissing a pass hides it from the diff and keeps the row. Two reasons. A dismissed review
//! is evidence about the reviewer — "the local model said this and I disagreed" is worth
//! being able to look at later — and re-running a pass you dismissed should be a decision
//! rather than something that quietly happens because the record of your dismissal was thrown
//! away.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::prdiff::{DiffReport, Severity};

// ---- pointing at a branch ----------------------------------------------------

/// Which branch, and what to compare it against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRef {
    /// `owner/repo`.
    pub repo: String,
    pub head: String,
    /// The base to compare against. `None` until resolved, then the repo's default branch —
    /// or whatever the operator named.
    pub base: Option<String>,
}

impl BranchRef {
    /// The stable key for this branch's reviews.
    pub fn key(&self) -> String {
        format!("{}@{}", self.repo, self.head)
    }

    /// The pull request number, when the operator pasted a PR link rather than a branch.
    ///
    /// `head` holds `#1334` until [`Runner::resolve`] swaps in the real branch — so the key,
    /// the passes and the diff cache all end up under the branch name, and pasting the PR link
    /// and the branch link resolve to the *same* review rather than two.
    pub fn pull_number(&self) -> Option<u64> {
        self.head.strip_prefix('#')?.parse().ok()
    }
}

/// Read whatever the operator pasted.
///
/// Five shapes, because all five are things people actually have in their clipboard:
///
/// - `owner/repo@branch` and `owner/repo:branch` — typed by hand
/// - `https://github.com/owner/repo/tree/branch` — GitHub's branch page
/// - `https://github.com/owner/repo/compare/base...head` — GitHub's compare view, the only
///   shape that names its own base
/// - **`https://github.com/owner/repo/pull/1334`** — a pull request. The feature is about
///   branches, but a PR link is by far the likeliest thing to be pasted into a box that says
///   "point at any branch", and refusing it with a lecture about branches would be a parser
///   being right at the operator's expense. The number is kept and the branch resolved from
///   it later; see [`PullRequest`].
///
/// A branch name can contain `/`, so everything after the marker is the branch. Splitting on
/// the last slash would turn `bh/otel1` into `otel1` and 404.
pub fn parse_branch(raw: &str) -> Result<BranchRef> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        bail!("paste a branch — `owner/repo@branch`, or a GitHub branch or compare URL");
    }

    // A pull request URL: read the number, resolve the branch from it at request time.
    if let Some(rest) = raw
        .split("/pull/")
        .nth(1)
        .or_else(|| raw.split("/pulls/").nth(1))
    {
        let repo = repo_from_url(raw).context("that pull request URL has no owner/repo in it")?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let number: u64 = digits
            .parse()
            .with_context(|| format!("`{rest}` has no pull request number in it"))?;
        return Ok(BranchRef {
            repo,
            // A placeholder the resolver replaces. Marked rather than left blank so a branch
            // that never resolved cannot be mistaken for one called "".
            head: format!("#{number}"),
            base: None,
        });
    }

    // A compare URL is the only shape that names a base, so it is read first.
    if let Some(rest) = raw.split("/compare/").nth(1) {
        let repo = repo_from_url(raw).context("that compare URL has no owner/repo in it")?;
        let spec = rest.split(['?', '#']).next().unwrap_or(rest);
        let (base, head) = spec
            .split_once("...")
            .or_else(|| spec.split_once(".."))
            .with_context(|| format!("`{spec}` is not a `base...head` comparison"))?;
        return Ok(BranchRef {
            repo,
            head: strip_owner(head).to_string(),
            base: Some(strip_owner(base).to_string()).filter(|b| !b.is_empty()),
        });
    }
    if let Some(rest) = raw.split("/tree/").nth(1) {
        let repo = repo_from_url(raw).context("that branch URL has no owner/repo in it")?;
        let head = rest.split(['?', '#']).next().unwrap_or(rest);
        return Ok(BranchRef {
            repo,
            head: head.to_string(),
            base: None,
        });
    }

    // `owner/repo@branch` / `owner/repo:branch`. Split on the *first* marker: a branch may
    // contain neither, and a repo contains exactly one `/`.
    let (repo, head) = raw
        .split_once('@')
        .or_else(|| raw.split_once(':'))
        .with_context(|| {
            format!(
                "`{}` names no branch — write `owner/repo@branch`, or paste a GitHub branch \
                 or compare URL",
                raw.chars().take(60).collect::<String>()
            )
        })?;
    let repo = repo.trim().trim_matches('/');
    if repo.split('/').filter(|s| !s.is_empty()).count() != 2 {
        bail!("`{repo}` is not an `owner/repo`");
    }
    let head = head.trim();
    if head.is_empty() {
        bail!("no branch after the `@`");
    }
    Ok(BranchRef {
        repo: repo.to_string(),
        head: head.to_string(),
        base: None,
    })
}

/// `owner/repo` out of a github.com URL.
fn repo_from_url(url: &str) -> Option<String> {
    let after = url.split("github.com/").nth(1)?;
    let mut parts = after.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

/// GitHub writes a cross-fork ref as `owner:branch` in a compare URL. The owner is not part
/// of the branch name.
fn strip_owner(spec: &str) -> &str {
    spec.split_once(':').map_or(spec, |(_, b)| b)
}

// ---- one reviewer's pass -----------------------------------------------------

/// Who produced a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reviewer {
    /// The model's own read of the code.
    Model,
    /// What this person is predicted to say. **A rehearsal**, and labelled as one wherever it
    /// is rendered — see [`crate::persona`].
    Persona(String),
}

impl Reviewer {
    pub fn as_key(&self) -> String {
        match self {
            Reviewer::Model => "model".into(),
            Reviewer::Persona(slug) => format!("persona:{slug}"),
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.strip_prefix("persona:") {
            Some(slug) if !slug.is_empty() => Reviewer::Persona(slug.to_string()),
            _ => Reviewer::Model,
        }
    }

    pub fn persona(&self) -> Option<&str> {
        match self {
            Reviewer::Persona(slug) => Some(slug),
            Reviewer::Model => None,
        }
    }
}

/// One comment, placed on the diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placed {
    pub path: String,
    pub severity: Severity,
    pub note: String,
    /// The line the producer quoted, verbatim.
    pub anchor: Option<String>,
    /// Resolved index into that file's patch lines. `None` when neither the anchor nor a line
    /// number matched — the comment then renders against the file rather than a line, which
    /// is what stops an unresolvable anchor becoming a note on the wrong code.
    pub patch_index: Option<usize>,
    /// Which reviewer said it. Carried on every comment rather than inferred from its pass,
    /// because the diff interleaves them and a prediction about a colleague must never be
    /// rendered as a plain review.
    pub by: Reviewer,
    /// Persona trait ids the point rested on — the difference between "they would object to
    /// this" and "they object to this kind of thing, here is where". Empty for a model review.
    #[serde(default)]
    pub because: Vec<String>,
}

/// The status of one review pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl PassStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PassStatus::Pending => "pending",
            PassStatus::Running => "running",
            PassStatus::Done => "done",
            PassStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "running" => PassStatus::Running,
            "done" => PassStatus::Done,
            "failed" => PassStatus::Failed,
            _ => PassStatus::Pending,
        }
    }
}

/// One reviewer's pass over one branch at one head sha.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pass {
    pub id: String,
    /// `owner/repo@branch`.
    pub branch_key: String,
    pub repo: String,
    pub head: String,
    pub base: String,
    /// The sha reviewed. A pass is about a commit, not a branch name: the branch moves and the
    /// review does not follow it, so a pass whose sha is behind the branch is *stale* rather
    /// than wrong, and the UI can say which.
    pub head_sha: String,
    pub by: Reviewer,
    pub provider: String,
    pub model: String,
    pub status: PassStatus,
    /// `approve` / `comment` / `request_changes`, or `would not engage` for a persona that
    /// wouldn't look.
    pub recommendation: Option<String>,
    /// The note above the button.
    pub summary: Option<String>,
    pub comments: Vec<Placed>,
    /// Hidden from the diff, kept on the record. See the module docs.
    pub dismissed: bool,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Pass {
    /// The workflow key: one pass per reviewer per model per sha, so re-asking the same
    /// question about unchanged code is a refused invocation rather than another model call.
    pub fn version(&self) -> String {
        format!(
            "{}@{}@{}@{}/{}",
            self.repo,
            self.head_sha,
            self.by.as_key(),
            self.provider,
            self.model
        )
    }

    /// Is this pass behind the branch?
    pub fn stale_against(&self, head_sha: &str) -> bool {
        !head_sha.is_empty() && self.head_sha != head_sha
    }
}

/// What a reviewer was asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub by: Reviewer,
    pub provider: String,
    pub model: String,
}

/// Read the operator's chosen panel into one ask per reviewer.
///
/// `personas` empty means the model's own review only. Naming personas *adds* their passes; it
/// does not replace the model's, because the two answer different questions and the whole
/// point of the view is seeing them against each other.
pub fn asks(personas: &[String], provider: &str, model: &str, generic: bool) -> Vec<Ask> {
    let mut out = Vec::new();
    if generic {
        out.push(Ask {
            by: Reviewer::Model,
            provider: provider.to_string(),
            model: model.to_string(),
        });
    }
    for slug in personas {
        let slug = slug.trim();
        if slug.is_empty() {
            continue;
        }
        let ask = Ask {
            by: Reviewer::Persona(slug.to_string()),
            provider: provider.to_string(),
            model: model.to_string(),
        };
        if !out.contains(&ask) {
            out.push(ask);
        }
    }
    out
}

// ---- placing comments --------------------------------------------------------

/// Place a model review's comments onto the diff.
///
/// The comments arrive from [`crate::prdiff`] already anchored — this only re-stamps them with
/// their reviewer, so the diff can interleave producers without losing track of which said
/// what.
pub fn place_model(comments: Vec<crate::prdiff::ReviewComment>) -> Vec<Placed> {
    comments
        .into_iter()
        .map(|c| Placed {
            path: c.path,
            severity: c.severity,
            note: c.note,
            anchor: c.anchor,
            patch_index: c.patch_index,
            by: Reviewer::Model,
            because: Vec::new(),
        })
        .collect()
}

/// Place a persona prediction's points onto the diff.
///
/// `PredictedPoint::line` is a *copied line*, not a line number, so it goes in as the anchor
/// and through the same matcher a model review uses. A point naming no file cannot be placed
/// at all and is dropped: the persona's `summary` is where an unanchored opinion belongs, and
/// a file-less inline comment would have to be rendered somewhere arbitrary.
pub fn place_persona(
    slug: &str,
    points: Vec<crate::persona::PredictedPoint>,
    report: &DiffReport,
) -> Vec<Placed> {
    points
        .into_iter()
        .filter_map(|p| {
            let path = p
                .path
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())?;
            let patch = report
                .files
                .iter()
                .find(|f| f.path == path)
                .and_then(|f| f.patch.as_deref());
            let patch_index =
                patch.and_then(|patch| crate::prdiff::anchor_index(patch, p.line.as_deref(), None));
            Some(Placed {
                path,
                // A prediction is not a severity judgement — the persona is being asked what
                // they would say, not how bad it is. `Concern` rather than `Nit` because a
                // colleague bothering to comment is the signal, and rendering every predicted
                // point as a nit would flatten exactly the thing being predicted.
                severity: Severity::Concern,
                note: p.text,
                anchor: p.line,
                patch_index,
                by: Reviewer::Persona(slug.to_string()),
                because: p.because,
            })
        })
        .collect()
}

/// Comments to render, in file-then-position order, with dismissed passes excluded.
///
/// One place decides what the diff shows, so "dismissed" cannot mean one thing in the gutter
/// and another in the summary. Unplaced comments sort first within their file: a note about
/// the file as a whole belongs above the hunks, not after the last line it failed to match.
pub fn visible(passes: &[Pass]) -> Vec<Placed> {
    let mut out: Vec<Placed> = passes
        .iter()
        .filter(|p| !p.dismissed && p.status == PassStatus::Done)
        .flat_map(|p| p.comments.iter().cloned())
        .collect();
    out.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.patch_index.is_some().cmp(&b.patch_index.is_some()))
            .then(a.patch_index.cmp(&b.patch_index))
    });
    out
}

/// How many comments each reviewer contributed to the visible set — the header line.
pub fn tally(passes: &[Pass]) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for p in passes.iter().filter(|p| !p.dismissed) {
        let label = match &p.by {
            Reviewer::Model => format!("{}/{}", p.provider, p.model),
            Reviewer::Persona(slug) => slug.clone(),
        };
        match out.iter_mut().find(|(l, _)| *l == label) {
            Some((_, n)) => *n += p.comments.len(),
            None => out.push((label, p.comments.len())),
        }
    }
    out
}

// ---- the runner --------------------------------------------------------------

/// Runs the review passes for one branch.
pub struct Runner {
    store: std::sync::Arc<crate::store::Store>,
    diffs: std::sync::Arc<crate::prdiff::DiffReader>,
    personas: std::sync::Arc<crate::persona::Engine>,
    github: Option<crate::github::GithubClient>,
    factory: crate::restate::workflows::ReasonerFactory,
}

impl Runner {
    pub fn new(
        store: std::sync::Arc<crate::store::Store>,
        diffs: std::sync::Arc<crate::prdiff::DiffReader>,
        personas: std::sync::Arc<crate::persona::Engine>,
        token: Option<String>,
        factory: crate::restate::workflows::ReasonerFactory,
    ) -> Self {
        Self {
            store,
            diffs,
            personas,
            github: token.and_then(|t| crate::github::GithubClient::new(t).ok()),
            factory,
        }
    }

    pub fn ready(&self) -> bool {
        self.github.is_some()
    }

    /// An inert runner for test harnesses that need a complete [`crate::tools::Tools`].
    ///
    /// No token, so `ready()` is false and nothing can reach GitHub or a metered model by
    /// constructing one — the same reasoning as `Ingress::offline`.
    #[cfg(test)]
    pub fn for_tests(store: std::sync::Arc<crate::store::Store>) -> Self {
        let reasoner: std::sync::Arc<dyn crate::reasoner::Reasoner> =
            std::sync::Arc::new(crate::reasoner::MockReasoner::new("{}"));
        Self::new(
            store.clone(),
            std::sync::Arc::new(
                crate::prdiff::DiffReader::new(None, reasoner.clone(), "test").expect("reader"),
            ),
            std::sync::Arc::new(crate::persona::Engine::for_tests(store)),
            None,
            std::sync::Arc::new(move |_, _| reasoner.clone()),
        )
    }

    /// Resolve a pasted branch: its head sha, and the base to compare against.
    ///
    /// A base the operator named (from a compare URL) wins over the repo's default. Resolving
    /// here rather than inside the pass means a typo'd branch is an error on the button, and
    /// every pass in one panel is pinned to the *same* sha — otherwise two reviewers could
    /// end up reviewing different commits and their disagreement would be an artifact.
    pub async fn resolve(&self, r: &BranchRef) -> Result<(BranchRef, String, Option<u64>)> {
        let gh = self
            .github
            .as_ref()
            .context("reviewing a branch needs a stored `github` credential")?;

        // A pasted pull request link becomes its branch here, so the key, the passes and the
        // diff cache all live under the branch name — pasting the PR link and pasting the
        // branch link land on the same review rather than two rows about the same code.
        let mut r = r.clone();
        if let Some(number) = r.pull_number() {
            let (head, base) = gh.pull_branches(&r.repo, number).await?;
            r.head = head;
            r.base = r.base.or(Some(base));
        }

        let (head_sha, default_branch) = gh.branch_head(&r.repo, &r.head).await?;
        let base = r.base.clone().unwrap_or(default_branch);
        if base == r.head {
            bail!(
                "{} is the base branch — there is nothing to compare it against",
                r.head
            );
        }
        let pr = gh.pull_for_branch(&r.repo, &r.head).await.unwrap_or(None);
        let resolved = BranchRef {
            repo: r.repo,
            head: r.head,
            base: Some(base),
        };
        Ok((resolved, head_sha, pr))
    }

    /// Queue one pass per ask, all pinned to the same commit.
    /// Returns the queued passes, the **resolved** branch (a pasted pull request link has become
    /// its head branch by here), and the open PR on it if there is one.
    pub async fn request(
        &self,
        r: &BranchRef,
        asks: &[Ask],
    ) -> Result<(Vec<Pass>, BranchRef, Option<u64>)> {
        if asks.is_empty() {
            bail!("pick at least one reviewer — the model, a persona, or both");
        }
        let (r, head_sha, pr) = self.resolve(r).await?;
        let base = r.base.clone().unwrap_or_default();
        // Read the diff once for the whole panel, here, and cache it against the commit. Five
        // reviewers reading the same branch is one compare call, not five — and the view needs
        // the diff to render comments against long after the pass that fetched it was
        // dismissed, which is why it is cached by sha rather than held on a pass.
        let report = self.diffs.read_branch(&r.repo, &base, &r.head).await;
        if let Some(err) = &report.error {
            bail!("{err}");
        }
        self.store.set_branch_diff(&r.key(), &head_sha, &report)?;

        let mut out = Vec::new();
        for ask in asks {
            // A persona that has never been modelled cannot be predicted from, and a pass that
            // would fail on its first line is better refused here where the operator is looking.
            if let Some(slug) = ask.by.persona() {
                if self.store.get_persona(slug)?.is_none() {
                    bail!("no persona `{slug}` — model them first");
                }
            }
            let pass = Pass {
                id: format!("bp/{}", crate::store::new_id()),
                branch_key: r.key(),
                repo: r.repo.clone(),
                head: r.head.clone(),
                base: base.clone(),
                head_sha: head_sha.clone(),
                by: ask.by.clone(),
                provider: ask.provider.clone(),
                model: ask.model.clone(),
                status: PassStatus::Pending,
                recommendation: None,
                summary: None,
                comments: Vec::new(),
                dismissed: false,
                error: None,
                created_at: String::new(),
                updated_at: String::new(),
            };
            out.push(self.store.queue_branch_pass(&pass)?);
        }
        Ok((out, r, pr))
    }

    /// Run one pass to completion.
    pub async fn run(&self, id: &str) -> Result<Pass> {
        let Some(pass) = self.store.get_branch_pass(id)? else {
            bail!("no such review pass {id}");
        };
        self.store.set_branch_pass_status(id, "running")?;
        match self.run_inner(&pass).await {
            Ok(done) => Ok(done),
            Err(e) => {
                let msg = format!("{e:#}");
                // On the row, not only returned: the operator is watching a panel, and a
                // failure they cannot see is a spinner that never stops.
                self.store.fail_branch_pass(id, &msg)?;
                Err(e)
            }
        }
    }

    async fn run_inner(&self, pass: &Pass) -> Result<Pass> {
        // The cached diff for *this pass's* commit, so every reviewer in a panel reads exactly
        // the same code — otherwise two of them could disagree because they read different
        // commits, and the disagreement would be an artifact rather than a finding.
        let report = match self
            .store
            .get_branch_diff(&pass.branch_key, &pass.head_sha)?
        {
            Some(r) => r,
            None => {
                // A re-ask after the branch moved, or a cache wiped by a board reset.
                let fresh = self
                    .diffs
                    .read_branch(&pass.repo, &pass.base, &pass.head)
                    .await;
                if let Some(err) = &fresh.error {
                    bail!("{err}");
                }
                // The branch may have moved since the pass was queued. Reviewing the new code
                // under the old sha would file a review against a commit it never read.
                if let Some(sha) = fresh.head_sha.as_deref() {
                    if sha != pass.head_sha {
                        bail!(
                            "{} moved to {} while this was queued — ask again to review the \
                             new commit",
                            pass.head,
                            &sha[..sha.len().min(8)]
                        );
                    }
                }
                self.store
                    .set_branch_diff(&pass.branch_key, &pass.head_sha, &fresh)?;
                fresh
            }
        };
        let reasoner = (self.factory)(&pass.provider, &pass.model);

        match &pass.by {
            Reviewer::Model => {
                let tier = format!("{}/{}", pass.provider, pass.model);
                let review = self
                    .diffs
                    .review_with(&pass.repo, 0, &report, &*reasoner, &tier)
                    .await
                    .context("the model returned no review of this branch")?;
                let comments = place_model(review.comments);
                Ok(self.store.finish_branch_pass(
                    &pass.id,
                    Some(review.recommendation.as_str()),
                    Some(&review.rationale),
                    &comments,
                )?)
            }
            Reviewer::Persona(slug) => {
                // Through `Engine::predict` rather than reaching past it into the predictor:
                // that path is what applies `verify_prediction`, which drops any point citing
                // no trait. A predicted review with uncited points is an accusation dressed as
                // a rehearsal, and bypassing the check to save a lookup would lose it.
                //
                // The branch key stands in for the subject: a branch has none, and the
                // prediction lands on the pass rather than in the per-subject prediction table.
                let prediction = self
                    .personas
                    .predict(
                        slug,
                        &pass.branch_key,
                        crate::persona::PredictionKind::CodeReview,
                        &pass.head_sha,
                        dossier_for(&report, pass),
                        Some(crate::prdiff::render_for_prompt(&report)),
                        &format!("{}/{}", pass.provider, pass.model),
                        &*reasoner,
                    )
                    .await?;
                let comments = place_persona(slug, prediction.points, &report);
                // "They would not look at this" is a real answer and the most useful one a
                // predictor can give — recorded as the recommendation rather than as an empty
                // review, which would read as approval.
                let recommendation = if prediction.would_engage {
                    prediction
                        .recommendation
                        .clone()
                        .unwrap_or_else(|| "comment".into())
                } else {
                    "would not engage".into()
                };
                Ok(self.store.finish_branch_pass(
                    &pass.id,
                    Some(&recommendation),
                    Some(&prediction.summary),
                    &comments,
                )?)
            }
        }
    }
}

/// The "subject" a persona prediction is given for a branch.
///
/// A branch has no subject, no discussion and no attributed signals — so the dossier is what a
/// reviewer would actually have in front of them: which branch, against what, and the diff's
/// own summary. Passing an empty dossier would leave the predictor guessing what the change is
/// for, and it says so in its caveats rather than inventing one.
fn dossier_for(report: &DiffReport, pass: &Pass) -> String {
    let mut out = format!(
        "Branch `{}` of {}, against `{}`.\n{} file(s), +{} −{}.\n",
        pass.head, pass.repo, pass.base, report.file_count, report.additions, report.deletions
    );
    if let Some(summary) = report.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str("\nWhat the change does, read from the diff:\n");
        out.push_str(summary);
        out.push('\n');
    }
    if report.truncated {
        out.push_str("\n(The diff was truncated — say so if the answer depends on the rest.)\n");
    }
    out
}

#[cfg(test)]
mod live {
    use super::*;

    /// Review a **real** branch with the real GitHub API and real models, against the
    /// operator's own store. The unit tests prove the parser and the placement on fixtures;
    /// this is the other half — that a branch resolves, that `base...head` returns a diff, that
    /// both reviewer kinds answer, and that their comments land on lines that exist.
    ///
    /// Ignored: it spends model calls and reads a real repository.
    ///
    /// ```text
    /// MUGGLEBOT_BRANCH='restatedev/restate-cloud@bh/otel1' \
    ///   MUGGLEBOT_REVIEW_PERSONA=pavel \
    ///   cargo test reviews_a_real_branch -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn reviews_a_real_branch() {
        let spec = std::env::var("MUGGLEBOT_BRANCH").expect("set MUGGLEBOT_BRANCH");
        let db = std::env::var("MUGGLEBOT_DB").unwrap_or_else(|_| "data/mugglebot.sqlite".into());
        let branch = parse_branch(&spec).expect("that branch did not parse");
        println!("repo {} head {}", branch.repo, branch.head);

        let store = std::sync::Arc::new(
            crate::store::Store::open(std::path::Path::new(&db)).expect("open the store"),
        );
        let secrets = crate::secrets::Secrets::for_tests(store.clone());
        let token = secrets.get_opt("github").expect("no `github` credential");

        let factory: crate::restate::workflows::ReasonerFactory =
            std::sync::Arc::new(move |provider: &str, model: &str| {
                crate::reasoner::build(
                    crate::reasoner::provider_label(provider),
                    model,
                    &crate::config::Reasoner::default(),
                    None,
                )
            });
        let reasoner = (factory)("claude", "claude-opus-5");
        let diffs = std::sync::Arc::new(
            crate::prdiff::DiffReader::new(Some(token.clone()), reasoner, "claude/claude-opus-5")
                .expect("diff reader"),
        );
        let personas = std::sync::Arc::new(crate::persona::Engine::for_tests(store.clone()));
        let runner = Runner::new(store.clone(), diffs, personas, Some(token), factory);
        assert!(runner.ready());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let persona = std::env::var("MUGGLEBOT_REVIEW_PERSONA").ok();
        let asks = asks(persona.as_slice(), "claude", "claude-opus-5", true);
        println!("panel: {} reviewer(s)", asks.len());

        let (passes, branch, pr) = rt
            .block_on(runner.request(&branch, &asks))
            .expect("request");
        println!("resolved to {}", branch.key());
        println!("open PR on this branch: {pr:?}");
        for p in &passes {
            println!(
                "  queued {} — {:?} on {}/{}",
                p.id, p.by, p.provider, p.model
            );
        }

        for p in &passes {
            match rt.block_on(runner.run(&p.id)) {
                Ok(done) => {
                    println!(
                        "\n--- {:?} → {} ({} inline) ---\n{}",
                        done.by,
                        done.recommendation.as_deref().unwrap_or("-"),
                        done.comments.len(),
                        done.summary.as_deref().unwrap_or("")
                    );
                    for c in &done.comments {
                        println!(
                            "  [{:?}] {}{}\n      {}",
                            c.severity,
                            c.path,
                            match c.patch_index {
                                Some(i) => format!(" @patch line {i}"),
                                None => " (file level — anchor matched nothing)".into(),
                            },
                            c.note.chars().take(150).collect::<String>()
                        );
                    }
                }
                Err(e) => println!("\n--- {:?} FAILED: {e:#} ---", p.by),
            }
        }

        // The guarantee: every placed comment names a file in the diff, and every resolved
        // index is inside that file's patch. A comment anchored past the end of a patch would
        // render nowhere, which looks identical to no comment at all.
        let all = store.branch_passes(&branch.key()).expect("passes");
        let sha = all.first().map(|p| p.head_sha.clone()).unwrap_or_default();
        let report = store
            .get_branch_diff(&branch.key(), &sha)
            .expect("diff")
            .expect("a diff was cached");
        for pass in &all {
            for c in &pass.comments {
                let file = report.files.iter().find(|f| f.path == c.path);
                assert!(
                    file.is_some() || c.patch_index.is_none(),
                    "{} is placed on a file not in the diff: {}",
                    c.path,
                    c.note
                );
                if let (Some(i), Some(patch)) =
                    (c.patch_index, file.and_then(|f| f.patch.as_deref()))
                {
                    assert!(
                        i < patch.lines().count(),
                        "comment on {} resolved to line {i}, past the end of the patch",
                        c.path
                    );
                }
            }
        }
        println!("\nevery placed comment is inside a patch that exists");
    }

    /// List a repo's branches, so the live review test can be pointed at one that exists.
    ///
    /// ```text
    /// MUGGLEBOT_REPO=owner/repo cargo test lists_real_branches -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn lists_real_branches() {
        let repo = std::env::var("MUGGLEBOT_REPO").expect("set MUGGLEBOT_REPO");
        let db = std::env::var("MUGGLEBOT_DB").unwrap_or_else(|_| "data/mugglebot.sqlite".into());
        let store = std::sync::Arc::new(
            crate::store::Store::open(std::path::Path::new(&db)).expect("open the store"),
        );
        let secrets = crate::secrets::Secrets::for_tests(store);
        let token = secrets.get_opt("github").expect("no `github` credential");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let names: Vec<String> = rt.block_on(async {
            let client = reqwest::Client::new();
            let resp = client
                .get(format!(
                    "https://api.github.com/repos/{repo}/branches?per_page=40"
                ))
                .bearer_auth(&token)
                .header(reqwest::header::USER_AGENT, "mugglebot")
                .send()
                .await
                .expect("listing branches");
            assert!(resp.status().is_success(), "GitHub said {}", resp.status());
            let v: serde_json::Value = resp.json().await.expect("parsing");
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|b| b.get("name")?.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        });
        println!("{} branch(es) on {repo}:", names.len());
        for n in &names {
            println!("  {n}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::PredictedPoint;
    use crate::prdiff::{DiffFile, ReviewComment};

    // ---- pointing at a branch ----

    #[test]
    fn a_hand_typed_branch_parses() {
        let b = parse_branch("restatedev/restate-cloud@bh/otel1").unwrap();
        assert_eq!(b.repo, "restatedev/restate-cloud");
        assert_eq!(b.head, "bh/otel1");
        assert_eq!(b.base, None);
        assert_eq!(b.key(), "restatedev/restate-cloud@bh/otel1");
    }

    /// The failure a last-slash split would cause. Namespaced branches are the norm here —
    /// `bh/otel1`, `codex/issue-1267` — and truncating one 404s.
    #[test]
    fn a_branch_name_may_contain_slashes() {
        for raw in [
            "restatedev/restate-cloud@bh/otel1",
            "restatedev/restate-cloud:codex/issue-1267",
            "https://github.com/restatedev/restate-cloud/tree/bh/otel1",
        ] {
            let b = parse_branch(raw).unwrap();
            assert!(b.head.contains('/'), "{raw} lost its namespace: {}", b.head);
            assert_eq!(b.repo, "restatedev/restate-cloud");
        }
    }

    /// The shape the operator actually pasted first. The feature is about branches, but a PR
    /// link is by far the likeliest thing in a clipboard, and refusing it with a lecture about
    /// branches would be a parser being right at the operator's expense.
    #[test]
    fn a_pull_request_url_parses_and_carries_its_number() {
        for raw in [
            "https://github.com/restatedev/restate-cloud/pull/1334",
            "https://github.com/restatedev/restate-cloud/pull/1334/files",
            "https://github.com/restatedev/restate-cloud/pull/1334#discussion_r1",
        ] {
            let b = parse_branch(raw).unwrap();
            assert_eq!(b.repo, "restatedev/restate-cloud", "{raw}");
            assert_eq!(b.pull_number(), Some(1334), "{raw}");
        }
    }

    /// The placeholder is marked, so a branch that never resolved cannot be mistaken for one
    /// with an empty name — and a real branch is never read as a PR number.
    #[test]
    fn a_real_branch_is_not_mistaken_for_a_pull_request() {
        assert_eq!(parse_branch("o/r@bh/otel1").unwrap().pull_number(), None);
        assert_eq!(parse_branch("o/r@1334").unwrap().pull_number(), None);
    }

    #[test]
    fn a_pull_request_url_with_no_number_is_refused() {
        assert!(parse_branch("https://github.com/o/r/pull/").is_err());
        assert!(parse_branch("https://github.com/o/r/pull/abc").is_err());
    }

    #[test]
    fn a_branch_url_parses() {
        let b =
            parse_branch("https://github.com/restatedev/restate-cloud/tree/bh/otel1?x=1").unwrap();
        assert_eq!(b.repo, "restatedev/restate-cloud");
        assert_eq!(b.head, "bh/otel1");
        assert_eq!(b.base, None);
    }

    /// The only shape that names a base, so it is the only one that keeps it.
    #[test]
    fn a_compare_url_carries_its_base() {
        let b = parse_branch("https://github.com/restatedev/restate-cloud/compare/main...bh/otel1")
            .unwrap();
        assert_eq!(b.base.as_deref(), Some("main"));
        assert_eq!(b.head, "bh/otel1");
    }

    /// A cross-fork compare writes `owner:branch`; the owner is not part of the branch.
    #[test]
    fn a_cross_fork_compare_drops_the_owner_prefix() {
        let b = parse_branch("https://github.com/restatedev/restate/compare/main...someone:fix-1")
            .unwrap();
        assert_eq!(b.head, "fix-1");
        assert_eq!(b.base.as_deref(), Some("main"));
    }

    #[test]
    fn a_two_dot_compare_is_accepted_too() {
        let b = parse_branch("https://github.com/o/r/compare/main..feature").unwrap();
        assert_eq!(b.base.as_deref(), Some("main"));
        assert_eq!(b.head, "feature");
    }

    #[test]
    fn nonsense_says_what_was_expected() {
        for raw in ["", "   ", "just-some-words", "owner/repo", "@branch"] {
            let e = parse_branch(raw).unwrap_err();
            let msg = format!("{e:#}").to_lowercase();
            assert!(
                msg.contains("branch") || msg.contains("owner/repo"),
                "{raw:?} gave an unhelpful error: {msg}"
            );
        }
    }

    #[test]
    fn a_repo_with_the_wrong_number_of_parts_is_refused() {
        assert!(parse_branch("justowner@branch").is_err());
        assert!(parse_branch("a/b/c@branch").is_err());
    }

    // ---- the panel ----

    #[test]
    fn the_panel_is_the_model_plus_each_persona() {
        let asks = asks(
            &["pavel".into(), "luke".into()],
            "claude",
            "claude-opus-5",
            true,
        );
        assert_eq!(asks.len(), 3);
        assert_eq!(asks[0].by, Reviewer::Model);
        assert_eq!(asks[1].by, Reviewer::Persona("pavel".into()));
        assert_eq!(asks[2].by, Reviewer::Persona("luke".into()));
    }

    /// Naming personas *adds* to the model's read rather than replacing it — the two answer
    /// different questions, and seeing them against each other is the point of the view.
    #[test]
    fn personas_do_not_displace_the_generic_review() {
        let with = asks(&["pavel".into()], "claude", "m", true);
        assert!(with.iter().any(|a| a.by == Reviewer::Model));
        // …but it can be turned off for someone who only wants the rehearsal.
        let without = asks(&["pavel".into()], "claude", "m", false);
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].by, Reviewer::Persona("pavel".into()));
    }

    #[test]
    fn duplicate_and_blank_personas_collapse() {
        let a = asks(
            &["pavel".into(), "pavel".into(), "  ".into()],
            "claude",
            "m",
            false,
        );
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn a_reviewer_round_trips_through_its_key() {
        for r in [Reviewer::Model, Reviewer::Persona("pavel".into())] {
            assert_eq!(Reviewer::parse(&r.as_key()), r);
        }
        // An empty slug is not a persona.
        assert_eq!(Reviewer::parse("persona:"), Reviewer::Model);
    }

    // ---- placement ----

    const PATCH: &str = "@@ -1,4 +1,6 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n+    unwrap_or_default();\n }";

    fn report() -> DiffReport {
        DiffReport {
            repo: "o/r".into(),
            number: 0,
            files: vec![DiffFile {
                path: "src/main.rs".into(),
                additions: 2,
                deletions: 1,
                patch: Some(PATCH.into()),
                patch_omitted: false,
            }],
            file_count: 1,
            additions: 2,
            deletions: 1,
            summary: None,
            truncated: false,
            error: None,
            head_sha: Some("abc123".into()),
        }
    }

    /// A persona's `line` is already a copied line, so it goes through the same matcher a
    /// model review uses — no second anchoring rule.
    #[test]
    fn a_persona_point_is_placed_by_the_line_it_quoted() {
        let placed = place_persona(
            "pavel",
            vec![PredictedPoint {
                text: "He'd ask why this can't fail loudly.".into(),
                path: Some("src/main.rs".into()),
                line: Some("+    unwrap_or_default();".into()),
                because: vec!["pavel/style/1".into()],
            }],
            &report(),
        );
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].by, Reviewer::Persona("pavel".into()));
        assert!(
            placed[0].patch_index.is_some(),
            "must resolve onto the patch"
        );
        assert_eq!(placed[0].because, vec!["pavel/style/1".to_string()]);
    }

    /// An anchor that matches nothing degrades to a file-level comment. It must not be
    /// attached to whatever line happened to be nearby.
    #[test]
    fn an_unresolvable_anchor_becomes_a_file_level_comment() {
        let placed = place_persona(
            "pavel",
            vec![PredictedPoint {
                text: "Something about code that isn't here.".into(),
                path: Some("src/main.rs".into()),
                line: Some("+    let never_written = true;".into()),
                because: vec!["t".into()],
            }],
            &report(),
        );
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].patch_index, None);
    }

    /// A point naming no file cannot be placed anywhere; the persona's summary is where an
    /// unanchored opinion belongs.
    #[test]
    fn a_point_with_no_file_is_dropped_rather_than_placed_arbitrarily() {
        let placed = place_persona(
            "pavel",
            vec![PredictedPoint {
                text: "Generally unhappy.".into(),
                path: None,
                line: None,
                because: vec!["t".into()],
            }],
            &report(),
        );
        assert!(placed.is_empty());
    }

    /// A point about a file that is not in this diff cannot be placed on it — but it is still
    /// the persona's comment about that file, so it survives unplaced rather than vanishing.
    #[test]
    fn a_point_about_an_untouched_file_survives_unplaced() {
        let placed = place_persona(
            "pavel",
            vec![PredictedPoint {
                text: "The migration should come with a rollback.".into(),
                path: Some("migrations/003.sql".into()),
                line: None,
                because: vec!["t".into()],
            }],
            &report(),
        );
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].path, "migrations/003.sql");
        assert_eq!(placed[0].patch_index, None);
    }

    #[test]
    fn a_model_comment_keeps_its_anchor_and_gains_a_reviewer() {
        let placed = place_model(vec![ReviewComment {
            path: "src/main.rs".into(),
            severity: Severity::Concern,
            note: "This swallows the error.".into(),
            anchor: Some("+    unwrap_or_default();".into()),
            line: None,
            patch_index: Some(4),
        }]);
        assert_eq!(placed[0].by, Reviewer::Model);
        assert_eq!(placed[0].patch_index, Some(4));
        assert!(placed[0].because.is_empty());
    }

    // ---- what the diff shows ----

    fn pass(by: Reviewer, comments: Vec<Placed>, status: PassStatus, dismissed: bool) -> Pass {
        Pass {
            id: format!("bp/{}", by.as_key()),
            branch_key: "o/r@feature".into(),
            repo: "o/r".into(),
            head: "feature".into(),
            base: "main".into(),
            head_sha: "abc123".into(),
            by,
            provider: "claude".into(),
            model: "claude-opus-5".into(),
            status,
            recommendation: Some("comment".into()),
            summary: Some("s".into()),
            comments,
            dismissed,
            error: None,
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }

    fn placed(path: &str, idx: Option<usize>, by: Reviewer) -> Placed {
        Placed {
            path: path.into(),
            severity: Severity::Nit,
            note: "n".into(),
            anchor: None,
            patch_index: idx,
            by,
            because: vec![],
        }
    }

    #[test]
    fn a_dismissed_pass_leaves_the_diff() {
        let passes = vec![
            pass(
                Reviewer::Model,
                vec![placed("a.rs", Some(1), Reviewer::Model)],
                PassStatus::Done,
                false,
            ),
            pass(
                Reviewer::Persona("pavel".into()),
                vec![placed("a.rs", Some(2), Reviewer::Persona("pavel".into()))],
                PassStatus::Done,
                true,
            ),
        ];
        let shown = visible(&passes);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].by, Reviewer::Model);
        // …and the tally does not count it either, so the header and the gutter agree.
        let t = tally(&passes);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, "claude/claude-opus-5");
    }

    /// A pass still running has nothing to show. Rendering its empty comment list as "no
    /// findings" would read as a clean review.
    #[test]
    fn only_finished_passes_contribute_comments() {
        for status in [PassStatus::Pending, PassStatus::Running, PassStatus::Failed] {
            let passes = vec![pass(
                Reviewer::Model,
                vec![placed("a.rs", Some(1), Reviewer::Model)],
                status,
                false,
            )];
            assert!(visible(&passes).is_empty(), "{status:?} must not render");
        }
    }

    /// File-level comments sort above the hunks of their file rather than after the last line
    /// they failed to match.
    #[test]
    fn unplaced_comments_come_first_within_their_file() {
        let passes = vec![pass(
            Reviewer::Model,
            vec![
                placed("b.rs", Some(3), Reviewer::Model),
                placed("a.rs", Some(9), Reviewer::Model),
                placed("a.rs", None, Reviewer::Model),
                placed("a.rs", Some(2), Reviewer::Model),
            ],
            PassStatus::Done,
            false,
        )];
        let shown = visible(&passes);
        let order: Vec<(&str, Option<usize>)> = shown
            .iter()
            .map(|c| (c.path.as_str(), c.patch_index))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a.rs", None),
                ("a.rs", Some(2)),
                ("a.rs", Some(9)),
                ("b.rs", Some(3))
            ]
        );
    }

    #[test]
    fn the_tally_names_the_model_and_the_personas_separately() {
        let passes = vec![
            pass(
                Reviewer::Model,
                vec![placed("a", Some(1), Reviewer::Model)],
                PassStatus::Done,
                false,
            ),
            pass(
                Reviewer::Persona("pavel".into()),
                vec![
                    placed("a", Some(2), Reviewer::Persona("pavel".into())),
                    placed("a", Some(3), Reviewer::Persona("pavel".into())),
                ],
                PassStatus::Done,
                false,
            ),
        ];
        assert_eq!(
            tally(&passes),
            vec![
                ("claude/claude-opus-5".to_string(), 1),
                ("pavel".to_string(), 2)
            ]
        );
    }

    // ---- staleness ----

    /// A pass is about a commit, not a branch name. When the branch moves the review does not
    /// follow it, and saying so is the difference between stale and wrong.
    #[test]
    fn a_pass_knows_when_the_branch_has_moved_past_it() {
        let p = pass(Reviewer::Model, vec![], PassStatus::Done, false);
        assert!(!p.stale_against("abc123"));
        assert!(p.stale_against("def456"));
        // An unknown head is not evidence of staleness.
        assert!(!p.stale_against(""));
    }

    /// The key is what makes re-asking free. Same reviewer, same model, same sha → the same
    /// key, so Restate refuses the duplicate rather than spending another model call.
    #[test]
    fn the_version_key_separates_reviewers_models_and_shas() {
        let base = pass(Reviewer::Model, vec![], PassStatus::Done, false);
        let same = pass(Reviewer::Model, vec![], PassStatus::Done, false);
        assert_eq!(base.version(), same.version());

        let mut other_persona = pass(
            Reviewer::Persona("pavel".into()),
            vec![],
            PassStatus::Done,
            false,
        );
        assert_ne!(base.version(), other_persona.version());

        other_persona.model = "gpt-5.6-sol".into();
        let mut other_model = pass(
            Reviewer::Persona("pavel".into()),
            vec![],
            PassStatus::Done,
            false,
        );
        other_model.model = "claude-opus-5".into();
        assert_ne!(other_persona.version(), other_model.version());

        let mut moved = pass(Reviewer::Model, vec![], PassStatus::Done, false);
        moved.head_sha = "def456".into();
        assert_ne!(base.version(), moved.version());
    }
}
