//! Reasoning via Ollama's `/api/chat`. Same wire protocol against two endpoints:
//! the on-device instance (`ollama_local` — truly local, nothing leaves the Mac,
//! used for sources pinned in `local_only_sources`) and Ollama Cloud (`ollama`,
//! authenticated with a bearer API key). Also a vision-capable option for chat.

use anyhow::{Context, Result};
use async_trait::async_trait;

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
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
            "options": { "temperature": req.temperature, "num_predict": req.max_tokens },
        });

        #[derive(serde::Deserialize)]
        struct Resp {
            message: Msg,
        }
        #[derive(serde::Deserialize)]
        struct Msg {
            content: String,
        }
        let mut req_b = self
            .client
            .post(format!("{}/api/chat", self.url.trim_end_matches('/')))
            .json(&body);
        if let Some(key) = &self.api_key {
            req_b = req_b.bearer_auth(key);
        }
        let resp: Resp = req_b
            .send()
            .await
            .context("ollama chat request")?
            .error_for_status()
            .context("ollama chat status")?
            .json()
            .await
            .context("parsing ollama chat")?;
        Ok(resp.message.content)
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
