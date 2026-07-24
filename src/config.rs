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
    pub ambient: String,
    pub ambient_model: String,
    pub heavy: String,
    pub heavy_model: String,
    pub ollama_url: String,
    /// Ollama Cloud host. When an `ollama` credential (API key) is set, hosted
    /// models here are folded into the selectable list alongside local ones.
    pub ollama_cloud_url: String,
    pub ollama_model: String,
    pub local_only_sources: Vec<String>,
}

impl Default for Reasoner {
    fn default() -> Self {
        Self {
            ambient: "claude".into(),
            ambient_model: "claude-sonnet-5".into(),
            heavy: "claude".into(),
            heavy_model: "claude-opus-4-8".into(),
            ollama_url: "http://127.0.0.1:11434".into(),
            ollama_cloud_url: "https://ollama.com".into(),
            ollama_model: "llama3.1".into(),
            local_only_sources: vec![],
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
        assert_eq!(cfg.reasoner.ambient_model, "claude-sonnet-5");
        assert_eq!(cfg.reasoner.heavy_model, "claude-opus-4-8");
    }
}
