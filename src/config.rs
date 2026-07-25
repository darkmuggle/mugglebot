//! Configuration: non-secret behavior loaded from a TOML file. Credentials are
//! **not** here — they live in the SQLite store (see [`crate::store`]).

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::signal::Severity;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub sources: Sources,
    pub notifications: Notifications,
    pub correlation: Correlation,
    pub live: Live,
    pub reasoner: Reasoner,
    pub investigation: Investigation,
    pub assigned: Assigned,
    pub browser: Browser,
    pub mcp: Mcp,
    pub ui: Ui,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        let cfg: Config = toml::from_str(&text).context("parsing TOML config")?;
        Ok(cfg)
    }

    /// Resolve `general.data_dir`, expanding a leading `~`.
    pub fn data_dir_path(&self) -> Result<PathBuf> {
        let raw = self.general.data_dir.trim();
        let path = if let Some(rest) = raw.strip_prefix("~/") {
            home()?.join(rest)
        } else if raw == "~" {
            home()?
        } else {
            PathBuf::from(raw)
        };
        Ok(path)
    }
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub data_dir: String,
    /// `"HH:MM-HH:MM"` local time; non-Critical notifications are suppressed inside it.
    pub quiet_hours: Option<String>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            data_dir: "~/.mugglebot".into(),
            quiet_hours: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub github: GithubSource,
    pub slack: SlackSource,
    pub granola: GranolaSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubSource {
    pub enabled: bool,
    pub watch: Vec<String>,
    pub poll_interval: String,
    /// Fetch the issue/PR/discussion (and the comment that triggered the
    /// notification) for each kept notification, so the signal carries real
    /// content — author, state, labels, and an excerpt — instead of just the
    /// subject title. Costs extra API calls per poll; disable to stay lean.
    pub enrich: bool,
    /// Drop notifications whose subject title starts with any of these prefixes.
    /// Bot noise like "CLA Assistant workflow run" never reaches a signal.
    pub ignore_prefixes: Vec<String>,
}

impl Default for GithubSource {
    fn default() -> Self {
        Self {
            enabled: false,
            watch: vec![],
            poll_interval: "60s".into(),
            enrich: true,
            ignore_prefixes: vec!["CLA Assistant workflow run".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackSource {
    pub enabled: bool,
    /// Your own Slack user id (e.g. `U0123ABC`) — used to flag your own messages
    /// and, when `search_mentions` is on, to build the default mention query.
    pub user_id: Option<String>,
    pub channels: Vec<String>,
    pub alert_channels: Vec<String>,
    pub keywords: Vec<String>,
    /// Find mentions of you across *every* conversation you can see — including
    /// channels not in `channels`, private channels, and DMs — via Slack's
    /// `search.messages`. Requires the stored `slack` credential to be a **user**
    /// token (`xoxp-…`, scope `search:read`); a bot token cannot search.
    pub search_mentions: bool,
    /// Override the search query. Defaults to `<@USER_ID>` (a raw @-mention of
    /// you). Set this to also catch your name, e.g. `"<@U0BHT8CSA9M> OR ben"`.
    pub mention_query: Option<String>,
    pub poll_interval: String,
}

impl Default for SlackSource {
    fn default() -> Self {
        Self {
            enabled: false,
            user_id: None,
            channels: vec![],
            alert_channels: vec![],
            keywords: vec![],
            search_mentions: false,
            mention_query: None,
            poll_interval: "30s".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GranolaSource {
    pub enabled: bool,
    pub poll_interval: String,
    /// Base URL for the Granola API. Overridable for self-hosted/proxy setups.
    pub api_base: String,
}

impl Default for GranolaSource {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval: "2m".into(),
            api_base: "https://api.granola.ai".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    pub min_severity: String,
    pub critical_sound: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            min_severity: "notice".into(),
            critical_sound: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Correlation {
    pub window: String,
    pub dedup_threshold: f64,
    pub auto_merge: bool,
    /// Minimum confidence from the **local** classifier before new activity
    /// reopens a snoozed/handled thread. Handled threads are never sent to a
    /// cloud reasoner, so this decision is on-device by construction.
    pub reopen_min_confidence: f64,
}

impl Default for Correlation {
    fn default() -> Self {
        Self {
            window: "30m".into(),
            dedup_threshold: 0.8,
            // Let high-confidence "same" verdicts from the Sonnet relation pass
            // actually collapse duplicate threads (e.g. Slack chatter about the
            // same topic), not just annotate them with an edge.
            auto_merge: true,
            reopen_min_confidence: 0.6,
        }
    }
}

/// Root-cause investigation: the repo index and the issue/PR/commit search that
/// turns a symptom into the change that probably caused it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Investigation {
    pub enabled: bool,
    /// The GitHub org whose repositories are indexed and searched.
    pub org: String,
    /// How often the repo index re-reads the org's READMEs. Conditional (ETag)
    /// requests make a no-change refresh cheap and LLM-free.
    pub refresh_interval: String,
    /// Deterministic symptom→repo routes, applied before the model reads the
    /// index. The key is a substring matched case-insensitively against the
    /// symptom text; the value is the repos it routes to.
    pub routes: std::collections::BTreeMap<String, Vec<String>>,
    /// Where to look when no route matches and the model picks nothing.
    pub default_repos: Vec<String>,
    /// Skip summarizing repos with no push inside `stale_repo_days`. They stay in
    /// the index as metadata, so a symptom naming one still resolves.
    pub skip_stale_repos: bool,
    pub stale_repo_days: i64,
    /// How far back to scan a repo's commit log for a candidate cause, relative
    /// to the thread's earliest signal.
    pub commit_window: String,
    /// Search code for the symptom's identifiers when no issue, PR, or commit
    /// explains it — the "if none, find the code" fallback.
    pub code_search: bool,
    /// How many candidates survive the **local** shortlisting pass and get sent to
    /// the cloud reasoner for a final verdict. This is the escalation boundary:
    /// crawling, searching, and filtering are on-device; only this many
    /// already-narrowed candidates cost a metered call.
    pub shortlist_size: usize,
}

impl Default for Investigation {
    fn default() -> Self {
        let mut routes = std::collections::BTreeMap::new();
        // The two anchors from the design: the runtime and the control plane.
        // Everything else is routed by the model reading the README index.
        for key in ["cloud", "environment", "control plane", "provisioning"] {
            routes.insert(key.into(), vec!["restatedev/restate-cloud".into()]);
        }
        for key in ["restate runtime", "invocation", "partition processor"] {
            routes.insert(key.into(), vec!["restatedev/restate".into()]);
        }
        Self {
            enabled: true,
            org: "restatedev".into(),
            refresh_interval: "24h".into(),
            routes,
            default_repos: vec!["restatedev/restate".into()],
            skip_stale_repos: true,
            stale_repo_days: 365,
            commit_window: "72h".into(),
            code_search: true,
            shortlist_size: 8,
        }
    }
}

/// Issues assigned to you on GitHub, and what MuggleBot does with them.
///
/// Assignment is a commitment, not a notification: an issue can sit assigned to
/// you for weeks without producing a single notification event. So these are
/// polled directly and always get a board card, independent of the notification
/// feed — and each one is triaged against the actual source code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Assigned {
    pub enabled: bool,
    pub poll_interval: String,
    /// Where repositories are checked out for reading. Relative paths resolve
    /// under `general.data_dir`.
    pub checkout_dir: String,
    /// Skip checking out anything bigger than this (MB) — a huge monorepo isn't
    /// worth cloning to read three files.
    pub max_checkout_mb: u64,
    /// Ceiling on the **whole** checkout cache (MB). The per-repo limit says nothing
    /// about their sum, and indexing an org means a clone per repo — on a real org,
    /// five repos came to 427MB. Over budget, least-recently-used checkouts are
    /// evicted. `0` disables the cap.
    pub max_cache_mb: u64,
    /// How many candidate patches to ask for.
    pub patches: usize,
    /// Source files fed to the model as context.
    pub max_files: usize,
    /// Characters kept per file. Enough to carry a module's shape without one
    /// large file crowding out the rest.
    pub max_file_chars: usize,
    /// Re-triage an issue when its checkout advances past the commit the last
    /// triage read. Off → triage once and keep it until asked to redo it.
    pub retriage_on_new_commits: bool,
    /// Characters of *judged* comment text folded into a prompt. Every comment is
    /// scored regardless; this bounds how much of the substantive set is shown.
    pub max_comment_chars: usize,
    /// Scan the repo's open pull requests for one that already fixes the issue —
    /// quite possibly somebody else's. Reads the diff, critiques whether it really
    /// fixes it, and notes what else it would resolve.
    pub check_open_prs: bool,
}

impl Default for Assigned {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval: "5m".into(),
            checkout_dir: "repos".into(),
            max_checkout_mb: 500,
            max_cache_mb: 5_000,
            patches: 3,
            // Sized for a local coder model's context window, not for coverage: an
            // over-large source dump pushes the instructions and the issue out of
            // the front of the window, and the model answers "describe this code"
            // instead of the question. See `triage::MAX_SOURCE_CHARS`.
            max_files: 6,
            max_file_chars: 3_000,
            retriage_on_new_commits: true,
            max_comment_chars: 6_000,
            check_open_prs: true,
        }
    }
}

/// Authenticated browser control. MuggleBot drives the operator's *existing*
/// signed-in Chrome over the DevTools Protocol, through an agent CLI that has a
/// browser MCP server attached — the only genuinely scriptable way to reach a
/// dashboard that lives behind SSO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Browser {
    pub enabled: bool,
    /// Which agent CLI drives the browser: `claude` (`claude -p`) or `chatgpt`
    /// (`codex exec`).
    pub agent: String,
    pub model: String,
    /// CDP endpoint of the Chrome to attach to. Start Chrome with
    /// `--remote-debugging-port=9222` so this exists; the profile's existing
    /// Grafana/SSO session is then what the agent sees.
    pub browser_url: String,
    /// The browser MCP server, launched by the agent CLI over stdio.
    pub mcp_command: String,
    pub mcp_args: Vec<String>,
    /// Tool names the agent may call — deliberately read-only: navigate, read,
    /// screenshot. No click, fill, or evaluate, so an investigation cannot
    /// silence an alert or mutate a dashboard.
    pub allowed_tools: Vec<String>,
    /// URL substrings that mark a link as worth investigating.
    pub url_patterns: Vec<String>,
    /// Hard cap on one investigation, after which the agent is killed.
    pub timeout: String,
    /// Give up on a link after this many failed attempts.
    pub max_attempts: i64,
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            enabled: false,
            agent: "claude".into(),
            model: "claude-sonnet-5".into(),
            browser_url: "http://127.0.0.1:9222".into(),
            mcp_command: "npx".into(),
            mcp_args: vec!["-y".into(), "chrome-devtools-mcp@latest".into()],
            allowed_tools: vec![
                "navigate_page".into(),
                "take_snapshot".into(),
                "take_screenshot".into(),
                "list_console_messages".into(),
                "list_network_requests".into(),
                "wait_for".into(),
            ],
            url_patterns: vec!["grafana".into()],
            timeout: "5m".into(),
            max_attempts: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Live {
    pub debounce: String,
    pub debounce_max: String,
    pub red_alert: bool,
    pub red_alert_min_confidence: f64,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            debounce: "1m".into(),
            debounce_max: "5m".into(),
            red_alert: true,
            red_alert_min_confidence: 0.75,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Reasoner {
    /// **The default model for everything.** On-device, and the tier that answers
    /// unless a task grades hard enough to escalate. It's also the grader.
    pub local: String,
    pub local_model: String,
    /// Escalation tier: cleans up a `hard` local draft, and takes over when the
    /// local model can't answer.
    pub mid: String,
    pub mid_model: String,
    /// Top tier: `extra_hard` tasks only, where a plausible-but-wrong answer would
    /// mislead an engineer mid-incident.
    pub heavy: String,
    pub heavy_model: String,
    /// The **plain-English** tier: a small, fast cloud model whose only job is
    /// rewriting something already worked out into language you can read at a
    /// glance. Not used to reason — used to explain.
    pub brief: String,
    pub brief_model: String,
    /// How a task's difficulty picks between the three tiers above.
    pub routing: Routing,
    /// Reuse of previously-computed answers.
    pub cache: Cache,
    pub ollama_url: String,
    /// Ollama Cloud host. When an `ollama` credential (API key) is set, hosted
    /// models here are folded into the selectable list alongside local ones.
    pub ollama_cloud_url: String,
    /// Model used for sources pinned in `local_only_sources`.
    pub ollama_model: String,
    /// Model used for **embeddings**, which is a different capability from chat:
    /// coder and chat models have no embedding head and answer `/api/embeddings`
    /// with a 500. Keep this pointed at a real embedding model.
    pub embed_model: String,
    pub local_only_sources: Vec<String>,
}

impl Default for Reasoner {
    fn default() -> Self {
        Self {
            local: "ollama_local".into(),
            local_model: "deepseek-coder:33b".into(),
            mid: "claude".into(),
            mid_model: "claude-sonnet-5".into(),
            heavy: "claude".into(),
            heavy_model: "claude-opus-4-8".into(),
            brief: "claude".into(),
            brief_model: "claude-haiku-4-5".into(),
            routing: Routing::default(),
            cache: Cache::default(),
            ollama_url: "http://127.0.0.1:11434".into(),
            ollama_cloud_url: "https://ollama.com".into(),
            ollama_model: "deepseek-coder:33b".into(),
            embed_model: "nomic-embed-text".into(),
            local_only_sources: vec![],
        }
    }
}

/// Difficulty-based model routing. Before running a task, the local model grades
/// how much reasoning it needs; the grade picks the tier. See
/// [`crate::reasoner::router`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Routing {
    /// Off → every task runs on the local model, ungraded.
    pub enabled: bool,
    /// For `hard` tasks, pass the local draft to the mid tier to be corrected.
    /// Off → the local draft is returned as-is, keeping hard tasks on-device.
    pub cleanup: bool,
    /// Allow a cloud tier to take over when the local model fails or returns
    /// nothing. Off → a local outage surfaces as an error and nothing leaves the
    /// machine (callers' deterministic fallbacks still apply).
    pub cloud_fallback: bool,
}

impl Default for Routing {
    fn default() -> Self {
        Self {
            enabled: true,
            cleanup: true,
            cloud_fallback: true,
        }
    }
}

/// The completion cache: identical requests are answered from SQLite instead of
/// re-running the model. Persisted, so a restart doesn't re-buy work already done.
/// See [`crate::reasoner::cache`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Cache {
    pub enabled: bool,
    /// How long an answer stays reusable. Long enough to cover a restart and a
    /// busy day; short enough that changed grounding eventually re-reasons.
    pub ttl: String,
    /// LRU ceiling on stored answers. 0 disables the size cap.
    pub max_entries: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: "24h".into(),
            max_entries: 5_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Mcp {
    pub stdio: bool,
    pub http_listen: String,
}

impl Default for Mcp {
    fn default() -> Self {
        Self {
            stdio: true,
            http_listen: "127.0.0.1:8787".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub listen: String,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
        }
    }
}

/// Parse a compact duration string like `"30s"`, `"2m"`, `"6h"`, `"500ms"`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s
        .find(|c: char| c.is_ascii_alphabetic())
        .ok_or_else(|| anyhow!("duration '{s}' has no unit"))?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid duration number in '{s}'"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        other => bail!("unknown duration unit '{other}' in '{s}'"),
    };
    Ok(d)
}

/// Map a config severity string to a [`Severity`], defaulting to `Notice`.
pub fn severity_from_str(s: &str) -> Severity {
    match s.trim().to_ascii_lowercase().as_str() {
        "info" => Severity::Info,
        "notice" => Severity::Notice,
        "warning" => Severity::Warning,
        "critical" => Severity::Critical,
        _ => Severity::Notice,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21600));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert!(parse_duration("nope").is_err());
        assert!(parse_duration("10x").is_err());
    }

    #[test]
    fn severities() {
        assert_eq!(severity_from_str("critical"), Severity::Critical);
        assert_eq!(severity_from_str("Info"), Severity::Info);
        assert_eq!(severity_from_str("weird"), Severity::Notice);
    }

    /// The shipped example config must always deserialize — guards against drift
    /// between the docs and the schema.
    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml"))
            .expect("config.example.toml should deserialize");
        assert!(cfg.sources.github.enabled);
        // The default model is the local one; the cloud tiers are escalations.
        assert_eq!(cfg.reasoner.local, "ollama_local");
        assert_eq!(cfg.reasoner.local_model, "deepseek-coder:33b");
        assert_eq!(cfg.reasoner.mid_model, "claude-sonnet-5");
        assert_eq!(cfg.reasoner.heavy_model, "claude-opus-4-8");
        assert_eq!(cfg.reasoner.brief_model, "claude-haiku-4-5");
        assert!(cfg.reasoner.routing.enabled);
        // Keys written *after* a `[reasoner.routing]` header would silently belong
        // to the sub-table and fall back to defaults here — assert on a value the
        // example sets to something other than its default, so that ordering
        // mistake fails the build instead of quietly dropping settings.
        assert_eq!(cfg.reasoner.ollama_model, "deepseek-coder:33b");
        assert_eq!(cfg.assigned.max_files, 6);
        assert_eq!(cfg.assigned.max_cache_mb, 5_000);
        assert_eq!(cfg.investigation.org, "restatedev");
        assert_eq!(
            cfg.investigation.routes.get("cloud").map(Vec::as_slice),
            Some(&["restatedev/restate-cloud".to_string()][..])
        );
        // Browser control is opt-in: it needs Chrome on a debug port, so it must
        // not appear to be on when nothing is listening.
        assert!(!cfg.browser.enabled);
    }

    /// The read-only guarantee is enforced by the tool allowlist, so the shipped
    /// default must not name a mutating tool.
    #[test]
    fn default_browser_allowlist_is_read_only() {
        let browser = Browser::default();
        for tool in &browser.allowed_tools {
            for forbidden in ["click", "fill", "evaluate", "upload", "dialog", "drag"] {
                assert!(
                    !tool.contains(forbidden),
                    "default allowlist grants a mutating tool: {tool}"
                );
            }
        }
    }
}
