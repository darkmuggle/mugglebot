//! The repo index — the routing table from a symptom to the code that could
//! explain it.
//!
//! On startup (and on a slow refresh) MuggleBot lists the watched org's
//! repositories, checks each one out, and distills a two-line
//! *purpose + symptom keywords* card from its **code**. Those cards are the index.
//! When a Slack alert says "environment stuck provisioning", routing that to the
//! two or three repos worth searching is what makes the rest of the investigation
//! affordable — searching every repo would be slow, noisy, and rate-limited.
//!
//! **Why the code and not the README.** A README states intent, and intent goes
//! stale, turns aspirational, or is simply missing — plenty of real services have a
//! README that is one line and a badge. The directory layout, the manifests, and
//! the module names say what the thing actually *is*, and they can't drift from the
//! code because they are the code. A README, where one exists, is included as one
//! input among several rather than as the basis.
//!
//! Routing runs in two tiers, mirroring correlation's own shape:
//!
//! 1. **Deterministic keyword routes** (config `[investigation.routes]`) —
//!    "cloud" and "environment" mean `restate-cloud`, "restate" means `restate`.
//!    These always apply and never wait on a model.
//! 2. **The model reads the index** — for everything else, the reasoner picks
//!    from the summarized cards. With no reachable reasoner this tier is skipped
//!    and tier 1 (plus `default_repos`) stands, so investigation degrades rather
//!    than breaking.
//!
//! Characterizations are cached against the commit they were built from
//! (`indexed_sha`), so a refresh only re-reads repos whose code has actually moved.
//! A re-sync of an unchanged org costs a shallow `git fetch` per repo and *no* model
//! calls.
//!
//! Everything in this module reasons on the **local** classifier (Ollama). An org
//! crawl is dozens of summaries and routing runs on every investigation; that
//! volume belongs on-device. Cloud reasoning enters only later, once
//! [`crate::rootcause`] has narrowed the field to a shortlist.

use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// `meta` key holding how many repos the last **complete** org enumeration saw.
///
/// The completeness signal for the repo index, and deliberately a count from GitHub rather
/// than a boolean: comparing it against the rows actually present distinguishes "finished" from
/// "finished once, and then a crawl was interrupted after pruning".
pub const ENUMERATED_KEY: &str = "repo_index_enumerated";

/// `meta` key holding how many repos still want a code-derived card after the last crawl.
///
/// Written by the crawl itself rather than re-derived in SQL, because "does this repo want a
/// card?" is `worth_indexing`'s judgment — archived and long-stale repos deliberately never
/// get one, and a SQL predicate duplicating that rule would drift from it and hold the
/// catch-up cadence on forever.
pub const PENDING_KEY: &str = "repo_index_pending";

/// Repo cards written per crawl.
///
/// Sized to finish **well inside** the scheduler's catch-up cadence, because two crawls
/// running at once would pick the same uncarded repos in the same order and clone them into
/// the same directory — a corrupt working tree, which is precisely what the `checkout` vqueue
/// limit key protects the per-repo indexers from and what nothing protects this from.
///
/// Batch size is not a throughput lever here. Every card queues on the single GPU permit, so
/// total rate is fixed by the model; a larger batch only makes one invocation longer and the
/// overlap more likely. Keeping it small also means each run commits its progress, so a
/// restart costs one card rather than the whole crawl.
const CHARACTERIZE_BATCH: usize = 2;

/// Repos classified by the model per crawl.
///
/// Higher than the card batch because the call is far cheaper — one word from metadata, no
/// checkout — but still bounded, because every local call queues on the single GPU permit and
/// would otherwise sit in front of component carding.
const KIND_BATCH: usize = 8;

use crate::config::Investigation as InvestigationCfg;
use crate::github::GithubClient;
use crate::reasoner::{CompletionRequest, Reasoner};
use crate::store::{RepoEntry, RepoKind, Store};

/// Cap on repos handed to one search pass. More than this and the search API's
/// rate limit, not the reasoning, becomes the bottleneck.
pub const MAX_ROUTED_REPOS: usize = 4;

/// Characters of the structural digest shown to the model. Sized for a local
/// model's context window — the digest is names and layout, so this covers a large
/// tree without crowding out the instructions.
const DIGEST_CHARS: usize = 8_000;

/// Files walked while building a digest — a monorepo must not stall a sync.
const MAX_DIGEST_FILES: usize = 20_000;
/// Source paths listed in the digest.
const MAX_DIGEST_NAMES: usize = 400;
/// Characters kept from each manifest.
const MARKER_CHARS: usize = 700;

/// Directories that say nothing about what a repo does.
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
    "testdata",
    "fixtures",
];

/// Files whose *presence and name* identify a project: manifests, entry points,
/// deployment descriptors.
///
/// Note what is **absent**: `README.md`. Including it produced cards that quoted
/// marketing prose ("build resilient applications without a PhD") instead of naming
/// components — the exact failure this module exists to avoid. A manifest names the
/// package and its dependencies, which is domain vocabulary; a README states
/// positioning, which is not.
const MARKER_FILES: &[&str] = &[
    "cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "gemfile",
    "dockerfile",
    "chart.yaml",
    "main.tf",
];

pub struct RepoIndex {
    store: Arc<Store>,
    github: Option<GithubClient>,
    /// The **local** classifier (Ollama). Crawling an org's READMEs is dozens of
    /// summarization calls and routing runs on every investigation — exactly the
    /// mechanical, high-volume work that belongs on-device rather than on a
    /// metered cloud model.
    reasoner: Arc<dyn Reasoner>,
    /// Checkouts, so the index can read code rather than trust a README.
    checkouts: Option<Arc<crate::checkout::CheckoutCache>>,
    cfg: InvestigationCfg,
}

impl RepoIndex {
    pub fn new(
        store: Arc<Store>,
        token: Option<String>,
        reasoner: Arc<dyn Reasoner>,
        checkouts: Option<Arc<crate::checkout::CheckoutCache>>,
        cfg: InvestigationCfg,
    ) -> Self {
        // Background: crawling 147 repos is the other bulk consumer of the budget.
        let github = token.and_then(
            |t| match GithubClient::new(t).map(GithubClient::background) {
                Ok(c) => Some(c),
                Err(e) => {
                    warn!("repo index: GitHub client unavailable: {e:#}");
                    None
                }
            },
        );
        Self {
            store,
            github,
            reasoner,
            checkouts,
            cfg,
        }
    }

    /// Whether investigation can reach GitHub at all. Without a token the index
    /// stays empty and routing falls back to the configured repos.
    pub fn online(&self) -> bool {
        self.github.is_some()
    }

    pub fn list(&self) -> Result<Vec<RepoEntry>> {
        self.store.list_repos()
    }

    /// Refresh the index: list the org, then characterize each repo **from its
    /// code**, skipping anything whose code hasn't moved since it was last read.
    ///
    /// Returns how many repos were (re-)characterized. Archived repos and (when
    /// `skip_stale_repos` is set) long-dormant ones are indexed as metadata only —
    /// kept routable, without paying to check them out.
    pub async fn sync(&self) -> Result<usize> {
        let Some(gh) = &self.github else {
            debug!("repo index: no GitHub token; skipping sync");
            return Ok(0);
        };
        let org = &self.cfg.org;
        let repos = gh
            .list_org_repos(org)
            .await
            .with_context(|| format!("listing repos for org '{org}'"))?;
        info!("repo index: {} repo(s) in {org}", repos.len());

        let mut seen = BTreeSet::new();
        let mut characterized = 0usize;
        let mut deferred = 0usize;
        let now = Utc::now().to_rfc3339();

        // ---- pass 1: the list ------------------------------------------------------
        //
        // Every repo gets a row before *any* model runs or any repo is cloned. This is a
        // separate pass rather than a step inside the loop below, and that distinction is the
        // whole fix: with the stub written inline, rows 4..147 waited behind repo 3's clone,
        // and an interrupted crawl left the index knowing about 2 repos out of 147 — which
        // starves component carding, commit summaries, the dependency graph and scoring at
        // once, because the code indexer can only arm repos that are in this table.
        //
        // Two API pages and a metadata card apiece. Cheap enough to redo every crawl.
        for meta in &repos {
            seen.insert(meta.full_name.clone());
            if self.store.get_repo(&meta.full_name)?.is_some() {
                continue;
            }
            let mut stub = RepoEntry {
                full_name: meta.full_name.clone(),
                description: meta.description.clone(),
                topics: meta.topics.clone(),
                language: meta.language.clone(),
                archived: meta.archived,
                pushed_at: meta.pushed_at.clone(),
                readme_etag: None,
                readme: None,
                summary: None,
                indexed_sha: None,
                digest: None,
                // Auto-characterized from the name and topics; `None` when neither says, which
                // the board treats as code until someone tags it.
                kind: RepoKind::guess(&meta.full_name, &meta.topics),
                kind_pinned: false,
                fetched_at: now.clone(),
            };
            // Routable from metadata alone until a real card is written over it.
            stub.summary = Some(self.describe_from_metadata(&stub));
            self.store.put_repo(&stub, false)?;
        }
        // The list is complete from here, whatever happens to the rest of this crawl.
        self.store
            .meta_put(ENUMERATED_KEY, seen.len().to_string().as_bytes())?;

        // ---- pass 2: the cards -----------------------------------------------------
        for meta in repos {
            let existing = self.store.get_repo(&meta.full_name)?;
            let mut entry = RepoEntry {
                full_name: meta.full_name.clone(),
                description: meta.description.clone(),
                topics: meta.topics.clone(),
                language: meta.language.clone(),
                archived: meta.archived,
                pushed_at: meta.pushed_at.clone(),
                readme_etag: existing.as_ref().and_then(|e| e.readme_etag.clone()),
                readme: existing.as_ref().and_then(|e| e.readme.clone()),
                summary: existing.as_ref().and_then(|e| e.summary.clone()),
                indexed_sha: existing.as_ref().and_then(|e| e.indexed_sha.clone()),
                digest: existing.as_ref().and_then(|e| e.digest.clone()),
                // A pinned kind is the operator's answer and survives the crawl; otherwise
                // re-guess, so a repo renamed to `foo-examples` gets re-characterized.
                kind: match existing.as_ref() {
                    Some(e) if e.kind_pinned => e.kind,
                    _ => RepoKind::guess(&meta.full_name, &meta.topics),
                },
                kind_pinned: existing.as_ref().is_some_and(|e| e.kind_pinned),
                fetched_at: now.clone(),
            };

            if !self.worth_indexing(&meta.archived, meta.pushed_at.as_deref()) {
                // Metadata only — routable, but not worth a checkout.
                if entry.summary.is_none() {
                    entry.summary = Some(self.describe_from_metadata(&entry));
                    self.store.put_repo(&entry, true)?;
                } else {
                    self.store.put_repo(&entry, false)?;
                }
                continue;
            }

            // Bounded per run, for the reason every other batch here is bounded: a crawl
            // that characterizes 147 repos on a local model is hours inside one invocation,
            // reports nothing until it finishes, and loses all of it on a restart.
            if characterized >= CHARACTERIZE_BATCH {
                self.store.put_repo(&entry, false)?;
                deferred += 1;
                continue;
            }
            match self.characterize_repo(gh, &mut entry).await {
                Ok(true) => {
                    self.store.put_repo(&entry, true)?;
                    characterized += 1;
                }
                // Code unchanged since the last read: the stored characterization
                // still describes this commit, so there is nothing to redo.
                Ok(false) => self.store.put_repo(&entry, false)?,
                Err(e) => {
                    warn!("repo index: {} not characterized: {e:#}", meta.full_name);
                    // Never leave a repo unroutable just because the checkout
                    // failed — metadata still names it.
                    if entry.summary.is_none() {
                        entry.summary = Some(self.describe_from_metadata(&entry));
                        self.store.put_repo(&entry, true)?;
                    } else {
                        self.store.put_repo(&entry, false)?;
                    }
                }
            }
        }
        let pruned = self.store.prune_repos(&seen)?;
        if pruned > 0 {
            info!("repo index: pruned {pruned} repo(s) no longer in {org}");
        }
        // ---- pass 3: what the untagged ones are for ---------------------------------
        //
        // After the cards, because a repo with no component card is more useful to fix than one
        // with no label. Bounded, and only for repos the keyword guess declined.
        let mut classified = 0usize;
        for repo in self.store.list_repos()? {
            if classified >= KIND_BATCH {
                break;
            }
            if repo.kind.is_some() || repo.kind_pinned || !seen.contains(&repo.full_name) {
                continue;
            }
            if let Some(kind) = self.classify_kind(&repo).await {
                // Unpinned: a model guess stays revisable, and only a human's answer is pinned.
                self.store.put_repo_kind_guess(&repo.full_name, kind)?;
                classified += 1;
                debug!("repo kind: {} is {}", repo.full_name, kind.as_str());
            }
        }
        if classified > 0 {
            info!("repo index: classified {classified} untagged repo(s) with the local model");
        }

        // How much carding is still owed. Written *last*, so it records having finished the
        // pass rather than having started it — an interrupted crawl leaves the previous
        // (higher) count, which is what keeps the scheduler on its catch-up cadence instead
        // of dropping to daily. See `Scheduler::cadence_now`.
        self.store
            .meta_put(PENDING_KEY, deferred.to_string().as_bytes())?;
        if deferred > 0 {
            info!(
                "repo index: characterized {characterized}, {deferred} repo(s) deferred to the \
                 next pass"
            );
        }
        Ok(characterized)
    }

    /// Check a repo out and characterize what its code actually does.
    ///
    /// Returns `false` when the commit is the one already characterized — the
    /// cache hit, and the reason a re-sync of an unchanged org is nearly free.
    ///
    /// Reading the code rather than the README is the point: a README states
    /// intent, and intent goes stale, gets aspirational, or is simply missing. The
    /// directory layout, the manifests, and the module names say what the thing
    /// *is*. A README, when present, is included as one input among several rather
    /// than as the basis.
    async fn characterize_repo(&self, gh: &GithubClient, entry: &mut RepoEntry) -> Result<bool> {
        let Some(checkouts) = &self.checkouts else {
            anyhow::bail!("no checkout cache configured");
        };
        if !crate::checkout::have_git() {
            anyhow::bail!("`git` is not on PATH");
        }
        let (branch, size_kb) = gh.repo_checkout_info(&entry.full_name).await?;
        let checkout = checkouts.ensure(&entry.full_name, &branch, size_kb).await?;

        if entry.indexed_sha.as_deref() == Some(checkout.head_sha.as_str())
            && entry.summary.is_some()
        {
            debug!(
                "repo index: {} unchanged at {}",
                entry.full_name, checkout.head_sha
            );
            return Ok(false);
        }

        let digest = code_digest(&checkout.path);
        entry.indexed_sha = Some(checkout.head_sha.clone());
        entry.digest = Some(digest.clone());
        entry.summary = Some(self.summarize_code(entry, &digest).await);
        Ok(true)
    }

    /// Turn a structural digest of the tree into the routing card.
    async fn summarize_code(&self, entry: &RepoEntry, digest: &str) -> String {
        let system = "You are indexing a software organization's repositories so an ops agent can \
             route an incident symptom to the right codebase. You are given a structural digest of \
             a repository: its layout, manifests, and module/file names. Output at most two \
             lines:\n\
             PURPOSE: one sentence — what this repository is and what it runs, judged from the \
             structure. Describe the system, not its value proposition.\n\
             SYMPTOMS: a comma-separated list of 5-15 routing terms, EVERY ONE OF WHICH APPEARS \
             VERBATIM in the digest above — module names, directory names, package names, service \
             names, binary/CLI names. These are matched literally against incident text, so a term \
             that isn't in the code is worse than no term at all.\n\
             Forbidden in SYMPTOMS: invented failure descriptions (\"service not loading\", \"slow \
             performance\", \"broken links\"), generic technology words that would match any \
             repository (\"api\", \"server\", \"database\", \"typescript\"), and marketing language. \
             If the digest is too sparse to name real components, write SYMPTOMS with just the \
             repository's own name rather than padding it. No preamble, no markdown.";
        let prompt = format!(
            "Repository: {}\nGitHub description: {}\nTopics: {}\nPrimary language: {}\n\n\
             === STRUCTURE ===\n{}",
            entry.full_name,
            entry.description.as_deref().unwrap_or("(none)"),
            entry.topics.join(", "),
            entry.language.as_deref().unwrap_or("(unknown)"),
            truncate(digest, DIGEST_CHARS),
        );
        match self
            .reasoner
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(300),
            )
            .await
        {
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            Ok(_) => self.describe_from_metadata(entry),
            Err(e) => {
                debug!(
                    "repo index: characterizing {} skipped: {e:#}",
                    entry.full_name
                );
                self.describe_from_metadata(entry)
            }
        }
    }

    /// A repo earns a checkout + characterization if it's live code. Archived repos
    /// and (when configured) long-dormant ones don't.
    fn worth_indexing(&self, archived: &bool, pushed_at: Option<&str>) -> bool {
        if *archived {
            return false;
        }
        if !self.cfg.skip_stale_repos {
            return true;
        }
        let Some(pushed) = pushed_at.and_then(|p| chrono::DateTime::parse_from_rfc3339(p).ok())
        else {
            // No push timestamp — index it rather than guess it's dead.
            return true;
        };
        let age = Utc::now().signed_duration_since(pushed.with_timezone(&Utc));
        age.num_days() <= self.cfg.stale_repo_days
    }

    /// The deterministic card: what GitHub already tells us. Used when there's no
    /// README and when no reasoner answers.
    /// Ask the local model what an untagged repo is for.
    ///
    /// Only for repos the name-and-topics guess declined. That guess deliberately covers just the
    /// unambiguous cases, which leaves a long tail — `restatedev/cli`, `restatedev/skills`,
    /// `restatedev/relay` — where the answer is knowable from the description but not from a
    /// keyword. Asking a model is exactly right for that: it is a one-line judgment over metadata
    /// already in hand, with a bounded set of answers.
    ///
    /// Stored **unpinned**, so a human still overrides it and a later crawl can revise it. A model
    /// guess is a guess; only the operator's answer is pinned.
    ///
    /// No checkout and no code: name, description, topics and the existing card are enough to tell
    /// a demo from a service, and reading a repository to answer it would cost minutes for a
    /// three-way classification.
    async fn classify_kind(&self, entry: &RepoEntry) -> Option<RepoKind> {
        let system = "You classify what a code repository is FOR. Reply with ONLY one word: \
             `code`, `example`, or `docs`.\n\
             - `example`: samples, demos, templates, starters, tutorials, playgrounds — things \
               written to be read and copied, not run in production.\n\
             - `docs`: documentation, websites, specs, handbooks.\n\
             - `code`: everything else — services, libraries, SDKs, CLIs, operators, \
               infrastructure. This is the default: if it could plausibly run in production, it \
               is `code`.";
        let prompt = format!(
            "Repository: {}\nDescription: {}\nTopics: {}\nLanguage: {}\n\nWhat is it for?",
            entry.full_name,
            entry.description.as_deref().unwrap_or("(none)"),
            if entry.topics.is_empty() {
                "(none)".to_string()
            } else {
                entry.topics.join(", ")
            },
            entry.language.as_deref().unwrap_or("(unknown)")
        );
        let mut req = CompletionRequest::single(prompt);
        req.system = Some(system.to_string());
        // One word. A larger budget invites a paragraph that then has to be parsed out of.
        req.max_tokens = 8;
        let raw = match self.reasoner.complete(&req).await {
            Ok(r) => r,
            Err(e) => {
                debug!(
                    "repo kind for {}: model unavailable ({e:#})",
                    entry.full_name
                );
                return None;
            }
        };
        // An unparseable answer means "unclassified", not "code": leaving it NULL keeps the repo
        // in the retry set, whereas defaulting to code would silently settle it on a non-answer.
        let kind = RepoKind::parse(raw.trim());
        if kind.is_none() {
            debug!(
                "repo kind for {}: model said {:?}, which is not one of the three",
                entry.full_name,
                raw.trim()
            );
        }
        kind
    }

    fn describe_from_metadata(&self, entry: &RepoEntry) -> String {
        let mut card = format!(
            "PURPOSE: {}",
            entry
                .description
                .as_deref()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or("(no description)")
        );
        let mut symptoms: Vec<String> = entry.topics.clone();
        // The repo's own name is the most reliable routing term there is.
        if let Some(short) = entry.full_name.split('/').nth(1) {
            symptoms.insert(0, short.to_string());
        }
        if let Some(lang) = &entry.language {
            symptoms.push(lang.clone());
        }
        card.push_str(&format!("\nSYMPTOMS: {}", symptoms.join(", ")));
        card
    }

    /// Route a symptom description to the repos worth searching.
    ///
    /// Deterministic keyword routes come first and always apply; the reasoner then
    /// fills the remaining slots from the index. The result is capped at
    /// [`MAX_ROUTED_REPOS`] and every entry is verified to exist in the index (or
    /// to be an explicitly configured repo), so a hallucinated repo name never
    /// reaches the GitHub API.
    pub async fn route(&self, symptoms: &str) -> Result<Vec<String>> {
        let index = self.store.list_repos()?;
        let known: BTreeSet<&str> = index.iter().map(|r| r.full_name.as_str()).collect();
        let mut routed: Vec<String> = Vec::new();

        // Tier 1 — configured keyword routes.
        let haystack = symptoms.to_ascii_lowercase();
        for (keyword, repos) in &self.cfg.routes {
            if !haystack.contains(&keyword.to_ascii_lowercase()) {
                continue;
            }
            for repo in repos {
                push_unique(&mut routed, repo);
            }
        }

        // Tier 2 — the model reads the index. Skipped when tier 1 already filled
        // the budget, or when there's nothing indexed to choose from.
        if routed.len() < MAX_ROUTED_REPOS && !index.is_empty() {
            match self.ask_index(symptoms, &index).await {
                Ok(picks) => {
                    for pick in picks {
                        if routed.len() >= MAX_ROUTED_REPOS {
                            break;
                        }
                        if known.contains(pick.as_str()) {
                            push_unique(&mut routed, &pick);
                        } else {
                            debug!("repo index: ignoring unknown routed repo '{pick}'");
                        }
                    }
                }
                Err(e) => debug!("repo index: model routing skipped: {e:#}"),
            }
        }

        // Nothing matched and nothing was picked — fall back to the configured
        // repos so an investigation still has somewhere to look.
        if routed.is_empty() {
            for repo in &self.cfg.default_repos {
                push_unique(&mut routed, repo);
            }
        }
        routed.truncate(MAX_ROUTED_REPOS);
        Ok(routed)
    }

    /// Ask the reasoner which indexed repos match the symptom. Returns full names
    /// in confidence order.
    async fn ask_index(&self, symptoms: &str, index: &[RepoEntry]) -> Result<Vec<String>> {
        let mut catalog = String::new();
        for r in index {
            let card = r
                .summary
                .as_deref()
                .unwrap_or("(not summarized)")
                .replace('\n', " ");
            catalog.push_str(&format!("- {}: {}\n", r.full_name, truncate(&card, 400)));
        }
        let system = "You route an incident symptom to the repositories whose code could explain it. \
             Given the symptom and a catalog of repositories, reply with ONLY a JSON array of at most \
             3 repository full names, most likely first, drawn verbatim from the catalog. Prefer \
             precision over coverage: return [] rather than guessing. No prose.";
        let prompt = format!("Symptom:\n{symptoms}\n\nRepositories:\n{catalog}");
        let raw = self
            .reasoner
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(200),
            )
            .await?;
        let Some(value) = crate::reasoner::extract_json(&raw) else {
            return Ok(Vec::new());
        };
        Ok(value
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Build a structural digest of a checked-out tree: what the code *is*, compressed
/// enough to fit a local model's context.
///
/// Deliberately **names and layout, not contents**. Two reasons. Directory and
/// module names are the highest-signal-per-token description a codebase offers —
/// `crates/ingress/src/services/proxy/` tells you more per character than any
/// paragraph of prose. And it keeps the digest bounded regardless of repo size, so
/// indexing a monorepo costs the same as indexing a small service.
///
/// Manifests are the exception: their first lines carry the package name,
/// description, and dependency list, which name the domain directly.
fn code_digest(root: &Path) -> String {
    let mut markers: Vec<String> = Vec::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut by_ext: std::collections::BTreeMap<String, usize> = Default::default();
    let mut files: Vec<String> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut walked = 0usize;

    while let Some((dir, depth)) = stack.pop() {
        if walked >= MAX_DIGEST_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if path.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(root) {
                    // Layout matters near the top; deep leaf directories are noise.
                    if depth < 3 {
                        dirs.insert(rel.to_string_lossy().to_string());
                    }
                }
                stack.push((path, depth + 1));
                continue;
            }
            walked += 1;
            if MARKER_FILES.contains(&name.as_str()) {
                if let Some(excerpt) = read_head(&path, MARKER_CHARS) {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    markers.push(format!("--- {rel} ---\n{excerpt}"));
                }
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if !ext.is_empty() {
                *by_ext.entry(ext).or_default() += 1;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                if files.len() < MAX_DIGEST_NAMES {
                    files.push(rel.to_string_lossy().to_string());
                }
            }
        }
    }

    let mut out = String::new();
    let mut langs: Vec<(String, usize)> = by_ext.into_iter().collect();
    langs.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    if !langs.is_empty() {
        out.push_str("File types: ");
        out.push_str(
            &langs
                .iter()
                .take(10)
                .map(|(ext, n)| format!(".{ext} ({n})"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push_str("\n\n");
    }
    if !dirs.is_empty() {
        out.push_str("Layout:\n");
        for d in dirs.iter().take(120) {
            out.push_str(&format!("  {d}/\n"));
        }
        out.push('\n');
    }
    if !markers.is_empty() {
        out.push_str("Manifests:\n");
        for m in markers.iter().take(8) {
            out.push_str(m);
            out.push('\n');
        }
        out.push('\n');
    }
    if !files.is_empty() {
        files.sort();
        out.push_str("Source files:\n");
        for f in files.iter().take(200) {
            out.push_str(&format!("  {f}\n"));
        }
    }
    if out.trim().is_empty() {
        return "(the repository appears to contain no readable source)".into();
    }
    out
}

/// First `max` characters of a file, if it's readable text.
fn read_head(path: &Path, max: usize) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    Some(truncate(body.trim(), max))
}

fn push_unique(out: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if value.is_empty() || out.iter().any(|v| v == value) {
        return;
    }
    out.push(value.to_string());
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    /// The model classifier only runs where the keyword guess declined, and only its three
    /// answers count.
    ///
    /// An unparseable answer must leave the kind NULL rather than defaulting to `code`: NULL keeps
    /// the repo in the retry set, while a default silently settles it on a non-answer.
    #[test]
    fn only_the_three_kinds_are_accepted_from_a_model() {
        assert_eq!(RepoKind::parse("code"), Some(RepoKind::Code));
        assert_eq!(RepoKind::parse("example"), Some(RepoKind::Example));
        assert_eq!(RepoKind::parse("docs"), Some(RepoKind::Docs));
        // The shapes a model actually emits around a one-word answer.
        assert_eq!(RepoKind::parse("  DOCS  "), Some(RepoKind::Docs));
        assert_eq!(RepoKind::parse("Examples"), Some(RepoKind::Example));
        assert_eq!(RepoKind::parse("demo"), Some(RepoKind::Example));
        // And the ones that must not be forced into a bucket.
        for junk in ["", "library", "it depends", "`code`", "code — a service"] {
            assert_eq!(RepoKind::parse(junk), None, "{junk:?} must not classify");
        }
    }

    use super::*;
    use crate::reasoner::MockReasoner;

    fn index(reasoner_response: &str) -> RepoIndex {
        let store = Arc::new(Store::open_in_memory().unwrap());
        RepoIndex::new(
            store,
            None,
            Arc::new(MockReasoner::new(reasoner_response)),
            None,
            InvestigationCfg::default(),
        )
    }

    fn seed(idx: &RepoIndex, full_name: &str, summary: &str) {
        idx.store
            .put_repo(
                &RepoEntry {
                    full_name: full_name.into(),
                    description: None,
                    topics: vec![],
                    language: None,
                    archived: false,
                    pushed_at: None,
                    readme_etag: None,
                    readme: None,
                    summary: Some(summary.into()),
                    indexed_sha: None,
                    digest: None,
                    kind: None,
                    kind_pinned: false,
                    fetched_at: Utc::now().to_rfc3339(),
                },
                true,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn keyword_routes_win_without_a_model() {
        let idx = index("[]");
        seed(&idx, "restatedev/restate", "PURPOSE: the runtime");
        seed(
            &idx,
            "restatedev/restate-cloud",
            "PURPOSE: the control plane",
        );
        let routed = idx
            .route("environment env-2abc stuck provisioning in cloud")
            .await
            .unwrap();
        assert_eq!(routed[0], "restatedev/restate-cloud");
    }

    #[tokio::test]
    async fn model_picks_fill_remaining_slots_and_unknowns_are_dropped() {
        let idx = index(r#"["restatedev/restate-sdk-java","restatedev/not-a-real-repo"]"#);
        seed(&idx, "restatedev/restate-sdk-java", "PURPOSE: the Java SDK");
        let routed = idx.route("java sdk serialization failure").await.unwrap();
        assert_eq!(routed, vec!["restatedev/restate-sdk-java"]);
    }

    #[tokio::test]
    async fn falls_back_to_default_repos_when_nothing_matches() {
        let idx = index("[]");
        let routed = idx.route("totally unrelated symptom text").await.unwrap();
        assert_eq!(routed, InvestigationCfg::default().default_repos);
    }

    #[tokio::test]
    async fn routing_is_capped() {
        let mut cfg = InvestigationCfg::default();
        cfg.routes.insert(
            "everything".into(),
            (0..10).map(|i| format!("org/repo-{i}")).collect(),
        );
        let store = Arc::new(Store::open_in_memory().unwrap());
        let idx = RepoIndex::new(store, None, Arc::new(MockReasoner::new("[]")), None, cfg);
        let routed = idx.route("everything is broken").await.unwrap();
        assert_eq!(routed.len(), MAX_ROUTED_REPOS);
    }

    #[test]
    fn stale_and_archived_repos_are_not_summarized() {
        let idx = index("[]");
        assert!(!idx.worth_indexing(&true, Some("2026-07-01T00:00:00Z")));
        assert!(idx.worth_indexing(&false, Some("2026-07-01T00:00:00Z")));
        assert!(!idx.worth_indexing(&false, Some("2019-01-01T00:00:00Z")));
        // No timestamp: index it rather than assume it's dead.
        assert!(idx.worth_indexing(&false, None));
    }

    #[test]
    fn description_fallback_uses_the_repo_name_as_a_routing_term() {
        let idx = index("[]");
        let card = idx.describe_from_metadata(&RepoEntry {
            full_name: "restatedev/restate-operator".into(),
            description: Some("Kubernetes operator".into()),
            topics: vec!["kubernetes".into()],
            language: Some("Rust".into()),
            archived: false,
            pushed_at: None,
            readme_etag: None,
            readme: None,
            summary: None,
            indexed_sha: None,
            digest: None,
            kind: None,
            kind_pinned: false,
            fetched_at: Utc::now().to_rfc3339(),
        });
        assert!(card.contains("Kubernetes operator"));
        assert!(card.contains("restate-operator"));
    }
}
