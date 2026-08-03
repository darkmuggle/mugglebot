//! Local source checkouts — "pull the code" so a model can read it.
//!
//! Reasoning about an issue from its title and body alone is guesswork. The
//! difference between "connection pool exhausted" as a sentence and *the actual
//! pool implementation* is the difference between a plausible patch and a correct
//! one. So before triaging an assigned issue, MuggleBot checks the repository out
//! and gives the model real source to read.
//!
//! Checkouts start **shallow** (`--depth 1`, single branch): the triage only ever
//! reads the current tree, and full history on a large repo costs minutes and
//! gigabytes for nothing. Refreshes are `fetch --depth 1` + `reset --hard`, so a
//! repo is cloned once and then updated cheaply.
//!
//! The code *index* needs one thing history can give and the tip cannot — which files
//! each commit touched — so [`CheckoutCache::ensure_history`] deepens those clones on
//! demand with `--filter=blob:none`. File names live in tree objects, so that fetches
//! the shape of history without its contents: on a real repo, one commit became 173 for
//! +0.5MB. A deepened clone is then refreshed *without* `--depth`, because `--depth 1`
//! would move the boundary back up and discard the history on the very next tick.
//!
//! Nothing here writes to a repository. Clones are read-only working copies under
//! the data dir; there is no commit, no push, and no remote mutation anywhere in
//! this module. The token reaches git as an `Authorization` header set through
//! git's *environment* config — never in the remote URL (which would persist it to
//! `.git/config`) and never in `argv` (which would expose it to `ps`).

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

/// A checked-out repository.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub full_name: String,
    pub path: PathBuf,
    /// The commit the working tree is at — what a triage records so a later run
    /// can tell whether its analysis has gone stale.
    pub head_sha: String,
}

/// Git operations are bounded: a wedged clone must not hold the triage worker
/// forever.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

pub struct CheckoutCache {
    root: PathBuf,
    token: Option<String>,
    /// Per-repo ceiling, judged from GitHub's reported size.
    max_mb: u64,
    /// Ceiling on the **whole cache**. `0` disables it.
    max_total_mb: u64,
}

impl CheckoutCache {
    pub fn new(root: PathBuf, token: Option<String>, max_mb: u64, max_total_mb: u64) -> Self {
        Self {
            root,
            token: token.filter(|t| !t.trim().is_empty()),
            max_mb,
            max_total_mb,
        }
    }

    /// Where a repo lives once checked out.
    pub fn path_for(&self, full_name: &str) -> PathBuf {
        // `owner/name` is already a safe two-segment relative path, but a crafted
        // repo name must not be able to escape the cache root.
        let safe: String = full_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || "-_./".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(safe.replace("..", "_"))
    }

    /// Clone or update `full_name` at `branch`, returning the checkout.
    ///
    /// `size_kb` is GitHub's reported repository size; anything over `max_mb` is
    /// refused rather than cloned, since a multi-gigabyte monorepo isn't worth
    /// pulling to read a handful of files.
    pub async fn ensure(&self, full_name: &str, branch: &str, size_kb: u64) -> Result<Checkout> {
        if self.max_mb > 0 && size_kb / 1024 > self.max_mb {
            bail!(
                "{full_name} is {}MB, over the {}MB checkout limit",
                size_kb / 1024,
                self.max_mb
            );
        }
        let path = self.path_for(full_name);
        if path.join(".git").is_dir() {
            // Best effort: a fetch failure (offline, force-push) still leaves a
            // usable older tree, which beats failing the whole triage.
            if let Err(e) = self.update(&path, branch).await {
                debug!("checkout: refreshing {full_name} failed, using the existing tree: {e:#}");
            }
        } else {
            // Make room first. Indexing a whole org means many clones, and the
            // per-repo limit says nothing about their sum.
            self.enforce_total_budget(&path);
            self.clone_fresh(full_name, &path, branch).await?;
        }
        let head_sha = self.head_sha(&path).await?;
        Ok(Checkout {
            full_name: full_name.to_string(),
            path,
            head_sha,
        })
    }

    /// The checkout already on disk, without touching the network.
    ///
    /// [`Self::ensure`] needs a branch and a size, and both come from an API call — so a
    /// caller that has been refused by the GitHub budget cannot use it at all. This is the
    /// escape hatch: a tree that is a few commits stale is still the right input for work
    /// that reads history rather than the tip, and it costs nothing to answer.
    pub async fn existing(&self, full_name: &str) -> Option<Checkout> {
        let path = self.path_for(full_name);
        if !path.join(".git").is_dir() {
            return None;
        }
        let head_sha = self.head_sha(&path).await.ok()?;
        Some(Checkout {
            full_name: full_name.to_string(),
            path,
            head_sha,
        })
    }

    /// Evict least-recently-used checkouts until the cache fits its budget.
    ///
    /// The per-repo limit is not a bound on the cache: indexing an org clones a
    /// hundred repos, and GitHub's reported size undercounts what lands on disk
    /// (a docs site whose git size is modest can carry hundreds of megabytes of
    /// assets). Measured on a real org, five repos came to 427MB — so without this
    /// the cache grows to whatever the org happens to weigh.
    ///
    /// Eviction is by directory mtime, which a `fetch`/`reset` bumps, so the
    /// checkouts in active use are the ones kept. `keep` is the checkout we're
    /// about to create and must never be removed. Everything here is best-effort:
    /// failing to evict must not fail the clone.
    fn enforce_total_budget(&self, keep: &Path) {
        if self.max_total_mb == 0 {
            return;
        }
        let budget = self.max_total_mb.saturating_mul(1024 * 1024);
        let mut entries = self.checkout_sizes();
        let mut total: u64 = entries.iter().map(|(_, _, size)| *size).sum();
        if total <= budget {
            return;
        }
        // Oldest-touched first.
        entries.sort_by_key(|(_, mtime, _)| *mtime);
        for (path, _, size) in entries {
            if total <= budget {
                break;
            }
            if path == keep {
                continue;
            }
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    debug!(
                        "checkout: evicted {} ({}MB) to stay under {}MB",
                        path.display(),
                        size / (1024 * 1024),
                        self.max_total_mb
                    );
                    total = total.saturating_sub(size);
                }
                Err(e) => debug!("checkout: could not evict {}: {e}", path.display()),
            }
        }
    }

    /// `(path, mtime, bytes)` for each `owner/name` checkout in the cache.
    fn checkout_sizes(&self) -> Vec<(PathBuf, std::time::SystemTime, u64)> {
        let mut out = Vec::new();
        // The cache is two levels deep: <root>/<owner>/<name>.
        let Ok(owners) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for owner in owners.flatten() {
            if !owner.path().is_dir() {
                continue;
            }
            let Ok(repos) = std::fs::read_dir(owner.path()) else {
                continue;
            };
            for repo in repos.flatten() {
                let path = repo.path();
                if !path.is_dir() {
                    continue;
                }
                let mtime = repo
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                out.push((path.clone(), mtime, dir_size(&path)));
            }
        }
        out
    }

    async fn clone_fresh(&self, full_name: &str, path: &Path, branch: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // A partial directory from an interrupted clone would make git refuse.
        if path.exists() {
            std::fs::remove_dir_all(path).ok();
        }
        let url = format!("https://github.com/{full_name}.git");
        debug!("checkout: cloning {full_name} ({branch})");
        self.git(
            None,
            &[
                "clone",
                "--depth",
                "1",
                "--single-branch",
                "--branch",
                branch,
                "--no-tags",
                &url,
                &path.to_string_lossy(),
            ],
        )
        .await
        .with_context(|| format!("cloning {full_name}"))?;
        Ok(())
    }

    async fn update(&self, path: &Path, branch: &str) -> Result<()> {
        // `--depth 1` moves the shallow boundary *back up* to one commit, so running it on
        // a checkout that has been deepened for indexing would throw that history away —
        // every tick, undoing the deepen it just paid for. A deepened clone refreshes
        // without a depth argument, which leaves its boundary alone.
        if self.is_deepened(path).await {
            self.git(Some(path), &["fetch", "--no-tags", "origin", branch])
                .await?;
        } else {
            self.git(
                Some(path),
                &["fetch", "--depth", "1", "--no-tags", "origin", branch],
            )
            .await?;
        }
        // Discard anything local. This is a read cache, not a working tree — there
        // is nothing here worth preserving, and a dirty state would block updates
        // forever.
        self.git(Some(path), &["reset", "--hard", "FETCH_HEAD"])
            .await?;
        self.git(Some(path), &["clean", "-fdx"]).await?;
        Ok(())
    }

    /// The files a commit touched, read out of the local clone.
    ///
    /// Matches what GitHub's commit API reports, which is the diff against the **first
    /// parent** — so a merge is the change it brought in, not the union of both sides.
    /// Getting that right needs the explicit two-tree form: bare `diff-tree <merge>`
    /// prints *nothing* for a merge, and `-m --first-parent` prints the union of every
    /// parent's diff (verified both). Recording "nothing" for a merge would be worse than
    /// failing, because the indexer would file it as summarized-with-no-code and move on.
    ///
    /// `Err` means the clone cannot answer — usually the commit is outside a shallow
    /// boundary — and the caller should fall back to the API.
    pub async fn commit_files(&self, full_name: &str, sha: &str) -> Result<Vec<String>> {
        let path = self.path_for(full_name);
        if !path.join(".git").is_dir() {
            bail!("{full_name} has no checkout");
        }
        // `-z` because git otherwise quotes and escapes unusual pathnames, and a quoted
        // path would not match the ones the rest of the index stores.
        let first_parent = format!("{sha}^1");
        let out = match self
            .git(
                Some(&path),
                &[
                    "diff-tree",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    "-z",
                    &first_parent,
                    sha,
                ],
            )
            .await
        {
            Ok(out) => out,
            // No first parent: either a root commit, or the commit isn't here at all. The
            // `--root` form distinguishes them — it succeeds only for the former.
            Err(_) => self
                .git(
                    Some(&path),
                    &[
                        "diff-tree",
                        "--root",
                        "--no-commit-id",
                        "--name-only",
                        "-r",
                        "-z",
                        sha,
                    ],
                )
                .await
                .with_context(|| format!("{full_name}@{sha} is not in the local clone"))?,
        };
        Ok(out
            .split('\0')
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Has this checkout been deepened for indexing? Recorded as the partial-clone filter
    /// on the remote, which [`Self::ensure_history`] sets and nothing else does.
    async fn is_deepened(&self, path: &Path) -> bool {
        self.git(
            Some(path),
            &["config", "--get", "remote.origin.partialclonefilter"],
        )
        .await
        .is_ok_and(|v| !v.trim().is_empty())
    }

    /// The oldest commit reachable from HEAD, RFC3339. `None` if git can't say.
    async fn oldest_commit(&self, path: &Path) -> Option<DateTime<Utc>> {
        let out = self
            .git(Some(path), &["log", "--format=%cI", "HEAD"])
            .await
            .ok()?;
        let last = out.lines().rfind(|l| !l.trim().is_empty())?;
        DateTime::parse_from_rfc3339(last.trim())
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    /// Fetch history back to `since` **without file contents**.
    ///
    /// The indexing pass needs one thing from history that the shallow tip cannot give it:
    /// the list of files each commit touched. That used to cost one GitHub API call per
    /// commit — thousands of them, against a 5000/hour budget — while the same answer is
    /// derivable locally from the commit's tree.
    ///
    /// `--filter=blob:none` is what makes this affordable: file *names* live in tree
    /// objects, so the blobs (the actual file contents, and nearly all of the bytes) are
    /// never downloaded. On a real repo this turned one commit into 173 for +0.5MB in 0.6s.
    /// Git protocol also doesn't spend the REST budget, so this trades a rationed resource
    /// for an unrationed one.
    ///
    /// Idempotent and cheap to call repeatedly: it returns immediately once history already
    /// reaches `since`.
    pub async fn ensure_history(&self, full_name: &str, since: DateTime<Utc>) -> Result<()> {
        let path = self.path_for(full_name);
        if !path.join(".git").is_dir() {
            bail!("{full_name} has no checkout to deepen");
        }
        if self
            .oldest_commit(&path)
            .await
            .is_some_and(|oldest| oldest <= since)
        {
            return Ok(());
        }
        // Marking the remote a promisor is what lets a filtered fetch land on a clone that
        // was created unfiltered; without it git refuses the filter.
        for (k, v) in [
            ("remote.origin.promisor", "true"),
            ("remote.origin.partialclonefilter", "blob:none"),
        ] {
            self.git(Some(&path), &["config", k, v])
                .await
                .with_context(|| format!("configuring {k} on {full_name}"))?;
        }
        // Fetch further back than asked. The dates come from two clocks: the index stores
        // GitHub's *author* date, while `--shallow-since` and `git log %cI` work in
        // *committer* dates, and a rebase or a squash separates them — observed two days
        // apart on a real repo, which left the boundary short of the target, so the
        // short-circuit above never fired and every tick re-fetched. A month of slack costs
        // almost nothing when blobs aren't coming and removes the whole class of problem.
        let since_arg = format!(
            "--shallow-since={}",
            (since - chrono::Duration::days(30)).to_rfc3339()
        );
        debug!("checkout: deepening {full_name} back to {since}");
        self.git(
            Some(&path),
            &[
                "fetch",
                &since_arg,
                "--filter=blob:none",
                "--no-tags",
                "origin",
            ],
        )
        .await
        .with_context(|| format!("deepening {full_name}"))?;
        Ok(())
    }

    async fn head_sha(&self, path: &Path) -> Result<String> {
        Ok(self
            .git(Some(path), &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string())
    }

    /// Run git, returning stdout. The token rides in a header argument rather than
    /// in the remote URL, so it is never persisted to `.git/config`.
    async fn git(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("git");
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if let Some(token) = &self.token {
            // Basic, not Bearer: git's HTTP transport authenticates a GitHub token
            // as `x-access-token:<token>` over Basic auth. Bearer is rejected with a
            // bare "Authentication failed", which is a confusing way to learn this.
            //
            // Injected through git's *environment* config rather than `-c`: a `-c`
            // argument is visible in `ps` for the life of the process, and a
            // credential in the remote URL would be persisted into `.git/config` on
            // disk. The env form leaves the token in neither.
            let basic = base64(format!("x-access-token:{token}").as_bytes());
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                );
        }
        // Never let git stop on an interactive credential or SSH prompt: under a
        // background daemon that prompt is never answered, and the task would hang
        // until the timeout instead of failing cleanly.
        cmd.args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("SSH_ASKPASS", "true")
            .env("GCM_INTERACTIVE", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().context("spawning git")?;
        let out = match tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output()).await {
            Ok(result) => result.context("running git")?,
            Err(_) => bail!("git {} timed out after {GIT_TIMEOUT:?}", args[0]),
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "git {} failed: {}",
                args[0],
                redact(&stderr, self.token.as_deref())
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

/// Strip a token out of text before it reaches a log or an error surfaced in the
/// UI. git echoes its arguments in some failure messages.
fn redact(text: &str, token: Option<&str>) -> String {
    let mut out = text.trim().to_string();
    if let Some(token) = token.filter(|t| t.len() > 6) {
        out = out.replace(token, "«token»");
    }
    out.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no error details")
        .chars()
        .take(300)
        .collect()
}

/// Recursive size of a directory in bytes. Symlinks are not followed, so a linked
/// tree elsewhere on disk isn't counted against this cache's budget.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Standard base64, for the Basic auth header. Hand-rolled to avoid pulling in a
/// crate for twenty lines used in exactly one place.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Is `git` on `PATH`?
pub fn have_git() -> bool {
    crate::reasoner::cli::have("git")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> CheckoutCache {
        CheckoutCache::new(
            PathBuf::from("/tmp/mb-checkouts"),
            Some("ghp_secret123".into()),
            500,
            5_000,
        )
    }

    /// Build a fake cache on disk: `<root>/<owner>/<name>` each holding `mb` of data.
    fn seeded_cache(root: &Path, repos: &[(&str, u64)]) -> CheckoutCache {
        for (name, mb) in repos {
            let dir = root.join("org").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("blob.bin"),
                vec![0u8; (*mb as usize) * 1024 * 1024],
            )
            .unwrap();
            // Stagger mtimes so eviction order is deterministic: earlier in the
            // list = older = evicted first.
            std::thread::sleep(std::time::Duration::from_millis(20));
            let touch = dir.join(".touch");
            std::fs::write(&touch, b"x").unwrap();
            std::fs::remove_file(&touch).unwrap();
        }
        CheckoutCache::new(root.to_path_buf(), None, 500, 3)
    }

    #[test]
    fn path_is_owner_slash_name_under_the_root() {
        let p = cache().path_for("restatedev/restate");
        assert!(p.ends_with("restatedev/restate"));
        assert!(p.starts_with("/tmp/mb-checkouts"));
    }

    /// A repo name is remote input; it must not be able to walk out of the cache.
    #[test]
    fn traversal_in_a_repo_name_cannot_escape() {
        let p = cache().path_for("../../etc/passwd");
        assert!(
            p.starts_with("/tmp/mb-checkouts"),
            "escaped the cache root: {}",
            p.display()
        );
        assert!(!p.to_string_lossy().contains(".."));
    }

    #[test]
    fn odd_characters_are_replaced_not_passed_through() {
        let p = cache().path_for("owner/name;rm -rf ~");
        let s = p.to_string_lossy();
        assert!(!s.contains(';'));
        assert!(!s.contains(' '));
    }

    #[tokio::test]
    async fn oversized_repos_are_refused_before_cloning() {
        // 2GB reported by GitHub (in KB), against a 500MB limit.
        let err = cache()
            .ensure("restatedev/huge", "main", 2_000_000)
            .await
            .expect_err("an oversized repo must be refused");
        assert!(format!("{err:#}").contains("checkout limit"));
    }

    /// The per-repo limit says nothing about the cache's total size — measured on a
    /// real org, five repos came to 427MB. Without a total budget the cache grows to
    /// whatever the org weighs.
    #[test]
    fn the_cache_evicts_least_recently_used_to_stay_in_budget() {
        let root = std::env::temp_dir().join(format!("mb-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 4MB of checkouts against a 3MB budget; `old` is the least recently used.
        let cache = seeded_cache(&root, &[("old", 2), ("recent", 2)]);

        let incoming = root.join("org").join("incoming");
        cache.enforce_total_budget(&incoming);

        assert!(
            !root.join("org").join("old").exists(),
            "the least recently used checkout should have been evicted"
        );
        assert!(
            root.join("org").join("recent").exists(),
            "the most recently used checkout must be kept"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The checkout we are about to populate must never be the one evicted.
    #[test]
    fn eviction_never_removes_the_incoming_checkout() {
        let root = std::env::temp_dir().join(format!("mb-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cache = seeded_cache(&root, &[("wanted", 4)]);

        let wanted = root.join("org").join("wanted");
        cache.enforce_total_budget(&wanted);
        assert!(
            wanted.exists(),
            "the checkout being prepared must survive even when over budget"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_zero_budget_disables_eviction() {
        let root = std::env::temp_dir().join(format!("mb-nobudget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        seeded_cache(&root, &[("a", 2), ("b", 2)]);
        let unlimited = CheckoutCache::new(root.clone(), None, 500, 0);

        unlimited.enforce_total_budget(&root.join("org").join("new"));
        assert!(root.join("org").join("a").exists());
        assert!(root.join("org").join("b").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dir_size_sums_nested_files() {
        let root = std::env::temp_dir().join(format!("mb-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        std::fs::write(root.join("a.txt"), vec![0u8; 1000]).unwrap();
        std::fs::write(root.join("nested/b.txt"), vec![0u8; 2000]).unwrap();
        std::fs::write(root.join("nested/deep/c.txt"), vec![0u8; 3000]).unwrap();
        assert_eq!(dir_size(&root), 6000);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn errors_never_carry_the_token() {
        let raw =
            "fatal: could not read Authorization: Bearer ghp_secret123 for 'https://github.com'";
        let clean = redact(raw, Some("ghp_secret123"));
        assert!(!clean.contains("ghp_secret123"));
        assert!(clean.contains("«token»"));
    }

    #[test]
    fn redact_keeps_the_last_meaningful_line() {
        let clean = redact("Cloning into 'x'...\n\nfatal: repository not found\n", None);
        assert_eq!(clean, "fatal: repository not found");
    }

    /// Checked against RFC 4648's own vectors — a wrong pad here shows up as an
    /// opaque "Authentication failed" from git, so it's worth pinning exactly.
    #[test]
    fn base64_matches_the_spec_including_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn basic_auth_uses_the_github_token_username() {
        // git authenticates a GitHub token as `x-access-token:<token>` over Basic.
        assert_eq!(
            base64(b"x-access-token:ghp_abc"),
            "eC1hY2Nlc3MtdG9rZW46Z2hwX2FiYw=="
        );
    }
}

#[cfg(test)]
mod git_history_tests {
    use super::*;

    /// A real repository, built locally, so the git semantics are tested rather than
    /// assumed. This is the file-list source the code index now depends on.
    struct Repo {
        root: PathBuf,
        cache: CheckoutCache,
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    impl Repo {
        async fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("mb-git-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&root).ok();
            // The cache expects <root>/<owner>/<name>.
            let repo = root.join("org").join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            let cache = CheckoutCache::new(root.clone(), None, 0, 0);
            for args in [
                vec!["init", "-q"],
                vec!["config", "user.email", "t@example.com"],
                vec!["config", "user.name", "t"],
            ] {
                cache.git(Some(&repo), &args).await.unwrap();
            }
            Self { root, cache }
        }

        fn path(&self) -> PathBuf {
            self.root.join("org").join("repo")
        }

        async fn git(&self, args: &[&str]) -> String {
            self.cache.git(Some(&self.path()), args).await.unwrap()
        }

        async fn commit(&self, file: &str, msg: &str) -> String {
            std::fs::write(self.path().join(file), msg).unwrap();
            self.git(&["add", "."]).await;
            self.git(&["commit", "-q", "-m", msg]).await;
            self.git(&["rev-parse", "HEAD"]).await.trim().to_string()
        }

        async fn files(&self, sha: &str) -> Vec<String> {
            self.cache.commit_files("org/repo", sha).await.unwrap()
        }
    }

    #[tokio::test]
    async fn a_root_commit_reports_the_files_it_added() {
        let repo = Repo::new("root").await;
        let root = repo.commit("base.txt", "base").await;
        // `<sha>^1` does not exist for a root commit, so this only works via `--root`.
        assert_eq!(repo.files(&root).await, vec!["base.txt".to_string()]);
    }

    #[tokio::test]
    async fn an_ordinary_commit_reports_only_what_it_changed() {
        let repo = Repo::new("ordinary").await;
        repo.commit("base.txt", "base").await;
        let second = repo.commit("second.txt", "second").await;
        assert_eq!(repo.files(&second).await, vec!["second.txt".to_string()]);
    }

    #[tokio::test]
    async fn a_merge_reports_its_first_parent_diff_not_nothing_and_not_the_union() {
        // The case that made this worth writing by hand. Bare `diff-tree <merge>` prints
        // nothing, which the indexer would store as "changed no code" and never revisit;
        // `-m --first-parent` prints the union of both parents' diffs. Neither matches what
        // GitHub's commit API returns, which is the first-parent diff.
        let repo = Repo::new("merge").await;
        repo.commit("base.txt", "base").await;
        repo.git(&["checkout", "-qb", "feature"]).await;
        repo.commit("feature.txt", "feat").await;
        repo.git(&["checkout", "-q", "-"]).await;
        repo.commit("mainonly.txt", "main work").await;
        repo.git(&["merge", "-q", "--no-ff", "feature", "-m", "merge"])
            .await;
        let merge = repo.git(&["rev-parse", "HEAD"]).await.trim().to_string();

        let files = repo.files(&merge).await;
        assert_eq!(
            files,
            vec!["feature.txt".to_string()],
            "a merge is the change it brought in"
        );
        assert!(
            !files.iter().any(|f| f == "mainonly.txt"),
            "the first parent's own work is not part of the merge: {files:?}"
        );
    }

    #[tokio::test]
    async fn a_commit_that_is_not_in_the_clone_is_an_error_not_an_empty_list() {
        // The caller distinguishes these: an error falls back to the GitHub API, whereas an
        // empty list would be recorded as "this commit touched no code" and never retried.
        let repo = Repo::new("absent").await;
        repo.commit("base.txt", "base").await;
        let absent = "0123456789abcdef0123456789abcdef01234567";
        assert!(repo.cache.commit_files("org/repo", absent).await.is_err());
    }

    #[tokio::test]
    async fn paths_with_spaces_survive_the_round_trip() {
        // `-z` output, because git quotes and escapes awkward pathnames otherwise and a
        // quoted path would not match what the index stores elsewhere.
        let repo = Repo::new("spaces").await;
        let sha = repo.commit("a file with spaces.txt", "x").await;
        assert_eq!(
            repo.files(&sha).await,
            vec!["a file with spaces.txt".to_string()]
        );
    }

    #[tokio::test]
    async fn a_missing_checkout_is_an_error() {
        let cache = CheckoutCache::new(PathBuf::from("/nonexistent-checkout-root"), None, 0, 0);
        assert!(cache.commit_files("org/repo", "HEAD").await.is_err());
        assert!(cache.existing("org/repo").await.is_none());
    }
}
