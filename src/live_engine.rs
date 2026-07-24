//! Live assist engine (Phase 4).
//!
//! Threads the user is active in (detected via their Slack `user_id`) are marked
//! _live_ and get a **debounced** re-analysis — 1 minute after the last activity,
//! with a 5-minute hard cap so a fast-moving thread still gets looked at. A pass
//! produces grounded [`Hint`]s: hints (a runbook, a related thread), suggestions
//! (a next step / generic mitigation), and **flags** on the user's own messages
//! (`factual_error` / `risky_action`). A high-confidence flag flips the UI to
//! red-alert and fires a Critical macOS notification.
//!
//! It is strictly advisory: it warns and cites, it never edits or sends anything.
//! With no reachable reasoner it degrades to deterministic mitigation suggestions
//! so the panel is never empty when it matters.

use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::context::ContextManager;
use crate::correlation::{Correlator, ThreadView};
use crate::event::{Event, RedAlert};
use crate::live::{FlagType, Hint, HintKind, HintState};
use crate::memory::MemoryManager;
use crate::mitigations;
use crate::notify::Notifier;
use crate::reasoner::{self, CompletionRequest, Reasoner};
use crate::signal::{Severity, Signal, SignalKind, Source};
use crate::store::Store;

struct Pending {
    first: Instant,
    last: Instant,
}

pub struct LiveEngine {
    store: Arc<Store>,
    correlator: Arc<Correlator>,
    /// Heavy reasoner (Opus) for the full pass on high-stakes threads.
    reasoner: Arc<dyn Reasoner>,
    /// Cheap reasoner (ambient/Sonnet). Runs the triage gate, and also the full
    /// pass itself for low-stakes Slack-only threads — Slack chatter doesn't earn
    /// an Opus call.
    ambient: Arc<dyn Reasoner>,
    memory: Arc<MemoryManager>,
    context: Arc<ContextManager>,
    notifier: Arc<Notifier>,
    events: broadcast::Sender<Event>,
    debounce: Duration,
    debounce_max: Duration,
    red_alert: bool,
    red_alert_min_confidence: f64,
    pending: Mutex<HashMap<String, Pending>>,
}

impl LiveEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        correlator: Arc<Correlator>,
        reasoner: Arc<dyn Reasoner>,
        ambient: Arc<dyn Reasoner>,
        memory: Arc<MemoryManager>,
        context: Arc<ContextManager>,
        notifier: Arc<Notifier>,
        events: broadcast::Sender<Event>,
        debounce: Duration,
        debounce_max: Duration,
        red_alert: bool,
        red_alert_min_confidence: f64,
    ) -> Self {
        Self {
            store,
            correlator,
            reasoner,
            ambient,
            memory,
            context,
            notifier,
            events,
            debounce,
            debounce_max,
            red_alert,
            red_alert_min_confidence,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Record activity in a thread: mark it live and (re)start its debounce timer.
    pub fn on_activity(&self, thread_id: &str) {
        let _ = self.store.set_thread_live(thread_id, true);
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        pending
            .entry(thread_id.to_string())
            .and_modify(|p| p.last = now)
            .or_insert(Pending {
                first: now,
                last: now,
            });
        debug!("live: thread {thread_id} scheduled for re-analysis");
    }

    /// Background loop: every tick, run a live pass for threads whose debounce has
    /// elapsed (quiet for `debounce`) or hit the `debounce_max` hard cap.
    pub async fn run(self: Arc<Self>) {
        let tick = self
            .debounce
            .min(Duration::from_secs(15))
            .max(Duration::from_secs(2));
        loop {
            tokio::time::sleep(tick).await;
            let due = self.take_due();
            for tid in due {
                if let Err(e) = self.analyze_thread(&tid).await {
                    warn!("live pass for {tid} failed: {e:#}");
                }
            }
        }
    }

    fn take_due(&self) -> Vec<String> {
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap();
        let due: Vec<String> = pending
            .iter()
            .filter(|(_, p)| {
                now.duration_since(p.last) >= self.debounce
                    || now.duration_since(p.first) >= self.debounce_max
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &due {
            pending.remove(id);
        }
        due
    }

    /// Run one grounded live-assist pass over a thread. Public so a client can
    /// trigger it directly (and for tests).
    pub async fn analyze_thread(&self, thread_id: &str) -> Result<()> {
        let Some(view) = self.correlator.thread_view(thread_id)? else {
            return Ok(());
        };
        // Cheap Sonnet triage before the expensive full-context pass: a casual
        // mention ("hi!") shouldn't cost a grounded heavy-reasoner call.
        if !self.warrants_full_pass(&view).await {
            debug!("live: thread {thread_id} triaged as non-operational; skipping full pass");
            self.store.clear_active_hints(thread_id)?;
            return Ok(());
        }
        let grounding = self.gather_grounding(&view).await;
        let hints = match self.llm_pass(&view, &grounding).await {
            Ok(h) if !h.is_empty() => h,
            Ok(_) => self.fallback_hints(&view, &grounding),
            Err(e) => {
                debug!("live: llm pass unavailable ({e:#}); using deterministic fallback");
                self.fallback_hints(&view, &grounding)
            }
        };

        self.store.clear_active_hints(thread_id)?;
        for hint in &hints {
            self.store.put_hint(hint)?;
            let _ = self.events.send(Event::Hint(hint.clone()));
            // Escalate a high-confidence flag to red-alert.
            if self.red_alert
                && hint.kind == HintKind::Flag
                && hint.confidence >= self.red_alert_min_confidence
            {
                self.notifier.notify_critical(
                    &format!("MuggleBot: {}", flag_label(hint.flag_type)),
                    &hint.text,
                );
                let _ = self.events.send(Event::RedAlert(RedAlert {
                    thread_id: thread_id.to_string(),
                    hint_id: hint.id.clone(),
                    message: hint.text.clone(),
                }));
            }
        }
        Ok(())
    }

    /// Triage gate: is this thread worth the full grounded pass on the heavy
    /// reasoner? A genuine alert always is and bypasses the classifier; anything
    /// else gets a cheap Sonnet yes/no. **Fails open** — any error or
    /// unparseable answer proceeds to the full pass, so triage can never silence
    /// a real incident, only spare the cost of obvious chatter.
    async fn warrants_full_pass(&self, view: &ThreadView) -> bool {
        if view.signals.iter().any(|s| {
            s.severity >= Severity::Warning
                || matches!(s.kind, SignalKind::Alert | SignalKind::CiFailure)
        }) {
            return true;
        }
        let mut ev = String::new();
        for s in &view.signals {
            ev.push_str(&format!(
                "- {}: {}\n",
                s.title,
                s.body.as_deref().unwrap_or("")
            ));
        }
        let system = "You are a fast triage filter for an ops-awareness tool. Decide whether a \
            thread of signals needs an on-call engineer's attention: it concerns an error, failure, \
            incident, alert, degraded system, or a concrete request to review or act. Casual \
            greetings, social chatter, thanks, and FYIs do NOT. Respond with ONLY JSON: \
            {\"needs_attention\":true|false}.";
        let req =
            CompletionRequest::single(format!("Thread: {}\nSignals:\n{ev}", view.thread.title))
                .with_system(system)
                .max_tokens(60);
        match self.ambient.complete(&req).await {
            Ok(text) => reasoner::extract_json(&text)
                .and_then(|v| v.get("needs_attention").and_then(|x| x.as_bool()))
                .unwrap_or(true),
            Err(e) => {
                debug!("live: triage unavailable ({e:#}); proceeding with full pass");
                true
            }
        }
    }

    /// Route the full pass by stakes: a thread touching a non-Slack source or a
    /// real alert gets the heavy (Opus) reasoner; pure Slack chatter is handled
    /// by the cheaper ambient (Sonnet) model.
    fn full_pass_reasoner(&self, view: &ThreadView) -> &Arc<dyn Reasoner> {
        if wants_heavy(&view.signals) {
            &self.reasoner
        } else {
            &self.ambient
        }
    }

    async fn gather_grounding(&self, view: &ThreadView) -> String {
        let query = view.thread.title.clone()
            + " "
            + &view
                .signals
                .iter()
                .map(|s| s.title.clone())
                .collect::<Vec<_>>()
                .join(" ");
        let mut out = String::new();
        // Tag-matched memory first (the thread's tags are set by the ambient
        // classifier pass), then a vector-similarity fill, de-duplicated.
        let mut mem_seen: Vec<String> = Vec::new();
        if let Ok(tagged) = self.store.memory_by_tags(&view.thread.tags) {
            // Tag-matched is high-precision — feed the full fact, not just the gloss.
            for m in tagged.into_iter().take(3) {
                out.push_str(&memory_block(&m.id, &m.summary, &m.text));
                mem_seen.push(m.id);
            }
        }
        if let Ok(hits) = self.memory.search(&query, 3).await {
            for h in hits.into_iter().filter(|h| h.score > 0.05) {
                if mem_seen.contains(&h.memory.id) {
                    continue;
                }
                out.push_str(&format!("[mem:{}] {}\n", h.memory.id, h.memory.summary));
                mem_seen.push(h.memory.id);
            }
        }
        // Tag-matched context first, then a vector-similarity fill, de-duplicated.
        let mut seen: Vec<String> = Vec::new();
        if let Ok(tagged) = self.store.context_by_tags(&view.thread.tags) {
            // Tag-matched entries carry a body excerpt (runbook steps, not a gloss).
            for c in tagged.into_iter().take(3) {
                out.push_str(&context_block(&c));
                seen.push(c.id);
            }
        }
        if let Ok(hits) = self.context.search(&query, 3).await {
            for h in hits.into_iter().filter(|h| h.score > 0.05) {
                if seen.contains(&h.context.id) {
                    continue;
                }
                out.push_str(&context_line(
                    &h.context.id,
                    &h.context.location,
                    h.context.summary.as_deref(),
                ));
                seen.push(h.context.id);
            }
        }
        out
    }

    async fn llm_pass(&self, view: &ThreadView, grounding: &str) -> Result<Vec<Hint>> {
        let mut ev = String::new();
        for s in &view.signals {
            let is_self = s
                .raw
                .get("is_self")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            ev.push_str(&format!(
                "[sig:{}]{} {}: {}\n",
                s.id,
                if is_self { " (YOUR MESSAGE)" } else { "" },
                s.title,
                s.body.as_deref().unwrap_or("")
            ));
            if let Some(summary) = s.raw.get("link_summary").and_then(|v| v.as_str()) {
                let url = s.raw.get("link_url").and_then(|v| v.as_str()).unwrap_or("");
                ev.push_str(&format!("    ↳ linked page {url}: {summary}\n"));
            }
        }
        let catalog = mitigations::CATALOG
            .iter()
            .map(|m| format!("- {}: {}", m.name, m.description))
            .collect::<Vec<_>>()
            .join("\n");
        let system = "You are MuggleBot's live-assist. You watch a thread the engineer is active in \
            and help without acting. Produce, grounded in the provided runbooks/memory and citing by \
            id ([sig:ID], [ctx:ID], [mem:ID]): (1) hints — a relevant runbook, a past incident, a \
            connection they may have missed; (2) suggestions — a sensible next step or a generic \
            mitigation from the catalog; (3) flags on THEIR OWN messages only (marked YOUR MESSAGE) \
            when they state something the grounding contradicts (factual_error) or propose a \
            risky/irreversible action (risky_action). Only flag with real evidence — do not cry wolf. \
            Respond with ONLY JSON: {\"hints\":[{\"text\":\"\",\"citations\":[]}],\
            \"suggestions\":[{\"text\":\"\",\"citations\":[]}],\
            \"flags\":[{\"signal_id\":\"\",\"flag_type\":\"factual_error|risky_action\",\"text\":\"\",\
            \"rationale\":\"\",\"confidence\":0.0,\"citations\":[]}]}.";
        let prompt = format!(
            "Thread: {}\n\nSignals:\n{ev}\n\nGrounding:\n{grounding}\n\nGeneric mitigations catalog:\n{catalog}",
            view.thread.title
        );
        let req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(900)
            .session(format!("thread:{}", view.thread.id));
        let text = self.full_pass_reasoner(view).complete(&req).await?;
        let v = reasoner::extract_json(&text)
            .ok_or_else(|| anyhow::anyhow!("no JSON in live-assist response"))?;
        Ok(self.parse_hints(&view.thread.id, &v))
    }

    fn parse_hints(&self, thread_id: &str, v: &serde_json::Value) -> Vec<Hint> {
        let now = Utc::now();
        let mut out = Vec::new();
        let mk = |kind: HintKind,
                  text: String,
                  rationale: Option<String>,
                  citations: Vec<String>,
                  confidence: f64,
                  flag_type: Option<FlagType>| Hint {
            id: format!("hint/{}", crate::store::new_id()),
            thread_id: thread_id.to_string(),
            kind,
            flag_type,
            text,
            rationale,
            citations,
            confidence,
            state: HintState::Active,
            created_at: now,
        };
        for h in v
            .get("hints")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(text) = h
                .get("text")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                out.push(mk(
                    HintKind::Hint,
                    text.into(),
                    None,
                    citations(h),
                    0.5,
                    None,
                ));
            }
        }
        for h in v
            .get("suggestions")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(text) = h
                .get("text")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                out.push(mk(
                    HintKind::Suggestion,
                    text.into(),
                    None,
                    citations(h),
                    0.5,
                    None,
                ));
            }
        }
        for f in v
            .get("flags")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            let Some(text) = f
                .get("text")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let flag_type = f
                .get("flag_type")
                .and_then(|x| x.as_str())
                .and_then(FlagType::parse)
                .unwrap_or(FlagType::FactualError);
            let confidence = f.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.5);
            let rationale = f
                .get("rationale")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            let mut cites = citations(f);
            if let Some(sid) = f.get("signal_id").and_then(|x| x.as_str()) {
                cites.push(sid.to_string());
            }
            out.push(mk(
                HintKind::Flag,
                text.into(),
                rationale,
                cites,
                confidence,
                Some(flag_type),
            ));
        }
        out
    }

    /// No-LLM fallback: deterministic mitigation suggestions + a grounding pointer.
    fn fallback_hints(&self, view: &ThreadView, grounding: &str) -> Vec<Hint> {
        let now = Utc::now();
        let mut out = Vec::new();
        for m in mitigations::suggest(&view.signals).into_iter().take(2) {
            out.push(Hint {
                id: format!("hint/{}", crate::store::new_id()),
                thread_id: view.thread.id.clone(),
                kind: HintKind::Suggestion,
                flag_type: None,
                text: format!("{}: {}", m.mitigation.name, m.mitigation.description),
                rationale: Some("matched the generic-mitigations catalog by keyword".into()),
                citations: m.cited_signals,
                confidence: 0.4,
                state: HintState::Active,
                created_at: now,
            });
        }
        if !grounding.trim().is_empty() {
            if let Some(first) = grounding.lines().next() {
                out.push(Hint {
                    id: format!("hint/{}", crate::store::new_id()),
                    thread_id: view.thread.id.clone(),
                    kind: HintKind::Hint,
                    flag_type: None,
                    text: format!("Relevant grounding: {first}"),
                    rationale: None,
                    citations: vec![],
                    confidence: 0.4,
                    state: HintState::Active,
                    created_at: now,
                });
            }
        }
        out
    }
}

/// A thread earns the heavy (Opus) pass when it touches a non-Slack source or a
/// real alert (Warning+, alert/CI-failure kinds); otherwise it's Slack-only
/// chatter that the ambient (Sonnet) model handles. An empty thread is not heavy.
fn context_line(id: &str, location: &str, summary: Option<&str>) -> String {
    format!("[ctx:{}] {} — {}\n", id, location, summary.unwrap_or(""))
}

/// Per-entry body excerpt for high-precision tag-matched entries — enough to
/// carry a runbook's steps / a memory's full fact into the prompt without
/// letting one entry dominate.
const GROUNDING_BODY_CHARS: usize = 2_000;

/// A fuller context entry: summary line plus a bounded excerpt of the source body.
fn context_block(c: &crate::context::Context) -> String {
    let mut out = context_line(&c.id, &c.location, c.summary.as_deref());
    if let Some(raw) = c.raw.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        out.push_str(&excerpt_line(raw));
    }
    out
}

/// A fuller memory entry: summary line plus a bounded excerpt of the full fact.
fn memory_block(id: &str, summary: &str, text: &str) -> String {
    let mut out = format!("[mem:{id}] {summary}\n");
    let text = text.trim();
    if !text.is_empty() && text != summary.trim() {
        out.push_str(&excerpt_line(text));
    }
    out
}

fn excerpt_line(body: &str) -> String {
    let excerpt: String = body.chars().take(GROUNDING_BODY_CHARS).collect();
    let ellipsis = if body.chars().count() > GROUNDING_BODY_CHARS {
        " …"
    } else {
        ""
    };
    format!("    {excerpt}{ellipsis}\n")
}

fn wants_heavy(signals: &[Signal]) -> bool {
    signals.iter().any(|s| {
        s.source != Source::Slack
            || s.severity >= Severity::Warning
            || matches!(s.kind, SignalKind::Alert | SignalKind::CiFailure)
    })
}

fn citations(v: &serde_json::Value) -> Vec<String> {
    v.get("citations")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn flag_label(f: Option<FlagType>) -> &'static str {
    match f {
        Some(FlagType::FactualError) => "possible factual error",
        Some(FlagType::RiskyAction) => "risky action",
        None => "flag",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Notifications;
    use crate::embed::HashEmbedder;
    use crate::reasoner::MockReasoner;
    use crate::signal::{Entity, Severity, Signal, SignalKind, Source, State};

    fn engine(
        response: &str,
    ) -> (
        Arc<Store>,
        Arc<Correlator>,
        Arc<LiveEngine>,
        broadcast::Receiver<Event>,
    ) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new(response));
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
        let notifier = Arc::new(Notifier::new(&Notifications::default(), None));
        let (tx, rx) = broadcast::channel(64);
        let engine = Arc::new(LiveEngine::new(
            store.clone(),
            correlator.clone(),
            reasoner.clone(),
            reasoner,
            memory,
            context,
            notifier,
            tx,
            Duration::from_secs(60),
            Duration::from_secs(300),
            true,
            0.75,
        ));
        (store, correlator, engine, rx)
    }

    fn self_msg() -> Signal {
        Signal {
            id: Signal::make_id(Source::Slack, "m1"),
            source: Source::Slack,
            external_id: "m1".into(),
            kind: SignalKind::Mention,
            title: "we should just delete the prod database".into(),
            body: Some("deleting prod db now".into()),
            url: None,
            actor: Some("UME".into()),
            entities: vec![Entity::new("channel", "#incidents")],
            severity: Severity::Warning,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: serde_json::json!({ "is_self": true }),
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn flag_stored_and_red_alert_fired() {
        let response = r#"{"hints":[],"suggestions":[],"flags":[
            {"signal_id":"slack/m1","flag_type":"risky_action","text":"Deleting the prod database is irreversible","rationale":"runbook says never delete prod","confidence":0.95,"citations":["ctx:x"]}
        ]}"#;
        let (store, correlator, engine, mut rx) = engine(response);
        let s = self_msg();
        store.insert_signal(&s).unwrap();
        let tid = correlator.ingest(&s).unwrap();

        engine.analyze_thread(&tid).await.unwrap();

        let hints = store.list_hints(Some(&tid)).unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].kind, HintKind::Flag);
        assert_eq!(hints[0].flag_type, Some(FlagType::RiskyAction));

        // A Hint event and a RedAlert event should have been broadcast.
        let mut saw_alert = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, Event::RedAlert(_)) {
                saw_alert = true;
            }
        }
        assert!(saw_alert, "high-confidence flag must raise red-alert");
    }

    fn casual_mention() -> Signal {
        Signal {
            id: Signal::make_id(Source::Slack, "hi1"),
            source: Source::Slack,
            external_id: "hi1".into(),
            kind: SignalKind::Mention,
            title: "HI!".into(),
            body: Some("HI!".into()),
            url: None,
            actor: Some("U9".into()),
            entities: vec![Entity::new("channel", "#dev")],
            severity: Severity::Notice,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: serde_json::json!({ "mentions_me": true }),
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn triage_skips_full_pass_for_casual_mention() {
        // Triage (ambient) answers "not operational"; the expensive full pass
        // never runs, so no hints are produced.
        let (store, correlator, engine, _rx) = engine(r#"{"needs_attention":false}"#);
        let s = casual_mention();
        store.insert_signal(&s).unwrap();
        let tid = correlator.ingest(&s).unwrap();
        engine.analyze_thread(&tid).await.unwrap();
        assert!(store.list_hints(Some(&tid)).unwrap().is_empty());
    }

    #[test]
    fn source_routing_keeps_slack_chatter_on_ambient() {
        // Pure Slack, low severity → ambient (Sonnet).
        assert!(!wants_heavy(&[casual_mention()]));
        // A real alert in the mix → heavy (Opus).
        assert!(wants_heavy(&[self_msg()]));
        // A non-Slack signal → heavy, even at low severity.
        let mut gh = casual_mention();
        gh.source = Source::GitHub;
        gh.severity = Severity::Info;
        assert!(wants_heavy(&[gh]));
    }

    #[tokio::test]
    async fn fallback_when_no_json() {
        // Reasoner returns prose (no JSON) → deterministic mitigation fallback.
        let (store, correlator, engine, _rx) = engine("I couldn't produce JSON, sorry.");
        let mut s = self_msg();
        s.title = "connection pool exhausted, cpu saturation".into();
        s.body = Some("pool exhausted".into());
        store.insert_signal(&s).unwrap();
        let tid = correlator.ingest(&s).unwrap();
        engine.analyze_thread(&tid).await.unwrap();
        let hints = store.list_hints(Some(&tid)).unwrap();
        assert!(hints.iter().any(|h| h.kind == HintKind::Suggestion));
    }
}
