//! Assigned-issue triage: read the code, characterize the issue, propose patches,
//! then say it in plain English.
//!
//! An issue assigned to you is work you've committed to, and the expensive part
//! isn't noticing it — it's the cold start. Reloading what the issue is really
//! about, finding the code involved, and working out what the options are costs
//! twenty minutes every time you come back to it. This pipeline does that pass
//! ahead of you, against the actual source:
//!
//! 1. **Check the repo out** ([`crate::checkout`]) — shallow, read-only.
//! 2. **Find the relevant files** by matching identifiers from the issue text
//!    against the tree. Deterministic, so it works with no model at all.
//! 3. **Characterize** — the local coder model reads the issue *and the source*
//!    and states what's actually going on.
//! 4. **Propose patches** — three distinct approaches, each with its files, its
//!    trade-off, and its risk. Approaches, not applied diffs: MuggleBot proposes,
//!    you decide (see the copilot-not-autopilot principle).
//! 5. **Plain English** — a fast, cheap cloud model (Haiku) rewrites the result
//!    into something readable at a glance on the board. This tier does no
//!    reasoning; it only re-renders what steps 3 and 4 concluded, which is why a
//!    small model is the right tool and why it can't introduce new claims.
//!
//! Steps 2–4 are on-device: reading source code is exactly the work you don't want
//! leaving the machine, and a coder model is better at it than a generalist.
//!
//! Nothing here writes to a repository. The output is a proposal in MuggleBot's
//! own store.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::checkout::CheckoutCache;
use crate::comments::CommentJudge;
use crate::config::Assigned as AssignedCfg;
use crate::github::GithubClient;
use crate::reasoner::{self, CompletionRequest, Reasoner};
use crate::store::{IssueTriage, Store};

/// Source extensions worth showing a coder model. Lock files, minified bundles,
/// and generated output are noise that would crowd out the real code.
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "go", "java", "kt", "py", "rb", "c", "h", "cc", "cpp", "hpp",
    "cs", "scala", "swift", "sql", "proto", "toml", "yaml", "yml",
];

/// Directories never worth walking.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    ".venv",
    "__pycache__",
    ".next",
    "coverage",
];

/// Cap on files walked, so a huge tree can't stall the worker.
const MAX_WALK: usize = 20_000;

/// Total source characters in one prompt.
///
/// This is a **correctness** limit, not a cost one. A local coder model has a
/// modest context window (deepseek-coder is 16k tokens ≈ 60k characters for
/// everything — instructions, issue, source, and the answer). Overflow doesn't
/// error; it silently drops the front of the prompt, which is where the
/// instructions and the issue live. The symptom is a model that dutifully
/// describes the code you pasted and never mentions the issue at all. Budgeting
/// the source is what keeps the actual question in the window.
const MAX_SOURCE_CHARS: usize = 24_000;

pub struct Triager {
    store: Arc<Store>,
    checkouts: Arc<CheckoutCache>,
    github: Option<GithubClient>,
    /// Local coder model: reads the source, characterizes, proposes patches.
    coder: Arc<dyn Reasoner>,
    /// Small fast cloud model: plain-English rendering only.
    brief: Arc<dyn Reasoner>,
    /// Finds open PRs that may already fix the issue — possibly somebody else's.
    pr_fixes: crate::prfix::PrFixFinder,
    /// Scores the issue's comments so the discussion is read on merit.
    comments: CommentJudge,
    cfg: AssignedCfg,
}

impl Triager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        checkouts: Arc<CheckoutCache>,
        token: Option<String>,
        coder: Arc<dyn Reasoner>,
        brief: Arc<dyn Reasoner>,
        routed: Arc<dyn Reasoner>,
        analyst: Arc<crate::correlation::Analyst>,
        cfg: AssignedCfg,
    ) -> Self {
        Self {
            store: store.clone(),
            checkouts,
            github: token.and_then(|t| GithubClient::new(t).ok()),
            pr_fixes: crate::prfix::PrFixFinder::new(store, coder.clone(), brief.clone(), routed)
                .with_analyst(analyst),
            comments: CommentJudge::new(coder.clone()),
            coder,
            brief,
            cfg,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && self.github.is_some()
    }

    /// Triage one issue end to end, persisting progress as it goes.
    ///
    /// A re-triage of an issue whose code hasn't moved would otherwise be served
    /// entirely from the completion cache — correct, but not what "re-triage"
    /// means. So a repeat run against the *same* commit forces fresh calls; a run
    /// against new code is a natural cache miss anyway.
    pub async fn triage(&self, issue_key: &str) -> Result<IssueTriage> {
        let Some(mut t) = self.store.get_issue_triage(issue_key)? else {
            anyhow::bail!("no triage row for {issue_key}");
        };
        t.status = "running".into();
        t.error = None;
        self.store.put_issue_triage(&t)?;
        // Drop the previous PR analysis: those pull requests may have merged,
        // closed, or moved on, and stale conclusions about them are worse than none.
        if let Err(e) = self.store.clear_pr_fixes(issue_key) {
            debug!("triage {issue_key}: clearing previous PR analysis failed: {e:#}");
        }

        // A repeat run against a commit we already analyzed is a deliberate redo,
        // not new work — force fresh model calls rather than replaying the cache.
        let previous_sha = t.head_sha.clone();
        match self.run(&mut t, previous_sha).await {
            Ok(()) => {
                t.status = "complete".into();
                t.error = None;
            }
            Err(e) => {
                warn!("triage {issue_key} failed: {e:#}");
                t.status = "failed".into();
                t.error = Some(format!("{e:#}"));
            }
        }
        self.store.put_issue_triage(&t)?;
        Ok(t)
    }

    async fn run(&self, t: &mut IssueTriage, previous_sha: Option<String>) -> Result<()> {
        let gh = self.github.as_ref().context("no GitHub client")?;
        if !crate::checkout::have_git() {
            anyhow::bail!(
                "`git` is not on PATH — cannot check out {} to read it",
                t.repo
            );
        }

        // 1. Pull the code.
        let (branch, size_kb) = gh
            .repo_checkout_info(&t.repo)
            .await
            .with_context(|| format!("reading {} metadata", t.repo))?;
        let checkout = self.checkouts.ensure(&t.repo, &branch, size_kb).await?;
        t.checkout = Some(checkout.path.to_string_lossy().to_string());
        t.head_sha = Some(checkout.head_sha.clone());
        self.store.put_issue_triage(t)?;
        debug!(
            "triage {}: checked out at {}",
            t.issue_key, checkout.head_sha
        );
        // What kind of system is this? Proposals are only useful in the idiom of
        // the stack they're for, and the model can't know the stack unless it's
        // told. Detected from markers actually present in the tree.
        let eco = crate::ecosystem::detect(&checkout.path);
        debug!(
            "triage {}: ecosystem {:?} {:?}",
            t.issue_key, eco.platforms, eco.languages
        );

        // Same code as last time → this is a redo, so don't serve it from cache.
        let redo = previous_sha.as_deref() == Some(checkout.head_sha.as_str());
        if redo {
            debug!("triage {}: re-analyzing unchanged code", t.issue_key);
        }

        // 2. Read the discussion. The comments on an issue are usually where its
        // real content is — what was tried, what was ruled out, what a maintainer
        // decided — so this goes in alongside the body, not instead of it.
        let discussion = self.discussion(gh, t, redo).await;
        let issue_text = format!(
            "{}\n{}\n\n=== DISCUSSION ===\n{discussion}",
            t.title,
            self.issue_body(t).unwrap_or_default()
        );
        let files = self.relevant_files(&checkout.path, &issue_text);
        t.files = files.iter().map(|(p, _)| p.clone()).collect();
        self.store.put_issue_triage(t)?;
        if files.is_empty() {
            debug!("triage {}: no matching source files", t.issue_key);
        }

        // 3 + 4. Characterize and propose, on-device, with the source in hand.
        let source = render_source(&files, self.cfg.max_file_chars);
        t.characterization = Some(
            self.characterize(t, &issue_text, &source, &eco, redo)
                .await?,
        );
        self.store.put_issue_triage(t)?;

        t.patches = json!(
            self.propose_patches(
                t,
                &issue_text,
                &source,
                t.characterization.as_deref().unwrap_or(""),
                &eco,
                redo,
            )
            .await?
        );
        self.store.put_issue_triage(t)?;

        // 5. Is somebody already fixing this? Worth knowing before you start —
        // and the answer is often a PR by someone else.
        if self.cfg.check_open_prs {
            let others = self.other_open_issues(t);
            match self
                .pr_fixes
                .find(
                    gh,
                    &crate::prfix::Subject::from(&*t),
                    &issue_text,
                    &others,
                    redo,
                )
                .await
            {
                Ok(found) if !found.is_empty() => info!(
                    "triage {}: {} open PR(s) may address this",
                    t.issue_key,
                    found.len()
                ),
                Ok(_) => {}
                Err(e) => warn!("triage {}: PR scan failed: {e:#}", t.issue_key),
            }
        }

        // 6. Say it in plain English.
        t.plain_summary = self.plain_english(t).await;
        Ok(())
    }

    /// The issue's comments, every one scored for merit and the substantive ones
    /// kept in conversation order. See [`crate::comments`] for why selection is by
    /// merit rather than by position.
    async fn discussion(&self, gh: &GithubClient, t: &IssueTriage, fresh: bool) -> String {
        let comments = match gh.issue_comments(&t.repo, t.number as u64).await {
            Ok(c) => c,
            Err(e) => {
                debug!("triage {}: comments unavailable: {e:#}", t.issue_key);
                return "(comments unavailable)".into();
            }
        };
        let total = comments.len();
        let judged = self
            .comments
            .select(&comments, &t.title, self.cfg.max_comment_chars, fresh)
            .await;
        debug!(
            "triage {}: {} of {total} comment(s) judged substantive",
            t.issue_key,
            judged.len()
        );
        crate::comments::render(&judged, total)
    }

    /// The other assigned issues in the same repo, as `owner/repo#N — title`.
    ///
    /// Supplied to the PR analysis so it can say "this patch would also close
    /// #418" — and constrained to issues we actually know about, so the model
    /// can't invent references.
    fn other_open_issues(&self, t: &IssueTriage) -> Vec<String> {
        self.store
            .list_issue_triage()
            .unwrap_or_default()
            .into_iter()
            .filter(|other| other.repo == t.repo && other.issue_key != t.issue_key)
            .map(|other| format!("{} — {}", other.issue_key, other.title))
            .take(20)
            .collect()
    }

    /// The issue body, from the signal the watcher stored it on.
    fn issue_body(&self, t: &IssueTriage) -> Option<String> {
        let signal_id = t.signal_id.as_deref()?;
        let signal = self.store.get_signal(signal_id).ok()??;
        signal.body
    }

    /// Rank the tree's source files against the issue text.
    ///
    /// Deterministic on purpose: it runs before any model call, so file selection
    /// works even with nothing reachable, and the model is never asked to guess at
    /// paths it hasn't seen. Scoring is crude but effective — a file whose *path*
    /// matches an identifier from the issue is almost always relevant, and content
    /// matches break the ties.
    fn relevant_files(&self, root: &Path, issue_text: &str) -> Vec<(String, String)> {
        let terms = identifiers(issue_text);
        if terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, String, String)> = Vec::new();
        for path in walk_source(root) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Skip minified/generated blobs: one 400k-char line is not readable
            // code and would blow the context budget.
            if body.len() > 400_000 {
                continue;
            }
            let lower_path = rel.to_ascii_lowercase();
            let lower_body = body.to_ascii_lowercase();
            let mut score = 0usize;
            for term in &terms {
                // A path hit is worth far more than a body hit — `pool.rs` for a
                // pool bug beats a file that merely mentions "pool" once.
                if lower_path.contains(term) {
                    score += 8;
                }
                let hits = lower_body.matches(term.as_str()).count();
                score += hits.min(5);
            }
            if score > 0 {
                scored.push((score, rel, body));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(self.cfg.max_files)
            .map(|(_, rel, body)| (rel, body))
            .collect()
    }

    /// What is actually going on here — read against the source, not just the text.
    async fn characterize(
        &self,
        t: &IssueTriage,
        issue: &str,
        source: &str,
        eco: &crate::ecosystem::Ecosystem,
        redo: bool,
    ) -> Result<String> {
        let system = "You are a senior engineer triaging an issue against the repository's actual \
             source code. Write a tight technical characterization in Markdown, at most 200 words:\n\
             - **What it is**: the real problem, in the code's own terms.\n\
             - **Where**: the specific files/functions involved, by path.\n\
             - **Why it happens**: the mechanism, if the source shows it.\n\
             - **Unclear**: what you could not determine from what you were given.\n\
             Only claim what the code and issue support. If the provided files don't cover the \
             relevant code, say so plainly rather than inventing a mechanism. No preamble.";
        // The task is restated *after* the source. A long code dump otherwise
        // dominates a small model's attention and it answers the implicit question
        // "describe this code" instead of the one that was asked — which is exactly
        // what happens if the issue scrolls out of the front of the window.
        let prompt = format!(
            "=== ISSUE ===\nRepository: {} (at {})\nIssue #{}: {}\n\n{issue}\n\n\
             === ECOSYSTEM ===\n{}\n\n\
             === SOURCE (excerpts selected for this issue) ===\n{source}\n\n\
             === YOUR TASK ===\nAnalyze ISSUE #{} above — do NOT summarize or describe the \
             repository. Using the source only as evidence, write the four sections \
             (**What it is**, **Where**, **Why it happens**, **Unclear**).",
            t.repo,
            t.head_sha.as_deref().unwrap_or("unknown"),
            t.number,
            t.title,
            eco.render(),
            t.number,
        );
        let mut req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(700);
        req.no_cache = redo;
        let out = self.coder.complete(&req).await?;
        if out.trim().is_empty() {
            anyhow::bail!("the coder model returned an empty characterization");
        }
        Ok(out.trim().to_string())
    }

    /// Three distinct approaches, each with its trade-off.
    ///
    /// Distinctness is the point: three variations on one idea is a single option
    /// wearing three hats, and doesn't help you choose. The prompt asks for
    /// genuinely different strategies and for the risk of each to be stated.
    async fn propose_patches(
        &self,
        t: &IssueTriage,
        issue: &str,
        source: &str,
        characterization: &str,
        eco: &crate::ecosystem::Ecosystem,
        redo: bool,
    ) -> Result<Vec<Value>> {
        let want = self.cfg.patches.max(1);
        let system = format!(
            "You propose {want} DISTINCT candidate fixes for an issue, for an engineer to choose \
             between. Reply with ONLY JSON:\n\
             {{\"patches\":[{{\"title\":\"<short imperative name>\",\"approach\":\"<2-4 sentences: \
             what to change and where>\",\"mechanism\":\"<the platform extension point this uses, \
             from the ECOSYSTEM section, or 'application code' if genuinely none applies>\",\
             \"new_dependency\":\"<name of any tool/library not already in the dependency list, \
             else empty>\",\"files\":[\"<path>\"],\"sketch\":\"<a few lines of \
             illustrative code or diff, or empty>\",\"risk\":\"<what could go wrong / what to test>\",\
             \"effort\":\"small|medium|large\",\"confidence\":0.0-1.0}}]}}\n\
             Rules:\n\
             - The {want} options must be genuinely different strategies (e.g. a minimal targeted \
             fix, a more thorough refactor, and a mitigation/workaround) — not three wordings of \
             one idea.\n\
             - USE THE ECOSYSTEM. The ECOSYSTEM section states the platform and its native \
             extension points. A proposal must work the way this platform works, not the way the \
             language generically would: on Kubernetes, input validation is an admission \
             policy/webhook or a CRD schema, not a hand-rolled parser call; in Helm it's \
             values.schema.json; in Terraform it's a validation block. Set `mechanism` to the \
             specific extension point you are using.\n\
             - DO NOT INVENT TOOLS OR LIBRARIES. If you name one, it must either appear in the \
             existing dependencies listed in ECOSYSTEM, or be a well-known component of the stated \
             platform — and then you must set `new_dependency` to its name so the engineer knows \
             it's a new thing to adopt. A plausible-sounding package that does not exist is the \
             worst possible answer.\n\
             - Cite only file paths that appear in the provided source, and never invent APIs that \
             aren't in it.\n\
             - If the source is insufficient to propose a real fix, return fewer options and say \
             why in the `approach` field."
        );
        // Same shape as `characterize`: the ask is repeated after the source so it
        // isn't lost behind a long dump.
        let prompt = format!(
            "=== ISSUE ===\nRepository: {}\nIssue #{}: {}\n\n{issue}\n\n\
             === CHARACTERIZATION ===\n{characterization}\n\n\
             === ECOSYSTEM ===\n{}\n\n\
             === SOURCE (excerpts selected for this issue) ===\n{source}\n\n\
             === YOUR TASK ===\nPropose {want} distinct ways to fix issue #{}, each using this \
             platform's own mechanisms. Reply with the JSON object described above and nothing else.",
            t.repo,
            t.number,
            t.title,
            eco.render(),
            t.number,
        );
        let mut req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(2_000);
        req.no_cache = redo;
        let raw = self.coder.complete(&req).await?;
        let Some(value) = reasoner::extract_json(&raw) else {
            anyhow::bail!("the coder model did not return usable patch JSON");
        };
        let known: Vec<&str> = t.files.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        for (i, p) in value
            .get("patches")
            .and_then(|p| p.as_array())
            .into_iter()
            .flatten()
            .enumerate()
            .take(want)
        {
            let title = p
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Untitled approach")
                .trim();
            // Only keep paths the model was actually shown. A patch citing a file
            // that doesn't exist reads as authoritative and sends you hunting.
            let files: Vec<String> = p
                .get("files")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|f| f.as_str())
                        .filter(|f| known.contains(f))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            // A named dependency that is already present isn't new — correcting this
            // keeps the "you'd be adopting something" warning meaningful.
            let claimed = p
                .get("new_dependency")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("none"));
            let new_dependency = claimed.filter(|d| {
                !eco.dependencies
                    .iter()
                    .any(|have| have.eq_ignore_ascii_case(d))
            });
            out.push(json!({
                "id": format!("patch-{i}"),
                "title": title,
                "approach": p.get("approach").and_then(|v| v.as_str()).unwrap_or("").trim(),
                "mechanism": p.get("mechanism").and_then(|v| v.as_str()).unwrap_or("").trim(),
                "new_dependency": new_dependency,
                "files": files,
                "sketch": p.get("sketch").and_then(|v| v.as_str()).unwrap_or("").trim(),
                "risk": p.get("risk").and_then(|v| v.as_str()).unwrap_or("").trim(),
                "effort": p.get("effort").and_then(|v| v.as_str()).unwrap_or("medium"),
                "confidence": p.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0),
            }));
        }
        if out.is_empty() {
            anyhow::bail!("the coder model proposed no usable patches");
        }
        Ok(out)
    }

    /// Rewrite the technical result as plain English.
    ///
    /// Deliberately a *re-rendering* job, not a reasoning one: the model is told to
    /// add nothing, which is what makes a small fast model correct here rather than
    /// merely cheap. Returns `None` on failure — the technical analysis stands on
    /// its own, so a missing gloss is cosmetic.
    async fn plain_english(&self, t: &IssueTriage) -> Option<String> {
        let mut options = String::new();
        for p in t.patches.as_array().into_iter().flatten() {
            options.push_str(&format!(
                "- {}: {}\n",
                p.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                p.get("approach").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }
        let system = "Rewrite an engineer's technical triage notes as plain English for someone \
             skimming a board. Three to five sentences, no jargon, no Markdown headers, no bullet \
             list: what the issue is, and what the options are. Add NO new information, opinions, \
             or recommendations beyond what the notes say — you are re-wording, not analyzing. If \
             the notes are uncertain, keep the uncertainty.";
        let prompt = format!(
            "Issue: {} (#{} in {})\n\nNotes:\n{}\n\nOptions:\n{options}",
            t.title,
            t.number,
            t.repo,
            t.characterization.as_deref().unwrap_or("(none)")
        );
        match self
            .brief
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(400),
            )
            .await
        {
            Ok(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
            Ok(_) => None,
            Err(e) => {
                debug!("triage {}: plain-English pass skipped: {e:#}", t.issue_key);
                None
            }
        }
    }
}

/// The queue worker. Triage is slow (a clone plus several local model passes on a
/// 33b model), so it runs one at a time, off the poll path.
pub struct TriageWorker {
    store: Arc<Store>,
    triager: Arc<Triager>,
    on_complete: Arc<dyn Fn(IssueTriage) + Send + Sync>,
}

impl TriageWorker {
    pub fn new(
        store: Arc<Store>,
        triager: Arc<Triager>,
        on_complete: Arc<dyn Fn(IssueTriage) + Send + Sync>,
    ) -> Self {
        Self {
            store,
            triager,
            on_complete,
        }
    }

    pub async fn run(self: Arc<Self>) {
        if !self.triager.enabled() {
            debug!("triage worker: disabled");
            return;
        }
        match self.store.requeue_running_issue_triage() {
            Ok(n) if n > 0 => info!("triage worker: requeued {n} interrupted triage(s)"),
            Ok(_) => {}
            Err(e) => warn!("triage worker: requeue failed: {e:#}"),
        }
        loop {
            match self.step().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => warn!("triage worker: {e:#}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    }

    async fn step(&self) -> Result<bool> {
        let Some(job) = self.store.claim_issue_triage()? else {
            return Ok(false);
        };
        info!("triage worker: analyzing {}", job.issue_key);
        let done = self.triager.triage(&job.issue_key).await?;
        if done.status == "complete" {
            info!(
                "triage worker: {} complete ({} patch option(s))",
                done.issue_key,
                done.patches.as_array().map(Vec::len).unwrap_or(0)
            );
        }
        (self.on_complete)(done);
        Ok(true)
    }
}

// ---- helpers -----------------------------------------------------------------

/// Distinctive identifiers from issue text: the terms worth grepping the tree for.
///
/// Keeps `snake_case`, `camelCase`, `Dotted.Names`, and anything in backticks —
/// the shapes that actually name code — and drops ordinary prose words, which
/// would match everything.
pub fn identifiers(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let term = raw
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .to_ascii_lowercase();
        if term.len() < 4 || term.len() > 60 || STOPWORDS.contains(&term.as_str()) {
            return;
        }
        if !out.contains(&term) {
            out.push(term);
        }
    };
    // Backticked spans are the strongest signal an author can give.
    for span in text.split('`').skip(1).step_by(2) {
        for word in span.split_whitespace() {
            push(word);
        }
    }
    for word in text.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',') {
        let cleaned = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.');
        if cleaned.len() < 4 {
            continue;
        }
        let codey = cleaned.contains('_')
            || cleaned.contains('.')
            || cleaned.chars().any(|c| c.is_ascii_uppercase())
                && cleaned.chars().any(|c| c.is_ascii_lowercase());
        if codey {
            push(cleaned);
        }
    }
    out.truncate(12);
    out
}

const STOPWORDS: &[&str] = &[
    "this", "that", "with", "from", "when", "then", "have", "will", "should", "would", "could",
    "there", "which", "about", "issue", "error", "github", "https", "http", "com",
];

/// Source files under `root`, skipping build output and dependencies.
fn walk_source(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= MAX_WALK {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if CODE_EXTENSIONS.contains(&ext.as_str()) {
                out.push(path);
            }
        }
    }
    out
}

/// Lay the selected files out for the model, each labeled with its path so the
/// analysis can cite them.
///
/// Bounded twice: `max_chars` per file so one large file can't crowd out the
/// rest, and [`MAX_SOURCE_CHARS`] overall so the instructions and the issue stay
/// inside the model's context window. Files arrive best-match-first, so cutting
/// from the end drops the least relevant ones.
fn render_source(files: &[(String, String)], max_chars: usize) -> String {
    if files.is_empty() {
        return "(no matching source files were found in the repository)".into();
    }
    let mut out = String::new();
    let mut dropped = 0usize;
    for (path, body) in files {
        let chunk = format!("\n--- {path} ---\n{}\n", truncate(body, max_chars));
        if out.len() + chunk.len() > MAX_SOURCE_CHARS && !out.is_empty() {
            dropped += 1;
            continue;
        }
        out.push_str(&chunk);
    }
    if dropped > 0 {
        // Say so rather than letting the model assume it saw everything.
        out.push_str(&format!(
            "\n({dropped} further matching file(s) omitted to stay within the context window)\n"
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "\n… (truncated)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_keep_code_shapes_and_drop_prose() {
        let terms = identifiers(
            "The `connection_pool` in ConnectionManager.acquire leaks when the server is busy",
        );
        assert!(terms.contains(&"connection_pool".to_string()));
        assert!(terms.iter().any(|t| t.contains("connectionmanager")));
        // Ordinary prose would match every file in the repo.
        assert!(!terms.contains(&"when".to_string()));
        assert!(!terms.contains(&"busy".to_string()));
    }

    #[test]
    fn identifiers_ignore_urls_and_boilerplate() {
        let terms = identifiers("see https://github.com/foo/bar issue error this that");
        assert!(!terms.contains(&"https".to_string()));
        assert!(!terms.contains(&"issue".to_string()));
        assert!(!terms.contains(&"error".to_string()));
    }

    #[test]
    fn identifiers_are_bounded() {
        let text = (0..100)
            .map(|i| format!("some_ident_{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(identifiers(&text).len() <= 12);
    }

    #[test]
    fn rendered_source_labels_each_file_and_truncates() {
        let files = vec![
            ("src/pool.rs".to_string(), "a".repeat(50)),
            ("src/lib.rs".to_string(), "short".to_string()),
        ];
        let out = render_source(&files, 10);
        assert!(out.contains("--- src/pool.rs ---"));
        assert!(out.contains("--- src/lib.rs ---"));
        assert!(out.contains("(truncated)"));
        assert!(out.contains("short"));
    }

    #[test]
    fn empty_selection_says_so_rather_than_pretending() {
        let out = render_source(&[], 100);
        assert!(out.contains("no matching source files"));
    }

    #[test]
    fn walk_skips_dependencies_and_build_output() {
        let root = std::env::temp_dir().join(format!("mb-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["src", "node_modules", "target", ".git"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("a.rs"), "fn main() {}").unwrap();
        }
        std::fs::write(root.join("src").join("notes.md"), "# not code").unwrap();

        let found: Vec<String> = walk_source(&root)
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(found, vec!["src/a.rs"], "got {found:?}");
        std::fs::remove_dir_all(&root).ok();
    }
}
