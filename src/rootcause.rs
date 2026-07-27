//! Root-cause investigation: from a symptom to the change that probably caused it.
//!
//! This is the tier above correlation. Correlation answers "what lit up, and what
//! else is part of the same event?". This answers the next question — *why?* —
//! by going looking in the code:
//!
//! 1. **Extract symptoms** from the subject's signals, including whatever the
//!    browser investigation read off the dashboard.
//! 2. **Route to repos** through the code-derived repo index ([`crate::repos`]).
//! 3. **Search issues and PRs** in those repos for the symptom. Someone may have
//!    already filed it — that's the cheapest possible answer.
//! 4. **Scan the commit log** over the incident window for changes that could
//!    plausibly have introduced it.
//! 5. **Shortlist locally**, then **rank on the cloud model**, producing candidate
//!    causes each carrying its citation.
//! 6. **Fall back to code search** when nothing above explains it: if no issue,
//!    PR, or commit matches, point at the code that implements the failing thing.
//!
//! # Where each model runs
//!
//! Steps 1–4 and the shortlisting in step 5 run entirely on the **local**
//! classifier (Ollama). Those are the wide, mechanical passes — reading dozens of
//! issue titles and commit subjects to decide what's even worth considering. Only
//! the final verdict over an already-narrowed shortlist reaches the metered cloud
//! model. So an investigation costs a handful of local calls and *one* Claude
//! call, and with no cloud reasoner at all it still produces the local shortlist.
//!
//! # Copilot, not autopilot
//!
//! Every candidate is a *hypothesis with a citation*, never a conclusion: an
//! issue/PR/commit link, a confidence, and the rationale that produced it. Nothing
//! here mutates a repository, closes an issue, or reverts anything.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::config::Investigation as InvestigationCfg;
use crate::github::{CodeHit, GithubClient, IssueHit};
use crate::reasoner::{self, CompletionRequest, Reasoner};
use crate::repos::RepoIndex;
use crate::signal::Signal;
use crate::store::{CommitEntry, RootCauseReport, Store};
use crate::subject::Handled;
use crate::subject::{Attributor, SubjectView};

/// Issues/PRs pulled per search.
const ISSUE_LIMIT: usize = 20;
/// Commits scanned per repo. Wider than the shortlist so the local pass has
/// something to actually filter.
const COMMIT_LIMIT: usize = 60;
/// Code-search hits kept for the fallback.
const CODE_LIMIT: usize = 10;
/// Associated PRs judged per subject. Each is a diff fetch plus a model read.
const MAX_JUDGED_PRS: usize = 3;
/// How long a symptom search stays fresh. An investigation re-runs on every
/// re-analysis of a busy subject; the underlying issue list does not change on
/// that timescale.
const SEARCH_TTL: Duration = Duration::from_secs(900);

pub struct Investigator {
    store: Arc<Store>,
    attributor: Arc<Attributor>,
    repos: Arc<RepoIndex>,
    github: Option<GithubClient>,
    /// Symptom extraction, and shortlisting the wide search results down to what is
    /// worth reasoning over at all.
    local: Arc<dyn Reasoner>,
    /// The final ranking pass, over the shortlist only.
    ranker: Arc<dyn Reasoner>,
    /// Judges the PRs correlation has already associated with this subject.
    pr_fixes: crate::prfix::PrFixFinder,
    cfg: InvestigationCfg,
}

impl Investigator {
    pub fn new(
        store: Arc<Store>,
        attributor: Arc<Attributor>,
        repos: Arc<RepoIndex>,
        token: Option<String>,
        local: Arc<dyn Reasoner>,
        ranker: Arc<dyn Reasoner>,
        cfg: InvestigationCfg,
    ) -> Self {
        let github = token.and_then(|t| GithubClient::new(t).ok());
        Self {
            pr_fixes: crate::prfix::PrFixFinder::new(store.clone(), local.clone(), ranker.clone()),
            store,
            attributor,
            repos,
            github,
            local,
            ranker,
            cfg,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && self.github.is_some()
    }

    pub fn get(&self, subject_key: &str) -> Result<Option<RootCauseReport>> {
        self.store.get_root_cause(subject_key)
    }

    /// Investigate a subject, persisting the report as it goes so the UI can show
    /// `running` immediately rather than waiting on the whole pipeline.
    ///
    /// Refuses to run on a **handled** subject: snoozed, resolved, and acknowledged
    /// subjects are settled work, and spending model calls (least of all metered
    /// ones) re-litigating them is exactly what the operator asked us not to do.
    pub async fn investigate(&self, subject_key: &str) -> Result<RootCauseReport> {
        let Some(view) = self.attributor.subject_view(subject_key)? else {
            anyhow::bail!("no subject {subject_key}");
        };
        if view.subject.handled.is_handled() {
            anyhow::bail!(
                "subject {subject_key} is {} — handled subjects are not investigated",
                view.subject.handled.as_str()
            );
        }
        if !self.enabled() {
            anyhow::bail!(
                "investigation is unavailable (enabled = {}, GitHub token = {})",
                self.cfg.enabled,
                self.github.is_some()
            );
        }

        let mut report = RootCauseReport {
            subject_key: subject_key.to_string(),
            status: "running".into(),
            symptoms: Vec::new(),
            repos: Vec::new(),
            candidates: json!([]),
            verdict: None,
            error: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        self.store.put_root_cause(&report)?;

        match self.run(&view, &mut report).await {
            Ok(()) => {
                report.status = "complete".into();
                report.error = None;
            }
            Err(e) => {
                warn!("investigation for {subject_key} failed: {e:#}");
                report.status = "failed".into();
                report.error = Some(format!("{e:#}"));
            }
        }
        self.store.put_root_cause(&report)?;
        Ok(report)
    }

    /// The pipeline proper. Writes progress into `report` as each stage lands so a
    /// later failure still leaves the useful partial result behind.
    async fn run(&self, view: &SubjectView, report: &mut RootCauseReport) -> Result<()> {
        let gh = self.github.as_ref().context("no GitHub client")?;

        // 1. Symptoms — the search vocabulary, on the local model.
        let symptoms = self.extract_symptoms(view).await;
        report.symptoms = symptoms.clone();
        self.store.put_root_cause(report)?;
        if symptoms.is_empty() {
            anyhow::bail!("no searchable symptoms could be extracted from this subject");
        }
        let symptom_text = symptoms.join(", ");
        debug!(
            "investigation {}: symptoms = {symptom_text}",
            view.subject.key
        );

        // 2. Route to repos — keyword rules plus the local model reading the index.
        let repos = self.repos.route(&symptom_text).await?;
        report.repos = repos.clone();
        self.store.put_root_cause(report)?;
        if repos.is_empty() {
            anyhow::bail!("no repositories could be routed for these symptoms");
        }
        info!(
            "investigation {}: searching {}",
            view.subject.key,
            repos.join(", ")
        );

        // 3 + 4. Existing issues/PRs, and the commit log over the incident window.
        let issues = self.search_issues(gh, &symptoms, &repos).await;
        let since = self.commit_since(view);
        let commits = self.commit_log(gh, &repos, since).await;
        debug!(
            "investigation {}: {} issue/PR hit(s), {} commit(s) since {since}",
            view.subject.key,
            issues.len(),
            commits.len()
        );

        // 5. Shortlist locally, then get one cloud verdict over what survived.
        let shortlist = self
            .shortlist(&symptom_text, &issues, &commits, &repos)
            .await;

        // 6. Nothing plausible in the history — ask what code implements the
        // failing behavior instead.
        let code = if shortlist.is_empty() && self.cfg.code_search {
            self.search_code(gh, &symptoms, &repos).await
        } else {
            Vec::new()
        };

        let (candidates, verdict) = self
            .rank(view, &symptom_text, &shortlist, &code, gh)
            .await?;
        report.candidates = json!(candidates);
        report.verdict = verdict;
        self.store.put_root_cause(report)?;

        // 7. Judge the PRs this subject is *associated* with — the ones correlation
        // resolved through a branch, a CI run, or a closing keyword. Ranking a PR as
        // a candidate says it looks relevant; judging it says whether it actually
        // fixes the thing, which is the question worth answering. Doing the
        // association work and then not reading the PR would be pointless.
        self.judge_associated_prs(view, gh).await;
        Ok(())
    }

    /// Judge every pull request correlation has attached to this subject.
    ///
    /// Sources of association, in the order the hierarchy establishes them: a `pr`
    /// entity on the subject (a CI run resolved to its PR, or a PR notification), and
    /// any pull request that survived ranking. Deduplicated, and capped so a busy
    /// subject can't fan out into a dozen diff reads.
    async fn judge_associated_prs(&self, view: &SubjectView, gh: &GithubClient) {
        // The subject: the issue this subject is about, else the subject itself.
        let issue_entity = view.keys.iter().find(|e| e.kind == "issue");
        let subject = match issue_entity {
            Some(e) => match split_reference(&e.value) {
                Some((repo, number)) => crate::prfix::Subject {
                    key: e.value.clone(),
                    repo,
                    number: number as i64,
                    title: view.subject.title.clone(),
                },
                None => return,
            },
            // No issue: key the judgment on the subject so it still surfaces.
            None => crate::prfix::Subject {
                key: view.subject.key.to_string(),
                repo: String::new(),
                number: 0,
                title: view.subject.title.clone(),
            },
        };

        let mut seen: Vec<(String, u64)> = Vec::new();
        for e in view.keys.iter().filter(|e| e.kind == "pr") {
            if let Some(pair) = split_reference(&e.value) {
                if !seen.contains(&pair) {
                    seen.push(pair);
                }
            }
        }
        if seen.is_empty() {
            return;
        }
        seen.truncate(MAX_JUDGED_PRS);

        let body = self.evidence_block(view);
        for (repo, number) in seen {
            match self
                .pr_fixes
                .judge_known_pr(gh, &subject, &repo, number, &body, &[], false)
                .await
            {
                Some(fix) => info!(
                    "investigation {}: {} judged `{}`",
                    view.subject.key,
                    fix.reference(),
                    fix.verdict
                ),
                None => debug!(
                    "investigation {}: no usable judgment for {repo}#{number}",
                    view.subject.key
                ),
            }
        }
    }

    // ---- stage 1: symptoms --------------------------------------------------

    /// Turn a subject into searchable symptom terms, on the **local** model.
    ///
    /// Falls back to deterministic extraction (entity values plus the subject
    /// title) when no model answers, so an investigation is never blocked on the
    /// classifier being up.
    async fn extract_symptoms(&self, view: &SubjectView) -> Vec<String> {
        let evidence = self.evidence_block(view);
        let system =
            "You extract search terms from an incident so an engineer can find the bug that \
             caused it. Read the evidence and reply with ONLY a JSON array of 3–8 short search \
             terms: error strings, component or service names, symptom phrases, and identifiers \
             that appear in the evidence. Use the exact wording from the evidence — these terms go \
             straight into a GitHub search. No explanations, no invented terms.";
        let prompt = format!("Incident: {}\n\nEvidence:\n{evidence}", view.subject.title);
        let terms = match self
            .local
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(300),
            )
            .await
        {
            Ok(raw) => reasoner::extract_json(&raw)
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str())
                            .map(|t| t.trim().to_string())
                            .filter(|t| t.len() > 2)
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default(),
            Err(e) => {
                debug!("investigation: local symptom extraction failed: {e:#}");
                Vec::new()
            }
        };
        if !terms.is_empty() {
            return dedup(terms, 8);
        }
        dedup(deterministic_symptoms(view), 8)
    }

    /// The evidence the investigation reasons over: the subject's signals, plus any
    /// browser findings read off a linked dashboard. Browser findings come first —
    /// they carry the actual numbers, where the Slack message usually carries only
    /// "something's wrong".
    fn evidence_block(&self, view: &SubjectView) -> String {
        let mut ev = String::new();
        if let Ok(investigations) = self
            .store
            .browser_investigations_for_subject(view.subject.key.as_str())
        {
            for inv in investigations {
                if let Some(f) = inv.findings.filter(|f| !f.trim().is_empty()) {
                    ev.push_str(&format!("[dashboard {}]\n{f}\n\n", inv.url));
                }
            }
        }
        for s in &view.signals {
            ev.push_str(&format!(
                "- {} · {}: {}\n",
                s.source,
                s.occurred_at.to_rfc3339(),
                s.title
            ));
            if let Some(body) = s.body.as_deref().filter(|b| !b.trim().is_empty()) {
                ev.push_str(&format!("  {}\n", truncate(body.trim(), 500)));
            }
        }
        for e in &view.keys {
            ev.push_str(&format!("- entity {}={}\n", e.kind, e.value));
        }
        truncate(&ev, 8_000)
    }

    // ---- stages 3 & 4: search ------------------------------------------------

    /// Search each routed repo for the symptoms, cached per exact query.
    async fn search_issues(
        &self,
        gh: &GithubClient,
        symptoms: &[String],
        repos: &[String],
    ) -> Vec<IssueHit> {
        // GitHub search ANDs bare terms, so a long conjunction matches nothing.
        // Take the most distinctive few and let the ranking stage sort it out.
        let terms = symptoms
            .iter()
            .take(4)
            .map(|t| quote_if_phrase(t))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut all = Vec::new();
        for repo in repos {
            let query = format!("{terms} repo:{repo}");
            match self.store.get_issue_search(&query, SEARCH_TTL) {
                Ok(Some(cached)) => {
                    if let Ok(hits) = serde_json::from_value::<Vec<IssueHit>>(cached) {
                        all.extend(hits);
                        continue;
                    }
                }
                Ok(None) => {}
                Err(e) => debug!("investigation: issue cache read failed: {e:#}"),
            }
            match gh.search_issues(&query, ISSUE_LIMIT).await {
                Ok(hits) => {
                    if let Err(e) = self.store.put_issue_search(&query, &json!(hits)) {
                        debug!("investigation: issue cache write failed: {e:#}");
                    }
                    all.extend(hits);
                }
                Err(e) => warn!("investigation: issue search in {repo} failed: {e:#}"),
            }
        }
        all
    }

    /// The window to scan a commit log over: `commit_window` before the subject's
    /// earliest signal. A cause precedes its symptom.
    fn commit_since(&self, view: &SubjectView) -> DateTime<Utc> {
        let earliest = view
            .signals
            .iter()
            .map(|s| s.occurred_at)
            .min()
            .unwrap_or_else(Utc::now);
        let window = crate::config::parse_duration(&self.cfg.commit_window)
            .unwrap_or(Duration::from_secs(72 * 3600));
        earliest - ChronoDuration::from_std(window).unwrap_or(ChronoDuration::hours(72))
    }

    /// Commits from each repo since `since`, served from the cache when it already
    /// covers the window.
    async fn commit_log(
        &self,
        gh: &GithubClient,
        repos: &[String],
        since: DateTime<Utc>,
    ) -> Vec<CommitEntry> {
        let mut all = Vec::new();
        for repo in repos {
            let covered = self
                .store
                .commit_window(repo)
                .ok()
                .flatten()
                .is_some_and(|cached_since| cached_since <= since);
            if !covered {
                match gh.commits(repo, since, COMMIT_LIMIT).await {
                    Ok(commits) => {
                        if let Err(e) = self.store.put_commits(&commits) {
                            debug!("investigation: commit cache write failed: {e:#}");
                        }
                        if let Err(e) = self.store.set_commit_window(repo, since) {
                            debug!("investigation: commit window write failed: {e:#}");
                        }
                    }
                    Err(e) => warn!("investigation: commit log for {repo} failed: {e:#}"),
                }
            }
            match self.store.commits_since(repo, since, COMMIT_LIMIT) {
                Ok(commits) => all.extend(commits),
                Err(e) => warn!("investigation: reading cached commits for {repo}: {e:#}"),
            }
        }
        all
    }

    async fn search_code(
        &self,
        gh: &GithubClient,
        symptoms: &[String],
        repos: &[String],
    ) -> Vec<CodeHit> {
        // Code search wants identifiers, not prose: the longest single-token
        // symptoms are the ones that plausibly appear in source.
        let mut terms: Vec<&String> = symptoms.iter().filter(|s| !s.contains(' ')).collect();
        terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
        let query = terms
            .iter()
            .take(3)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" OR ");
        if query.is_empty() {
            return Vec::new();
        }
        match gh.search_code(&query, repos, CODE_LIMIT).await {
            Ok(hits) => hits,
            Err(e) => {
                debug!("investigation: code search failed: {e:#}");
                Vec::new()
            }
        }
    }

    // ---- stage 5: local shortlist, then one cloud verdict --------------------

    /// Narrow the wide search down on the **local** model. This is the pass that
    /// keeps the cloud bill flat: dozens of issue titles and commit subjects go in,
    /// at most `shortlist_size` plausible candidates come out.
    ///
    /// With no local model reachable, falls back to deterministic scoring — term
    /// overlap for issues, recency for commits — so the pipeline still shortlists.
    async fn shortlist(
        &self,
        symptom_text: &str,
        issues: &[IssueHit],
        commits: &[CommitEntry],
        repos: &[String],
    ) -> Vec<Candidate> {
        let mut pool: Vec<Candidate> = issues
            .iter()
            .map(Candidate::from_issue)
            .chain(commits.iter().map(Candidate::from_commit))
            .collect();
        if pool.is_empty() {
            return Vec::new();
        }
        // Deterministic pre-rank, so the local model sees the most plausible
        // material first and a truncated list is still the best material.
        let terms: Vec<String> = symptom_text
            .split(&[',', ' '][..])
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| t.len() > 3)
            .collect();
        for c in pool.iter_mut() {
            c.overlap = term_overlap(&c.searchable(), &terms);
        }
        pool.sort_by_key(|c| std::cmp::Reverse(c.overlap));
        pool.truncate(60);

        let want = self.cfg.shortlist_size.max(1);
        let mut catalog = String::new();
        for (i, c) in pool.iter().enumerate() {
            catalog.push_str(&format!("{i}. {}\n", c.line()));
        }
        let system = "You are filtering candidate causes for an incident. Given the symptoms and a \
             numbered list of existing issues, pull requests, and recent commits, reply with ONLY a \
             JSON array of the indices that could plausibly be related to these symptoms — most \
             likely first. Include a candidate if it touches the same component or describes the \
             same failure. Exclude unrelated routine work (dependency bumps, docs, formatting, \
             release chores) unless the symptoms point at them. Return [] if none are plausible.";
        let prompt = format!(
            "Symptoms: {symptom_text}\nRepositories: {}\n\nCandidates:\n{catalog}\n\nReturn at most {want} indices.",
            repos.join(", ")
        );
        let picked: Vec<usize> = match self
            .local
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(300),
            )
            .await
        {
            Ok(raw) => reasoner::extract_json(&raw)
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|n| n.as_u64())
                            .map(|n| n as usize)
                            .filter(|n| *n < pool.len())
                            .collect()
                    })
                })
                .unwrap_or_default(),
            Err(e) => {
                debug!("investigation: local shortlisting failed: {e:#}");
                Vec::new()
            }
        };
        if !picked.is_empty() {
            let mut out = Vec::new();
            for i in picked.into_iter().take(want) {
                out.push(pool[i].clone());
            }
            return out;
        }
        // No usable local verdict: keep the deterministic pre-rank, but only where
        // there was real term overlap — recency alone is not evidence.
        pool.retain(|c| c.overlap > 0);
        pool.truncate(want);
        pool
    }

    /// The one metered call: rank the shortlist and write the verdict.
    ///
    /// Degrades to the local shortlist (marked as such) when the cloud reasoner is
    /// unreachable, so an investigation always returns something citable.
    async fn rank(
        &self,
        view: &SubjectView,
        symptom_text: &str,
        shortlist: &[Candidate],
        code: &[CodeHit],
        gh: &GithubClient,
    ) -> Result<(Vec<Value>, Option<String>)> {
        if shortlist.is_empty() && code.is_empty() {
            return Ok((
                Vec::new(),
                Some(
                    "No existing issue, pull request, commit, or code match explains these \
                     symptoms. This looks unreported."
                        .into(),
                ),
            ));
        }

        // Enrich only what's about to be ranked: file lists make commit ranking far
        // sharper, and at shortlist size it's a handful of calls, not hundreds.
        let mut shortlist = shortlist.to_vec();
        for c in shortlist.iter_mut().filter(|c| c.kind == "commit") {
            if let (Some(repo), Some(sha)) = (c.repo.clone(), c.sha.clone()) {
                if let Ok(files) = gh.commit_files(&repo, &sha).await {
                    c.files = files.into_iter().take(20).collect();
                }
            }
        }

        let mut catalog = String::new();
        for (i, c) in shortlist.iter().enumerate() {
            catalog.push_str(&format!("{i}. {}\n", c.detailed()));
        }
        for (i, hit) in code.iter().enumerate() {
            catalog.push_str(&format!(
                "c{i}. [code] {}:{} — {}\n",
                hit.repo,
                hit.path,
                hit.fragments.join(" / ")
            ));
        }
        let evidence = self.evidence_block(view);
        let system = "You are MuggleBot identifying what caused an incident, for an on-call engineer. \
             You are given the incident evidence and a shortlist of candidate issues, pull requests, \
             commits, and code locations. Reply with ONLY JSON:\n\
             {\"verdict\": \"<2-3 sentences: the most likely cause and what to check to confirm it, \
             or plainly that the cause is not identifiable from this evidence>\", \
             \"candidates\": [{\"index\": <the number from the list>, \"confidence\": 0.0-1.0, \
             \"relation\": \"cause|fix|duplicate|context\", \"rationale\": \"<one sentence tying it \
             to specific evidence>\"}]}\n\
             Rules: include only candidates you can justify from the evidence — an empty array is a \
             valid and useful answer. `relation` is `cause` for a change that likely introduced the \
             problem, `fix` for one that addresses it, `duplicate` for an existing report of the same \
             thing, `context` for related-but-not-causal. Never invent a candidate that is not in the \
             list. Do not speculate beyond the evidence; say what is unknown.";
        let prompt = format!(
            "Incident: {}\nSymptoms: {symptom_text}\n\nEvidence:\n{evidence}\n\nCandidates:\n{catalog}",
            view.subject.title
        );
        let raw = match self
            .ranker
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(1200),
            )
            .await
        {
            Ok(raw) => raw,
            Err(e) => {
                // The local pass already produced citable candidates; surface them
                // rather than throwing the whole investigation away.
                warn!("investigation: cloud ranking unavailable: {e:#}");
                let candidates = shortlist
                    .iter()
                    .map(|c| c.to_value(0.3, "context", "Shortlisted locally; not yet ranked."))
                    .collect();
                return Ok((
                    candidates,
                    Some(format!(
                        "Local shortlist only — the cloud reasoner was unreachable ({}). \
                         Candidates are unranked.",
                        first_line(&format!("{e:#}"))
                    )),
                ));
            }
        };
        let parsed = reasoner::extract_json(&raw);
        let verdict = parsed
            .as_ref()
            .and_then(|v| v.get("verdict"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut out = Vec::new();
        if let Some(items) = parsed
            .as_ref()
            .and_then(|v| v.get("candidates"))
            .and_then(|v| v.as_array())
        {
            for item in items {
                let confidence = item
                    .get("confidence")
                    .and_then(|c| c.as_f64())
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0);
                let relation = item
                    .get("relation")
                    .and_then(|r| r.as_str())
                    .unwrap_or("context");
                let rationale = item
                    .get("rationale")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .trim();
                // Indices are validated against the list we actually sent, so a
                // hallucinated candidate is dropped rather than shown as evidence.
                match item.get("index") {
                    Some(Value::Number(n)) => {
                        if let Some(c) = n.as_u64().and_then(|i| shortlist.get(i as usize)) {
                            out.push(c.to_value(confidence, relation, rationale));
                        }
                    }
                    Some(Value::String(s)) => {
                        if let Some(hit) = s
                            .trim()
                            .strip_prefix('c')
                            .and_then(|i| i.parse::<usize>().ok())
                            .and_then(|i| code.get(i))
                        {
                            out.push(code_value(hit, confidence, relation, rationale));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok((out, verdict))
    }
}

/// A candidate cause before ranking.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// `issue`, `pull_request`, or `commit`.
    kind: String,
    repo: Option<String>,
    sha: Option<String>,
    number: Option<u64>,
    title: String,
    url: Option<String>,
    state: Option<String>,
    author: Option<String>,
    when: Option<String>,
    labels: Vec<String>,
    files: Vec<String>,
    body: Option<String>,
    /// Deterministic term-overlap score from the pre-rank.
    overlap: usize,
}

impl Candidate {
    fn from_issue(i: &IssueHit) -> Self {
        Self {
            kind: i.kind.clone(),
            repo: Some(i.repo.clone()),
            sha: None,
            number: Some(i.number),
            title: i.title.clone(),
            url: Some(i.url.clone()),
            state: Some(i.state.clone()),
            author: None,
            when: i.updated_at.clone().or_else(|| i.created_at.clone()),
            labels: i.labels.clone(),
            files: Vec::new(),
            body: i.body.clone(),
            overlap: 0,
        }
    }

    fn from_commit(c: &CommitEntry) -> Self {
        Self {
            kind: "commit".into(),
            repo: Some(c.full_name.clone()),
            sha: Some(c.sha.clone()),
            number: None,
            title: c.subject().to_string(),
            url: c.url.clone(),
            state: None,
            author: c.author.clone(),
            when: Some(c.committed_at.to_rfc3339()),
            labels: Vec::new(),
            files: c.files.clone(),
            body: Some(c.message.clone()),
            overlap: 0,
        }
    }

    /// A stable human reference: `owner/repo#12` or `owner/repo@abc1234`.
    fn reference(&self) -> String {
        let repo = self.repo.as_deref().unwrap_or("?");
        match (&self.number, &self.sha) {
            (Some(n), _) => format!("{repo}#{n}"),
            (_, Some(sha)) => format!("{repo}@{}", &sha[..sha.len().min(8)]),
            _ => repo.to_string(),
        }
    }

    /// Text the deterministic pre-rank scores against.
    fn searchable(&self) -> String {
        format!(
            "{} {} {} {}",
            self.title,
            self.body.as_deref().unwrap_or(""),
            self.labels.join(" "),
            self.files.join(" ")
        )
        .to_ascii_lowercase()
    }

    /// One line for the local shortlisting prompt — cheap and scannable.
    fn line(&self) -> String {
        let mut line = format!("[{}] {} — {}", self.kind, self.reference(), self.title);
        if let Some(state) = &self.state {
            line.push_str(&format!(" ({state})"));
        }
        if let Some(when) = &self.when {
            line.push_str(&format!(" [{}]", &when[..when.len().min(10)]));
        }
        truncate(&line, 300)
    }

    /// The fuller form for the cloud ranking prompt, where accuracy matters more
    /// than token count.
    fn detailed(&self) -> String {
        let mut out = self.line();
        if let Some(author) = &self.author {
            out.push_str(&format!("\n   author: {author}"));
        }
        if !self.labels.is_empty() {
            out.push_str(&format!("\n   labels: {}", self.labels.join(", ")));
        }
        if !self.files.is_empty() {
            out.push_str(&format!("\n   files: {}", self.files.join(", ")));
        }
        if let Some(body) = self.body.as_deref().filter(|b| !b.trim().is_empty()) {
            out.push_str(&format!("\n   {}", truncate(body.trim(), 400)));
        }
        out
    }

    fn to_value(&self, confidence: f64, relation: &str, rationale: &str) -> Value {
        json!({
            "kind": self.kind,
            "reference": self.reference(),
            "repo": self.repo,
            "number": self.number,
            "sha": self.sha,
            "title": self.title,
            "url": self.url,
            "state": self.state,
            "author": self.author,
            "when": self.when,
            "labels": self.labels,
            "files": self.files,
            "relation": relation,
            "confidence": confidence,
            "rationale": rationale,
        })
    }
}

fn code_value(hit: &CodeHit, confidence: f64, relation: &str, rationale: &str) -> Value {
    json!({
        "kind": "code",
        "reference": format!("{}:{}", hit.repo, hit.path),
        "repo": hit.repo,
        "number": Value::Null,
        "sha": Value::Null,
        "title": hit.path,
        "url": hit.url,
        "state": Value::Null,
        "author": Value::Null,
        "when": Value::Null,
        "labels": Vec::<String>::new(),
        "files": vec![hit.path.clone()],
        "fragments": hit.fragments,
        "relation": relation,
        "confidence": confidence,
        "rationale": rationale,
    })
}

// ---- helpers -----------------------------------------------------------------

/// Split `owner/repo#123` into its repo and number.
fn split_reference(value: &str) -> Option<(String, u64)> {
    let (repo, number) = value.rsplit_once('#')?;
    Some((repo.to_string(), number.trim().parse().ok()?))
}

/// A subject the operator has already dealt with. These are never sent to a cloud
/// reasoner — see [`Investigator::investigate`] and [`crate::correlation::Analyst`].
///
/// Kept as a free function so the call sites read the same as before; the rule
/// itself lives on [`Handled`].
pub fn is_handled(handled: Handled) -> bool {
    handled.is_handled()
}

pub fn state_label(handled: Handled) -> &'static str {
    handled.as_str()
}

/// Symptoms without a model: the entity values plus the distinctive words of the
/// subject title. Crude, but it keeps the pipeline running with Ollama down.
fn deterministic_symptoms(view: &SubjectView) -> Vec<String> {
    let mut out: Vec<String> = view
        .keys
        .iter()
        .filter(|e| e.kind != "person" && e.kind != "channel")
        .map(|e| e.value.clone())
        .collect();
    out.extend(
        view.subject
            .title
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-'))
            .filter(|w| w.len() > 4 && !STOPWORDS.contains(&w.to_ascii_lowercase().as_str()))
            .map(str::to_string),
    );
    out
}

const STOPWORDS: &[&str] = &[
    "alert", "error", "failed", "failure", "issue", "there", "which", "would", "should", "about",
    "after", "before", "again", "because", "cannot", "could",
];

/// How many symptom terms appear in a candidate's text.
fn term_overlap(text: &str, terms: &[String]) -> usize {
    terms.iter().filter(|t| text.contains(t.as_str())).count()
}

/// Wrap a multi-word symptom in quotes so GitHub searches it as a phrase.
fn quote_if_phrase(term: &str) -> String {
    if term.contains(' ') {
        format!("\"{}\"", term.replace('"', ""))
    } else {
        term.to_string()
    }
}

fn dedup(values: Vec<String>, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in values {
        let v = v.trim().to_string();
        if v.is_empty() || out.iter().any(|e| e.eq_ignore_ascii_case(&v)) {
            continue;
        }
        out.push(v);
        if out.len() >= max {
            break;
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

fn first_line(s: &str) -> String {
    truncate(s.lines().next().unwrap_or(s).trim(), 160)
}

/// Extract the citable references a root-cause report contains, for the summary
/// prompt's evidence block.
pub fn report_evidence(report: &RootCauseReport) -> String {
    let mut out = String::new();
    if let Some(verdict) = report.verdict.as_deref().filter(|v| !v.trim().is_empty()) {
        out.push_str(&format!("Root-cause assessment: {verdict}\n"));
    }
    for c in report.candidates.as_array().into_iter().flatten() {
        let reference = c.get("reference").and_then(|v| v.as_str()).unwrap_or("?");
        out.push_str(&format!(
            "[cause:{reference}] {} {} (confidence {:.2}) — {}\n",
            c.get("relation").and_then(|v| v.as_str()).unwrap_or("?"),
            c.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0),
            c.get("rationale").and_then(|v| v.as_str()).unwrap_or(""),
        ));
    }
    out
}

/// Signals whose links are worth a browser investigation, paired with the URL.
pub fn dashboard_links(sig: &Signal, matches: impl Fn(&str) -> bool) -> Option<&str> {
    sig.raw
        .get("urls")
        .and_then(|v| v.as_array())
        .and_then(|urls| {
            urls.iter()
                .filter_map(|v| v.as_str())
                .find(|url| matches(url))
        })
}

/// The investigation stack wired offline — no GitHub token, browser disabled — for
/// test harnesses that need a complete [`crate::tools::Tools`] without reaching the
/// network. Investigation tools then report themselves unavailable, which is the
/// same behavior an operator sees before storing a token.
#[cfg(test)]
pub fn offline_stack(
    store: Arc<Store>,
    attributor: Arc<Attributor>,
    reasoner: Arc<dyn Reasoner>,
) -> (
    Arc<Investigator>,
    Arc<RepoIndex>,
    Arc<crate::browser::BrowserDriver>,
) {
    let cfg = InvestigationCfg::default();
    let repos = Arc::new(RepoIndex::new(
        store.clone(),
        None,
        reasoner.clone(),
        None,
        cfg.clone(),
    ));
    let investigator = Arc::new(Investigator::new(
        store,
        attributor,
        repos.clone(),
        None,
        reasoner.clone(),
        reasoner,
        cfg,
    ));
    let browser = Arc::new(crate::browser::BrowserDriver::new(
        crate::config::Browser::default(),
    ));
    (investigator, repos, browser)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{ResolutionKey, Severity, SignalKind, Source};

    fn view(title: &str, keys: Vec<ResolutionKey>) -> SubjectView {
        let now = Utc::now();
        let key = crate::subject::SubjectKey::issue("o/r", 1);
        SubjectView {
            subject: crate::subject::Subject {
                rank: key.rank(),
                key,
                title: title.into(),
                summary: None,
                created_at: now,
                updated_at: now,
                last_reasoned_at: None,
                live: false,
                tags: vec![],
                tags_pinned: false,
                handled: Handled::Open,
                snoozed_until: None,
                same_as: None,
                parent: None,
                merge_key: None,
            },
            signals: vec![],
            keys,
            severity: Severity::Warning,
            edges: vec![],
            context: vec![],
            children: vec![],
            pull_requests: vec![],
            explanations: vec![],
            attention: crate::subject::Attention {
                needed: false,
                reason: None,
                decorated: Default::default(),
            },
        }
    }

    #[test]
    fn issue_references_split_into_repo_and_number() {
        assert_eq!(
            split_reference("restatedev/restate#412"),
            Some(("restatedev/restate".to_string(), 412))
        );
        assert_eq!(split_reference("no-number-here"), None);
        assert_eq!(split_reference("restatedev/restate#abc"), None);
    }

    #[test]
    fn handled_states_are_off_limits_to_cloud_reasoning() {
        assert!(is_handled(Handled::Snoozed));
        assert!(is_handled(Handled::Resolved));
        assert!(is_handled(Handled::Acknowledged));
        assert!(!is_handled(Handled::Open));
        assert!(!is_handled(Handled::Seen));
    }

    #[test]
    fn deterministic_symptoms_keep_entities_and_drop_noise_words() {
        let v = view(
            "invocation retry storm failed on partition processor",
            vec![
                ResolutionKey::new("service", "restate-worker"),
                ResolutionKey::new("person", "ben"),
            ],
        );
        let terms = deterministic_symptoms(&v);
        assert!(terms.contains(&"restate-worker".to_string()));
        assert!(
            !terms.contains(&"ben".to_string()),
            "people aren't symptoms"
        );
        assert!(terms.iter().any(|t| t == "invocation"));
        assert!(!terms.iter().any(|t| t == "failed"), "stopword");
    }

    #[test]
    fn phrases_are_quoted_for_github_search() {
        assert_eq!(quote_if_phrase("pool exhausted"), "\"pool exhausted\"");
        assert_eq!(quote_if_phrase("ECONNRESET"), "ECONNRESET");
    }

    #[test]
    fn candidate_reference_shapes() {
        let issue = Candidate::from_issue(&IssueHit {
            repo: "restatedev/restate".into(),
            number: 12,
            title: "pool exhausted".into(),
            state: "open".into(),
            kind: "issue".into(),
            url: "https://github.com/restatedev/restate/issues/12".into(),
            body: None,
            labels: vec!["bug".into()],
            created_at: None,
            updated_at: None,
            closed_at: None,
        });
        assert_eq!(issue.reference(), "restatedev/restate#12");

        let commit = Candidate::from_commit(&CommitEntry {
            full_name: "restatedev/restate".into(),
            sha: "abcdef1234567890".into(),
            author: Some("octocat".into()),
            committed_at: Utc::now(),
            message: "fix: bound the pool\n\nbody".into(),
            url: None,
            files: vec!["src/pool.rs".into()],
        });
        assert_eq!(commit.reference(), "restatedev/restate@abcdef12");
        assert_eq!(commit.title, "fix: bound the pool");
    }

    #[test]
    fn term_overlap_scores_matching_candidates_higher() {
        let terms = vec!["pool".to_string(), "exhausted".to_string()];
        assert_eq!(term_overlap("connection pool exhausted here", &terms), 2);
        assert_eq!(term_overlap("bump serde to 1.0.200", &terms), 0);
    }

    #[test]
    fn report_evidence_cites_every_candidate() {
        let report = RootCauseReport {
            subject_key: "thr/1".into(),
            status: "complete".into(),
            symptoms: vec!["pool exhausted".into()],
            repos: vec!["restatedev/restate".into()],
            candidates: json!([{
                "reference": "restatedev/restate#12",
                "relation": "cause",
                "title": "pool ceiling lowered",
                "confidence": 0.71,
                "rationale": "touches the pool config the alert names",
            }]),
            verdict: Some("Likely the pool ceiling change.".into()),
            error: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        let ev = report_evidence(&report);
        assert!(ev.contains("[cause:restatedev/restate#12]"));
        assert!(ev.contains("Likely the pool ceiling change."));
        assert!(ev.contains("0.71"));
    }

    #[test]
    fn dashboard_links_finds_the_matching_url() {
        let sig = Signal {
            id: "slack/1".into(),
            source: Source::Slack,
            external_id: "1".into(),
            kind: SignalKind::Alert,
            title: "alert".into(),
            body: None,
            url: None,
            actor: None,
            keys: vec![],
            severity: Severity::Warning,
            version: None,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: json!({ "urls": ["https://github.com/x", "https://x.grafana.net/d/1"] }),
            tags: vec![],
        };
        let found = dashboard_links(&sig, |u| u.contains("grafana"));
        assert_eq!(found, Some("https://x.grafana.net/d/1"));
        assert!(dashboard_links(&sig, |u| u.contains("datadog")).is_none());
    }
}
