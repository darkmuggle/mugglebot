//! Building the code index: commit summaries, component cards, and the dependency graph.
//!
//! The index is what turns "which repo and component is this issue about?" from a crawl
//! into a retrieval. Three pieces, all derived from the checkout and the commit log:
//!
//! - **A summary per commit**, keyed by sha. A sha is immutable, so each is computed
//!   exactly once and is correct forever — which is what makes eager indexing a one-time
//!   cost rather than a running bill.
//! - **A card per component**, re-derived when the component's code moves.
//! - **Dependency edges** between indexed repos, from manifests that are actually
//!   present.
//!
//! Everything here runs on the **local** model. Reading code to describe it is exactly the
//! work that shouldn't leave the machine, and there is one Ollama — so a queue of one is
//! faster than four concurrent requests as well as cheaper. That queue is not this module's
//! job: it lives in [`crate::reasoner::ollama`], at the resource, so indexing shares it with
//! triage and correlation instead of holding a private one and adding to their contention.
//!
//! Bounded batches, not one long pass. A first index over a large org is thousands of
//! model calls; doing it in one invocation would mean a single failure loses the lot and
//! nothing is usable until all of it finishes. In batches, every tick leaves the index
//! strictly more complete than it found it, and the scorer works off a partial index from
//! the first batch onwards.

use anyhow::Result;
use std::collections::BTreeSet;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::checkout::CheckoutCache;
use crate::components::{self, Component};
use crate::ecosystem;
use crate::embed::{self, Embedder};
use crate::reasoner::{CompletionRequest, Reasoner};
use crate::store::{ComponentSummary, Store};

/// Commits summarized per tick.
///
/// Small because the model calls are serialized across *every* repo being indexed (see
/// [`CodeIndexer::local`]): with six repos armed, a batch of forty is an hour inside one
/// durable step, and a step that long reports no progress and loses everything it was doing
/// on a restart. Throughput is unchanged — the permit is the bottleneck, not the cadence —
/// but each tick now journals, and `commits_done` visibly advances.
const COMMIT_BATCH: usize = 10;

/// Components per repo. A cap keeps a pathological monorepo from minting hundreds of
/// cards; the largest are kept, being the likeliest answers.
const MAX_COMPONENTS: usize = 40;

/// Component cards written per tick. Each is a local model call on a 30B-class model, so a
/// monorepo's worth in one invocation is twenty minutes inside a single durable step — long
/// enough that a restart throws away a journal with nothing in it. Bounded, the same tick
/// re-runs and skips what SQLite already has.
const COMPONENT_BATCH: usize = 8;

/// Commits fetched per tick, walking backwards from the oldest already cached.
///
/// The index needs the history to *exist* locally before it can summarize it, and nothing
/// else fetches it: the shallow checkout has one commit, and the investigation path only
/// ever caches a 72-hour window around a specific incident. So the indexer walks it itself.
const HISTORY_PAGE: usize = 100;

/// Cursor value meaning the backward history walk reached the repository's root commit.
///
/// `repo_commit_windows.since` is shared with the investigation cache and only ever moves
/// backwards. The Unix epoch is older than any Git repository, so it is an unambiguous,
/// durable completion marker without adding a second account of the cursor to SQLite.
fn full_history_sentinel() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(0, 0).expect("the Unix epoch is a valid timestamp")
}

/// File suffixes whose changes never explain a behavioural symptom. Skipping them is most
/// of what keeps a one-time index affordable — a lockfile bump is the single most common
/// commit in many repos and the least useful thing to have summarized.
const NOISE_SUFFIXES: &[&str] = &[
    "lock", ".lock", ".md", ".txt", ".svg", ".png", ".jpg", ".ico", ".snap", ".pot", ".po",
];

pub struct CodeIndexer {
    pub store: Arc<Store>,
    pub checkouts: Arc<CheckoutCache>,
    /// Resolves each repo's default branch and size before a shallow clone. `None` means
    /// no stored GitHub token, which is the one condition that disables indexing outright.
    pub github: Option<crate::github::GithubClient>,
    /// The local coder model. Never a cloud tier: this is bulk work over source.
    pub coder: Arc<dyn Reasoner>,
    pub embedder: Arc<dyn Embedder>,
}

/// What one indexing tick achieved, for the log and the progress panel.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct IndexProgress {
    pub components_written: usize,
    /// More components than one tick's batch. Keeps the fast cadence until they're all
    /// carded, so "complete" can't be reported off a repo that is one batch deep.
    pub components_pending: bool,
    /// Commits added to the local history this tick. Distinct from `commits_summarized`:
    /// fetching is a cheap API walk, summarizing is a model call apiece.
    pub commits_fetched: usize,
    /// Whether the history walk reached the repository's root commit. Until it has,
    /// `commits_total` is only what has been fetched so far, and done/total would otherwise
    /// read 100% on a repo whose history has barely been touched.
    pub history_complete: bool,
    pub commits_summarized: usize,
    pub commits_skipped: usize,
    pub dep_edges: usize,
    /// Summarized / total, so "still indexing" is distinguishable from "nothing to do".
    pub commits_done: i64,
    pub commits_total: i64,
    /// Component cards this repo now has, not just the ones written this tick. An absolute
    /// figure because the object publishes it as its own state for the board to read, and a
    /// per-tick delta would need someone to accumulate it — which is the second account of one
    /// fact that publishing state is meant to remove.
    pub components_total: u64,
    /// How far back the history walk has reached, RFC3339. `None` means not started.
    pub history_back_to: Option<String>,
    /// The newest cached commit — the other end of the walk, and the one an operator reads as
    /// "when did this repo last change".
    pub last_commit: Option<String>,
}

impl IndexProgress {
    pub fn complete(&self) -> bool {
        self.history_complete && !self.components_pending && self.commits_done >= self.commits_total
    }
}

impl CodeIndexer {
    /// Whether indexing can run at all.
    pub fn enabled(&self) -> bool {
        self.github.is_some() && crate::checkout::have_git()
    }

    /// Index one repo: dependency edges, then component cards, then a batch of commits.
    ///
    /// That order is cheapest-first, and it is what makes a partial index useful. Edges cost
    /// a manifest read. Components are few, change rarely, and alone answer "which
    /// component" — so one tick per repo is enough to route with. Commit summaries are the
    /// long tail, and the scorer works without them.
    pub async fn index_repo(
        &self,
        full_name: &str,
        known_repos: &[String],
    ) -> Result<IndexProgress> {
        let mut progress = IndexProgress::default();

        let Some(gh) = &self.github else {
            anyhow::bail!("code indexing is unavailable: no GitHub token stored");
        };
        if self.store.get_repo(full_name)?.is_none() {
            anyhow::bail!("{full_name} is not in the repo index");
        }
        if !crate::checkout::have_git() {
            anyhow::bail!("`git` is not on PATH");
        }
        let (branch, size_kb) = gh.repo_checkout_info(full_name).await?;
        let checkout = self.checkouts.ensure(full_name, &branch, size_kb).await?;

        // ---- dependency edges -------------------------------------------------
        // First, because it is the cheapest artifact and the most distinctive one. Two
        // manifest reads and no model call at all — whereas the component pass below is
        // minutes per card, and behind it the edges would land last despite being the only
        // thing that can reach a repo the issue's own words never mention.
        progress.dep_edges = self.write_deps(full_name, &checkout.path, known_repos)?;

        // ---- components -------------------------------------------------------
        let discovered = components::discover(&checkout.path, MAX_COMPONENTS);
        let existing = self.store.components_for_repo(full_name)?;
        for component in &discovered {
            // A component whose code hasn't moved keeps its card: re-summarizing an
            // unchanged component is the same waste the repo index avoids with
            // `indexed_sha`.
            let unchanged = existing.iter().any(|e| {
                e.path == component.path
                    && e.indexed_sha.as_deref() == Some(checkout.head_sha.as_str())
            });
            if unchanged {
                continue;
            }
            if let Err(e) = self
                .write_component(full_name, component, &checkout.head_sha)
                .await
            {
                warn!("component {}/{}: {e:#}", full_name, component.path);
                continue;
            }
            progress.components_written += 1;
            if progress.components_written >= COMPONENT_BATCH {
                progress.components_pending = true;
                break;
            }
        }

        // ---- commit history ---------------------------------------------------
        progress.commits_fetched = match self.fetch_history(full_name, gh).await {
            Ok(n) => n,
            Err(e) => {
                // A rate limit or a flaky page is transient, and the commits already cached
                // are still worth summarizing this tick.
                warn!("history {full_name}: {e:#}");
                0
            }
        };

        // ---- commits ----------------------------------------------------------
        let component_paths: Vec<String> = discovered.iter().map(|c| c.path.clone()).collect();
        for mut commit in self
            .store
            .commits_needing_summary(full_name, COMMIT_BATCH)?
        {
            // The commit list endpoint doesn't carry changed files, so a freshly fetched
            // commit arrives with none. That is *unknown*, not "changed nothing" — and
            // conflating the two would fill the index with placeholder rows and then report
            // itself complete, which is worse than an empty index because it looks done.
            if commit.files.is_empty() {
                match gh.commit_files(full_name, &commit.sha).await {
                    Ok(files) => {
                        commit.files = files;
                        // Cached on the row: the file list is immutable per sha, and this is
                        // one API call apiece.
                        self.store.put_commits(std::slice::from_ref(&commit))?;
                    }
                    Err(e) => {
                        // Left unsummarized. The next tick retries; a rate limit must not
                        // permanently record a commit as empty.
                        warn!("files for {full_name}@{}: {e:#}", commit.short_sha());
                        break;
                    }
                }
            }
            let code_files: Vec<&String> = commit.files.iter().filter(|f| !is_noise(f)).collect();
            if code_files.is_empty() {
                // Recorded as summarized-with-nothing rather than skipped, or every tick
                // re-examines the same lockfile bumps forever and never reaches the code.
                self.store.put_commit_summary(
                    full_name,
                    &commit.sha,
                    "(no code changes: dependency, documentation or asset files only)",
                    &[],
                    None,
                    None,
                )?;
                progress.commits_skipped += 1;
                continue;
            }
            let touched: Vec<String> = {
                let mut set = BTreeSet::new();
                for f in &code_files {
                    if let Some(c) = components::attribute_path(f, &component_paths) {
                        set.insert(c.clone());
                    }
                }
                set.into_iter().collect()
            };
            match self.summarize_commit(&commit, &code_files).await {
                Ok(summary) => {
                    let embedding = self.embed(&summary).await;
                    self.store.put_commit_summary(
                        full_name,
                        &commit.sha,
                        &summary,
                        &touched,
                        embedding.as_deref(),
                        Some("local"),
                    )?;
                    progress.commits_summarized += 1;
                }
                Err(e) => {
                    // Left unsummarized so the next tick retries it. A model that is down
                    // must not cause the commit to be permanently recorded as empty.
                    warn!("commit {}@{}: {e:#}", full_name, commit.short_sha());
                    break;
                }
            }
        }

        let (done, total) = self.store.commit_index_progress(full_name)?;
        progress.commits_done = done;
        progress.commits_total = total;
        progress.components_total = self.store.components_for_repo(full_name)?.len() as u64;
        progress.history_back_to = self.store.oldest_commit_at(full_name)?;
        let sentinel = full_history_sentinel();
        progress.history_complete = self
            .store
            .commit_window(full_name)?
            .is_some_and(|c| c <= sentinel);
        if progress.commits_summarized > 0 || progress.components_written > 0 {
            info!(
                "index {full_name}: {} component(s), {} commit(s) summarized, {} skipped, \
                 {} fetched ({done}/{total} commits done)",
                progress.components_written,
                progress.commits_summarized,
                progress.commits_skipped,
                progress.commits_fetched
            );
        }
        Ok(progress)
    }

    /// Walk one page further back through this repo's history.
    ///
    /// The cursor is `repo_commit_windows.since` — "history is cached back to here" — which
    /// the investigation path already maintains, so the two share a walk rather than each
    /// keeping its own idea of what has been fetched. It only ever moves backwards
    /// (`set_commit_window` takes the MIN), so a 72-hour investigation window can't undo
    /// a completed full-history walk.
    ///
    /// Returns how many commits were new to the cache.
    async fn fetch_history(
        &self,
        full_name: &str,
        gh: &crate::github::GithubClient,
    ) -> Result<usize> {
        let sentinel = full_history_sentinel();
        let cursor = self.store.commit_window(full_name)?;
        if cursor.is_some_and(|c| c <= sentinel) {
            return Ok(0);
        }
        let until = cursor.unwrap_or_else(chrono::Utc::now);
        let batch = gh.commits_before(full_name, until, HISTORY_PAGE).await?;
        if batch.is_empty() {
            // The root commit is behind us. Park the cursor at the completion sentinel so
            // this stops asking; without it, the repo would re-request an empty page on
            // every tick forever.
            self.store.set_commit_window(full_name, sentinel)?;
            return Ok(0);
        }
        let oldest = batch
            .iter()
            .map(|c| c.committed_at)
            .min()
            .unwrap_or(sentinel);
        let (_, before) = self.store.commit_index_progress(full_name)?;
        self.store.put_commits(&batch)?;
        let (_, after) = self.store.commit_index_progress(full_name)?;

        // A page that was entirely already-cached means the walk isn't advancing — the
        // boundary commit is inclusive, so a page of one repeat is the natural end. Jump the
        // cursor at the completion sentinel rather than looping on the same page.
        let advanced = oldest < until;
        self.store
            .set_commit_window(full_name, if advanced { oldest } else { sentinel })?;
        let added = (after - before).max(0) as usize;
        debug!(
            "history {full_name}: {} fetched, {added} new, back to {}",
            batch.len(),
            oldest.to_rfc3339()
        );
        Ok(added)
    }

    async fn write_component(
        &self,
        full_name: &str,
        component: &Component,
        head_sha: &str,
    ) -> Result<()> {
        let system = "You describe one component of a codebase in exactly two lines, for an \
             on-call engineer routing an incident. Answer with the two lines and nothing else.";
        let mut req = CompletionRequest::single(components::card_prompt(full_name, component));
        req.system = Some(system.to_string());
        req.max_tokens = 200;
        let card = self.coder.complete(&req).await?;
        let (purpose, symptoms) = components::split_card(&card);
        if purpose.is_none() && symptoms.is_none() {
            anyhow::bail!("the model did not answer with a PURPOSE/SYMPTOMS card");
        }
        // Embedded on the routing text, not the digest: the query is an issue's prose, so
        // similarity should be against prose.
        let text = format!(
            "{} {} {}",
            component.path,
            purpose.as_deref().unwrap_or(""),
            symptoms.as_deref().unwrap_or("")
        );
        let embedding = self.embed(&text).await;
        self.store.put_component_summary(
            &ComponentSummary {
                full_name: full_name.to_string(),
                path: component.path.clone(),
                purpose,
                symptoms,
                digest: Some(component.digest.clone()),
                indexed_sha: Some(head_sha.to_string()),
            },
            embedding.as_deref(),
        )?;
        Ok(())
    }

    async fn summarize_commit(
        &self,
        commit: &crate::store::CommitEntry,
        files: &[&String],
    ) -> Result<String> {
        let system = "You summarize one commit for an engineer searching for the cause of a \
             bug. One or two sentences, behavioural: what changed about how the system \
             behaves, not which lines moved. Name the mechanism if the message and paths \
             support one. If it is a pure refactor or a version bump, say so plainly — \
             \"no behavioural change\" is a useful answer here. Never invent a mechanism the \
             message and paths do not support.";
        let prompt = format!(
            "=== COMMIT {} ===\nAuthor: {}\nDate: {}\n\nMessage:\n{}\n\nFiles changed:\n{}\n\n\
             === YOUR TASK ===\nSummarize what this change does behaviourally.",
            commit.short_sha(),
            commit.author.as_deref().unwrap_or("unknown"),
            commit.committed_at.to_rfc3339(),
            truncate(&commit.message, 1_200),
            files
                .iter()
                .take(40)
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mut req = CompletionRequest::single(prompt);
        req.system = Some(system.to_string());
        req.max_tokens = 220;
        let out = self.coder.complete(&req).await?.trim().to_string();
        if out.is_empty() {
            anyhow::bail!("the model returned an empty summary");
        }
        Ok(out)
    }

    /// Resolve declared dependencies onto repos we actually index.
    ///
    /// Only edges to known repos are stored. An edge to `serde` is true and useless — the
    /// graph exists to propagate a score to somewhere the index can look, and nothing can
    /// look inside a crate we don't have.
    fn write_deps(
        &self,
        full_name: &str,
        checkout: &std::path::Path,
        known_repos: &[String],
    ) -> Result<usize> {
        let eco = ecosystem::detect(checkout);
        let source = eco
            .evidence
            .first()
            .cloned()
            .unwrap_or_else(|| "manifest".into());
        let mut edges: Vec<(String, String, String)> = Vec::new();
        for dep in &eco.dependencies {
            for repo in known_repos {
                if repo == full_name {
                    continue;
                }
                if repo_matches_dep(repo, dep) {
                    edges.push((repo.clone(), dep.clone(), source.clone()));
                }
            }
        }
        edges.sort();
        edges.dedup();
        let count = edges.len();
        self.store.put_repo_deps(full_name, &edges)?;
        if count > 0 {
            debug!("deps {full_name}: {count} edge(s) to indexed repos");
        }
        Ok(count)
    }

    async fn embed(&self, text: &str) -> Option<Vec<u8>> {
        match self.embedder.embed(text).await {
            Ok(v) if !v.is_empty() => Some(embed::to_blob(&v)),
            Ok(_) => None,
            Err(e) => {
                // Recall degrades to lexical matching, which still works. Not worth
                // failing an index over.
                debug!("embedding failed, storing without one: {e:#}");
                None
            }
        }
    }
}

/// Does a declared dependency name refer to this repo?
///
/// Matched on the repo's own name, both exact and as a suffix after a scope or a path
/// separator, so `@restatedev/restate-sdk`, `github.com/restatedev/restate`, and
/// `restate-sdk` all resolve. Deliberately not fuzzy: a wrong edge propagates a score to
/// an unrelated repo and reads as a real finding.
fn repo_matches_dep(full_name: &str, dep: &str) -> bool {
    let Some(name) = full_name.rsplit('/').next() else {
        return false;
    };
    let dep = dep.trim().trim_start_matches('@').to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if name.is_empty() || dep.is_empty() {
        return false;
    }
    dep == name || dep == full_name.to_ascii_lowercase() || dep.ends_with(&format!("/{name}"))
}

fn is_noise(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    // A file called exactly `Cargo.lock` / `package-lock.json` / `go.sum`, or with a
    // noise extension.
    file.ends_with("lock.json")
        || file == "go.sum"
        || NOISE_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `complete` gates the cadence: true drops the tick from every 30s to hourly. Three
    /// separate things can be unfinished, and each one used to be able to report done —
    /// most insidiously a repo whose history has never been fetched, which has 0 of 0
    /// commits summarized.
    #[test]
    fn nothing_is_complete_until_all_three_halves_are() {
        let done = IndexProgress {
            history_complete: true,
            commits_done: 10,
            commits_total: 10,
            ..Default::default()
        };
        assert!(done.complete());

        assert!(
            !IndexProgress {
                history_complete: false,
                ..done.clone()
            }
            .complete(),
            "an unwalked history is not a complete index"
        );
        assert!(
            !IndexProgress {
                components_pending: true,
                ..done.clone()
            }
            .complete(),
            "a repo one component batch deep is not done"
        );
        assert!(!IndexProgress {
            commits_done: 3,
            ..done.clone()
        }
        .complete());
        // The case that motivated the flag: no history at all reads as 0/0.
        assert!(
            !IndexProgress::default().complete(),
            "0 of 0 commits is not completeness"
        );
    }

    #[test]
    fn dependency_names_resolve_onto_repos_we_index() {
        // The forms a manifest actually uses.
        assert!(repo_matches_dep("restatedev/restate", "restate"));
        assert!(repo_matches_dep(
            "restatedev/restate-sdk",
            "@restatedev/restate-sdk"
        ));
        assert!(repo_matches_dep(
            "restatedev/restate",
            "github.com/restatedev/restate"
        ));
        assert!(repo_matches_dep("restatedev/restate", "restatedev/restate"));
    }

    /// `repo_matches_dep` is unit-tested on strings, but the strings have to arrive in the
    /// shape it expects. This pins the join: a real `package.json` through
    /// `ecosystem::detect` and out the other side as an edge to a repo we index.
    #[test]
    fn a_real_manifest_resolves_to_an_edge_end_to_end() {
        let dir = std::env::temp_dir().join("mugglebot-dep-edge-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{
                 "name": "website",
                 "dependencies": {
                   "@restatedev/vercel-ai-middleware": "^0.2.0",
                   "next": "^15.0.0"
                 }
               }"#,
        )
        .unwrap();

        let eco = ecosystem::detect(&dir);
        let known = [
            "restatedev/vercel-ai-middleware".to_string(),
            "restatedev/website".to_string(),
            "vercel/next.js".to_string(),
        ];
        let edges: Vec<(&str, &String)> = known
            .iter()
            .filter(|repo| *repo != "restatedev/website")
            .flat_map(|repo| {
                eco.dependencies
                    .iter()
                    .filter(move |dep| repo_matches_dep(repo, dep))
                    .map(move |dep| (repo.as_str(), dep))
            })
            .collect();

        assert_eq!(
            edges.len(),
            1,
            "expected exactly the middleware edge, got {edges:?}"
        );
        assert_eq!(edges[0].0, "restatedev/vercel-ai-middleware");
        assert_eq!(edges[0].1, "@restatedev/vercel-ai-middleware");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_near_miss_is_not_an_edge() {
        // A wrong edge propagates a score to an unrelated repo and reads as a real
        // finding, so matching is exact rather than fuzzy.
        assert!(!repo_matches_dep("restatedev/restate", "restate-sdk"));
        assert!(!repo_matches_dep("restatedev/restate", "restated"));
        assert!(!repo_matches_dep("restatedev/restate", "my-restate-fork"));
        assert!(!repo_matches_dep("restatedev/restate", ""));
    }

    #[test]
    fn lockfiles_and_docs_are_noise_but_source_is_not() {
        for noisy in [
            "Cargo.lock",
            "package-lock.json",
            "go.sum",
            "README.md",
            "docs/guide.md",
            "assets/logo.svg",
            "tests/snapshots/x.snap",
        ] {
            assert!(is_noise(noisy), "{noisy} should be noise");
        }
        for real in [
            "src/pool.rs",
            "crates/engine/src/lib.rs",
            "Cargo.toml",
            "package.json",
            "main.go",
        ] {
            assert!(!is_noise(real), "{real} should not be noise");
        }
    }

    #[test]
    fn progress_reports_completeness_so_partial_is_not_mistaken_for_done() {
        let mut p = IndexProgress {
            commits_done: 40,
            commits_total: 900,
            history_complete: true,
            ..Default::default()
        };
        assert!(!p.complete(), "a partial index must not read as complete");
        p.commits_done = 900;
        assert!(p.complete());
    }
}
