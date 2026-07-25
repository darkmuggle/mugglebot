//! Reasoning via Ollama's `/api/chat`. Same wire protocol against two endpoints:
//! the on-device instance (`ollama_local` — truly local, nothing leaves the Mac,
//! used for sources pinned in `local_only_sources`) and Ollama Cloud (`ollama`,
//! authenticated with a bearer API key). Also a vision-capable option for chat.
//!
//! **Thinking is disabled.** On a reasoning model (`gemma4:12b`, `qwen3`, …)
//! Ollama spends the `num_predict` budget on a `thinking` field *before* it emits
//! any `content`. At the token budgets MuggleBot asks for, that means the answer
//! is silently truncated to `""` — the caller sees an empty response, falls back to
//! its deterministic path, and nothing looks broken. Every job routed here is
//! classification, extraction, or JSON shaping, none of which needs chain of
//! thought, so `think: false` is both correct and an order of magnitude cheaper.
//! Models that don't understand the field reject it, so a rejection retries once
//! without it.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tracing::debug;

use super::{CompletionRequest, Reasoner};

pub struct OllamaReasoner {
    client: reqwest::Client,
    url: String,
    model: String,
    /// Optional API key (Ollama Cloud / an authenticated proxy), sent as a bearer.
    api_key: Option<String>,
}

impl OllamaReasoner {
    pub fn new(url: String, model: String, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            model,
            api_key: api_key.filter(|k| !k.trim().is_empty()),
        }
    }

    /// POST one `/api/chat` body. A non-2xx carries Ollama's own `error` message,
    /// which is what distinguishes "model doesn't support thinking" from a real
    /// failure — so it's preserved rather than reduced to a status code.
    async fn chat(&self, body: &serde_json::Value) -> Result<ChatResponse> {
        let mut req_b = self
            .client
            .post(format!("{}/api/chat", self.url.trim_end_matches('/')))
            .json(body);
        if let Some(key) = &self.api_key {
            req_b = req_b.bearer_auth(key);
        }
        let resp = req_b.send().await.context("ollama chat request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or(text);
            bail!("ollama chat status {status}: {message}");
        }
        resp.json::<ChatResponse>()
            .await
            .context("parsing ollama chat")
    }
}

#[derive(serde::Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    /// `"stop"` on a complete answer, `"length"` when `num_predict` ran out.
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
    /// A reasoning model's chain of thought, returned separately from `content`.
    #[serde(default)]
    thinking: Option<String>,
}

impl ChatResponse {
    /// The model's answer, or an actionable error.
    ///
    /// Empty content is reported as a failure rather than returned as `""`.
    /// Callers treat an empty string as "the model had nothing to say" and fall
    /// back silently; a truncated reasoning model is a *configuration* problem
    /// (the answer never got written) and needs to say so.
    fn into_text(self, model: &str) -> Result<String> {
        if !self.message.content.trim().is_empty() {
            return Ok(self.message.content);
        }
        let thought = self.message.thinking.unwrap_or_default();
        if self.done_reason.as_deref() == Some("length") && !thought.trim().is_empty() {
            bail!(
                "`{model}` used its entire token budget on reasoning and produced no answer. \
                 It appears to ignore `think: false` — raise the caller's max_tokens or pin a \
                 non-reasoning classifier model."
            );
        }
        bail!("`{model}` returned an empty response");
    }
}

/// Does this error mean the model rejected the `think` field (rather than failing
/// for a real reason)?
fn is_unsupported_think(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_ascii_lowercase();
    msg.contains("think") && (msg.contains("not support") || msg.contains("unsupported"))
}

#[async_trait]
impl Reasoner for OllamaReasoner {
    async fn complete(&self, req: &CompletionRequest) -> Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = &req.system {
            messages.push(serde_json::json!({ "role": "system", "content": sys }));
        }
        for m in &req.messages {
            let mut obj = serde_json::json!({ "role": m.role.as_str(), "content": m.content });
            if !m.images.is_empty() {
                obj["images"] = serde_json::Value::Array(
                    m.images
                        .iter()
                        .map(|i| serde_json::Value::String(i.base64.clone()))
                        .collect(),
                );
            }
            messages.push(obj);
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
            // See the module docs: thinking would eat the whole token budget and
            // leave `content` empty.
            "think": false,
            "options": { "temperature": req.temperature, "num_predict": req.max_tokens },
        });

        match self.chat(&body).await {
            Ok(resp) => resp.into_text(&self.model),
            Err(e) if is_unsupported_think(&e) => {
                // An older or non-reasoning model rejects the field outright.
                debug!(
                    "ollama: {} does not accept `think`; retrying without",
                    self.model
                );
                body.as_object_mut().map(|o| o.remove("think"));
                self.chat(&body).await?.into_text(&self.model)
            }
            Err(e) => Err(e),
        }
    }

    fn supports_vision(&self) -> bool {
        // Vision depends on the pulled model; assume the operator picked a capable
        // one when routing images here.
        true
    }
}

/// List the models installed on the local Ollama instance (`GET /api/tags`).
/// Truly dynamic — reflects whatever the operator has pulled. `api_key` is the
/// optional Ollama Cloud / proxy bearer.
pub async fn list_models(url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Tags {
        models: Vec<Model>,
    }
    #[derive(serde::Deserialize)]
    struct Model {
        name: String,
    }
    let mut req = reqwest::Client::new().get(format!("{}/api/tags", url.trim_end_matches('/')));
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        req = req.bearer_auth(key);
    }
    let tags: Tags = req
        .send()
        .await
        .context("ollama tags request")?
        .error_for_status()
        .context("ollama tags status")?
        .json()
        .await
        .context("parsing ollama tags")?;
    Ok(tags.models.into_iter().map(|m| m.name).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(content: &str, thinking: Option<&str>, done: &str) -> ChatResponse {
        ChatResponse {
            message: ChatMessage {
                content: content.into(),
                thinking: thinking.map(str::to_string),
            },
            done_reason: Some(done.into()),
        }
    }

    #[test]
    fn content_passes_through() {
        let text = response("PURPOSE: the runtime", None, "stop")
            .into_text("gemma4:12b")
            .unwrap();
        assert_eq!(text, "PURPOSE: the runtime");
    }

    /// The failure this module exists to catch: the model thought until it ran out
    /// of budget and never wrote an answer. It must not look like "no opinion".
    #[test]
    fn budget_spent_on_thinking_is_an_actionable_error() {
        let err = response("", Some("Let me consider the README…"), "length")
            .into_text("gemma4:12b")
            .expect_err("truncated reasoning must not return an empty string");
        let msg = format!("{err:#}");
        assert!(msg.contains("entire token budget"));
        assert!(msg.contains("gemma4:12b"), "name the model to configure");
    }

    #[test]
    fn plain_empty_response_is_still_an_error() {
        let err = response("   ", None, "stop")
            .into_text("llama3.1")
            .expect_err("an empty answer is a failure, not a result");
        assert!(format!("{err:#}").contains("empty response"));
    }

    #[test]
    fn only_a_think_rejection_triggers_the_retry() {
        assert!(is_unsupported_think(&anyhow::anyhow!(
            "ollama chat status 400: \"gemma2\" does not support thinking"
        )));
        // A real failure must not be retried as if it were a capability problem.
        assert!(!is_unsupported_think(&anyhow::anyhow!(
            "ollama chat status 404: model 'gemma4:12b' not found"
        )));
        assert!(!is_unsupported_think(&anyhow::anyhow!(
            "ollama chat request: connection refused"
        )));
    }
}
