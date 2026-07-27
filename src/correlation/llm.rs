//! The semantic-reasoning tier (Phase 2): an LLM judges candidate subject pairs
//! and writes the relation graph, refreshes grounded subject summaries, and
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

use super::{ContextKind, Edge, Provenance, RelationKind, SubjectContext};
use crate::context::{ContextManager, ContextSourceKind};
use crate::memory::MemoryManager;
use crate::reasoner::{self, CompletionRequest, Reasoner};
use crate::signal::{Signal, SignalKind, Source};
use crate::store::Store;
use crate::subject::{Attributor, Handled, SubjectKey, SubjectView};
use crate::tags;

/// How many candidate subjects to judge per re-analysis, most-shared first — a cap
/// on LLM calls per pass.
const MAX_CANDIDATES: usize = 6;
/// Top-k grounding entries folded into a summary.
const GROUNDING_K: usize = 3;
/// Per-entry body excerpt for tag-matched contexts — enough to carry a runbook's
/// steps into the prompt without letting one document dominate.
const CONTEXT_BODY_CHARS: usize = 2_000;

pub struct Analyst {
    store: Arc<Store>,
    attributor: Arc<Attributor>,
    reasoner: Arc<dyn Reasoner>,
    /// The **local** classifier (Ollama). Two jobs live here, both deliberately
    /// off the cloud: tag/classification passes (high volume, mechanical) and the
    /// reopen-matching pass over handled subjects, which by policy must never reach
    /// a cloud model at all.
    classifier: Arc<dyn Reasoner>,
    memory: Arc<MemoryManager>,
    context: Arc<ContextManager>,
    dedup_threshold: f64,
    auto_merge: bool,
    /// Minimum local-classifier confidence to reopen a handled subject.
    reopen_min_confidence: f64,
    /// Widened window for candidate discovery relative to grouping.
    window: Duration,
}

impl Analyst {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        attributor: Arc<Attributor>,
        reasoner: Arc<dyn Reasoner>,
        classifier: Arc<dyn Reasoner>,
        memory: Arc<MemoryManager>,
        context: Arc<ContextManager>,
        dedup_threshold: f64,
        auto_merge: bool,
        reopen_min_confidence: f64,
        window: Duration,
    ) -> Self {
        Self {
            store,
            attributor,
            reasoner,
            classifier,
            memory,
            context,
            dedup_threshold,
            auto_merge,
            reopen_min_confidence,
            window,
        }
    }

    /// Full LLM pass over one subject: refresh its grounded summary, judge candidate
    /// pairs into relation edges, and (if `auto_merge`) collapse high-confidence
    /// duplicates. Existing user pins are honored — pinned pairs are not re-judged.
    pub async fn reanalyze(&self, subject_key: &str) -> Result<()> {
        self.reanalyze_with(subject_key, None).await
    }

    /// Like [`reanalyze`], but with an optional one-off reasoner override — the
    /// board's "reconsider with model X" uses this to re-run the summary and
    /// relation judgments on a chosen provider/model without changing the daemon's
    /// configured reasoners. `None` uses the analyst's default (heavy) reasoner.
    ///
    /// **Handled subjects are never reasoned over.** A snoozed, resolved, or
    /// acknowledged subject is settled work; re-summarizing it, re-judging its
    /// relations, and re-summarizing it spends model calls on a decision
    /// the operator already made. The only model allowed to look at a handled
    /// subject is the local classifier, via [`Self::triage_handled`], which decides
    /// whether new activity means it should come back. An explicit override on a
    /// handled subject is an error rather than a silent no-op, so "reconsider" never
    /// looks like it worked when it was skipped.
    pub async fn reanalyze_with(
        &self,
        subject_key: &str,
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
        let Some(view) = self.attributor.subject_view(subject_key)? else {
            return Ok(());
        };

        if view.subject.handled.is_handled() {
            let label = view.subject.handled.as_str();
            if is_override {
                anyhow::bail!(
                    "subject {subject_key} is {label}; handled subjects are not sent to a reasoner. \
                     Reopen it first to reconsider it."
                );
            }
            debug!("subject {subject_key} is {label}: skipping analysis");
            return Ok(());
        }

        // 0. Classify the subject into tags — the categorical routing key for
        // grounding. A human's pinned tags win and aren't reclassified.
        // Classification runs on the local classifier, not the cloud tier: it's a
        // high-volume mechanical mapping onto a fixed vocabulary.
        let tags = if view.subject.tags_pinned {
            view.subject.tags.clone()
        } else {
            let classified = self.classify_text(&grounding_query(&view)).await;
            self.store
                .set_subject_tags(subject_key, &classified, false)?;
            classified
        };

        // 1. Grounded summary.
        let grounding = self.gather_grounding(&view, &tags).await;
        match self
            .summarize_thread(&view, &grounding, reasoner, is_override)
            .await
        {
            Ok(summary) if !summary.trim().is_empty() => {
                self.store
                    .set_subject_summary(subject_key, summary.trim(), Utc::now())?;
            }
            Ok(_) => {}
            Err(e) if is_override => {
                return Err(e.context("reconsider with the chosen provider/model failed"));
            }
            Err(e) => warn!("subject {subject_key}: summary skipped: {e:#}"),
        }

        // 2. Candidate relation edges.
        let candidates = self.candidate_threads(&view)?;
        for cand in candidates {
            // Respect an existing user pin — don't re-judge what the human decided.
            if let Some(existing) = self
                .store
                .get_edge(subject_key, cand.subject.key.as_str())?
            {
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
                        debug!("auto-merging {} into {}", cand.subject.key, subject_key);
                        self.merge(subject_key, cand.subject.key.as_str())?;
                    }
                }
                Ok(None) => {}
                Err(e) => warn!(
                    "judge {} vs {} failed: {e:#}",
                    subject_key, cand.subject.key
                ),
            }
        }

        Ok(())
    }

    /// Pin a human-authored edge (provenance `user`) and reconcile. A `same` pin
    /// realizes as a merge; `related`/`distinct` become authoritative edges that
    /// stop the LLM from regrouping. Returns the surviving/canonical subject id.
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
                    subject_a: a.to_string(),
                    subject_b: b.to_string(),
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

    /// Detach signals the attribution got wrong.
    ///
    /// Under the synthetic-thread model this minted a new thread to move them into.
    /// There is no such thing to mint now — a subject *is* an upstream identity, and
    /// inventing one would create a card nothing can ever address again. So a split
    /// sends the signals to the unattributed lane and records the correction, which
    /// is also what stops a re-ingest from silently undoing it.
    ///
    /// Use [`Self::reattribute`] to move a signal to a *specific* subject instead.
    pub async fn split_subject(&self, subject_key: &str, signal_ids: &[String]) -> Result<usize> {
        let mut moved = 0;
        for sid in signal_ids {
            if let Some(sig) = self.store.get_signal(sid)? {
                if sig.subject.as_deref() == Some(subject_key) {
                    self.store.set_signal_subject(sid, None)?;
                    self.store.pin_attribution(sid, None)?;
                    moved += 1;
                }
            }
        }
        self.attributor.refresh_subject_metadata(subject_key)?;
        self.reanalyze(subject_key).await?;
        Ok(moved)
    }

    /// Move one signal onto a specific subject, overriding the ranked climb, and
    /// remember the override so re-ingest doesn't undo it.
    pub async fn reattribute(&self, signal_id: &str, to: Option<&SubjectKey>) -> Result<()> {
        let Some(sig) = self.store.get_signal(signal_id)? else {
            anyhow::bail!("no signal {signal_id}");
        };
        let previous = sig.subject.clone();
        self.store
            .set_signal_subject(signal_id, to.map(|k| k.as_str()))?;
        self.store.pin_attribution(signal_id, to)?;
        if let Some(prev) = &previous {
            self.attributor.refresh_subject_metadata(prev)?;
            self.reanalyze(prev).await?;
        }
        if let Some(to) = to {
            self.attributor.refresh_subject_metadata(to.as_str())?;
            self.reanalyze(to.as_str()).await?;
        }
        Ok(())
    }

    /// Attach ad-hoc grounding to a subject (free text or a URL), then re-analyze.
    /// A URL is fetched + summarized through the same pipeline as the context
    /// library; text is used as-is.
    pub async fn attach_subject_context(
        &self,
        subject_key: &str,
        kind: ContextKind,
        content: &str,
    ) -> Result<SubjectContext> {
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
        let tc = SubjectContext {
            id: format!("tctx/{}", crate::store::new_id()),
            subject_key: subject_key.to_string(),
            kind,
            content: content.to_string(),
            summary,
            created_at: Utc::now(),
        };
        self.store.add_subject_context(&tc)?;
        self.reanalyze(subject_key).await?;
        Ok(tc)
    }

    /// Collapse `drop_id` into `keep_id`: move its signals over, refresh, and
    /// remove the now-empty subject. Returns `keep_id`.
    pub fn merge(&self, keep_id: &str, drop_id: &str) -> Result<String> {
        if keep_id == drop_id {
            return Ok(keep_id.to_string());
        }
        for sig in self.store.signals_for_subject(drop_id)? {
            self.store.set_signal_subject(&sig.id, Some(keep_id))?;
        }
        // Move any attached context across.
        for tc in self.store.subject_context(drop_id)? {
            let mut moved = tc.clone();
            moved.subject_key = keep_id.to_string();
            self.store.add_subject_context(&moved)?;
        }
        // Carry the root-cause investigation over unless the surviving subject has
        // one already — it's expensive evidence and losing it with the collapsed
        // subject would silently discard work.
        if self.store.get_root_cause(keep_id)?.is_none() {
            self.store.move_root_cause(drop_id, keep_id)?;
        }
        self.attributor.refresh_subject_metadata(keep_id)?;
        self.store.delete_subject_if_empty(drop_id)?;
        Ok(keep_id.to_string())
    }

    // ---- internals ----------------------------------------------------------

    fn candidate_threads(&self, view: &SubjectView) -> Result<Vec<SubjectView>> {
        let target: std::collections::BTreeSet<String> = grouping_keys(&view.keys);
        let target_tags: std::collections::BTreeSet<&str> =
            view.subject.tags.iter().map(String::as_str).collect();
        // When the target has no strong entity key — e.g. a standalone Slack
        // message now that channel/person no longer group — fall back to judging
        // it against recent active subjects so topic duplicates still get caught.
        // Otherwise (it has a strong identity) we only pair on shared entity/tag,
        // to avoid re-judging every recent subject against a well-anchored one.
        let broaden = target.is_empty();
        // Candidates sit within a *tight time window* (a generous multiple of the
        // grouping window) of this subject's activity — so a deploy and the
        // incident it caused still pair up, but week-old subjects aren't re-judged
        // forever.
        let span = chrono::Duration::from_std(self.window * 8)
            .unwrap_or_else(|_| chrono::Duration::hours(4));
        // (tier, overlap, recency, view) — tier 2: shares a strong entity;
        // 1: shares a topic tag; 0: recency-only (broadening standalone subjects).
        let mut scored: Vec<(u8, usize, chrono::DateTime<Utc>, SubjectView)> = Vec::new();
        for t in self.store.list_subjects()? {
            if t.key == view.subject.key {
                continue;
            }
            if (t.updated_at - view.subject.updated_at).abs() > span {
                continue;
            }
            let tag_shared = t
                .tags
                .iter()
                .filter(|tg| target_tags.contains(tg.as_str()))
                .count();
            let Some(cand) = self.attributor.subject_view(t.key.as_str())? else {
                continue;
            };
            let entity_shared = grouping_keys(&cand.keys).intersection(&target).count();
            let (tier, overlap) = if entity_shared > 0 {
                (2, entity_shared)
            } else if tag_shared > 0 {
                (1, tag_shared)
            } else if broaden {
                (0, 0)
            } else {
                continue;
            };
            scored.push((tier, overlap, cand.subject.updated_at, cand));
        }
        // Best first: stronger tier, then more overlap, then more recent.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
        Ok(scored
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|(_, _, _, v)| v)
            .collect())
    }

    /// Classify arbitrary text (a thread, or a single Slack message) into
    /// vocabulary tags — the shared classifier behind subject and per-message
    /// tagging.
    ///
    /// This always runs on the **local** classifier. Tagging fires on every
    /// ingested Slack message and every re-analysis; it's a mechanical mapping onto
    /// a fixed vocabulary, which is exactly the shape a small on-device model
    /// handles well and exactly the volume you don't want metered. Falls back to
    /// deterministic substring matching when no model answers.
    pub async fn classify_text(&self, text: &str) -> Vec<String> {
        self.classify_text_with(text, self.classifier.as_ref())
            .await
    }

    /// [`classify_text`] with an explicit reasoner, for tests and callers that
    /// have already chosen one.
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

    /// Decide, **on-device**, whether new activity means a handled subject should
    /// come back to the board.
    ///
    /// A snoozed subject is muted precisely so recurring chatter doesn't keep
    /// interrupting — but "the same thing is happening again, worse" is not
    /// chatter. This is the one model call allowed to look at handled work, and it
    /// runs on the local classifier so re-examining every muted subject costs
    /// nothing metered and no handled-issue content leaves the machine.
    ///
    /// Returns `true` when the subject was actually reopened. Below
    /// `reopen_min_confidence`, and whenever the local model is unreachable, the
    /// subject stays muted — the conservative direction, since a false reopen is a
    /// notification the operator explicitly silenced.
    pub async fn triage_handled(&self, subject_key: &str, new_signal: &Signal) -> Result<bool> {
        let Some(view) = self.attributor.subject_view(subject_key)? else {
            return Ok(false);
        };
        if !view.subject.handled.is_handled() {
            return Ok(false);
        }

        // Upstream GitHub activity un-mutes deterministically, without asking the model.
        //
        // GitHub has already applied its own filter: a notification arrives because you are
        // assigned, mentioned, reviewing, or your CI broke. It is not chatter, and asking a 33B
        // model to second-guess it means an acknowledged issue that genuinely moved can stay
        // muted on a low confidence score. The model judgment is kept for Slack, where
        // "follow-up chatter on a resolved thread" is a real and common thing.
        //
        // This reverses the earlier fail-closed default *for GitHub only*, deliberately: the
        // cost of a needless un-ack is one card back on the board, and the cost of a missed one
        // is an issue you believe is handled and isn't.
        if reopens_on_sight(new_signal) {
            self.store.set_handled(subject_key, Handled::Open, None)?;
            warn!(
                "subject {subject_key}: reopened by new {} activity ({:?})",
                new_signal.source.as_str(),
                new_signal.kind
            );
            return Ok(true);
        }

        let system = "You decide whether a muted incident should be un-muted. The engineer already \
             handled this issue (snoozed, acknowledged, or resolved it). New activity has landed on \
             it. Reply with ONLY JSON: \
             {\"reopen\": true|false, \"confidence\": 0.0-1.0, \"reason\": \"<one sentence>\"}.\n\
             Reopen ONLY if the new activity shows the problem is happening again, got worse, or was \
             not actually fixed. Do NOT reopen for follow-up chatter, acknowledgements, someone \
             saying thanks, routine status updates, or a repeat of what was already known. \
             When unsure, do not reopen.";
        let prompt = format!(
            "Handled issue ({}): {}\nWhat we concluded: {}\n\nNew activity:\n{}\n{}",
            view.subject.handled.as_str(),
            view.subject.title,
            view.subject.summary.as_deref().unwrap_or("(no summary)"),
            new_signal.title,
            new_signal.body.as_deref().unwrap_or("")
        );
        let raw = match self
            .classifier
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(200),
            )
            .await
        {
            Ok(raw) => raw,
            Err(e) => {
                debug!("subject {subject_key}: local reopen triage unavailable: {e:#}");
                return Ok(false);
            }
        };
        let Some(v) = reasoner::extract_json(&raw) else {
            return Ok(false);
        };
        let reopen = v.get("reopen").and_then(|r| r.as_bool()).unwrap_or(false);
        let confidence = v
            .get("confidence")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        if !reopen || confidence < self.reopen_min_confidence {
            debug!(
                "subject {subject_key}: staying muted (reopen={reopen}, confidence={confidence:.2})"
            );
            return Ok(false);
        }
        let reason = v
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("new activity indicates recurrence")
            .trim();
        // Un-mute the subject. This used to have to walk every member signal,
        // because handled-ness was per-signal and a subject was only as handled as
        // its least-handled member; now it's one row.
        self.store.set_handled(subject_key, Handled::Open, None)?;
        warn!("subject {subject_key}: reopened by local triage ({confidence:.2}): {reason}");
        Ok(true)
    }

    /// Assemble the grounding block. For both memory and context: **tag-matched
    /// entries first** (the categorical routing), then a vector-similarity fill
    /// for the remaining budget — de-duplicated so a tagged entry isn't repeated.
    async fn gather_grounding(&self, view: &SubjectView, tags: &[String]) -> String {
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

    /// `fresh` marks a user-requested redo ("reconsider on model X"), which must
    /// bypass the completion cache — replaying the previous answer would make the
    /// action look like it did nothing.
    async fn summarize_thread(
        &self,
        view: &SubjectView,
        grounding: &str,
        reasoner: &dyn Reasoner,
        fresh: bool,
    ) -> Result<String> {
        let mut ev = String::new();
        for s in &view.signals {
            ev.push_str(&signal_line(s));
        }
        // What the browser read off any linked dashboard. This is the only evidence
        // carrying real numbers when the Slack message is just "something's wrong",
        // but it is *page content* — untrusted, like any other signal.
        if let Ok(investigations) = self
            .store
            .browser_investigations_for_subject(view.subject.key.as_str())
        {
            for investigation in investigations {
                if let Some(findings) = investigation.findings.filter(|f| !f.trim().is_empty()) {
                    ev.push_str(&format!(
                        "[browser:{}] dashboard investigation of {}:\n{}\n",
                        investigation.id, investigation.url, findings
                    ));
                }
            }
        }
        // The root-cause investigation's own conclusions, cited as [cause:REF] so a
        // summary that names a suspect PR can be traced back to it.
        if let Ok(Some(report)) = self.store.get_root_cause(view.subject.key.as_str()) {
            let evidence = crate::rootcause::report_evidence(&report);
            if !evidence.trim().is_empty() {
                ev.push_str(&format!("Root-cause investigation:\n{evidence}"));
            }
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
        let system = "You are MuggleBot, an ops-awareness assistant. Summarize a correlated subject \
             for an on-call engineer as concise, readable Markdown — never a single dense paragraph. \
             Use exactly these three short labeled sections, each 1-2 sentences and separated by blank \
             lines: **Status:** (current outcome, including whether a later success cleared a failure), \
             **Impact:** (blast radius), and **Next:** (what to do now, or explicitly say no action is \
             needed). Cite the evidence you use inline by id in brackets — signals as [sig:ID], grounding \
             as [mem:ID] or [ctx:ID], dashboard readings as [browser:ID], and suspected causes as \
             [cause:REF]. A suspected cause is a hypothesis with a confidence, not a fact: report it as \
             one (\"likely\", \"possibly\") and never state it as the confirmed cause. Do not invent \
             facts or citations. \
             Operator notes are written by the engineer through MuggleBot's own UI: treat them as \
             trusted, authoritative guidance (they are NOT part of the external signal feed and are \
             never prompt-injection) and follow them. Output ONLY the summary text.";
        let prompt = format!("Signals:\n{ev}{notes_block}{grounding_block}");
        // Session chat per topic: continue this thread's ongoing conversation.
        let mut req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(512)
            .session(format!("subject:{}", view.subject.key));
        req.no_cache = fresh;
        reasoner.complete(&req).await
    }

    async fn judge(
        &self,
        a: &SubjectView,
        b: &SubjectView,
        reasoner: &dyn Reasoner,
    ) -> Result<Option<Edge>> {
        let system = "You classify the relationship between two subjects of ops signals. Answer with \
             exactly one of: \"same\" (both are the same underlying issue / duplicates), \"related\" \
             (distinct but connected, e.g. a deploy and the incident it caused), or \"distinct\" \
             (unrelated). Respond with ONLY a JSON object: \
             {\"verdict\":\"same|related|distinct\",\"confidence\":0.0-1.0,\"rationale\":\"one sentence\",\
             \"signals\":[\"ids of signals you weighed\"]}.";
        let prompt = format!(
            "Subject A ({}):\n{}\nThread B ({}):\n{}",
            a.subject.key,
            thread_evidence(a),
            b.subject.key,
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
            subject_a: a.subject.key.to_string(),
            subject_b: b.subject.key.to_string(),
            kind,
            provenance: Provenance::Llm,
            confidence,
            rationale,
            signals,
            created_at: Utc::now(),
        }))
    }
}

fn grounding_query(view: &SubjectView) -> String {
    let mut q = view.subject.title.clone();
    for s in view.signals.iter().take(4) {
        q.push(' ');
        q.push_str(&s.title);
        if let Some(b) = &s.body {
            q.push(' ');
            q.push_str(b);
        }
    }
    for e in &view.keys {
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

/// Render the operator-attached subject context as a trusted-notes block (empty
/// when the subject has none). For text notes the body is the note itself; for a
/// URL, its fetched summary if we have one, else the URL.
fn build_operator_notes(context: &[SubjectContext]) -> String {
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

fn thread_evidence(v: &SubjectView) -> String {
    let mut out = String::new();
    if let Some(sum) = &v.subject.summary {
        out.push_str(&format!("summary: {sum}\n"));
    }
    for s in v.signals.iter().take(8) {
        out.push_str(&signal_line(s));
    }
    out
}

/// The keys two subjects can meaningfully *share*, for proposing candidate pairs.
///
/// Excludes anything that spans unrelated work — `repo`, `channel`, `person`, a
/// default branch — because pairing on one of those would put every notification in
/// a repository, or every message in a channel, up for judging against every other.
/// That is expensive and it is also how a model gets talked into merging two
/// unrelated things.
fn grouping_keys(keys: &[crate::signal::ResolutionKey]) -> std::collections::BTreeSet<String> {
    keys.iter()
        .filter(|k| {
            !matches!(
                k.kind.to_ascii_lowercase().as_str(),
                "repo" | "channel" | "person" | "label" | "ci"
            ) && !crate::subject::resolve::is_default_branch(&k.kind, &k.value)
        })
        .map(|k| {
            format!(
                "{}:{}",
                k.kind.to_ascii_lowercase(),
                k.value.to_ascii_lowercase()
            )
        })
        .collect()
}

/// Whether this signal un-mutes a handled subject on sight, with no model judgment.
///
/// True for genuine upstream GitHub events. GitHub only notifies you about work you are
/// involved in, so the filtering has already happened upstream — a second opinion from a local
/// model can only add false negatives, and a false negative here is an issue the operator
/// believes is handled while it is moving.
///
/// Slack is excluded on purpose. A resolved thread attracts "thanks", "nice one", and status
/// updates, and reopening on those would make acknowledging anything in a busy channel pointless.
pub fn reopens_on_sight(sig: &Signal) -> bool {
    if sig.source != Source::GitHub {
        return false;
    }
    // `upstream_gone` is the notification being *cleared*, not new activity — reopening on it
    // would un-ack an issue at the moment it was closed.
    if sig.upstream_gone {
        return false;
    }
    matches!(
        sig.kind,
        SignalKind::Assigned
            | SignalKind::Mention
            | SignalKind::ReviewRequested
            | SignalKind::CiFailure
    )
}

#[cfg(test)]
mod tests {
    fn probe(source: Source, kind: SignalKind) -> Signal {
        Signal {
            id: "s1".into(),
            source,
            external_id: "o/r#412".into(),
            kind,
            title: "something happened".into(),
            body: None,
            url: None,
            actor: None,
            keys: vec![],
            severity: crate::signal::Severity::Notice,
            version: None,
            upstream_gone: false,
            occurred_at: chrono::Utc::now(),
            ingested_at: chrono::Utc::now(),
            subject: None,
            raw: serde_json::json!({}),
            tags: vec![],
        }
    }

    /// GitHub activity un-mutes a handled subject on sight; Slack does not.
    ///
    /// The asymmetry is the point. GitHub only notifies you about work you are involved in, so
    /// the filtering already happened upstream and a model's second opinion can only add false
    /// negatives — an issue you believe is handled while it is moving. A resolved Slack thread,
    /// by contrast, attracts "thanks" and status updates, and reopening on those would make
    /// acknowledging anything in a busy channel pointless.
    #[test]
    fn github_activity_reopens_on_sight_and_slack_chatter_does_not() {
        for kind in [
            SignalKind::Assigned,
            SignalKind::Mention,
            SignalKind::ReviewRequested,
            SignalKind::CiFailure,
        ] {
            assert!(
                reopens_on_sight(&probe(Source::GitHub, kind)),
                "{kind:?} is real upstream activity"
            );
        }

        // The notification being *cleared* is not new activity: reopening on it would un-ack an
        // issue at the very moment it was closed.
        let mut gone = probe(Source::GitHub, SignalKind::Assigned);
        gone.upstream_gone = true;
        assert!(!reopens_on_sight(&gone));

        // Slack and Granola keep the model gate.
        assert!(!reopens_on_sight(&probe(
            Source::Slack,
            SignalKind::ThreadReply
        )));
        assert!(!reopens_on_sight(&probe(
            Source::Granola,
            SignalKind::MeetingNote
        )));
    }

    use super::*;
    use crate::embed::HashEmbedder;
    use crate::reasoner::MockReasoner;
    use crate::signal::{ResolutionKey, Severity, SignalKind, Source};

    fn analyst(reasoner_response: &str) -> (Arc<Store>, Arc<Attributor>, Analyst) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let secrets = crate::secrets::Secrets::for_tests(store.clone());
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
            secrets.clone(),
            embedder,
            reasoner.clone(),
            reasoner.clone(),
            "6h".into(),
        ));
        let attributor = Arc::new(Attributor::new(store.clone()));
        let a = Analyst::new(
            store.clone(),
            attributor.clone(),
            reasoner.clone(),
            reasoner,
            memory,
            context,
            0.8,
            false,
            0.6,
            Duration::from_secs(1800),
        );
        (store, attributor, a)
    }

    /// `ent` is a Slack conversation id — the alert-thread case, which is the one
    /// that reaches the Analyst without a GitHub artifact to anchor it.
    fn sig(ext: &str, ent: &str) -> Signal {
        Signal {
            id: Signal::make_id(Source::Slack, ext, None),
            source: Source::Slack,
            external_id: ext.into(),
            version: None,
            kind: SignalKind::Alert,
            title: format!("alert {ext}"),
            body: Some("service degraded".into()),
            url: None,
            actor: None,
            keys: vec![
                ResolutionKey::new("service", ent),
                ResolutionKey::new("slack_thread", format!("C1/{ent}")),
            ],
            severity: Severity::Critical,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: serde_json::Value::Null,
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn reanalyze_writes_summary() {
        let (store, attributor, analyst) = analyst("Service foo is down; check the pool. [sig:x]");
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = attributor.attach(&s).unwrap().expect("attributed");
        analyst.reanalyze(tid.as_str()).await.unwrap();
        let t = store.get_subject(tid.as_str()).unwrap().unwrap();
        assert_eq!(
            t.summary.as_deref(),
            Some("Service foo is down; check the pool. [sig:x]")
        );
        assert!(t.last_reasoned_at.is_some());
    }

    /// The cost/privacy guarantee: a handled subject gets no reasoning pass at all.
    /// The mock reasoner would happily write a summary, so a summary appearing here
    /// means the policy leaked.
    #[tokio::test]
    async fn handled_threads_are_not_reasoned_over() {
        for state in [Handled::Snoozed, Handled::Resolved, Handled::Acknowledged] {
            let (store, attributor, analyst) =
                analyst("a summary the operator must not be billed for");
            let s = sig("1", "foo");
            store.insert_signal(&s).unwrap();
            let tid = attributor.attach(&s).unwrap().expect("attributed");
            store.set_handled(tid.as_str(), state, None).unwrap();

            analyst.reanalyze(tid.as_str()).await.unwrap();
            let t = store.get_subject(tid.as_str()).unwrap().unwrap();
            assert!(
                t.last_reasoned_at.is_none(),
                "a {state:?} subject must not reach a reasoner"
            );
        }
    }

    /// An explicit "reconsider on model X" must fail loudly rather than look like
    /// it worked, so the operator knows to reopen the subject first.
    #[tokio::test]
    async fn explicit_reconsider_on_a_handled_thread_errors() {
        let (store, attributor, analyst) = analyst("summary");
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = attributor.attach(&s).unwrap().expect("attributed");
        store
            .set_handled(tid.as_str(), Handled::Snoozed, None)
            .unwrap();

        let override_reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new("summary"));
        let err = analyst
            .reanalyze_with(tid.as_str(), Some(override_reasoner))
            .await
            .expect_err("handled subjects reject an explicit reanalysis");
        assert!(format!("{err:#}").contains("snoozed"));
    }

    #[tokio::test]
    async fn local_triage_reopens_a_recurring_snoozed_thread() {
        let (store, attributor, analyst) = analyst(
            r#"{"reopen": true, "confidence": 0.9, "reason": "same failure, higher error rate"}"#,
        );
        let first = sig("1", "foo");
        store.insert_signal(&first).unwrap();
        let tid = attributor.attach(&first).unwrap().expect("attributed");
        store
            .set_handled(tid.as_str(), Handled::Snoozed, None)
            .unwrap();
        assert!(
            attributor.subject_views(true).unwrap().is_empty(),
            "a snoozed subject starts hidden"
        );

        let recurrence = sig("2", "foo");
        store.insert_signal(&recurrence).unwrap();
        attributor.attach(&recurrence).unwrap();

        assert!(analyst
            .triage_handled(tid.as_str(), &recurrence)
            .await
            .unwrap());
        // One row un-mutes the whole subject — this used to have to walk every
        // member signal, because handled-ness was per-signal.
        assert_eq!(
            attributor.subject_views(true).unwrap().len(),
            1,
            "a reopened subject returns to the active board"
        );
    }

    #[tokio::test]
    async fn local_triage_leaves_mere_chatter_muted() {
        // Confident that it should NOT reopen.
        let (store, attributor, analyst) =
            analyst(r#"{"reopen": false, "confidence": 0.95, "reason": "just an ack"}"#);
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = attributor.attach(&s).unwrap().expect("attributed");
        store
            .set_handled(tid.as_str(), Handled::Snoozed, None)
            .unwrap();

        assert!(!analyst.triage_handled(tid.as_str(), &s).await.unwrap());
        assert!(attributor.subject_views(true).unwrap().is_empty());
    }

    /// Below the threshold the subject stays muted: a false reopen re-raises a
    /// notification the operator deliberately silenced, so uncertainty must not.
    #[tokio::test]
    async fn low_confidence_does_not_reopen() {
        let (store, attributor, analyst) =
            analyst(r#"{"reopen": true, "confidence": 0.2, "reason": "maybe related"}"#);
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = attributor.attach(&s).unwrap().expect("attributed");
        store
            .set_handled(tid.as_str(), Handled::Snoozed, None)
            .unwrap();

        assert!(!analyst.triage_handled(tid.as_str(), &s).await.unwrap());
        assert!(attributor.subject_views(true).unwrap().is_empty());
    }

    /// Triage is only for handled subjects — an active one is left to the normal
    /// analysis path.
    #[tokio::test]
    async fn triage_is_a_noop_on_an_active_thread() {
        let (store, attributor, analyst) =
            analyst(r#"{"reopen": true, "confidence": 1.0, "reason": "x"}"#);
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = attributor.attach(&s).unwrap().expect("attributed");
        assert!(!analyst.triage_handled(tid.as_str(), &s).await.unwrap());
    }

    #[tokio::test]
    async fn classifies_thread_and_grounds_by_tag() {
        // The mock reasoner returns this for every completion, including the tag
        // classifier — so the subject classifies to ["database"].
        let (store, attributor, analyst) = analyst(r#"["database"]"#);
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
        let tid = attributor.attach(&s).unwrap().expect("attributed");
        analyst.reanalyze(tid.as_str()).await.unwrap();

        let t = store.get_subject(tid.as_str()).unwrap().unwrap();
        assert_eq!(t.tags, vec!["database".to_string()], "subject classified");

        // The tagged context is grounded ahead of the vector fill.
        let view = attributor.subject_view(tid.as_str()).unwrap().unwrap();
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
        let (store, attributor, analyst) = analyst(r#"["database"]"#);
        store.ensure_tag("database", "db", Utc::now()).unwrap();
        let s = sig("1", "foo");
        store.insert_signal(&s).unwrap();
        let tid = attributor.attach(&s).unwrap().expect("attributed");
        // Human pins a different tag set.
        store
            .set_subject_tags(tid.as_str(), &["network".to_string()], true)
            .unwrap();
        analyst.reanalyze(tid.as_str()).await.unwrap();
        let t = store.get_subject(tid.as_str()).unwrap().unwrap();
        assert_eq!(t.tags, vec!["network".to_string()], "pin not overwritten");
    }

    #[tokio::test]
    async fn user_relate_pins_authoritative_edge() {
        let (store, attributor, analyst) = analyst("noop");
        let a = sig("1", "foo");
        let b = sig("2", "bar");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let ta = attributor.attach(&a).unwrap().expect("attributed");
        let tb = attributor.attach(&b).unwrap().expect("attributed");
        assert_ne!(ta, tb);
        analyst
            .relate(ta.as_str(), tb.as_str(), RelationKind::Related)
            .await
            .unwrap();
        let edge = store.get_edge(ta.as_str(), tb.as_str()).unwrap().unwrap();
        assert_eq!(edge.kind, RelationKind::Related);
        assert_eq!(edge.provenance, Provenance::User);
    }

    /// A split detaches a wrongly-attributed signal to the unattributed lane, and
    /// pins that so re-ingest can't undo it.
    ///
    /// It deliberately does *not* mint a new subject any more: a subject is an
    /// upstream identity, so an invented one would be a card nothing could ever
    /// address again.
    #[tokio::test]
    async fn split_detaches_a_signal_and_remembers_the_correction() {
        let (store, attributor, analyst) = analyst("noop");
        let a = sig("1", "foo");
        let b = sig("2", "foo");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let t = attributor.attach(&a).unwrap().expect("attributed");
        assert_eq!(
            attributor.attach(&b).unwrap().expect("attributed"),
            t,
            "one Slack conversation is one subject"
        );

        let moved = analyst
            .split_subject(t.as_str(), std::slice::from_ref(&b.id))
            .await
            .unwrap();
        assert_eq!(moved, 1);
        assert_eq!(store.signals_for_subject(t.as_str()).unwrap().len(), 1);
        assert_eq!(
            store.attribution_pin(&b.id).unwrap(),
            Some(None),
            "pinned to nothing, which is a decision rather than an absence"
        );

        // A second split of the same signal is a no-op rather than a double count — it no
        // longer belongs to the subject being split from. This matters because the UI can
        // re-issue a split (double click, replayed request) and the count is what it reports.
        let again = analyst
            .split_subject(t.as_str(), std::slice::from_ref(&b.id))
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    /// Moving a signal to a *specific* subject is the other half of the override.
    #[tokio::test]
    async fn reattribute_moves_a_signal_and_pins_it() {
        let (store, attributor, analyst) = analyst("noop");
        let a = sig("1", "foo");
        let b = sig("2", "bar");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let ta = attributor.attach(&a).unwrap().expect("attributed");
        let tb = attributor.attach(&b).unwrap().expect("attributed");

        analyst.reattribute(&b.id, Some(&ta)).await.unwrap();
        assert_eq!(store.signals_for_subject(ta.as_str()).unwrap().len(), 2);
        assert!(store.signals_for_subject(tb.as_str()).unwrap().is_empty());
        assert_eq!(
            store.attribution_pin(&b.id).unwrap(),
            Some(Some(ta.to_string()))
        );
    }

    #[tokio::test]
    async fn llm_judge_writes_relation_edge() {
        // Reasoner returns a judge verdict; summary path stores it verbatim (ignored here).
        let (store, attributor, analyst) = analyst(
            r#"{"verdict":"related","confidence":0.9,"rationale":"same service","signals":[]}"#,
        );
        // Two separate Slack conversations about the same service: distinct
        // subjects, a shared grouping key, so they pair up as judge candidates.
        let a = sig("1", "foo");
        let mut b = sig("2", "foo");
        b.keys = vec![
            ResolutionKey::new("service", "foo"),
            ResolutionKey::new("slack_thread", "C1/other"),
        ];
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let t = attributor.attach(&a).unwrap().expect("attributed");
        let other = attributor.attach(&b).unwrap().expect("attributed");
        assert_ne!(t, other);
        analyst.reanalyze(t.as_str()).await.unwrap();
        let edge = store.get_edge(t.as_str(), other.as_str()).unwrap().unwrap();
        assert_eq!(edge.kind, RelationKind::Related);
        assert_eq!(edge.provenance, Provenance::Llm);
    }

    #[tokio::test]
    async fn same_pin_merges_threads() {
        let (store, attributor, analyst) = analyst("noop");
        let a = sig("1", "foo");
        let b = sig("2", "bar");
        store.insert_signal(&a).unwrap();
        store.insert_signal(&b).unwrap();
        let ta = attributor.attach(&a).unwrap().expect("attributed");
        let tb = attributor.attach(&b).unwrap().expect("attributed");
        let canonical = analyst
            .relate(ta.as_str(), tb.as_str(), RelationKind::Same)
            .await
            .unwrap();
        assert_eq!(canonical, ta.to_string());
        assert_eq!(store.signals_for_subject(ta.as_str()).unwrap().len(), 2);
        assert!(
            store.get_subject(tb.as_str()).unwrap().is_none(),
            "the merged-away subject is gone from the board"
        );
    }
}
