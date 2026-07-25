//! Local source checkouts — "pull the code" so a model can read it.
//!
//! Reasoning about an issue from its title and body alone is guesswork. The
//! difference between "connection pool exhausted" as a sentence and *the actual
//! pool implementation* is the difference between a plausible patch and a correct
//! one. So before triaging an assigned issue, MuggleBot checks the repository out
//! and gives the model real source to read.
//!
//! Checkouts are **shallow** (`--depth 1`, single branch): the triage only ever
//! reads the current tree, and full history on a large repo costs minutes and
//! gigabytes for nothing. Refreshes are `fetch --depth 1` + `reset --hard`, so a
//! repo is cloned once and then updated cheaply.
//!
//! Nothing here writes to a repository. Clones are read-only working copies under
//! the data dir; there is no commit, no push, and no remote mutation anywhere in
//! this module. The token reaches git as an `Authorization` header set through
//! git's *environment* config — never in the remote URL (which would persist it to
//! `.git/config`) and never in `argv` (which would expose it to `ps`).

use anyhow::{bail, Context, Result};
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
        self.git(
            Some(path),
            &["fetch", "--depth", "1", "--no-tags", "origin", branch],
        )
        .await?;
        // Discard anything local. This is a read cache, not a working tree — there
        // is nothing here worth preserving, and a dirty state would block updates
        // forever.
        self.git(Some(path), &["reset", "--hard", "FETCH_HEAD"])
            .await?;
        self.git(Some(path), &["clean", "-fdx"]).await?;
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
