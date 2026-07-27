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
//!
//! **One request at a time, process-wide.** See [`gate`].

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use tracing::debug;

use super::{CompletionRequest, Reasoner};

/// The process-wide permit for a self-hosted Ollama.
///
/// One Ollama, one GPU. Four concurrent requests to a 33B model are slower *and* worse than
/// a queue of one: they contend for the same weights, and Ollama serializes or thrashes
/// depending on how much memory the model needs. So the gate belongs here, at the resource,
/// rather than at any one caller.
///
/// It has to be **global** rather than a field, because `OllamaReasoner` instances are
/// created in several places and some of them at runtime: the configured tiers, the separate
/// vision handle, and a fresh one per request whenever the chat pane or a "reconsider on
/// model X" override names a model. A per-instance semaphore would gate each of those
/// independently and add up to exactly the concurrency it was meant to prevent.
///
/// Ollama **Cloud** is exempt — it is a fleet, not a GPU, and queueing against it would
/// throw away the one thing you are paying it for. See [`serialize_calls`].
static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// How many foreground calls are waiting for or holding the gate.
///
/// The whole priority mechanism. With one worker there is nothing to reserve, so instead
/// background work **defers**: it will not start while any foreground call wants the GPU, and
/// takes the gate only when this is zero. Indexing therefore fills the gaps rather than competing
/// for the slot.
///
/// A counter rather than a second semaphore because the question is "does anyone else want this?",
/// which a semaphore cannot answer — `available_permits` says whether the gate is free right now,
/// not whether a caller is about to ask for it.
static FOREGROUND_DEMAND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How often a deferring background call re-checks.
///
/// Coarse on purpose: the work being paced is minutes long, so a tighter poll would spend more
/// wakeups than it saves latency.
const DEFER_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// Raises foreground demand for as long as it is alive.
///
/// A guard rather than manual increment/decrement so an early `?` on the gate acquisition cannot
/// leave the count raised forever — which would stop indexing permanently, and look exactly like
/// the deadlock this module already had once.
struct Demand;

impl Demand {
    fn raise() -> Self {
        FOREGROUND_DEMAND.fetch_add(1, Ordering::SeqCst);
        Demand
    }
}

impl Drop for Demand {
    fn drop(&mut self) {
        FOREGROUND_DEMAND.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a local model call is for, which decides whether it may use the reserve.
///
/// The split exists for the same reason the GitHub budget has one: indexing is bulk work whose
/// lateness costs nothing much, and it will otherwise sit in front of the passes that make the
/// board useful — a notification arriving, a PR being judged, an issue being triaged. Those wait
/// on a human, so they get a worker that indexing cannot take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// Notifications, PR critiques, issue triage, correlation, explanations. Never held back.
    Foreground,
    /// Component carding, commit summaries, repo characterization.
    Background,
}

/// Default permits when nothing called [`init_gate`] — tests, and any ad-hoc reasoner built
/// before config is read.
const DEFAULT_PERMITS: usize = 1;

/// Fallback request timeout for a reasoner built without one.
///
/// Never `None`. `reqwest`'s default is no timeout at all, and with a shared single permit that
/// turns one hung request into a permanent stall of every local model call in the process —
/// measured at 2.5 hours of wedged indexing before anything complained.
const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How much longer than a request a caller will wait for the permit before giving up.
///
/// Defence in depth: with a bounded request the permit is always released, so this should never
/// fire. If it does, something is holding the gate that this module does not control, and an
/// error naming that is worth far more than a task that hangs and is never heard from again.
const GATE_GRACE: std::time::Duration = std::time::Duration::from_secs(120);

/// Size the gate from config. Called once, at startup, before any reasoner runs.
///
/// Idempotent and first-call-wins: `OnceLock` means a second call is ignored rather than
/// resizing a semaphore other tasks are already queued on.
pub fn init_gate(permits: usize) {
    let _ = GATE.set(Arc::new(Semaphore::new(permits.max(1))));
}

fn gate() -> &'static Arc<Semaphore> {
    GATE.get_or_init(|| Arc::new(Semaphore::new(DEFAULT_PERMITS)))
}

/// Whether any foreground call currently wants the GPU.
fn foreground_waiting() -> bool {
    FOREGROUND_DEMAND.load(Ordering::SeqCst) > 0
}

/// Whether calls to this endpoint have to queue.
///
/// Everything except Ollama Cloud. A self-hosted instance is one process with one GPU
/// whether it is on loopback or a box down the hall, so the host check is for the hosted
/// service specifically rather than for "is this localhost". An authenticated proxy in front
/// of a real cluster would be gated wrongly — raise `[reasoner] local_concurrency` for that.
fn serialize_calls(url: &str) -> bool {
    let host = host_of(url);
    host != "ollama.com" && !host.ends_with(".ollama.com")
}

/// The host from a URL, lowercased, without scheme, userinfo, port, path, or IPv6 brackets.
///
/// Hand-rolled because pulling in a URL parser to answer one question about one hostname is
/// not worth the dependency — but done properly rather than as a substring check, since
/// `contains("ollama.com")` would also match `http://localhost/?upstream=ollama.com`.
fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // An IPv6 literal keeps its colons inside brackets, so strip those before the port.
    let host = match host_port.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(h, _)| h),
        None => host_port.split_once(':').map_or(host_port, |(h, _)| h),
    };
    host.to_ascii_lowercase()
}

pub struct OllamaReasoner {
    client: reqwest::Client,
    /// Whether this handle's calls may use the reserved worker. See [`Local`].
    priority: Local,
    /// How long one request may take. Also bounds how long the shared permit can be held.
    timeout: std::time::Duration,
    url: String,
    model: String,
    /// Optional API key (Ollama Cloud / an authenticated proxy), sent as a bearer.
    api_key: Option<String>,
}

impl OllamaReasoner {
    pub fn new(url: String, model: String, api_key: Option<String>) -> Self {
        Self::with_timeout(url, model, api_key, DEFAULT_TIMEOUT)
    }

    /// Build with an explicit request timeout.
    ///
    /// The timeout is on the **client**, not just awaited around the call, so a connection that
    /// accepts and then goes silent is abandoned too — which is the shape the hang took: the
    /// socket stayed open and no bytes arrived.
    pub fn with_timeout(
        url: String,
        model: String,
        api_key: Option<String>,
        timeout: std::time::Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            timeout,
            priority: Local::Foreground,
            url,
            model,
            api_key: api_key.filter(|k| !k.trim().is_empty()),
        }
    }

    /// Mark this handle's calls as bulk work, so they cannot take the reserved worker.
    ///
    /// Used by the code indexer and the repo crawler — the two callers that generate thousands of
    /// calls and whose lateness costs nothing much.
    pub fn background(mut self) -> Self {
        self.priority = Local::Background;
        self
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

        // Held across the retry too: the retry is the *same* request re-sent without one
        // field, and releasing in between would let a queued caller in and put two
        // generations on the GPU at once — the exact thing the gate exists to stop.
        // Foreground demand is raised *before* queueing, not after acquiring, so a background
        // call that has not started yet sees it and stands aside. Raising it only on acquisition
        // would let indexing take the slot out from under a notification that was already asking.
        let _demand = match (serialize_calls(&self.url), self.priority) {
            (true, Local::Foreground) => Some(Demand::raise()),
            _ => None,
        };
        if serialize_calls(&self.url) && self.priority == Local::Background {
            self.defer_to_foreground(self.timeout + GATE_GRACE).await?;
        }
        let _permit = match serialize_calls(&self.url) {
            true => {
                let wait = self.timeout + GATE_GRACE;
                match tokio::time::timeout(wait, gate().acquire()).await {
                    Ok(p) => Some(p.context("ollama gate closed")?),
                    Err(_) => bail!(
                        "waited {}s for the local model queue and never got a slot — something \
                         is holding it longer than a request is allowed to take",
                        wait.as_secs()
                    ),
                }
            }
            false => None,
        };
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
}

impl OllamaReasoner {
    /// Stand aside until no foreground call wants the GPU.
    ///
    /// This is what "index when there is a free worker" means with a single worker: background work
    /// does not queue *alongside* foreground work, it waits for the queue to empty. A FIFO
    /// semaphore alone would not do it — a background call that got there first would make a
    /// notification wait its turn.
    ///
    /// **In-flight work is not preempted.** A card already talking to the model keeps the slot
    /// until it finishes, so the worst case for a foreground call is one carding pass — minutes,
    /// not the hours a whole batch would take. Cancelling mid-request would get foreground in
    /// sooner at the cost of throwing away the GPU time already spent; if that trade is wanted it
    /// belongs here.
    /// `give_up_after` bounds the wait. Passed in rather than derived from `self.timeout`, so a
    /// test can assert the deference behaviour without sitting through the production bound —
    /// deriving it added two minutes to every run of the suite.
    async fn defer_to_foreground(&self, give_up_after: std::time::Duration) -> Result<()> {
        if !foreground_waiting() {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + give_up_after;
        let mut waited = std::time::Duration::ZERO;
        while foreground_waiting() {
            if std::time::Instant::now() >= deadline {
                // Not an error worth retrying immediately: the caller is a bounded batch on a
                // durable timer, so the next tick tries again. Said out loud because an index that
                // silently never progresses on a busy board is the failure mode here.
                bail!(
                    "stood aside for foreground work for {}s and never got a free slot; \
                     indexing will retry on its next tick",
                    waited.as_secs()
                );
            }
            tokio::time::sleep(DEFER_POLL).await;
            waited += DEFER_POLL;
            // Logged once at a threshold rather than per poll: a deferring indexer is normal, an
            // indexer that has deferred for a minute is worth knowing about.
            if waited.as_secs() == 60 {
                debug!("ollama: indexing has stood aside for 60s of foreground work");
            }
        }
        Ok(())
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
    // Listing models is a metadata read; if it has not answered in ten seconds the server is
    // not going to.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut req = client.get(format!("{}/api/tags", url.trim_end_matches('/')));
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
    #[test]
    fn a_self_hosted_ollama_is_gated_and_the_cloud_is_not() {
        // Everything you run yourself is one process with one GPU, wherever it lives.
        for url in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://[::1]:11434",
            "http://gpu-box.lan:11434",
            "https://ollama.internal.example/",
        ] {
            assert!(serialize_calls(url), "{url} must queue");
        }
        // The hosted service is a fleet — queueing against it throws away what you pay for.
        for url in [
            "https://ollama.com",
            "https://api.ollama.com/v1",
            "https://OLLAMA.COM/",
        ] {
            assert!(!serialize_calls(url), "{url} must not queue");
        }
    }

    /// A substring check would have matched these and silently un-gated a local instance.
    #[test]
    fn the_cloud_host_is_matched_as_a_host_not_a_substring() {
        assert!(serialize_calls(
            "http://localhost:11434/?upstream=ollama.com"
        ));
        assert!(serialize_calls("http://ollama.com.evil.test:11434"));
        assert!(serialize_calls("http://user:pw@127.0.0.1:11434"));
        // ...while a real subdomain of the service still counts as the service.
        assert!(!serialize_calls("https://eu.ollama.com"));
    }

    #[test]
    fn host_parsing_strips_what_it_should() {
        assert_eq!(host_of("http://127.0.0.1:11434/api/chat"), "127.0.0.1");
        assert_eq!(
            host_of("https://user:pw@Example.COM:443/x?y#z"),
            "example.com"
        );
        assert_eq!(host_of("http://[fe80::1%eth0]:11434"), "fe80::1%eth0");
        assert_eq!(host_of("localhost:11434"), "localhost");
    }

    /// Background work stands aside while a foreground call wants the GPU.
    ///
    /// With one worker there is nothing to reserve, so priority has to come from deference: a FIFO
    /// semaphore alone would let a background call that arrived first make a notification wait its
    /// turn. Demand is raised *before* queueing for exactly that reason.
    #[tokio::test]
    async fn indexing_stands_aside_while_foreground_work_is_pending() {
        assert!(!foreground_waiting(), "no demand to begin with");

        // A foreground call announces itself before it queues.
        let demand = Demand::raise();
        assert!(
            foreground_waiting(),
            "indexing must see that someone is asking"
        );

        // A background call in this state gives up rather than taking the slot. Timed out
        // deliberately short: the assertion is that it does not proceed, not how long it waits.
        let bg = OllamaReasoner::with_timeout(
            "http://127.0.0.1:11434".into(),
            "m".into(),
            None,
            std::time::Duration::from_millis(1),
        )
        .background();
        let outcome = bg
            .defer_to_foreground(std::time::Duration::from_millis(50))
            .await;
        assert!(
            outcome.is_err(),
            "indexing took the slot out from under foreground work"
        );
        let msg = format!("{:#}", outcome.unwrap_err());
        // The message has to say it will retry, or a deferring index reads as a failed one.
        assert!(msg.contains("stood aside"), "{msg}");
        assert!(msg.contains("retry"), "{msg}");

        // Once the foreground call is done, indexing proceeds immediately.
        drop(demand);
        assert!(!foreground_waiting());
        assert!(
            bg.defer_to_foreground(std::time::Duration::from_millis(50))
                .await
                .is_ok(),
            "the gap must be usable"
        );
    }

    /// Demand is released even when the call that raised it fails.
    ///
    /// A guard rather than manual bookkeeping, because a leaked count stops indexing permanently
    /// and would look exactly like the deadlock this module already shipped once.
    #[test]
    fn foreground_demand_cannot_leak() {
        assert!(!foreground_waiting());
        {
            let _a = Demand::raise();
            {
                let _b = Demand::raise();
                assert!(foreground_waiting());
            }
            // Nested demand: one release must not clear the other's claim.
            assert!(
                foreground_waiting(),
                "an inner release cleared an outer claim"
            );
        }
        assert!(!foreground_waiting(), "demand leaked past its scope");
    }

    /// The gate is process-wide, so two reasoners built independently — which is what
    /// happens for the vision handle and for every chat-pane model override — must queue
    /// against each other. A per-instance semaphore passes a naive test and adds up to
    /// exactly the concurrency it was meant to prevent.
    ///
    /// Both assertions live in one test on purpose: they mutate and read the same global, and
    /// as separate `#[test]` fns the harness would run them on parallel threads, where one
    /// holding the permit makes the other's reading of `available_permits` a coin flip.
    #[tokio::test]
    async fn the_gate_is_one_process_wide_permit_sized_once() {
        init_gate(1);
        let g = gate();
        assert_eq!(g.available_permits(), 1);

        let held = g.acquire().await.unwrap();
        // A second caller — a different `OllamaReasoner` instance — finds nothing available.
        assert!(
            g.try_acquire().is_err(),
            "two generations would run on the GPU at once"
        );
        drop(held);
        assert_eq!(g.available_permits(), 1);

        // A queued caller must eventually give up with an error rather than hang, so a wedge is
        // reported instead of waited on forever. The wait length does not matter here — the
        // shape does: `timeout` returns `Err` while the gate is held, and a slot once it is not.
        let held = g.acquire().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), g.acquire())
                .await
                .is_err(),
            "a held gate must not hand out a permit"
        );
        drop(held);
        assert!(g.try_acquire().is_ok(), "and must free up once released");

        // Sizing is first-call-wins, not last: resizing a semaphore other tasks are already
        // queued on is how you end up with two callers holding a one-permit gate.
        init_gate(8);
        assert_eq!(
            gate().available_permits(),
            1,
            "the gate must not be resized"
        );
    }
    /// Live check against a real Ollama. Ignored by default: it needs a pulled model and
    /// takes real seconds.
    ///
    /// Measures wall-clock rather than counting tasks. Counting *tasks* proves nothing —
    /// they all start immediately and then block on the permit, so the peak is always N. What
    /// serialization actually implies is that N concurrent calls take about N times as long
    /// as one, and that is observable from outside without instrumenting production code.
    ///
    /// Run with: `cargo test ollama_gate_serializes_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn ollama_gate_serializes_live() {
        use std::time::Instant;
        // Parameterized so the negative control is runnable: with permits > 1 the three calls
        // should overlap and the assertion below should *fail*. An assertion that can't be
        // made to fail isn't measuring anything.
        let permits: usize = std::env::var("OLLAMA_TEST_PERMITS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        init_gate(permits);
        let model = std::env::var("OLLAMA_TEST_MODEL").unwrap_or_else(|_| "gemma4:12b".into());
        let r = std::sync::Arc::new(OllamaReasoner::new(
            "http://127.0.0.1:11434".into(),
            model.clone(),
            None,
        ));
        let ask = |n: usize| {
            let r = r.clone();
            async move {
                let mut req = CompletionRequest::single(format!(
                    "Reply with only the number {n}, no other words."
                ));
                req.max_tokens = 16;
                r.complete(&req).await
            }
        };

        // Warm the model in, so the first call's load time isn't charged to the comparison.
        ask(0)
            .await
            .unwrap_or_else(|e| panic!("is `{model}` pulled? {e:#}"));

        let t0 = Instant::now();
        ask(1).await.expect("single call");
        let single = t0.elapsed();

        let t1 = Instant::now();
        let (a, b, c) = tokio::join!(ask(2), ask(3), ask(4));
        let three = t1.elapsed();
        for r in [a, b, c] {
            r.expect("concurrent call");
        }

        println!("permits {permits}: single {single:?}, three concurrent {three:?}");
        // Generous factor: the point is that three calls cost roughly three calls, not one.
        // A concurrent Ollama would finish all three in about `single`.
        assert!(
            three > single.mul_f32(1.8),
            "three gated calls took {three:?} against a single {single:?} — they overlapped, \
             so the gate is not binding"
        );
    }
    /// The bug this guards: `reqwest::Client::new()` has no timeout, and with a single shared
    /// permit one hung request stalls every local model call in the process. Measured at 2.5
    /// hours of wedged indexing — twelve invocations "running", Ollama idle at 3% CPU.
    #[test]
    fn a_reasoner_always_has_a_finite_request_timeout() {
        let r = OllamaReasoner::new("http://127.0.0.1:11434".into(), "m".into(), None);
        assert_eq!(r.timeout, DEFAULT_TIMEOUT);
        assert!(
            r.timeout > std::time::Duration::ZERO,
            "an unbounded request can hold the shared permit forever"
        );
        // Generous enough for a real generation: carding a component on a 33B model is minutes.
        assert!(r.timeout >= std::time::Duration::from_secs(300));
    }

    #[test]
    fn an_explicit_timeout_is_honoured() {
        let r = OllamaReasoner::with_timeout(
            "http://127.0.0.1:11434".into(),
            "m".into(),
            None,
            std::time::Duration::from_secs(42),
        );
        assert_eq!(r.timeout, std::time::Duration::from_secs(42));
    }

    /// The gate wait has to exceed the request timeout, or a caller queued behind a legitimately
    /// slow generation would give up while the system is working correctly.
    #[test]
    fn the_gate_wait_outlasts_a_legitimate_request() {
        assert!(
            GATE_GRACE > std::time::Duration::ZERO,
            "no grace means a queued caller races the request it is waiting for"
        );
        let r = OllamaReasoner::new("http://127.0.0.1:11434".into(), "m".into(), None);
        let wait = r.timeout + GATE_GRACE;
        assert!(wait > r.timeout);
    }
}
