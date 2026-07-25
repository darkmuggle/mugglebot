//! LLM reasoning abstraction — the single seam through which correlation,
//! de-duplication, live-assist, and chat reach a model.
//!
//! One [`Reasoner`] trait, four implementations:
//!   - [`ollama::OllamaReasoner`] — on-device, OpenAI-shaped `/api/chat`.
//!   - [`cli::CliReasoner`] — the subscription bridge: shells out to `claude -p`
//!     or `codex exec`, riding your existing login (no API key, no metering).
//!   - [`api::ApiReasoner`] — direct Anthropic / OpenAI over an OpenAI-compatible
//!     `/v1/chat/completions`.
//!   - [`MockReasoner`] — a canned reasoner for tests.
//!
//! Routing (config `[reasoner]`) is **by task difficulty, not by call site**. The
//! local model answers by default and grades each task first; `hard` gets a cloud
//! cleanup pass and `extra_hard` goes straight to the top tier — see [`router`].
//! The [`build`] factory turns a provider label into a concrete reasoner,
//! preferring the CLI bridge for `claude`/`chatgpt` when the binary is on `PATH`.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::Reasoner as ReasonerCfg;

pub mod cache;
pub mod cli;
pub mod ollama;
pub mod router;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// An inline image (base64), for multimodal chat.
#[derive(Debug, Clone)]
pub struct Image {
    pub media_type: String,
    pub base64: String,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub images: Vec<Image>,
}

impl Message {
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Conversation key. When set, the CLI bridge keeps a persistent session for
    /// this key (a "session chat per topic") so reasoning about one thread
    /// continues the same conversation across passes.
    pub session: Option<String>,
    /// Skip the completion cache and force a fresh call.
    ///
    /// Set this when the *user asked for the work to be redone* — "reconsider on
    /// model X", "re-triage this issue". Serving those from cache would make the
    /// action look broken. Ordinary automatic passes leave it false so repeated
    /// identical work is free.
    pub no_cache: bool,
}

impl CompletionRequest {
    /// A single user-turn request — the common case for summaries and judgments.
    pub fn single(prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            messages: vec![Message::text(Role::User, prompt)],
            max_tokens: 1024,
            temperature: 0.2,
            session: None,
            no_cache: false,
        }
    }

    /// Force a fresh call, bypassing the completion cache — for actions where the
    /// user explicitly asked for the work to be redone.
    pub fn no_cache(mut self) -> Self {
        self.no_cache = true;
        self
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Attach a conversation key so the CLI bridge maintains a session for it.
    pub fn session(mut self, key: impl Into<String>) -> Self {
        self.session = Some(key.into());
        self
    }
}

#[async_trait]
pub trait Reasoner: Send + Sync {
    /// Run a completion and return the model's text.
    async fn complete(&self, req: &CompletionRequest) -> Result<String>;

    /// Convenience: a single-prompt completion.
    async fn summarize(&self, prompt: &str) -> Result<String> {
        self.complete(&CompletionRequest::single(prompt)).await
    }

    /// Whether this reasoner can see images. Chat routes vision to a capable one.
    fn supports_vision(&self) -> bool {
        false
    }
}

/// Extract the first JSON value embedded in a model response, tolerating
/// Markdown code fences and surrounding prose. Returns `None` if nothing parses.
pub fn extract_json(text: &str) -> Option<serde_json::Value> {
    let cleaned = text.trim();
    // Fast path: the whole thing is JSON.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(cleaned) {
        return Some(v);
    }
    // Strip a ```json … ``` fence if present.
    let inner = if let Some(rest) = cleaned.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        rest.rsplit_once("```")
            .map(|(a, _)| a)
            .unwrap_or(rest)
            .trim()
    } else {
        cleaned
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(inner) {
        return Some(v);
    }
    // Last resort: scan for the first balanced {...} or [...].
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let Some(start) = inner.find(open) {
            let mut depth = 0i32;
            let mut in_str = false;
            let mut esc = false;
            for (i, ch) in inner[start..].char_indices() {
                match ch {
                    _ if esc => esc = false,
                    '\\' if in_str => esc = true,
                    '"' => in_str = !in_str,
                    c if c == open && !in_str => depth += 1,
                    c if c == close && !in_str => {
                        depth -= 1;
                        if depth == 0 {
                            let slice = &inner[start..start + i + ch.len_utf8()];
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
                                return Some(v);
                            }
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

/// Map a UI-facing provider label (`anthropic`/`claude`, `openai`/`chatgpt`,
/// `ollama` for Ollama Cloud, `ollama_local` for the on-device instance) to the
/// internal reasoner label that [`build`] expects. Unknown values default to
/// Claude.
pub fn provider_label(ui: &str) -> &'static str {
    match ui.trim().to_ascii_lowercase().as_str() {
        "openai" | "chatgpt" => "chatgpt",
        "ollama_local" | "ollama-local" => "ollama_local",
        "ollama" | "ollama_cloud" | "ollama-cloud" => "ollama",
        _ => "claude",
    }
}

/// Build a reasoner for a provider label
/// (`claude` | `chatgpt` | `ollama` | `ollama_local`).
///
/// Reasoning **runs through the CLI bridge** (`claude -p` / `codex exec`), riding
/// your existing subscription login — no API keys. If the CLI binary isn't on
/// `PATH` the reasoner still constructs and simply errors at call time, so the
/// daemon starts and everything else keeps working (correlation/live-assist
/// degrade to deterministic behavior).
///
/// The two Ollama labels share a wire protocol but hit different endpoints:
/// `ollama_local` talks to the on-device instance (`ollama_url`); `ollama` is
/// Ollama Cloud (`ollama_cloud_url`), which needs the API key from the
/// credential store via `ollama_key`. Splitting them means a hosted model can't
/// 404 against localhost, and vice versa.
pub fn build(
    provider: &str,
    model: &str,
    cfg: &ReasonerCfg,
    ollama_key: Option<String>,
) -> Arc<dyn Reasoner> {
    match provider {
        "ollama" => Arc::new(ollama::OllamaReasoner::new(
            cfg.ollama_cloud_url.clone(),
            model.to_string(),
            ollama_key,
        )),
        "ollama_local" => Arc::new(ollama::OllamaReasoner::new(
            cfg.ollama_url.clone(),
            model.to_string(),
            ollama_key,
        )),
        "chatgpt" => Arc::new(cli::CliReasoner::codex(model.to_string())),
        // "claude" and anything unrecognized default to Claude.
        _ => Arc::new(cli::CliReasoner::claude(model.to_string())),
    }
}

/// List the models selectable for a provider (`claude` | `chatgpt` | `ollama`).
///
/// Ollama is genuinely dynamic — the list is whatever the operator has pulled
/// locally (`/api/tags`). Reasoning for Claude/Codex rides the subscription CLI
/// bridge, which has no key'd model-list API, so those return a curated set of
/// the current models; the config's own pinned models are folded in so the
/// active choice always appears.
pub async fn list_models(
    provider: &str,
    cfg: &ReasonerCfg,
    ollama_key: Option<String>,
) -> Result<Vec<String>> {
    let curated: Vec<String> = match provider {
        "ollama_local" => {
            // The on-device instance: whatever the operator has pulled (`/api/tags`).
            let mut models = ollama::list_models(&cfg.ollama_url, ollama_key.as_deref()).await?;
            // Unlike the curated providers, the Ollama list is ground truth: only
            // models actually pulled can serve a chat. Surface the configured
            // default as the first choice *when it's installed* — never inject it
            // otherwise, or it becomes a phantom default that 404s ("model not found").
            if let Some(pos) = models.iter().position(|m| *m == cfg.ollama_model) {
                let m = models.remove(pos);
                models.insert(0, m);
            }
            return Ok(models);
        }
        "ollama" => {
            // Ollama Cloud (ollama.com). The hosted catalog is only reachable with
            // an API key; without one there are no selectable models — return an
            // empty list rather than erroring so the picker degrades gracefully and
            // nudges the operator to set the key.
            match ollama_key.as_deref().filter(|k| !k.trim().is_empty()) {
                Some(key) => return ollama::list_models(&cfg.ollama_cloud_url, Some(key)).await,
                None => return Ok(Vec::new()),
            }
        }
        "chatgpt" => ["gpt-5.6-sol", "gpt-5.6"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        // "claude" and anything unrecognized.
        _ => ["claude-opus-4-8", "claude-sonnet-5", "claude-haiku-4-5"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let mut models = curated;
    // Surface whichever configured models belong to this provider.
    let pinned = [
        (cfg.local.as_str(), cfg.local_model.clone()),
        (cfg.mid.as_str(), cfg.mid_model.clone()),
        (cfg.heavy.as_str(), cfg.heavy_model.clone()),
    ]
    .into_iter()
    .filter(|(p, _)| *p == provider)
    .map(|(_, m)| m);
    dedup_prepend(&mut models, pinned);
    Ok(models)
}

/// Prepend `extra` values that aren't already present, preserving order.
fn dedup_prepend(models: &mut Vec<String>, extra: impl IntoIterator<Item = String>) {
    for m in extra {
        if m.trim().is_empty() || models.contains(&m) {
            continue;
        }
        models.insert(0, m);
    }
}

/// The reasoners MuggleBot routes to, built once at startup.
///
/// There are only two things a caller can ask for, and the distinction is about
/// **who is allowed to decide**, not about capability:
///
/// - [`Self::routed`] — the front door for task-shaped work. It grades each task
///   on the local model and escalates only when the difficulty warrants it (see
///   [`router`]). Almost everything should use this.
/// - [`Self::local`] — the raw on-device model, no grading and no escalation
///   possible. This is for work that must *never* reach a cloud model regardless
///   of how hard it looks: tag classification, repo crawling, and the reopen
///   triage over handled threads.
///
/// [`Self::vision`] exists only because images need a model that can see them.
#[derive(Clone)]
pub struct Reasoners {
    /// Difficulty-routed: local by default, escalating on grade.
    pub routed: Arc<dyn Reasoner>,
    /// On-device only, by policy. Never escalates.
    pub local: Arc<dyn Reasoner>,
    /// Vision-capable tier, for multimodal chat.
    pub vision: Arc<dyn Reasoner>,
    /// Small, fast cloud model for plain-English rewriting. Not a reasoning tier —
    /// it re-renders conclusions another model already reached.
    pub brief: Arc<dyn Reasoner>,
}

impl Reasoners {
    /// `ollama_key` is the stored Ollama API key (if any), fetched off
    /// the async runtime before this is called. `store` backs the completion
    /// cache; pass `None` to run uncached.
    pub fn from_config(
        cfg: &ReasonerCfg,
        ollama_key: Option<String>,
        store: Option<Arc<crate::store::Store>>,
    ) -> Self {
        // Every tier is wrapped, including the one the router uses to grade, so a
        // repeated identical request is free wherever it originates.
        let ttl = crate::config::parse_duration(&cfg.cache.ttl)
            .unwrap_or(std::time::Duration::from_secs(86_400));
        let wrap = |inner: Arc<dyn Reasoner>, provider: &str, model: &str| -> Arc<dyn Reasoner> {
            match (&store, cfg.cache.enabled) {
                (Some(store), true) => Arc::new(cache::CachingReasoner::new(
                    inner,
                    store.clone(),
                    format!("{provider}/{model}"),
                    ttl,
                )),
                _ => inner,
            }
        };
        let local = wrap(
            build(
                provider_label(&cfg.local),
                &cfg.local_model,
                cfg,
                ollama_key.clone(),
            ),
            &cfg.local,
            &cfg.local_model,
        );
        let mid = wrap(
            build(
                provider_label(&cfg.mid),
                &cfg.mid_model,
                cfg,
                ollama_key.clone(),
            ),
            &cfg.mid,
            &cfg.mid_model,
        );
        let heavy = wrap(
            build(
                provider_label(&cfg.heavy),
                &cfg.heavy_model,
                cfg,
                ollama_key.clone(),
            ),
            &cfg.heavy,
            &cfg.heavy_model,
        );
        let brief = wrap(
            build(
                provider_label(&cfg.brief),
                &cfg.brief_model,
                cfg,
                ollama_key,
            ),
            &cfg.brief,
            &cfg.brief_model,
        );
        Self {
            routed: Arc::new(router::RoutingReasoner::new(
                local.clone(),
                mid,
                heavy.clone(),
                cfg.routing.clone(),
            )),
            local,
            vision: heavy,
            brief,
        }
    }
}

/// A canned reasoner for tests: returns a fixed response for any request.
pub struct MockReasoner {
    pub response: String,
    pub vision: bool,
}

impl MockReasoner {
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            vision: true,
        }
    }
}

#[async_trait]
impl Reasoner for MockReasoner {
    async fn complete(&self, _req: &CompletionRequest) -> Result<String> {
        Ok(self.response.clone())
    }
    fn supports_vision(&self) -> bool {
        self.vision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_json() {
        let v = extract_json(r#"{"verdict":"same","confidence":0.9}"#).unwrap();
        assert_eq!(v["verdict"], "same");
    }

    #[test]
    fn extracts_fenced_json() {
        let v = extract_json("Here you go:\n```json\n{\"a\":1}\n```\nDone").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extracts_embedded_object() {
        let v = extract_json("blah blah {\"k\": [1,2,3]} trailing").unwrap();
        assert_eq!(v["k"][2], 3);
    }

    #[test]
    fn none_when_no_json() {
        assert!(extract_json("no json here").is_none());
    }
}
