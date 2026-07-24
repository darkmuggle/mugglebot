//! The shared tool surface.
//!
//! One implementation of every MuggleBot capability — read tools (the board,
//! threads, timelines, search, alerts, mitigations, health), correlation writes
//! (relate / split / attach-context / reanalyze), grounding (memory + context
//! CRUD and semantic recall), and live-assist (list / dismiss hints). Both the
//! MCP server and the built-in agent chat dispatch through here, so the two
//! reason over identical grounding with identical tools.
//!
//! Read tools are free; write tools carry `read_only = false` risk metadata so a
//! client (or the MCP gate) can treat them differently. Nothing here mutates a
//! production system — the writes are all to MuggleBot's own store.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::config::{self, Config};
use crate::context::ContextSourceKind;
use crate::correlation::{Analyst, ContextKind, Correlator, RelationKind};
use crate::live::HintState;
use crate::memory::MemoryManager;
use crate::mitigations;
use crate::reasoner::{CompletionRequest, Reasoner};
use crate::signal::{Signal, SignalKind, Source, State};
use crate::store::{SignalFilter, Store};

pub struct Tools {
    pub store: Arc<Store>,
    pub correlator: Arc<Correlator>,
    pub analyst: Arc<Analyst>,
    pub memory: Arc<MemoryManager>,
    pub context: Arc<crate::context::ContextManager>,
    /// Heavy reasoner, for on-demand deep work like postmortem drafting.
    pub reasoner: Arc<dyn Reasoner>,
    pub config: Arc<Config>,
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub schema: Value,
}

pub struct ResourceDef {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

impl Tools {
    /// Dispatch a tool call by name. Unknown names error.
    pub async fn call(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            // ---- read ----
            "list_signals" => self.list_signals(args),
            "get_signal" => self.get_signal(args),
            "list_threads" => self.list_threads(args),
            "get_thread" => self.get_thread(args),
            "timeline" => self.timeline(args),
            "search" => self.search(args),
            "list_alerts" => self.list_alerts(args),
            "suggest_mitigations" => self.suggest_mitigations(args).await,
            "source_health" => Ok(json!(self.store.source_health()?)),
            "draft_postmortem" => self.draft_postmortem(args).await,
            "distill_memory" => self.distill_memory(args).await,
            // ---- correlation (write) ----
            "relate" => self.relate(args).await,
            "split_thread" => self.split_thread(args).await,
            "attach_thread_context" => self.attach_thread_context(args).await,
            "reanalyze" => self.reanalyze(args).await,
            // ---- grounding ----
            "search_memory" => self.search_memory(args).await,
            "search_context" => self.search_context(args).await,
            "list_memories" => Ok(json!(self.memory.list()?)),
            "get_memory" => Ok(json!(self.memory.get(req_str(args, "id")?.as_str())?)),
            "put_memory" => self.put_memory(args).await,
            "edit_memory" => self.edit_memory(args).await,
            "tag_memory" => self.tag_memory(args).await,
            "delete_memory" => {
                self.memory.delete(req_str(args, "id")?.as_str())?;
                Ok(json!({ "ok": true }))
            }
            "list_context" => Ok(json!(self.context.list()?)),
            "get_context" => Ok(json!(self.context.get(req_str(args, "id")?.as_str())?)),
            "add_context" => self.add_context(args).await,
            "tag_context" => self.tag_context(args).await,
            "list_tags" => Ok(json!(self.store.list_tags()?)),
            "edit_tag" => self.edit_tag(args).await,
            "delete_tag" => self.delete_tag(args),
            "merge_tags" => self.merge_tags(args),
            "set_thread_tags" => self.set_thread_tags(args).await,
            "refresh_context" => {
                let changed = self.context.refresh(req_str(args, "id")?.as_str()).await?;
                Ok(json!({ "changed": changed }))
            }
            "remove_context" => {
                self.context.remove(req_str(args, "id")?.as_str())?;
                Ok(json!({ "ok": true }))
            }
            // ---- live assist ----
            "list_hints" => Ok(json!(self
                .store
                .list_hints(opt_str(args, "thread_id").as_deref())?)),
            "dismiss_hint" => self.dismiss_hint(args).await,
            other => bail!("unknown tool '{other}'"),
        }
    }

    // ---- read tools ---------------------------------------------------------

    fn list_signals(&self, args: &Value) -> Result<Value> {
        let filter = SignalFilter {
            source: opt_str(args, "source").and_then(|s| Source::parse(&s)),
            since: opt_str(args, "since")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            min_severity: opt_str(args, "severity").map(|s| config::severity_from_str(&s)),
            state: opt_str(args, "state").and_then(|s| parse_state(&s)),
            limit: args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize),
        };
        Ok(json!(self.store.list_signals(&filter)?))
    }

    fn get_signal(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        Ok(json!(self.store.get_signal(&id)?))
    }

    fn list_threads(&self, args: &Value) -> Result<Value> {
        let active_only = args
            .get("active_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        Ok(json!(self.correlator.thread_views(active_only)?))
    }

    fn get_thread(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        Ok(json!(self.correlator.thread_view(&id)?))
    }

    fn timeline(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        let events: Vec<Value> = self
            .store
            .signals_for_thread(&id)?
            .into_iter()
            .map(|s| {
                json!({
                    "signal_id": s.id,
                    "occurred_at": s.occurred_at.to_rfc3339(),
                    "source": s.source.as_str(),
                    "kind": s.kind,
                    "state": s.state,
                    "severity": s.severity,
                    "actor": s.actor,
                    "entities": s.entities,
                    "title": s.title,
                    "body": s.body,
                    "url": s.url,
                    "ci_outcome": s.raw.get("ci_outcome"),
                    "ci_log_url": s.raw.get("ci_log_url"),
                })
            })
            .collect();
        Ok(json!({ "thread_id": id, "events": events }))
    }

    fn search(&self, args: &Value) -> Result<Value> {
        let q = req_str(args, "query")?;
        Ok(json!(self.store.search_signals(&q, 50)?))
    }

    fn list_alerts(&self, args: &Value) -> Result<Value> {
        let state = opt_str(args, "state").and_then(|s| parse_state(&s));
        let alerts: Vec<_> = self
            .store
            .list_signals(&SignalFilter {
                source: Some(Source::Slack),
                state,
                limit: Some(500),
                ..Default::default()
            })?
            .into_iter()
            .filter(|s| s.kind == SignalKind::Alert)
            .collect();
        Ok(json!(alerts))
    }

    /// Mitigation-assist: generate thread-specific first moves with the reasoner,
    /// grounded in the thread's signals + context and seeded by the generic
    /// catalog. Falls back to deterministic keyword matching against the catalog
    /// when the reasoner is unreachable or returns nothing usable — so the panel
    /// still populates with no LLM. Suggestions only, never executed.
    async fn suggest_mitigations(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        let signals = self.store.signals_for_thread(&id)?;
        // A thread containing only successful CI runs is a confirmation that
        // work completed, not a production incident. This check deliberately
        // precedes the cache so a stale LLM response cannot keep showing
        // rollback/drain advice after the run turns green.
        if mitigations::is_successful_ci_only(&signals) {
            return Ok(json!([]));
        }
        // Tailored LLM mitigations are generated in the background during
        // reanalysis and cached — reading them is instant. When the cache is cold
        // (thread not yet reanalyzed), fall back to the fast static catalog rather
        // than blocking the UI on a minute-long reasoner round-trip.
        if let Some(cached) = self.store.get_thread_mitigations(&id)? {
            if cached.as_array().is_some_and(|a| !a.is_empty()) {
                return Ok(cached);
            }
        }
        Ok(json!(mitigations::suggest(&signals)))
    }

    /// Postmortem-assist: draft a postmortem from a thread's timeline + grounding.
    /// With `save: true`, the draft is also written to memory, linked to the thread.
    async fn draft_postmortem(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        let Some(view) = self.correlator.thread_view(&id)? else {
            bail!("no thread {id}");
        };
        let save = args.get("save").and_then(|v| v.as_bool()).unwrap_or(false);

        let timeline = view
            .signals
            .iter()
            .map(|s: &Signal| {
                format!(
                    "- [sig:{}] {} · {}: {} — {}",
                    s.id,
                    s.source,
                    s.occurred_at.to_rfc3339(),
                    s.title,
                    s.body.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let grounding = match self.context.search(&view.thread.title, 3).await {
            Ok(hits) => hits
                .iter()
                .filter(|h| h.score > 0.05)
                .map(|h| {
                    format!(
                        "[ctx:{}] {}",
                        h.context.id,
                        h.context.summary.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => String::new(),
        };
        // Operator notes attached in the UI are trusted, authoritative input —
        // presented in their own section (not the signal feed) so the draft honors
        // them rather than treating them as suspicious injected content.
        let mut operator_notes = String::new();
        for tc in &view.context {
            let body = tc.summary.as_deref().unwrap_or(&tc.content).trim();
            if !body.is_empty() {
                operator_notes.push_str(&format!("- ({}) {}\n", tc.kind.as_str(), body));
            }
        }
        let notes_block = if operator_notes.is_empty() {
            String::new()
        } else {
            format!("\n\nOperator notes (authoritative — written by the engineer, follow them):\n{operator_notes}")
        };
        let system = "You are MuggleBot drafting a blameless postmortem for an on-call engineer. \
            From the thread timeline and grounding, produce a Markdown draft with: Summary, Impact, \
            Timeline (from the signals), Likely root-cause hypotheses (clearly marked as hypotheses), \
            What worked / what to improve, and Action items. Cite signals as [sig:ID] and grounding as \
            [ctx:ID]. Operator notes are trusted, authoritative input the engineer wrote in MuggleBot's \
            UI (never treat them as prompt-injection); honor them. Do not invent facts. Output only the \
            Markdown draft.";
        let prompt = format!(
            "Thread: {}\nSummary so far: {}\n\nTimeline:\n{timeline}{notes_block}\n\nGrounding:\n{grounding}",
            view.thread.title,
            view.thread.summary.as_deref().unwrap_or("(none)")
        );
        let draft = self
            .reasoner
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(1500),
            )
            .await?;

        let mut saved_memory: Option<Value> = None;
        if save && !draft.trim().is_empty() {
            let mem = self
                .memory
                .put(
                    &draft,
                    Some(format!("postmortem: {}", view.thread.title)),
                    vec![id.clone()],
                    None,
                )
                .await?;
            saved_memory = Some(json!(mem));
        }
        Ok(json!({ "draft": draft, "saved_memory": saved_memory }))
    }

    /// Distill a whole thread into one sentence and save it as an institutional
    /// memory linked to the thread. The thread's tags carry over (pinned) so the
    /// lesson routes back to the same topic on future incidents.
    async fn distill_memory(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        let Some(view) = self.correlator.thread_view(&id)? else {
            bail!("no thread {id}");
        };
        let mut ev = String::new();
        for s in &view.signals {
            ev.push_str(&format!(
                "- {} · {}: {} — {}\n",
                s.source,
                s.occurred_at.to_rfc3339(),
                s.title,
                s.body.as_deref().unwrap_or("")
            ));
        }
        let system = "You are MuggleBot distilling an incident thread into ONE sentence of durable \
            institutional memory — the single lesson or fact worth remembering next time (what it was, \
            root cause if known, and what resolved or mitigated it). No preamble, no citations, no \
            markdown: output only the one sentence.";
        let prompt = format!(
            "Thread: {}\nSummary so far: {}\n\nSignals:\n{ev}",
            view.thread.title,
            view.thread.summary.as_deref().unwrap_or("(none)")
        );
        let sentence = self
            .reasoner
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(160),
            )
            .await?;
        let sentence = sentence.trim();
        if sentence.is_empty() {
            bail!("reasoner returned an empty summary");
        }
        // Carry the thread's tags over as pinned tags when it has them, so the
        // memory routes to the same topic; otherwise let the auto-tagger fill them.
        let tags = (!view.thread.tags.is_empty()).then(|| view.thread.tags.clone());
        let mem = self
            .memory
            .put(sentence, Some(sentence.to_string()), vec![id.clone()], tags)
            .await?;
        Ok(json!(mem))
    }

    // ---- correlation writes -------------------------------------------------

    async fn relate(&self, args: &Value) -> Result<Value> {
        let a = req_str(args, "thread_a")?;
        let b = req_str(args, "thread_b")?;
        let kind = RelationKind::parse(&req_str(args, "kind")?)
            .ok_or_else(|| anyhow!("kind must be same|related|distinct"))?;
        let canonical = self.analyst.relate(&a, &b, kind).await?;
        Ok(json!({ "ok": true, "canonical_thread": canonical }))
    }

    async fn split_thread(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        let signal_ids: Vec<String> = args
            .get("signal_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if signal_ids.is_empty() {
            bail!("signal_ids must be a non-empty array");
        }
        let new_id = self.analyst.split_thread(&id, &signal_ids).await?;
        Ok(json!({ "ok": true, "new_thread": new_id }))
    }

    async fn attach_thread_context(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        let (kind, content) = if let Some(url) = opt_str(args, "url") {
            (ContextKind::Url, url)
        } else if let Some(text) = opt_str(args, "text") {
            (ContextKind::Text, text)
        } else {
            bail!("provide either `text` or `url`");
        };
        let tc = self
            .analyst
            .attach_thread_context(&id, kind, &content)
            .await?;
        Ok(json!(tc))
    }

    async fn reanalyze(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        // Optional one-off model override: reconsider the thread on a chosen
        // provider/model without touching the daemon's configured reasoners.
        let reasoner = match (opt_str(args, "provider"), opt_str(args, "model")) {
            (Some(provider), Some(model)) => {
                let ollama_key = self.store.credential_get("ollama").ok().flatten();
                Some(crate::reasoner::build(
                    crate::reasoner::provider_label(&provider),
                    &model,
                    &self.config.reasoner,
                    ollama_key,
                ))
            }
            _ => None,
        };
        self.analyst.reanalyze_with(&id, reasoner).await?;
        Ok(json!({ "ok": true }))
    }

    // ---- grounding ----------------------------------------------------------

    async fn search_memory(&self, args: &Value) -> Result<Value> {
        let q = req_str(args, "query")?;
        let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        Ok(json!(self.memory.search(&q, k).await?))
    }

    async fn search_context(&self, args: &Value) -> Result<Value> {
        let q = req_str(args, "query")?;
        let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        Ok(json!(self.context.search(&q, k).await?))
    }

    async fn put_memory(&self, args: &Value) -> Result<Value> {
        let text = req_str(args, "text")?;
        let summary = opt_str(args, "summary");
        let links = str_array(args, "links");
        let tags = args.get("tags").map(|_| str_array(args, "tags"));
        Ok(json!(self.memory.put(&text, summary, links, tags).await?))
    }

    async fn tag_memory(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let tags = str_array(args, "tags");
        match self.memory.set_tags(&id, tags)? {
            Some(m) => Ok(json!(m)),
            None => bail!("no memory {id}"),
        }
    }

    async fn edit_memory(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let text = req_str(args, "text")?;
        let summary = opt_str(args, "summary");
        match self.memory.edit(&id, &text, summary).await? {
            Some(m) => Ok(json!(m)),
            None => bail!("no memory {id}"),
        }
    }

    async fn add_context(&self, args: &Value) -> Result<Value> {
        let (kind, location) = if let Some(url) = opt_str(args, "url") {
            (ContextSourceKind::Url, url)
        } else if let Some(path) = opt_str(args, "path") {
            (ContextSourceKind::File, path)
        } else {
            bail!("provide either `url` or `path`");
        };
        let credential = opt_str(args, "credential");
        let header = opt_str(args, "header");
        let refresh = opt_str(args, "refresh_interval");
        let tags = args.get("tags").map(|_| str_array(args, "tags"));
        let ctx = self
            .context
            .add(kind, &location, credential, header, refresh, tags)
            .await?;
        Ok(json!(ctx))
    }

    async fn tag_context(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let tags = str_array(args, "tags");
        Ok(json!(self.context.set_tags(&id, tags)?))
    }

    /// Set (pin) a thread's tags from a human edit on the board, then re-run its
    /// analysis so the corrected routing propagates — mirrors relation pins.
    async fn set_thread_tags(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "thread_id")?;
        let tags = crate::tags::normalize_tags(str_array(args, "tags"));
        for t in &tags {
            self.store.ensure_tag(t, "", chrono::Utc::now())?;
        }
        self.store.set_thread_tags(&id, &tags, true)?;
        self.analyst.reanalyze(&id).await?;
        Ok(json!({ "ok": true, "tags": tags }))
    }

    async fn edit_tag(&self, args: &Value) -> Result<Value> {
        let name = crate::tags::normalize_tag(&req_str(args, "name")?)
            .ok_or_else(|| anyhow!("invalid tag name"))?;
        let summary = req_str(args, "summary")?;
        self.store
            .set_tag_summary(&name, &summary, chrono::Utc::now())?;
        Ok(json!(self.store.get_tag(&name)?))
    }

    /// Remove a tag from the vocabulary and strip the label off all content that
    /// carried it, so the classifier no longer offers it and nothing keeps a
    /// dangling reference.
    fn delete_tag(&self, args: &Value) -> Result<Value> {
        let name = crate::tags::normalize_tag(&req_str(args, "name")?)
            .ok_or_else(|| anyhow!("invalid tag name"))?;
        let stripped = self.store.rewrite_tag_in_content(&name, None)?;
        self.store.delete_tag(&name)?;
        Ok(json!({ "ok": true, "stripped_from": stripped }))
    }

    /// Merge one tag into another (also serves rename when `into` is new):
    /// rewrite the label across all content, carry the source summary if the
    /// target has none, and drop the source from the vocabulary.
    fn merge_tags(&self, args: &Value) -> Result<Value> {
        let from = crate::tags::normalize_tag(&req_str(args, "from")?)
            .ok_or_else(|| anyhow!("invalid `from` tag"))?;
        let into = crate::tags::normalize_tag(&req_str(args, "into")?)
            .ok_or_else(|| anyhow!("invalid `into` tag"))?;
        if from == into {
            bail!("`from` and `into` are the same tag");
        }
        let now = chrono::Utc::now();
        // Ensure the target exists, carrying the source's summary if it has none.
        let carry = self
            .store
            .get_tag(&from)?
            .map(|t| t.summary)
            .unwrap_or_default();
        self.store.ensure_tag(&into, &carry, now)?;
        let moved = self.store.rewrite_tag_in_content(&from, Some(&into))?;
        self.store.delete_tag(&from)?;
        Ok(json!({ "ok": true, "into": into, "moved": moved }))
    }

    // ---- live assist --------------------------------------------------------

    async fn dismiss_hint(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let false_positive = args
            .get("false_positive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let Some(hint) = self.store.get_hint(&id)? else {
            bail!("no hint {id}");
        };
        let state = if false_positive {
            HintState::FalsePositive
        } else {
            HintState::Dismissed
        };
        self.store.set_hint_state(&id, state)?;
        // A false-positive teaches memory not to re-raise the same thing.
        if false_positive {
            let text = format!(
                "False positive (do not re-flag): {}. Rationale was: {}",
                hint.text,
                hint.rationale.as_deref().unwrap_or("n/a")
            );
            let _ = self
                .memory
                .put(
                    &text,
                    Some("live-assist false positive".into()),
                    vec![hint.thread_id.clone()],
                    None,
                )
                .await;
        }
        Ok(json!({ "ok": true }))
    }

    // ---- resources ----------------------------------------------------------

    pub async fn read_resource(&self, uri: &str) -> Result<Value> {
        match uri {
            "board://current" => Ok(json!({
                "signals": self.store.recent(200)?,
                "threads": self.correlator.thread_views(true)?,
                "health": self.store.source_health()?,
            })),
            "config://redacted" => Ok(serde_json::to_value(&*self.config)?),
            "memory://" => Ok(json!(self.memory.list()?)),
            "context://" => Ok(json!(self.context.list()?)),
            "live://hints" => Ok(json!(self.store.list_hints(None)?)),
            other => bail!("unknown resource '{other}'"),
        }
    }
}

/// The tool catalog for MCP `tools/list` and the chat system prompt.
pub fn definitions() -> Vec<ToolDef> {
    // Small schema builders keep the list readable.
    fn obj(props: Value, required: &[&str]) -> Value {
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        })
    }
    let s = || json!({ "type": "string" });
    let none = || json!({ "type": "object", "properties": {}, "additionalProperties": false });

    vec![
        ToolDef { name: "list_signals", read_only: true,
            description: "The current board: recent signals, optionally filtered by source, since (RFC3339), minimum severity, or state.",
            schema: obj(json!({ "source": s(), "since": s(), "severity": s(), "state": s(), "limit": {"type":"integer"} }), &[]) },
        ToolDef { name: "get_signal", read_only: true,
            description: "Full detail for one signal, including deep-link and raw payload.",
            schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "list_threads", read_only: true,
            description: "Correlated topics (threads) as views with their signals, summary, severity, state, relation edges, and attached context.",
            schema: obj(json!({ "active_only": {"type":"boolean"} }), &[]) },
        ToolDef { name: "get_thread", read_only: true,
            description: "One thread view: signals + summary + timeline + relation graph + context.",
            schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "timeline", read_only: true,
            description: "Reconstructed, ordered event timeline for a thread.",
            schema: obj(json!({ "thread_id": s() }), &["thread_id"]) },
        ToolDef { name: "search", read_only: true,
            description: "Keyword search across ingested signals (title + body).",
            schema: obj(json!({ "query": s() }), &["query"]) },
        ToolDef { name: "list_alerts", read_only: true,
            description: "Signals from Slack alert channels, optionally filtered by state.",
            schema: obj(json!({ "state": s() }), &[]) },
        ToolDef { name: "suggest_mitigations", read_only: true,
            description: "Generate thread-specific first mitigations with the reasoner (grounded in the thread's signals + context, seeded by the generic catalog); falls back to catalog keyword-matching with no reasoner. Suggestions only, never executed.",
            schema: obj(json!({ "thread_id": s() }), &["thread_id"]) },
        ToolDef { name: "draft_postmortem", read_only: false,
            description: "Draft a blameless postmortem from a thread's timeline + grounding. `save: true` also stores it to memory.",
            schema: obj(json!({ "thread_id": s(), "save": {"type":"boolean"} }), &["thread_id"]) },
        ToolDef { name: "source_health", read_only: true,
            description: "Per-watcher status: last poll, last success, current error, cursor.",
            schema: none() },
        ToolDef { name: "relate", read_only: false,
            description: "Pin a same|related|distinct edge between two threads (associate, mark duplicate/merge, or dissociate). Triggers re-analysis; pins always win.",
            schema: obj(json!({ "thread_a": s(), "thread_b": s(), "kind": s() }), &["thread_a","thread_b","kind"]) },
        ToolDef { name: "split_thread", read_only: false,
            description: "Pull wrongly-grouped signals out of a thread into a new one, then re-analyze both.",
            schema: obj(json!({ "thread_id": s(), "signal_ids": {"type":"array","items": s()} }), &["thread_id","signal_ids"]) },
        ToolDef { name: "attach_thread_context", read_only: false,
            description: "Attach ad-hoc grounding (free `text` or a `url`) to a thread; triggers re-analysis.",
            schema: obj(json!({ "thread_id": s(), "text": s(), "url": s() }), &["thread_id"]) },
        ToolDef { name: "reanalyze", read_only: false,
            description: "Force the LLM correlation pass to re-run for a thread. Optional `provider` (anthropic|openai|ollama|ollama_local) and `model` reconsider it on a chosen model for this run only.",
            schema: obj(json!({ "thread_id": s(), "provider": s(), "model": s() }), &["thread_id"]) },
        ToolDef { name: "distill_memory", read_only: false,
            description: "Summarize a thread down to a single-sentence institutional-memory entry (linked to the thread) and save it. Returns the created memory.",
            schema: obj(json!({ "thread_id": s() }), &["thread_id"]) },
        ToolDef { name: "search_memory", read_only: true,
            description: "Semantic recall over the memory store.",
            schema: obj(json!({ "query": s(), "k": {"type":"integer"} }), &["query"]) },
        ToolDef { name: "search_context", read_only: true,
            description: "Semantic recall over the curated context library.",
            schema: obj(json!({ "query": s(), "k": {"type":"integer"} }), &["query"]) },
        ToolDef { name: "list_memories", read_only: true, description: "Browse memory entries.", schema: none() },
        ToolDef { name: "get_memory", read_only: true, description: "Get one memory entry.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "put_memory", read_only: false,
            description: "Create a memory entry (one fact + one-line summary), optionally linked to signal/thread ids. Optional `tags` (array) pin routing tags; omit them to auto-suggest tags from the fact.",
            schema: obj(json!({ "text": s(), "summary": s(), "links": {"type":"array","items": s()}, "tags": {"type":"array","items": s()} }), &["text"]) },
        ToolDef { name: "edit_memory", read_only: false, description: "Edit a memory entry (re-tags automatically unless tags are pinned).",
            schema: obj(json!({ "id": s(), "text": s(), "summary": s() }), &["id","text"]) },
        ToolDef { name: "tag_memory", read_only: false,
            description: "Set (pin) a memory entry's tags; registers any new tags in the vocabulary.",
            schema: obj(json!({ "id": s(), "tags": {"type":"array","items": s()} }), &["id","tags"]) },
        ToolDef { name: "delete_memory", read_only: false, description: "Delete a memory entry.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "list_context", read_only: true, description: "Browse the context library.", schema: none() },
        ToolDef { name: "get_context", read_only: true, description: "Get one context source.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "add_context", read_only: false,
            description: "Add a context source: a `url` (optionally `credential`/`header` for authed fetch) or a local `path`. Optional `tags` (array) pin categorical routing tags; omit them to have the ingest pipeline auto-suggest tags.",
            schema: obj(json!({ "url": s(), "path": s(), "credential": s(), "header": s(), "refresh_interval": s(), "tags": {"type":"array","items": s()} }), &[]) },
        ToolDef { name: "tag_context", read_only: false,
            description: "Set (pin) a context source's tags. Overwrites auto-suggested tags with the given list; registers any new tags in the vocabulary.",
            schema: obj(json!({ "id": s(), "tags": {"type":"array","items": s()} }), &["id","tags"]) },
        ToolDef { name: "list_tags", read_only: true,
            description: "The tag vocabulary: each tag with the short summary the classifier reads to decide which tags apply to an issue.",
            schema: none() },
        ToolDef { name: "edit_tag", read_only: false,
            description: "Set a tag's summary (the description used to route issues to this tag).",
            schema: obj(json!({ "name": s(), "summary": s() }), &["name","summary"]) },
        ToolDef { name: "delete_tag", read_only: false,
            description: "Remove a tag from the vocabulary and strip the label off all content that carried it.",
            schema: obj(json!({ "name": s() }), &["name"]) },
        ToolDef { name: "merge_tags", read_only: false,
            description: "Merge one tag into another (also renames when `into` is new): rewrites the label across all content and drops the source tag.",
            schema: obj(json!({ "from": s(), "into": s() }), &["from","into"]) },
        ToolDef { name: "set_thread_tags", read_only: false,
            description: "Set (pin) the tags on a thread/issue on the board and re-run its analysis so grounding re-routes. Pinned tags are not overwritten by the classifier.",
            schema: obj(json!({ "thread_id": s(), "tags": {"type":"array","items": s()} }), &["thread_id","tags"]) },
        ToolDef { name: "refresh_context", read_only: false, description: "Force an immediate re-fetch of a context source.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "remove_context", read_only: false, description: "Remove a context source.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "list_hints", read_only: true, description: "Active live-assist hints, suggestions, and flags, optionally scoped to a thread.",
            schema: obj(json!({ "thread_id": s() }), &[]) },
        ToolDef { name: "dismiss_hint", read_only: false,
            description: "Dismiss a hint/flag. `false_positive: true` feeds it back to memory so it isn't re-raised.",
            schema: obj(json!({ "id": s(), "false_positive": {"type":"boolean"} }), &["id"]) },
    ]
}

pub fn resources() -> Vec<ResourceDef> {
    vec![
        ResourceDef {
            uri: "board://current",
            name: "Board",
            description: "Live board snapshot: signals, threads, source health.",
        },
        ResourceDef {
            uri: "config://redacted",
            name: "Config",
            description: "Effective configuration (no secrets — those live in the database).",
        },
        ResourceDef {
            uri: "memory://",
            name: "Memory",
            description: "Browsable institutional-memory store.",
        },
        ResourceDef {
            uri: "context://",
            name: "Context",
            description: "Browsable context library.",
        },
        ResourceDef {
            uri: "live://hints",
            name: "Live hints",
            description: "Active live-assist hints and flags.",
        },
    ]
}

// ---- arg helpers ------------------------------------------------------------

fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required string arg `{key}`"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_state(s: &str) -> Option<State> {
    match s.trim().to_ascii_lowercase().as_str() {
        "unseen" => Some(State::Unseen),
        "seen" => Some(State::Seen),
        "acknowledged" => Some(State::Acknowledged),
        "resolved" => Some(State::Resolved),
        "snoozed" => Some(State::Snoozed),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::reasoner::MockReasoner;
    use crate::signal::{Entity, Severity};
    use chrono::Utc;
    use std::time::Duration;

    fn tools(reasoner_response: &str) -> Tools {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new(reasoner_response));
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
        let analyst = Arc::new(Analyst::new(
            store.clone(),
            correlator.clone(),
            reasoner.clone(),
            memory.clone(),
            context.clone(),
            0.8,
            false,
            Duration::from_secs(1800),
        ));
        Tools {
            store,
            correlator,
            analyst,
            memory,
            context,
            reasoner,
            config: Arc::new(Config::default()),
        }
    }

    fn seed(t: &Tools) -> String {
        let s = Signal {
            id: Signal::make_id(Source::Slack, "1"),
            source: Source::Slack,
            external_id: "1".into(),
            kind: SignalKind::Alert,
            title: "service-foo 5xx spike".into(),
            body: Some("connection pool exhausted".into()),
            url: None,
            actor: None,
            entities: vec![Entity::new("service", "foo")],
            severity: Severity::Critical,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: serde_json::json!({ "is_alert": true }),
            tags: Vec::new(),
        };
        t.store.insert_signal(&s).unwrap();
        t.correlator.ingest(&s).unwrap()
    }

    #[tokio::test]
    async fn timeline_and_mitigations_dispatch() {
        let t = tools("noop");
        let tid = seed(&t);
        let tl = t
            .call("timeline", &json!({ "thread_id": tid }))
            .await
            .unwrap();
        assert_eq!(tl["events"].as_array().unwrap().len(), 1);
        assert_eq!(tl["events"][0]["body"], "connection pool exhausted");
        assert_eq!(tl["events"][0]["severity"], "critical");

        let mit = t
            .call("suggest_mitigations", &json!({ "thread_id": tid }))
            .await
            .unwrap();
        assert!(
            !mit.as_array().unwrap().is_empty(),
            "pool exhaustion should match a mitigation"
        );

        let alerts = t.call("list_alerts", &json!({})).await.unwrap();
        assert_eq!(alerts.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn analyst_generates_tailored_mitigations() {
        // The reasoner's JSON becomes shaped mitigations (gen-N ids), and phantom
        // citations not present in the thread are dropped.
        let sig_id = Signal::make_id(Source::Slack, "1");
        let resp = format!(
            r#"Here you go: {{"mitigations":[{{"name":"Upsize service-foo's connection pool","description":"Raise the pool ceiling to buy headroom while the leak is diagnosed.","reversible":true,"cited_signals":["{sig_id}","sig-phantom"]}}]}}"#
        );
        let t = tools(&resp);
        let tid = seed(&t);
        let gen = t
            .analyst
            .generate_mitigations(&tid, t.reasoner.as_ref())
            .await
            .unwrap();
        assert_eq!(gen.len(), 1);
        assert_eq!(gen[0]["name"], "Upsize service-foo's connection pool");
        assert_eq!(gen[0]["id"], "gen-0");
        let cited = gen[0]["cited_signals"].as_array().unwrap();
        assert_eq!(cited.len(), 1);
        assert_eq!(cited[0], sig_id);
    }

    #[tokio::test]
    async fn suggest_mitigations_reads_cache_else_catalog() {
        // Cold cache → the fast static catalog (seed's "connection pool exhausted"
        // ranks `upsize`), never a blocking reasoner call.
        let t = tools("{}");
        let tid = seed(&t);
        let cold = t
            .call("suggest_mitigations", &json!({ "thread_id": tid }))
            .await
            .unwrap();
        assert_eq!(cold.as_array().unwrap()[0]["id"], "upsize");

        // Warm cache (as background reanalysis populates it) → returned verbatim.
        let cached = json!([{
            "id": "gen-0", "name": "Fix the missing module", "description": "…",
            "reversible": true, "score": 1.0, "cited_signals": []
        }]);
        t.store.set_thread_mitigations(&tid, &cached).unwrap();
        let warm = t
            .call("suggest_mitigations", &json!({ "thread_id": tid }))
            .await
            .unwrap();
        assert_eq!(warm[0]["id"], "gen-0");
        assert_eq!(warm[0]["name"], "Fix the missing module");
    }

    #[tokio::test]
    async fn successful_ci_hides_stale_cached_mitigations() {
        let t = tools("{}");
        let signal = Signal {
            id: Signal::make_id(Source::GitHub, "ci-success"),
            source: Source::GitHub,
            external_id: "ci-success".into(),
            kind: SignalKind::CiFailure,
            title: "Data Plane Images workflow run succeeded for main branch".into(),
            body: Some("CI/CD log tail: all tests passed".into()),
            url: None,
            actor: None,
            entities: vec![],
            severity: Severity::Info,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: json!({ "subject_type": "CheckSuite", "ci_outcome": "success" }),
            tags: Vec::new(),
        };
        t.store.insert_signal(&signal).unwrap();
        let tid = t.correlator.ingest(&signal).unwrap();
        t.store
            .set_thread_mitigations(
                &tid,
                &json!([{
                    "id": "gen-0", "name": "Roll back", "description": "stale",
                    "reversible": true, "score": 1.0, "cited_signals": [signal.id]
                }]),
            )
            .unwrap();

        let result = t
            .call("suggest_mitigations", &json!({ "thread_id": tid }))
            .await
            .unwrap();
        assert_eq!(result, json!([]));
    }

    #[tokio::test]
    async fn draft_postmortem_saves_to_memory() {
        let t = tools("## Postmortem\nService foo saturated. [sig:x]");
        let tid = seed(&t);
        let r = t
            .call(
                "draft_postmortem",
                &json!({ "thread_id": tid, "save": true }),
            )
            .await
            .unwrap();
        assert!(r["draft"].as_str().unwrap().contains("Postmortem"));
        assert!(!r["saved_memory"].is_null());
        assert_eq!(t.memory.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn distill_memory_saves_one_sentence() {
        let t = tools(
            "Pool exhaustion under load saturates service-foo; raising the pool ceiling clears it.",
        );
        let tid = seed(&t);
        let r = t
            .call("distill_memory", &json!({ "thread_id": tid }))
            .await
            .unwrap();
        // The created memory's summary is the distilled sentence, linked to the thread.
        assert!(r["summary"].as_str().unwrap().contains("Pool exhaustion"));
        assert_eq!(r["links"][0].as_str().unwrap(), tid);
        assert_eq!(t.memory.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let t = tools("noop");
        assert!(t.call("nonexistent", &json!({})).await.is_err());
    }
}
