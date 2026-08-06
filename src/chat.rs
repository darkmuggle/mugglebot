//! Agent chat (Phase 4).
//!
//! A multimodal chat surface: the engineer talks to MuggleBot and can drop
//! screenshots, images, logs, or files. The agent reasons over everything
//! MuggleBot holds — the board, signals, subjects, memory, the context library —
//! through the **same [`crate::tools`] surface as the MCP server**, so
//! "what's going on with service-foo?" and "does this dashboard match the alert
//! in #alerts?" both work.
//!
//! Routing is **local**, like everything else MuggleBot does unsupervised. A message
//! carrying images goes to the local vision model instead, because a coder model has no
//! image encoder and would answer about an attachment it never saw. The chat pane's model
//! picker is how you ask a cloud model instead — that is the operator asking, by name.
//!
//! The agent runs a small tool-use loop: it emits a JSON action each turn — call a tool,
//! or give a final answer — and we execute tools and feed the results back until it
//! answers or the step budget is spent.

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
    /// The local text model — the default for a chat turn.
    reasoner: Arc<dyn Reasoner>,
    /// The local vision model, used when a turn carries images.
    vision: Arc<dyn Reasoner>,
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
    /// The persona this turn was answered *as*, if any.
    ///
    /// Returned so the UI can label the bubble. A simulated colleague rendered as an ordinary
    /// assistant reply is the one presentation this feature must not have — the answer is a
    /// prediction, and a prediction that looks like a quotation is worse than no prediction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
}

impl ChatAgent {
    pub fn new(tools: Arc<Tools>, reasoner: Arc<dyn Reasoner>, vision: Arc<dyn Reasoner>) -> Self {
        Self {
            tools,
            reasoner,
            vision,
        }
    }

    /// Respond to a conversation on the local models.
    ///
    /// Vision only when the turn actually carries an image: the local vision model is
    /// small, and sending ordinary text to it instead of the 33B coder would be a
    /// downgrade paid on every message for the sake of the few with screenshots.
    pub async fn respond(&self, history: &[ChatTurn], tags: &[String]) -> Result<ChatResponse> {
        let has_images = history.iter().any(|t| !t.images.is_empty());
        let reasoner = if has_images {
            &self.vision
        } else {
            &self.reasoner
        };
        self.respond_with(history, tags, reasoner).await
    }

    /// The local text model — what a turn runs on when the operator has not picked one.
    pub fn default_reasoner(&self) -> Arc<dyn Reasoner> {
        self.reasoner.clone()
    }

    /// Respond **as a persona** — talk to a simulated colleague.
    ///
    /// The point is rehearsal: "how will Pavel react if I propose moving this behind a flag?"
    /// is a question worth having an answer to before the meeting, and the honest answer is
    /// sometimes "the profile does not say".
    ///
    /// Three properties keep it honest, and they are the same three the prediction path has:
    ///
    /// - **It is grounded in the profile.** The system prompt is the profile, cited by trait
    ///   id. An empty profile refuses rather than improvising a person.
    /// - **It never fabricates a quotation.** Asked whether they said something, it answers
    ///   from the harvested excerpts or says it does not know. A model producing a plausible
    ///   quote from a real colleague is the worst output this feature can have.
    /// - **It is labelled.** [`ChatResponse::persona`] comes back set, so the pane renders it
    ///   as a simulation rather than as a reply.
    ///
    /// The tool loop is kept, deliberately: asked about a pull request, the simulation should
    /// go and read it rather than reacting to the title. What changes is the framing, not the
    /// grounding.
    pub async fn respond_as(
        &self,
        history: &[ChatTurn],
        tags: &[String],
        reasoner: &Arc<dyn Reasoner>,
        persona: &str,
    ) -> Result<ChatResponse> {
        let Some(profile) = self.tools.store.persona_profile(persona)? else {
            anyhow::bail!("no persona '{persona}'");
        };
        if profile.traits.is_empty() {
            // Refused rather than improvised. A model handed a name and no profile writes a
            // confident colleague out of its own priors, and the operator has no way to tell
            // that from a grounded one.
            return Ok(ChatResponse {
                answer: format!(
                    "Nothing is established about {} yet, so there is no profile to speak from. \
                     Harvest their activity and run a profile pass first — talking to an empty \
                     persona would just be me making somebody up.",
                    profile.persona.display_name
                ),
                tool_calls: Vec::new(),
                persona: Some(persona.to_string()),
            });
        }
        let system = self.persona_prompt(&profile, tags);
        let mut resp = self.run_loop(history, reasoner, system).await?;
        resp.persona = Some(persona.to_string());
        Ok(resp)
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
        let system = self.system_prompt(tags);
        self.run_loop(history, reasoner, system).await
    }

    /// The tool-use loop, driven by whichever system prompt the caller built.
    ///
    /// Extracted so [`Self::respond_as`] can reuse it: talking to a persona changes the
    /// *framing* and nothing about how tools are called, and a second copy of the loop would
    /// be a second place for the step budget and the tool-result truncation to drift.
    async fn run_loop(
        &self,
        history: &[ChatTurn],
        reasoner: &Arc<dyn Reasoner>,
        system: String,
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
                    persona: None,
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
                    return Ok(ChatResponse {
                        answer,
                        tool_calls,
                        persona: None,
                    });
                }
            }
        }
        Ok(ChatResponse {
            answer: "I ran out of steps before finishing — try narrowing the question.".into(),
            tool_calls,
            persona: None,
        })
    }

    /// The framing for talking *as* a persona.
    ///
    /// Written to make the two failure modes hard rather than merely discouraged:
    ///
    /// **Fabricated quotation.** The single worst output available here is a plausible
    /// sentence attributed to a real colleague. So the rule is absolute — paraphrase a
    /// predicted position freely, never present anything as something they said unless it is
    /// one of the harvested excerpts, quoted with its id.
    ///
    /// **Improvised personality.** A model handed a name fills in the rest from its priors,
    /// which is how you get a confident answer about somebody the profile says nothing about.
    /// So "the profile does not cover this" is named as a *good* answer, the same way
    /// `would_engage: false` is named as one in the prediction prompt. [`Self::respond_as`]
    /// refuses outright when the profile is empty, so by the time this prompt is used there is
    /// at least something real behind it.
    fn persona_prompt(&self, profile: &crate::persona::Profile, tags: &[String]) -> String {
        let tool_list = tools::definitions()
            .into_iter()
            // Read tools only. A simulated colleague has no business creating memories,
            // merging subjects, or dispatching workflows on the operator's behalf — and a
            // write executed "as Pavel" would be indistinguishable in the audit log from one
            // the operator asked for.
            .filter(|d| d.read_only)
            .map(|d| format!("- {} — {}", d.name, d.description))
            .collect::<Vec<_>>()
            .join("\n");
        let grounding = self.tag_grounding(tags);
        let grounding_block = if grounding.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nBackground the engineer attached:\n{grounding}")
        };
        format!(
            "You are simulating a specific colleague, {name}, so that the engineer can rehearse \
             a conversation before having it. Answer in the first person, as {name} would.\n\n\
             This is a prediction, not a person. Four rules:\n\
             1. **Never fabricate a quotation.** You may say what {name} would probably think or \
                ask. You may NOT present anything as something they actually said, unless it is \
                one of the excerpts below, quoted with its [ev:ID]. If the engineer asks \
                \"did they say X?\", answer from the excerpts or say you do not know.\n\
             2. **Stay inside the profile.** When something is not covered by it, say so as \
                yourself — \"the profile doesn't cover how they'd feel about that\" — rather \
                than inventing a reaction. That is a good answer, not a failure.\n\
             3. **Keep their register.** Their median message is {median} characters. Do not \
                write a considered essay for somebody who writes two lines.\n\
             4. **Be candid, and stay on behaviour.** If the profile says they will push back \
                hard, push back hard. Never speculate about their health, politics, personal \
                life, or worth as a colleague — none of it is in the evidence.\n\n\
             You can look things up before reacting, and should: react to the actual diff or \
             thread, not to its title. Each turn, respond with ONE JSON object and nothing \
             else:\n\
             - to use a tool: {{\"action\":\"tool\",\"tool\":\"<name>\",\"arguments\":{{...}}}}\n\
             - to answer: {{\"action\":\"final\",\"answer\":\"<markdown, in their voice>\"}}\n\n\
             THE PROFILE\n{profile_block}\n\n\
             THEIR OWN WORDS (the only quotable material — cite as [ev:ID])\n{excerpts}\n\n\
             Tools:\n{tool_list}{grounding_block}",
            name = profile.persona.display_name,
            median = profile.stats.median_excerpt_chars.max(1),
            profile_block = profile.render(),
            excerpts = self.persona_excerpts(&profile.persona.slug),
        )
    }

    /// A bounded block of the persona's own words, which is the only material the simulation
    /// may quote. Best-effort: a read failure degrades to "nothing quotable", which the prompt
    /// already handles, rather than failing the turn.
    fn persona_excerpts(&self, slug: &str) -> String {
        const MAX: usize = 25;
        match self.tools.store.persona_evidence(slug, Some(MAX)) {
            Ok(evidence) if !evidence.is_empty() => evidence
                .iter()
                .map(|e| e.render())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => "(none harvested)".to_string(),
        }
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
             context library. Answer using the tools below — never invent signal/subject ids, look \
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
    use crate::correlation::Analyst;
    use crate::embed::HashEmbedder;
    use crate::memory::MemoryManager;
    use crate::reasoner::MockReasoner;
    use crate::store::Store;
    use crate::subject::Attributor;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn tools() -> Arc<Tools> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let secrets = crate::secrets::Secrets::for_tests(store.clone());
        let scorer = Arc::new(crate::score::Scorer {
            store: store.clone(),
            embedder: Arc::new(crate::embed::HashEmbedder),
        });
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
            secrets.clone(),
            embedder,
            reasoner.clone(),
            reasoner.clone(),
            "6h".into(),
        ));
        let attributor = Arc::new(Attributor::new(store.clone()));
        let (investigator, repos, browser) =
            crate::rootcause::offline_stack(store.clone(), attributor.clone(), reasoner.clone());
        let analyst = Arc::new(Analyst::new(
            store.clone(),
            attributor.clone(),
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
            store: store.clone(),
            agents: Arc::new(crate::agent::AgentSessions::for_tests()),
            // Offline: a real ingress here points at 127.0.0.1:8080, which during development
            // is the operator's own running Restate server — so the suite invoked live handlers
            // against a database it never touched. See `Ingress::offline`.
            ingress: Arc::new(crate::restate::ingress::Ingress::offline()),
            scorer: scorer.clone(),
            secrets,
            attributor,
            analyst,
            memory,
            context,
            reasoner: reasoner.clone(),
            config: Arc::new(crate::config::Config::default()),
            investigator,
            repos,
            browser,
            threads: Arc::new(crate::thread::Analyser::for_tests(store.clone())),
            diffs: Arc::new(
                crate::prdiff::DiffReader::new(None, reasoner.clone(), "local").unwrap(),
            ),
            personas: Arc::new(crate::persona::Engine::for_tests(store)),
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
        let text = Arc::new(MockReasoner::new("Hello, I'm MuggleBot."));
        let agent = ChatAgent::new(tools(), text.clone(), text);
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
        let script: Arc<dyn Reasoner> = Arc::new(script);
        let agent = ChatAgent::new(t.clone(), script.clone(), script);
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
