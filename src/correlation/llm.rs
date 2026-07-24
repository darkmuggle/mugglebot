//! The semantic-reasoning tier (Phase 2): an LLM judges candidate thread pairs
//! and writes the relation graph, refreshes grounded thread summaries, and
//! reconciles the graph around human override pins.
//!
//! Everything here degrades gracefully: with no reachable reasoner (no key, no
//! bridge, Ollama down) the deterministic summary and grouping stand, edges are
//! simply not written, and the daemon keeps working.

use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use super::{ContextKind, Correlator, Edge, Provenance, RelationKind, ThreadContext, ThreadView};
use crate::context::{ContextManager, ContextSourceKind};
use crate::memory::MemoryManager;
use crate::reasoner::{self, CompletionRequest, Reasoner};
use crate::signal::Signal;
use crate::store::Store;
use crate::tags;

/// How many candidate threads to judge per re-analysis, most-shared first — a cap
/// on LLM calls per pass.
const MAX_CANDIDATES: usize = 6;
/// Top-k grounding entries folded into a summary.
const GROUNDING_K: usize = 3;
/// Per-entry body excerpt for tag-matched contexts — enough to carry a runbook's
/// steps into the prompt without letting one document dominate.
const CONTEXT_BODY_CHARS: usize = 2_000;

pub struct Analyst {
    store: Arc<Store>,
    correlator: Arc<Correlator>,
    reasoner: Arc<dyn Reasoner>,
    memory: Arc<MemoryManager>,
    context: Arc<ContextManager>,
    dedup_threshold: f64,
    auto_merge: bool,
    /// Widened window for candidate discovery relative to grouping.
    window: Duration,
}

impl Analyst {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        correlator: Arc<Correlator>,
        reasoner: Arc<dyn Reasoner>,
        memory: Arc<MemoryManager>,
        context: Arc<ContextManager>,
        dedup_threshold: f64,
        auto_merge: bool,
        window: Duration,
    ) -> Self {
        Self {
            store,
            correlator,
            reasoner,
            memory,
            context,
            dedup_threshold,
            auto_merge,
            window,
        }
    }

    /// Full LLM pass over one thread: refresh its grounded summary, judge candidate
    /// pairs into relation edges, and (if `auto_merge`) collapse high-confidence
    /// duplicates. Existing user pins are honored — pinned pairs are not re-judged.
    pub async fn reanalyze(&self, thread_id: &str) -> Result<()> {
        self.reanalyze_with(thread_id, None).await
    }

    /// Like [`reanalyze`], but with an optional one-off reasoner override — the
    /// board's "reconsider with model X" uses this to re-run the summary and
    /// relation judgments on a chosen provider/model without changing the daemon's
    /// configured reasoners. `None` uses the analyst's default (heavy) reasoner.
    pub async fn reanalyze_with(
        &self,
        thread_id: &str,
        reasoner: Option<Arc<dyn Reasoner>>,
    ) -> Result<()> {
        // An explicit override is a deliberate user choice ("reconsider on model
        // X"): a failure there is actionable and must surface, unlike the daemon's
        // automatic passes which stay fault-tolerant and swallow reasoner errors.
        let is_override = reasoner.is_some();
        let reasoner: &dyn Reasoner = match &reasoner {
            Some(r) => r.as_ref(),
            None => self.reasoner.as_ref(),
        };
        let Some(view) = self.correlator.thread_view(thread_id)? else {
            return Ok(());
        };

        // 0. Classify the thread into tags — the categorical routing key for
        // grounding. A human's pinned tags win and aren't reclassified.
        let tags = if view.thread.tags_pinned {
            view.thread.tags.clone()
        } else {
            let classified = self
                .classify_text_with(&grounding_query(&view), reasoner)
                .await;
            self.store.set_thread_tags(thread_id, &classified, false)?;
            classified
        };

        // 1. Grounded summary.
        let grounding = self.gather_grounding(&view, &tags).await;
        match self.summarize_thread(&view, &grounding, reasoner).await {
            Ok(summary) if !summary.trim().is_empty() => {
                self.store
                    .set_thread_summary(thread_id, summary.trim(), Utc::now())?;
            }
            Ok(_) => {}
            Err(e) if is_override => {
                return Err(e.context("reconsider with the chosen provider/model failed"));
            }
            Err(e) => warn!("thread {thread_id}: summary skipped: {e:#}"),
        }

        // 2. Candidate relation edges.
        let candidates = self.candidate_threads(&view)?;
        for cand in candidates {
            // Respect an existing user pin — don't re-judge what the human decided.
            if let Some(existing) = self.store.get_edge(thread_id, &cand.thread.id)? {
                if existing.provenance == Provenance::User {
                    continue;
                }
            }
            match self.judge(&view, &cand, reasoner).await {
                Ok(Some(edge)) => {
                    // A DISTINCT verdict from the LLM is anti-signal: it floods the
                    // relation graph with "these are unrelated" edges that tell the
                    // user nothing and carry no dedup benefit (only USER pins
                    // suppress future re-judging — see above). Persist only real
                    // links; a human's DISTINCT pin still goes through `relate`.
                    if edge.kind == RelationKind::Distinct {
                        continue;
                    }
                    self.store.put_edge(&edge)?;
                    if self.auto_merge
                        && edge.kind == RelationKind::Same
                        && edge.confidence >= self.dedup_threshold
                    {
                        debug!("auto-merging {} into {}", cand.thread.id, thread_id);
                        self.merge(thread_id, &cand.thread.id)?;
                    }
                }
                Ok(None) => {}
                Err(e) => warn!("judge {} vs {} failed: {e:#}", thread_id, cand.thread.id),
            }
        }

        // 3. Refresh the cached mitigations so the UI reads them instantly rather
        // than blocking on this (slow) reasoner round-trip when the thread opens.
        // Best effort — a miss leaves the last cache (or the fast catalog) in place.
        match self.generate_mitigations(thread_id, reasoner).await {
            Ok(mits) if !mits.is_empty() => {
                if let Err(e) = self
                    .store
                    .set_thread_mitigations(thread_id, &serde_json::json!(mits))
                {
                    warn!("thread {thread_id}: caching mitigations failed: {e:#}");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("thread {thread_id}: mitigations skipped: {e:#}"),
        }
        Ok(())
    }

    /// Ask the reasoner for mitigations tailored to this thread. Shaped like the
    /// static catalog (id/name/description/reversible/score/cited_signals) so the
    /// client renders both paths identically; cited ids are validated against the
    /// thread. Runs in the background during reanalysis and is cached, so the UI
    /// never blocks on this (slow) reasoner round-trip.
    pub async fn generate_mitigations(
        &self,
        thread_id: &str,
        reasoner: &dyn Reasoner,
    ) -> Result<Vec<serde_json::Value>> {
        let Some(view) = self.correlator.thread_view(thread_id)? else {
            anyhow::bail!("no thread {thread_id}");
        };
        let signals = &view.signals;
        // Passing CI is an outcome to record, not an incident to mitigate. Do
        // not ask the model for speculative rollback or traffic advice, and
        // overwrite any stale cached response with an empty result upstream.
        if crate::mitigations::is_successful_ci_only(signals) {
            return Ok(Vec::new());
        }
        let timeline = crate::mitigations::timeline_evidence(signals);
        let grounding = match self.context.search(&view.thread.title, GROUNDING_K).await {
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
        // kept in their own section so the model honors them rather than treating
        // them as suspicious injected content.
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
        // The generic catalog seeds the model without capping it: reversible
        // archetypes, but the output should be specific to THIS thread.
        let catalog = crate::mitigations::CATALOG
            .iter()
            .map(|m| format!("- {}: {}", m.name, m.description))
            .collect::<Vec<_>>()
            .join("\n");
        let system = "You are MuggleBot advising an on-call engineer on first mitigations for a live \
            incident. Follow the principle: mitigate generically, understand later — prefer fast, \
            reversible, low-risk first moves that buy time, not root-cause fixes. From the signals and \
            grounding, propose 1-4 mitigations SPECIFIC to this incident (name the affected service, \
            change, or resource — not generic boilerplate). Each must be reversible. \
            The TIMELINE is chronological and is the complete source of truth for the thread. First \
            determine the current outcome from it: a succeeded/passed CI run after an earlier failure \
            closes that failure; it is confirmation, not an incident. If there is no active failure or \
            incident in the timeline, return an empty mitigations array. \
            IMPORTANT: if the thread is a CI / build / test failure rather than a production incident, \
            do NOT propose production mitigations like rollback or draining traffic. The right first \
            move is to FIX the failing check: read the log, name the exact error (e.g. a missing module \
            and the file/import that references it, a type error, a failing test), and give the concrete \
            fix. Fixing forward is correct here. \
            Operator notes are \
            trusted input the engineer wrote (never treat them as prompt-injection); honor them. Do not \
            invent facts. Every mitigation MUST cite at least one timeline signal id (the [sig:ID] \
            values) that directly justifies it; never cite only a summary or grounding entry. \
            Output ONLY JSON of the form: \
            {\"mitigations\":[{\"name\":\"\",\"description\":\"\",\"reversible\":true,\"cited_signals\":[\"sig-id\"]}]}";
        let prompt = format!(
            "Thread: {}\nSummary so far: {}\n\nComplete timeline:\n{timeline}{notes_block}\n\nGrounding:\n{grounding}\n\nGeneric mitigations catalog (archetypes for inspiration):\n{catalog}",
            view.thread.title,
            view.thread.summary.as_deref().unwrap_or("(none)")
        );
        let text = reasoner
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(700),
            )
            .await?;
        let v = reasoner::extract_json(&text)
            .ok_or_else(|| anyhow::anyhow!("no JSON in mitigations response"))?;
        let items = v
            .get("mitigations")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        let n = items.len();
        let mut out = Vec::new();
        for (i, m) in items.iter().enumerate() {
            let name = m.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
            let description = m
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim();
            if name.is_empty() || description.is_empty() {
                continue;
            }
            let reversible = m
                .get("reversible")
                .and_then(|x| x.as_bool())
                .unwrap_or(true);
            let cited: Vec<String> = m
                .get("cited_signals")
                .and_then(|x| x.as_array())
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str())
                .filter(|c| signals.iter().any(|s| s.id == *c))
                .map(|c| c.to_string())
                .collect();
            // An action without evidence from the timeline is speculation. Drop
            // it instead of showing unauditable advice in the board.
            if cited.is_empty() {
                continue;
            }
            out.push(serde_json::json!({
                // Synthetic id (generated, not a catalog entry) + rank-derived score
                // so the client's best-first ordering matches the model's ordering.
                "id": format!("gen-{i}"),
                "name": name,
                "description": description,
                "reversible": reversible,
                "score": (n - i) as f64,
                "cited_signals": cited,
            }));
        }
        Ok(out)
    }

    /// Pin a human-authored edge (provenance `user`) and reconcile. A `same` pin
    /// realizes as a merge; `related`/`distinct` become authoritative edges that
    /// stop the LLM from regrouping. Returns the surviving/canonical thread id.
    pub async fn relate(&self, a: &str, b: &str, kind: RelationKind) -> Result<String> {
        if a == b {
            return Ok(a.to_string());
        }
        match kind {
            RelationKind::Same => {
                let canonical = self.merge(a, b)?;
                self.reanalyze(&canonical).await?;
                Ok(canonical)
            }
            RelationKind::Related | RelationKind::Distinct => {
                let edge = Edge {
                    thread_a: a.to_string(),
                    thread_b: b.to_string(),
                    kind,
                    provenance: Provenance::User,
                    confidence: 1.0,
                    rationale: "user pin".into(),
                    signals: vec![],
                    created_at: Utc::now(),
                };
                self.store.put_edge(&edge)?;
                self.reanalyze(a).await?;
                self.reanalyze(b).await?;
                Ok(a.to_string())
            }
        }
    }

    /// Pull the given signals out of their thread into a fresh thread. Re-analyzes
    /// both. Returns the new thread id.
    pub async fn split_thread(&self, thread_id: &str, signal_ids: &[String]) -> Result<String> {
        let now = Utc::now();
        let new_id = format!("thr/{}", crate::store::new_id());
        let mut title = None;
        for sid in signal_ids {
            if let Some(sig) = self.store.get_signal(sid)? {
                if sig.thread.as_deref() == Some(thread_id) {
                    if title.is_none() {
                        title = Some(sig.title.clone());
                    }
                    self.store.set_signal_thread(sid, Some(&new_id))?;
                }
            }
        }
        self.store.upsert_thread(&super::Thread {
            id: new_id.clone(),
            title: title.unwrap_or_else(|| "split thread".into()),
            summary: None,
            created_at: now,
            updated_at: now,
            last_reasoned_at: None,
            live: false,
            tags: Vec::new(),
            tags_pinned: false,
        })?;
        self.correlator.refresh_thread_metadata(&new_id)?;
        self.correlator.refresh_thread_metadata(thread_id)?;
        self.reanalyze(&new_id).await?;
        self.reanalyze(thread_id).await?;
        Ok(new_id)
    }

    /// Attach ad-hoc grounding to a thread (free text or a URL), then re-analyze.
    /// A URL is fetched + summarized through the same pipeline as the context
    /// library; text is used as-is.
    pub async fn attach_thread_context(
        &self,
        thread_id: &str,
        kind: ContextKind,
        content: &str,
    ) -> Result<ThreadContext> {
        let summary = match kind {
            ContextKind::Text => None,
            ContextKind::Url => {
                // Ingest the URL into the context library so it's summarized,
                // embedded, and refreshable — then cite that summary here.
                match self
                    .context
                    .add(ContextSourceKind::Url, content, None, None, None, None)
                    .await
                {
                    Ok(ctx) => ctx.summary,
                    Err(e) => {
                        warn!("attach url context failed: {e:#}");
                        None
                    }
                }
            }
        };
        let tc = ThreadContext {
            id: format!("tctx/{}", crate::store::new_id()),
            thread_id: thread_id.to_string(),
            kind,
            content: content.to_string(),
            summary,
            created_at: Utc::now(),
        };
        self.store.add_thread_context(&tc)?;
        self.reanalyze(thread_id).await?;
        Ok(tc)
    }

    /// Collapse `drop_id` into `keep_id`: move its signals over, refresh, and
    /// remove the now-empty thread. Returns `keep_id`.
    pub fn merge(&self, keep_id: &str, drop_id: &str) -> Result<String> {
        if keep_id == drop_id {
            return Ok(keep_id.to_string());
        }
        for sig in self.store.signals_for_thread(drop_id)? {
            self.store.set_signal_thread(&sig.id, Some(keep_id))?;
        }
        // Move any attached context across.
        for tc in self.store.thread_context(drop_id)? {
            let mut moved = tc.clone();
            moved.thread_id = keep_id.to_string();
            self.store.add_thread_context(&moved)?;
        }
        self.correlator.refresh_thread_metadata(keep_id)?;
        self.store.delete_thread_if_empty(drop_id)?;
        Ok(keep_id.to_string())
    }

    // ---- internals ----------------------------------------------------------

    fn candidate_threads(&self, view: &ThreadView) -> Result<Vec<ThreadView>> {
        let target: std::collections::BTreeSet<String> = super::engine::entity_keys(&view.entities);
        let target_tags: std::collections::BTreeSet<&str> =
            view.thread.tags.iter().map(String::as_str).collect();
        // When the target has no strong entity key — e.g. a standalone Slack
        // message now that channel/person no longer group — fall back to judging
        // it against recent active threads so topic duplicates still get caught.
        // Otherwise (it has a strong identity) we only pair on shared entity/tag,
        // to avoid re-judging every recent thread against a well-anchored one.
        let broaden = target.is_empty();
        // Candidates sit within a *tight time window* (a generous multiple of the
        // grouping window) of this thread's activity — so a deploy and the
        // incident it caused still pair up, but week-old threads aren't re-judged
        // forever.
        let span = chrono::Duration::from_std(self.window * 8)
            .unwrap_or_else(|_| chrono::Duration::hours(4));
        // (tier, overlap, recency, view) — tier 2: shares a strong entity;
        // 1: shares a topic tag; 0: recency-only (broadening standalone threads).
        let mut scored: Vec<(u8, usize, chrono::DateTime<Utc>, ThreadView)> = Vec::new();
        for t in self.store.list_threads()? {
            if t.id == view.thread.id {
                continue;
            }
            if (t.updated_at - view.thread.updated_at).abs() > span {
                continue;
            }
            let tag_shared = t
                .tags
                .iter()
                .filter(|tg| target_tags.contains(tg.as_str()))
                .count();
            let Some(cand) = self.correlator.thread_view(&t.id)? else {
                continue;
            };
            let entity_shared = super::engine::entity_keys(&cand.entities)
                .intersection(&target)
                .count();
            let (tier, overlap) = if entity_shared > 0 {
                (2, entity_shared)
            } else if tag_shared > 0 {
                (1, tag_shared)
            } else if broaden {
                (0, 0)
            } else {
                continue;
            };
            scored.push((tier, overlap, cand.thread.updated_at, cand));
        }
        // Best first: stronger tier, then more overlap, then more recent.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
        Ok(scored
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|(_, _, _, v)| v)
            .collect())
    }

    /// Classify a thread into tags drawn from the context library's vocabulary.
    /// Classify arbitrary text (e.g. a single Slack message) into vocabulary tags
    /// — the shared classifier behind thread and per-message tagging. LLM with a
    /// deterministic substring fallback.
    pub async fn classify_text(&self, text: &str) -> Vec<String> {
        self.classify_text_with(text, self.reasoner.as_ref()).await
    }

    /// [`classify_text`] with an explicit reasoner (the reanalyze override path).
    async fn classify_text_with(&self, text: &str, reasoner: &dyn Reasoner) -> Vec<String> {
        let vocab = self.store.list_tags().unwrap_or_default();
        if vocab.is_empty() {
            return Vec::new();
        }
        match tags::classify(reasoner, &vocab, text).await {
            Some(t) => t,
            None => {
                let names: Vec<String> = vocab.into_iter().map(|t| t.name).collect();
                tags::deterministic_match(&names, text)
            }
        }
    }

    /// Assemble the grounding block. For both memory and context: **tag-matched
    /// entries first** (the categorical routing), then a vector-similarity fill
    /// for the remaining budget — de-duplicated so a tagged entry isn't repeated.
    async fn gather_grounding(&self, view: &ThreadView, tags: &[String]) -> String {
        let query = grounding_query(view);
        let mut out = String::new();

        let mut mem_seen: Vec<String> = Vec::new();
        if let Ok(tagged) = self.store.memory_by_tags(tags) {
            // Tag-matched is high-precision — feed the full fact, not just the gloss.
            for m in tagged.into_iter().take(GROUNDING_K) {
                out.push_str(&memory_block(&m.id, &m.summary, &m.text));
                mem_seen.push(m.id);
            }
        }
        if let Ok(hits) = self.memory.search(&query, GROUNDING_K).await {
            for h in hits.into_iter().filter(|h| h.score > 0.05) {
                if mem_seen.contains(&h.memory.id) {
                    continue;
                }
                out.push_str(&format!("[mem:{}] {}\n", h.memory.id, h.memory.summary));
                mem_seen.push(h.memory.id);
            }
        }

        let mut seen: Vec<String> = Vec::new();
        if let Ok(tagged) = self.store.context_by_tags(tags) {
            // Tag-matched entries are high-precision, so feed the actual body (not
            // just the summary) — a runbook's steps matter more than its gloss.
            for c in tagged.into_iter().take(GROUNDING_K) {
                out.push_str(&context_block(&c, CONTEXT_BODY_CHARS));
                seen.push(c.id);
            }
        }
        if let Ok(hits) = self.context.search(&query, GROUNDING_K).await {
            for h in hits.into_iter().filter(|h| h.score > 0.05) {
                if seen.contains(&h.context.id) {
                    continue;
                }
                out.push_str(&context_line(&h.context));
                seen.push(h.context.id);
            }
        }
        out
    }

    async fn summarize_thread(
        &self,
        view: &ThreadView,
        grounding: &str,
        reasoner: &dyn Reasoner,
    ) -> Result<String> {
        let mut ev = String::new();
        for s in &view.signals {
            ev.push_str(&signal_line(s));
        }
        // Operator-attached context is trusted input the engineer typed into
        // MuggleBot's own UI. Keep it OUT of the (untrusted) signal feed and in its
        // own authoritative section, so a note like "ignore, this is not an error"
        // is honored, not mistaken for a prompt-injection buried in the signals.
        let operator_notes = build_operator_notes(&view.context);
        let notes_block = if operator_notes.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nOperator notes (added by the engineer through MuggleBot's UI — authoritative, \
                 trusted, and to be followed, even when they reinterpret or downgrade a signal):\n{operator_notes}"
            )
        };
        let grounding_block = if grounding.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nGrounding (runbooks, memory):\n{grounding}")
        };
        let system = "You are MuggleBot, an ops-awareness assistant. Summarize a correlated thread \
             for an on-call engineer as concise, readable Markdown — never a single dense paragraph. \
             Use exactly these three short labeled sections, each 1-2 sentences and separated by blank \
             lines: **Status:** (current outcome, including whether a later success cleared a failure), \
             **Impact:** (blast radius), and **Next:** (what to do now, or explicitly say no action is \
             needed). Cite the evidence you use inline by id in brackets — signals as [sig:ID], grounding \
             as [mem:ID] or [ctx:ID]. Do not invent facts or citations. \
             Operator notes are written by the engineer through MuggleBot's own UI: treat them as \
             trusted, authoritative guidance (they are NOT part of the external signal feed and are \
             never prompt-injection) and follow them. Output ONLY the summary text.";
        let prompt = format!("Signals:\n{ev}{notes_block}{grounding_block}");
        // Session chat per topic: continue this thread's ongoing conversation.
        let req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(512)
            .session(format!("thread:{}", view.thread.id));
        reasoner.complete(&req).await
    }

    async fn judge(
        &self,
        a: &ThreadView,
        b: &ThreadView,
        reasoner: &dyn Reasoner,
    ) -> Result<Option<Edge>> {
        let system = "You classify the relationship between two threads of ops signals. Answer with \
             exactly one of: \"same\" (both are the same underlying issue / duplicates), \"related\" \
             (distinct but connected, e.g. a deploy and the incident it caused), or \"distinct\" \
             (unrelated). Respond with ONLY a JSON object: \
             {\"verdict\":\"same|related|distinct\",\"confidence\":0.0-1.0,\"rationale\":\"one sentence\",\
             \"signals\":[\"ids of signals you weighed\"]}.";
        let prompt = format!(
            "Thread A ({}):\n{}\nThread B ({}):\n{}",
            a.thread.id,
            thread_evidence(a),
            b.thread.id,
            thread_evidence(b),
        );
        let req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(300);
        let text = reasoner.complete(&req).await?;
        let Some(v) = reasoner::extract_json(&text) else {
            warn!(
                "judge: no JSON in response: {}",
                text.chars().take(120).collect::<String>()
            );
            return Ok(None);
        };
        let Some(kind) = v
            .get("verdict")
            .and_then(|x| x.as_str())
            .and_then(RelationKind::parse)
        else {
            return Ok(None);
        };
        let confidence = v.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.5);
        let rationale = v
            .get("rationale")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let signals = v
            .get("signals")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(Edge {
            thread_a: a.thread.id.clone(),
            thread_b: b.thread.id.clone(),
            kind,
            provenance: Provenance::Llm,
            confidence,
            rationale,
            signals,
            created_at: Utc::now(),
        }))
    }
}

fn grounding_query(view: &ThreadView) -> String {
    let mut q = view.thread.title.clone();
    for s in view.signals.iter().take(4) {
        q.push(' ');
        q.push_str(&s.title);
        if let Some(b) = &s.body {
            q.push(' ');
            q.push_str(b);
        }
    }
    for e in &view.entities {
        q.push(' ');
        q.push_str(&e.value);
    }
    q
}

fn context_line(c: &crate::context::Context) -> String {
    let tags = if c.tags.is_empty() {
        String::new()
    } else {
        format!(" #{}", c.tags.join(" #"))
    };
    format!(
        "[ctx:{}]{} {} — {}\n",
        c.id,
        tags,
        c.location,
        c.summary.as_deref().unwrap_or("")
    )
}

/// A fuller grounding entry: the summary line plus a bounded excerpt of the
/// source body, so the reasoner sees the actual content (runbook steps, config)
/// and not only the gloss. Used for high-precision tag-matched contexts.
fn context_block(c: &crate::context::Context, body_chars: usize) -> String {
    let mut out = context_line(c);
    if let Some(raw) = c.raw.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        out.push_str(&excerpt_line(raw, body_chars));
    }
    out
}

/// A fuller memory entry: the summary line plus a bounded excerpt of the full
/// fact — used for high-precision tag-matched memories.
fn memory_block(id: &str, summary: &str, text: &str) -> String {
    let mut out = format!("[mem:{id}] {summary}\n");
    let text = text.trim();
    if !text.is_empty() && text != summary.trim() {
        out.push_str(&excerpt_line(text, CONTEXT_BODY_CHARS));
    }
    out
}

fn excerpt_line(body: &str, body_chars: usize) -> String {
    let excerpt: String = body.chars().take(body_chars).collect();
    let ellipsis = if body.chars().count() > body_chars {
        " …"
    } else {
        ""
    };
    format!("    {excerpt}{ellipsis}\n")
}

fn signal_line(s: &Signal) -> String {
    let mut line = format!(
        "[sig:{}] {} · {} · {}: {} — {}\n",
        s.id,
        s.source,
        signal_kind(s),
        s.occurred_at.to_rfc3339(),
        s.title,
        s.body.as_deref().unwrap_or("")
    );
    // A summary of a page linked in the message, if we fetched one.
    if let Some(summary) = s.raw.get("link_summary").and_then(|v| v.as_str()) {
        let url = s.raw.get("link_url").and_then(|v| v.as_str()).unwrap_or("");
        line.push_str(&format!("    ↳ linked page {url}: {summary}\n"));
    }
    line
}

/// Render the operator-attached thread context as a trusted-notes block (empty
/// when the thread has none). For text notes the body is the note itself; for a
/// URL, its fetched summary if we have one, else the URL.
fn build_operator_notes(context: &[ThreadContext]) -> String {
    let mut out = String::new();
    for tc in context {
        let body = match tc.kind {
            ContextKind::Text => tc.content.trim(),
            _ => tc.summary.as_deref().unwrap_or(&tc.content).trim(),
        };
        if !body.is_empty() {
            out.push_str(&format!("- ({}) {}\n", tc.kind.as_str(), body));
        }
    }
    out
}

fn signal_kind(s: &Signal) -> String {
    serde_json::to_value(s.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".into())
}

fn thread_evidence(v: &ThreadView) -> String {
    let mut out = String::new();
    if let Some(sum) = &v.thread.summary {
        out.push_str(&format!("summary: {sum}\n"));
    }
    for s in v.signals.iter().take(8) {
        out.push_str(&signal_line(s));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::reasoner::MockReasoner;
    use crate::signal::{Entity, Severity, SignalKind, Source, State};

    fn analyst(reasoner_response: &str) -> (Arc<Store>, Arc<Correlator>, Analyst) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new(reasoner_response));
        let memory = Arc::new(MemoryManager::new(
            store.clone(),
            embedder.clone(),
            reasoner.clone(),
            reasoner.clone(),
        ));
        let context = Arc::new(ContextManager::new(
            store.clone(),
            embedder,
            reasoner.clone(),
            reasoner.clone(),
            "6h".into(),
        ));
        let correlator = Arc::new(Correlator::new(store.clone(), Duration::from_secs(1800)));
        let a = Analyst::new(
            store.clone(),
            correlator.clone(),
            reasoner,
            memory,
            context,
            0.8,
            false,
            Duration::from_secs(1800),
        );
        (store, correlator, a)
    }

    fn sig(ext: &str, ent: &str) -> Signal {
        Signal {
            id: Signal::make_id(Source::Slack, ext),
            source: Source::Slack,
            external_id: ext.into(),
            kind: SignalKind::Alert,
            title: format!("alert {ext}"),
            body: Some("service degraded".into()),
            url: None,
            actor: None,
            entities: vec![Entity::new("service", ent)],
            severity: Severity::Critical,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: serde_json::Value::Null,
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn reanalyze_writes_summary() {
        let (store, correlator, analyst) = analyst("Service foo is down; check the pool. [sig:x]");
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = correlator.ingest(&s).unwrap();
        analyst.reanalyze(&tid).await.unwrap();
        let t = store.get_thread(&tid).unwrap().unwrap();
        assert_eq!(
            t.summary.as_deref(),
            Some("Service foo is down; check the pool. [sig:x]")
        );
        assert!(t.last_reasoned_at.is_some());
    }

    #[tokio::test]
    async fn classifies_thread_and_grounds_by_tag() {
        // The mock reasoner returns this for every completion, including the tag
        // classifier — so the thread classifies to ["database"].
        let (store, correlator, analyst) = analyst(r#"["database"]"#);
        store
            .ensure_tag(
                "database",
                "database incidents and recovery runbooks",
                Utc::now(),
            )
            .unwrap();
        let ctx = crate::context::Context {
            id: "ctx/db".into(),
            kind: crate::context::ContextSourceKind::File,
            location: "db-runbook.md".into(),
            credential: None,
            header: None,
            tags: vec!["database".into()],
            tags_pinned: true,
            summary: Some("How to recover the primary database".into()),
            raw: Some("restart the primary".into()),
            etag: None,
            last_modified: None,
            mtime: None,
            fetched_at: None,
            refresh_interval: "6h".into(),
            created_at: Utc::now(),
        };
        store.put_context(&ctx, None).unwrap();

        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = correlator.ingest(&s).unwrap();
        analyst.reanalyze(&tid).await.unwrap();

        let t = store.get_thread(&tid).unwrap().unwrap();
        assert_eq!(t.tags, vec!["database".to_string()], "thread classified");

        // The tagged context is grounded ahead of the vector fill.
        let view = correlator.thread_view(&tid).unwrap().unwrap();
        let grounding = analyst
            .gather_grounding(&view, &["database".to_string()])
            .await;
        assert!(
            grounding.contains("[ctx:ctx/db]"),
            "tagged context grounded"
        );
        assert!(
            grounding.contains("#database"),
            "tags shown on the ctx line"
        );
    }

    #[tokio::test]
    async fn pinned_thread_tags_survive_reanalysis() {
        let (store, correlator, analyst) = analyst(r#"["database"]"#);
        store.ensure_tag("database", "db", Utc::now()).unwrap();
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = correlator.ingest(&s).unwrap();
        // Human pins a different tag set.
        store
            .set_thread_tags(&tid, &["network".to_string()], true)
            .unwrap();
        analyst.reanalyze(&tid).await.unwrap();
        let t = store.get_thread(&tid).unwrap().unwrap();
        assert_eq!(t.tags, vec!["network".to_string()], "pin not overwritten");
    }

    #[tokio::test]
    async fn user_relate_pins_authoritative_edge() {
        let (store, correlator, analyst) = analyst("noop");
        let a = sig("1", "foo");
        let b = sig("2", "bar");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let ta = correlator.ingest(&a).unwrap();
        let tb = correlator.ingest(&b).unwrap();
        assert_ne!(ta, tb);
        analyst
            .relate(&ta, &tb, RelationKind::Related)
            .await
            .unwrap();
        let edge = store.get_edge(&ta, &tb).unwrap().unwrap();
        assert_eq!(edge.kind, RelationKind::Related);
        assert_eq!(edge.provenance, Provenance::User);
    }

    #[tokio::test]
    async fn split_pulls_signal_into_new_thread() {
        let (store, correlator, analyst) = analyst("noop");
        let a = sig("1", "foo");
        let b = sig("2", "foo");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let t = correlator.ingest(&a).unwrap();
        assert_eq!(
            correlator.ingest(&b).unwrap(),
            t,
            "shared entity groups together"
        );

        let new = analyst
            .split_thread(&t, std::slice::from_ref(&b.id))
            .await
            .unwrap();
        assert_ne!(new, t);
        assert_eq!(store.signals_for_thread(&t).unwrap().len(), 1);
        assert_eq!(store.signals_for_thread(&new).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn llm_judge_writes_relation_edge() {
        // Reasoner returns a judge verdict; summary path stores it verbatim (ignored here).
        let (store, correlator, analyst) = analyst(
            r#"{"verdict":"related","confidence":0.9,"rationale":"same service","signals":[]}"#,
        );
        let a = sig("1", "foo");
        let b = sig("2", "foo");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let t = correlator.ingest(&a).unwrap();
        correlator.ingest(&b).unwrap();
        // Split so two entity-sharing threads exist as judge candidates.
        let other = analyst
            .split_thread(&t, std::slice::from_ref(&b.id))
            .await
            .unwrap();
        analyst.reanalyze(&t).await.unwrap();
        let edge = store.get_edge(&t, &other).unwrap().unwrap();
        assert_eq!(edge.kind, RelationKind::Related);
        assert_eq!(edge.provenance, Provenance::Llm);
    }

    #[tokio::test]
    async fn same_pin_merges_threads() {
        let (store, correlator, analyst) = analyst("noop");
        let a = sig("1", "foo");
        let b = sig("2", "bar");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let ta = correlator.ingest(&a).unwrap();
        let tb = correlator.ingest(&b).unwrap();
        let canonical = analyst.relate(&ta, &tb, RelationKind::Same).await.unwrap();
        assert_eq!(canonical, ta);
        assert_eq!(store.signals_for_thread(&ta).unwrap().len(), 2);
        assert!(
            store.get_thread(&tb).unwrap().is_none(),
            "merged thread removed"
        );
    }
}
