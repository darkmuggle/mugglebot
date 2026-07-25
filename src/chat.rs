//! Agent chat (Phase 4).
//!
//! A multimodal chat surface: the engineer talks to MuggleBot and can drop
//! screenshots, images, logs, or files. The agent reasons over everything
//! MuggleBot holds — the board, signals, threads, memory, the context library —
//! through the **same [`crate::tools`] surface as the MCP server**, so
//! "what's going on with service-foo?" and "does this dashboard match the alert
//! in #alerts?" both work.
//!
//! Routing is to the heavy reasoner (Claude), which handles vision for dropped
//! images. The agent runs a small tool-use loop: it emits a JSON action each
//! turn — call a tool, or give a final answer — and we execute tools and feed the
//! results back until it answers or the step budget is spent.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

use crate::reasoner::{self, CompletionRequest, Image, Message, Reasoner, Role};
use crate::tools::{self, Tools};

/// Max tool calls before we force a final answer — a runaway guard.
const MAX_STEPS: usize = 8;

pub struct ChatAgent {
    tools: Arc<Tools>,
    reasoner: Arc<dyn Reasoner>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageInput {
    pub media_type: String,
    pub base64: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub images: Vec<ImageInput>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub tool: String,
    pub arguments: Value,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    pub answer: String,
    pub tool_calls: Vec<ToolCall>,
}

impl ChatAgent {
    pub fn new(tools: Arc<Tools>, reasoner: Arc<dyn Reasoner>) -> Self {
        Self { tools, reasoner }
    }

    /// Respond to a conversation using the agent's default reasoner.
    pub async fn respond(&self, history: &[ChatTurn], tags: &[String]) -> Result<ChatResponse> {
        self.respond_with(history, tags, &self.reasoner).await
    }

    /// Respond to a conversation (the full turn history, last turn = the new user
    /// message, which may carry images), driving the given `reasoner`. The chat
    /// pane picks the provider/model, so the server builds a reasoner per request
    /// and passes it here. `tags` are the routing tags the user attached to the
    /// chat: their tag-matched memory and context are folded in as grounding so
    /// the agent starts with the relevant runbooks/lessons in hand.
    pub async fn respond_with(
        &self,
        history: &[ChatTurn],
        tags: &[String],
        reasoner: &Arc<dyn Reasoner>,
    ) -> Result<ChatResponse> {
        let mut messages: Vec<Message> = history
            .iter()
            .map(|t| {
                let role = match t.role.as_str() {
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                };
                Message {
                    role,
                    content: t.content.clone(),
                    images: t
                        .images
                        .iter()
                        .map(|i| Image {
                            media_type: i.media_type.clone(),
                            base64: i.base64.clone(),
                        })
                        .collect(),
                }
            })
            .collect();

        let system = self.system_prompt(tags);
        let mut tool_calls = Vec::new();

        for _ in 0..MAX_STEPS {
            let req = CompletionRequest {
                system: Some(system.clone()),
                messages: messages.clone(),
                max_tokens: 1500,
                temperature: 0.3,
                // Chat carries its full history in `messages`, so no CLI session
                // (that would double the context).
                session: None,
                // Chat is a conversation, not a recomputation: asking the same
                // question twice should get a fresh answer over whatever the board
                // looks like now, not a replay of the earlier one.
                no_cache: true,
            };
            let text = reasoner.complete(&req).await?;
            let Some(action) = reasoner::extract_json(&text) else {
                // No JSON — treat the whole reply as the final answer.
                return Ok(ChatResponse {
                    answer: text,
                    tool_calls,
                });
            };
            match action.get("action").and_then(|a| a.as_str()) {
                Some("tool") => {
                    let name = action
                        .get("tool")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = action.get("arguments").cloned().unwrap_or(json!({}));
                    debug!("chat: tool {name} {args}");
                    let result = match self.tools.call(&name, &args).await {
                        Ok(v) => v,
                        Err(e) => json!({ "error": format!("{e:#}") }),
                    };
                    tool_calls.push(ToolCall {
                        tool: name,
                        arguments: args,
                        result: result.clone(),
                    });
                    // Feed the call + result back into the transcript.
                    messages.push(Message::text(Role::Assistant, text));
                    messages.push(Message::text(
                        Role::User,
                        format!(
                            "TOOL_RESULT:\n{}",
                            serde_json::to_string(&truncate_result(&result)).unwrap_or_default()
                        ),
                    ));
                }
                _ => {
                    let answer = action
                        .get("answer")
                        .and_then(|a| a.as_str())
                        .map(str::to_string)
                        .unwrap_or(text);
                    return Ok(ChatResponse { answer, tool_calls });
                }
            }
        }
        Ok(ChatResponse {
            answer: "I ran out of steps before finishing — try narrowing the question.".into(),
            tool_calls,
        })
    }

    fn system_prompt(&self, tags: &[String]) -> String {
        let tool_list = tools::definitions()
            .into_iter()
            .map(|d| format!("- {} — {}", d.name, d.description))
            .collect::<Vec<_>>()
            .join("\n");
        let grounding = self.tag_grounding(tags);
        let grounding_block = if grounding.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\nThe engineer attached these tags: {}. Relevant grounding (cite by id — \
                 [mem:ID], [ctx:ID]):\n{grounding}",
                tags.join(", ")
            )
        };
        format!(
            "You are MuggleBot, an ops-awareness assistant with a live view of the engineer's \
             GitHub/Slack/Granola signals, correlated threads, institutional memory, and a curated \
             context library. Answer using the tools below — never invent signal/thread ids, look \
             them up. Cite the evidence you rely on. You inform and propose; you never take an \
             action on a production system.\n\n\
             Each turn, respond with ONE JSON object and nothing else:\n\
             - to use a tool: {{\"action\":\"tool\",\"tool\":\"<name>\",\"arguments\":{{...}}}}\n\
             - to answer: {{\"action\":\"final\",\"answer\":\"<markdown>\"}}\n\n\
             Tools:\n{tool_list}{grounding_block}"
        )
    }

    /// Build a grounding block from the memory and context entries tagged with any
    /// of `tags` — the "tags attach context" behavior. Best-effort and bounded so
    /// a broad tag can't blow the prompt budget.
    fn tag_grounding(&self, tags: &[String]) -> String {
        if tags.is_empty() {
            return String::new();
        }
        const PER_KIND: usize = 5;
        const BODY_CHARS: usize = 1_200;
        let mut out = String::new();
        if let Ok(mems) = self.tools.store.memory_by_tags(tags) {
            for m in mems.into_iter().take(PER_KIND) {
                out.push_str(&format!("[mem:{}] {}\n", m.id, m.summary));
            }
        }
        if let Ok(ctxs) = self.tools.store.context_by_tags(tags) {
            for c in ctxs.into_iter().take(PER_KIND) {
                let summary = c.summary.as_deref().unwrap_or("");
                out.push_str(&format!("[ctx:{}] {} — {}\n", c.id, c.location, summary));
                if let Some(body) = c.raw.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                    let excerpt: String = body.chars().take(BODY_CHARS).collect();
                    out.push_str(&format!("    {excerpt}\n"));
                }
            }
        }
        out
    }
}

/// Cap a tool result fed back into the transcript so a huge board dump can't blow
/// the context budget.
fn truncate_result(v: &Value) -> Value {
    let s = v.to_string();
    if s.len() <= 6000 {
        return v.clone();
    }
    json!({ "truncated": true, "preview": s.chars().take(6000).collect::<String>() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::correlation::{Analyst, Correlator};
    use crate::embed::HashEmbedder;
    use crate::memory::MemoryManager;
    use crate::reasoner::MockReasoner;
    use crate::store::Store;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn tools() -> Arc<Tools> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new("x"));
        let memory = Arc::new(MemoryManager::new(
            store.clone(),
            embedder.clone(),
            reasoner.clone(),
            reasoner.clone(),
        ));
        let context = Arc::new(crate::context::ContextManager::new(
            store.clone(),
            embedder,
            reasoner.clone(),
            reasoner.clone(),
            "6h".into(),
        ));
        let correlator = Arc::new(Correlator::new(store.clone(), Duration::from_secs(1800)));
        let (investigator, repos, browser) =
            crate::rootcause::offline_stack(store.clone(), correlator.clone(), reasoner.clone());
        let analyst = Arc::new(Analyst::new(
            store.clone(),
            correlator.clone(),
            reasoner.clone(),
            reasoner.clone(),
            memory.clone(),
            context.clone(),
            0.8,
            false,
            0.6,
            Duration::from_secs(1800),
        ));
        Arc::new(Tools {
            store,
            correlator,
            analyst,
            memory,
            context,
            reasoner,
            config: Arc::new(crate::config::Config::default()),
            investigator,
            repos,
            browser,
        })
    }

    /// Returns scripted responses in order — to exercise the tool loop.
    struct ScriptReasoner {
        responses: Vec<String>,
        idx: AtomicUsize,
    }
    #[async_trait]
    impl Reasoner for ScriptReasoner {
        async fn complete(&self, _req: &CompletionRequest) -> Result<String> {
            let i = self.idx.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .responses
                .get(i)
                .cloned()
                .unwrap_or_else(|| "{}".into()))
        }
    }

    #[tokio::test]
    async fn plain_text_reply_is_final() {
        let agent = ChatAgent::new(
            tools(),
            Arc::new(MockReasoner::new("Hello, I'm MuggleBot.")),
        );
        let resp = agent
            .respond(
                &[ChatTurn {
                    role: "user".into(),
                    content: "hi".into(),
                    images: vec![],
                }],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(resp.answer, "Hello, I'm MuggleBot.");
        assert!(resp.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn runs_a_tool_then_answers() {
        // First a put_memory tool call, then a final answer.
        let script = ScriptReasoner {
            responses: vec![
                r#"{"action":"tool","tool":"put_memory","arguments":{"text":"pool exhaustion → restart"}}"#.into(),
                r#"{"action":"final","answer":"Saved that to memory."}"#.into(),
            ],
            idx: AtomicUsize::new(0),
        };
        let t = tools();
        let agent = ChatAgent::new(t.clone(), Arc::new(script));
        let resp = agent
            .respond(
                &[ChatTurn {
                    role: "user".into(),
                    content: "remember this".into(),
                    images: vec![],
                }],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(resp.answer, "Saved that to memory.");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].tool, "put_memory");
        assert_eq!(t.memory.list().unwrap().len(), 1);
    }
}
