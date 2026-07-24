//! HTTP + WebSocket server backing the LCARS web UI.
//!
//! The WebSocket streams a full [`Snapshot`] on connect and then incremental
//! [`Event`]s (new signals, thread updates, live-assist hints, red-alert). The
//! REST surface is thin: typed convenience routes for the board and triage, a
//! generic `/api/tool/:name` that dispatches through the shared [`crate::tools`]
//! surface (so the UI and MCP share one implementation), the agent-chat endpoint,
//! and database-backed credential management for the config page.
//!
//! The Rust server both serves the built LCARS UI (from `ui/dist`, same-origin)
//! and exposes the API + WebSocket, so opening `http://<ui.listen>` is all you
//! need in production; the Vite dev server (`:5173`) is also allowed for hot
//! reload. CORS and the WebSocket handshake are restricted to those origins, so a
//! foreign page in the user's browser can't read the board or drive triage.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State as AxumState,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use tracing::{debug, info};

use crate::chat::{ChatAgent, ChatTurn};
use crate::config::Config;
use crate::event::{Event, Snapshot};
use crate::live_engine::LiveEngine;
use crate::notify::Notifier;
use crate::reasoner;
use crate::signal::State;
use crate::store::Store;
use crate::tools::Tools;

/// Credential accounts the config page manages the presence of. Reasoning rides
/// the CLI bridge (no LLM API keys); `ollama` is the optional Ollama Cloud key.
const KNOWN_CREDENTIALS: &[&str] = &["github", "slack", "granola", "ollama"];

#[derive(Clone)]
pub struct AppState {
    pub tools: Arc<Tools>,
    pub chat: Arc<ChatAgent>,
    pub live: Arc<LiveEngine>,
    pub events: broadcast::Sender<Event>,
    pub notifier: Arc<Notifier>,
    /// Origins permitted to call the API and open the WebSocket cross-origin —
    /// derived from the bound address plus the Vite dev server.
    pub allowed_origins: Arc<Vec<String>>,
    /// Path to the TOML config file, for the editable config page.
    pub config_path: Arc<String>,
    /// Credential store — source tokens and authed-context secrets live here.
    pub store: Arc<Store>,
}

/// Origins allowed for CORS + the WS handshake, given the address we bound to.
fn allowed_origins(addr: &str) -> Vec<String> {
    let port = addr.rsplit(':').next().unwrap_or("8080");
    vec![
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        "http://localhost:5173".into(),
        "http://127.0.0.1:5173".into(),
    ]
}

/// Where the built UI lives (override with `$MUGGLEBOT_UI_DIR`).
fn ui_dir() -> String {
    std::env::var("MUGGLEBOT_UI_DIR").unwrap_or_else(|_| "ui/dist".into())
}

pub async fn serve(addr: String, mut state: AppState) -> anyhow::Result<()> {
    let origins = allowed_origins(&addr);
    state.allowed_origins = Arc::new(origins.clone());

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(
            origins
                .iter()
                .filter_map(|o| HeaderValue::from_str(o).ok())
                .collect::<Vec<_>>(),
        ))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE]);

    let ui = ui_dir();
    let serve_ui = std::path::Path::new(&ui).is_dir();
    if serve_ui {
        info!("serving UI from {ui}");
    } else {
        info!("no built UI at {ui} (run `cd ui && npm run build`, or use the Vite dev server)");
    }

    let mut app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/signals", get(list_signals))
        .route("/api/signals/{id}/state", post(set_state))
        .route("/api/threads", get(list_threads))
        .route("/api/board/reset", post(reset_board))
        .route("/api/threads/{id}", get(get_thread))
        .route("/api/config", get(get_config))
        .route("/api/config/raw", get(get_config_raw).put(put_config))
        .route("/api/chat", post(chat))
        .route("/api/chats", get(list_chats))
        .route(
            "/api/chats/{id}",
            get(get_chat).put(save_chat).delete(delete_chat),
        )
        .route("/api/models/{provider}", get(list_models))
        .route("/api/tool/{name}", post(call_tool))
        .route(
            "/api/credentials",
            get(list_credentials).post(set_credential),
        )
        .route("/api/credentials/{account}", delete(delete_credential))
        .route("/ws", get(ws_handler));

    if serve_ui {
        app = app.fallback_service(ServeDir::new(ui));
    }

    let app = app.layer(cors).with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("web UI/API listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn err_response(e: anyhow::Error) -> axum::response::Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
}

async fn list_signals(
    AxumState(st): AxumState<AppState>,
    Query(q): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let args = json!({
        "source": q.get("source"),
        "since": q.get("since"),
        "severity": q.get("severity"),
        "state": q.get("state"),
        "limit": q.get("limit").and_then(|l| l.parse::<u64>().ok()),
    });
    match st.tools.call("list_signals", &args).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn list_threads(
    AxumState(st): AxumState<AppState>,
    Query(q): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let active_only = q.get("active_only").map(|v| v != "false").unwrap_or(true);
    match st
        .tools
        .call("list_threads", &json!({ "active_only": active_only }))
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_thread(
    AxumState(st): AxumState<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match st.tools.call("get_thread", &json!({ "id": id })).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn get_config(AxumState(st): AxumState<AppState>) -> impl IntoResponse {
    match st.tools.read_resource("config://redacted").await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

/// The raw TOML config file, for the editable config page.
async fn get_config_raw(AxumState(st): AxumState<AppState>) -> impl IntoResponse {
    match std::fs::read_to_string(&*st.config_path) {
        Ok(text) => text.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reading {}: {e}", st.config_path),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ConfigBody {
    toml: String,
}

/// Validate submitted TOML parses as a full config, then write it to disk. The
/// running subsystems captured their settings at startup, so most changes apply
/// on the next restart — surfaced in the response so the UI can say so.
async fn put_config(
    AxumState(st): AxumState<AppState>,
    Json(body): Json<ConfigBody>,
) -> impl IntoResponse {
    if let Err(e) = toml::from_str::<Config>(&body.toml) {
        return (StatusCode::BAD_REQUEST, format!("invalid config: {e}")).into_response();
    }
    match std::fs::write(&*st.config_path, &body.toml) {
        Ok(()) => Json(json!({
            "ok": true,
            "message": "Saved. Restart MuggleBot to apply changes to watchers and reasoners.",
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("writing {}: {e}", st.config_path),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct StateBody {
    state: State,
}

async fn set_state(
    AxumState(st): AxumState<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StateBody>,
) -> impl IntoResponse {
    if let Err(e) = st.tools.store.set_state(&id, body.state) {
        return err_response(e);
    }
    // Rebroadcast the changed signal, then reconcile the board (a resolve can
    // drop the thread from the active set).
    if let Ok(Some(sig)) = st.tools.store.get_signal(&id) {
        // Triaging a thread resets its notification dedup, so new activity on it
        // can notify again instead of being suppressed as "already seen".
        if let Some(tid) = &sig.thread {
            st.notifier.clear_notified(tid);
        }
        let _ = st.events.send(Event::Signal(sig));
    }
    if let Ok(views) = st.tools.correlator.thread_views(true) {
        let _ = st.events.send(Event::Board(views));
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Board reset: delete all persisted board events and their derived thread
/// analysis. Source health, configuration, credentials, memories, context, and
/// chats remain intact. An upstream source may add a still-active notification
/// again on its next poll, because reset does not mutate the source of record.
async fn reset_board(AxumState(st): AxumState<AppState>) -> impl IntoResponse {
    let (cleared, threads) = match st.tools.store.clear_board_events() {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    // Reset notification dedup for removed thread ids. A newly ingested event
    // must be allowed to notify even if it happens to reuse a prior thread id.
    for tid in &threads {
        st.notifier.clear_notified(tid);
    }
    // Push the authoritative active board so resolved threads drop out for every
    // connected client (reconcile removes anything no longer in the active set).
    if let Ok(views) = st.tools.correlator.thread_views(true) {
        let _ = st.events.send(Event::Board(views));
    }
    Json(json!({ "cleared": cleared })).into_response()
}

/// Generic tool dispatch — the UI reaches the full read/write surface here.
async fn call_tool(
    AxumState(st): AxumState<AppState>,
    Path(name): Path<String>,
    Json(args): Json<Value>,
) -> impl IntoResponse {
    match st.tools.call(&name, &args).await {
        Ok(v) => {
            broadcast_after(&st, &name).await;
            Json(v).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// After a write tool that changes threads, push the authoritative active-thread
/// set so clients reconcile (and drop threads that merged or split away).
async fn broadcast_after(st: &AppState, tool: &str) {
    let touches_threads = matches!(
        tool,
        "relate" | "split_thread" | "attach_thread_context" | "reanalyze" | "set_thread_tags"
    );
    if touches_threads {
        if let Ok(views) = st.tools.correlator.thread_views(true) {
            let _ = st.events.send(Event::Board(views));
        }
    }
}

#[derive(serde::Deserialize)]
struct ChatBody {
    messages: Vec<ChatTurn>,
    /// Optional provider override picked in the chat pane (`anthropic` | `openai`
    /// | `ollama`). Absent → the agent's default reasoner.
    #[serde(default)]
    provider: Option<String>,
    /// Optional model override; only honored alongside `provider`.
    #[serde(default)]
    model: Option<String>,
    /// Routing tags the user attached to this chat — their tag-matched memory and
    /// context are folded in as grounding for the agent.
    #[serde(default)]
    tags: Vec<String>,
}

use crate::reasoner::provider_label;

async fn chat(AxumState(st): AxumState<AppState>, Json(body): Json<ChatBody>) -> impl IntoResponse {
    let result = match (&body.provider, &body.model) {
        (Some(provider), Some(model)) if !model.trim().is_empty() => {
            let ollama_key = st.store.credential_get("ollama").ok().flatten();
            let reasoner = reasoner::build(
                provider_label(provider),
                model,
                &st.tools.config.reasoner,
                ollama_key,
            );
            st.chat
                .respond_with(&body.messages, &body.tags, &reasoner)
                .await
        }
        _ => st.chat.respond(&body.messages, &body.tags).await,
    };
    match result {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

/// List persisted agent chats (metadata only), newest activity first.
async fn list_chats(AxumState(st): AxumState<AppState>) -> impl IntoResponse {
    match st.store.list_chats() {
        Ok(chats) => Json(chats).into_response(),
        Err(e) => err_response(e),
    }
}

/// Fetch one chat's full transcript.
async fn get_chat(AxumState(st): AxumState<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match st.store.get_chat(&id) {
        Ok(Some(chat)) => Json(chat).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(serde::Deserialize)]
struct SaveChatBody {
    title: String,
    messages: Value,
}

/// Create or update a chat (client-supplied id, upsert).
async fn save_chat(
    AxumState(st): AxumState<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SaveChatBody>,
) -> impl IntoResponse {
    match st.store.upsert_chat(&id, &body.title, &body.messages) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

/// Delete a chat.
async fn delete_chat(
    AxumState(st): AxumState<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match st.store.delete_chat(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

/// List the models selectable for a provider — dynamic for Ollama (installed
/// models), curated for the CLI-bridge providers.
async fn list_models(
    AxumState(st): AxumState<AppState>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    let ollama_key = st.store.credential_get("ollama").ok().flatten();
    match reasoner::list_models(
        provider_label(&provider),
        &st.tools.config.reasoner,
        ollama_key,
    )
    .await
    {
        Ok(models) => Json(json!({ "models": models })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

async fn list_credentials(AxumState(st): AxumState<AppState>) -> impl IntoResponse {
    let mut out = serde_json::Map::new();
    for &acc in KNOWN_CREDENTIALS {
        let present = st.store.credential_get(acc).ok().flatten().is_some();
        out.insert(acc.to_string(), Value::Bool(present));
    }
    Json(Value::Object(out))
}

#[derive(serde::Deserialize)]
struct CredentialBody {
    account: String,
    secret: String,
}

async fn set_credential(
    AxumState(st): AxumState<AppState>,
    Json(body): Json<CredentialBody>,
) -> impl IntoResponse {
    if body.account.trim().is_empty() || body.secret.is_empty() {
        return (StatusCode::BAD_REQUEST, "account and secret required").into_response();
    }
    match st.store.credential_set(&body.account, &body.secret) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_credential(
    AxumState(st): AxumState<AppState>,
    Path(account): Path<String>,
) -> impl IntoResponse {
    match st.store.credential_delete(&account) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    AxumState(st): AxumState<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // WebSocket upgrades bypass CORS, so validate Origin ourselves. No Origin
    // (native/local clients) is allowed.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !st.allowed_origins.iter().any(|o| o == origin) {
            return (StatusCode::FORBIDDEN, "cross-origin websocket rejected").into_response();
        }
    }
    ws.on_upgrade(move |socket| ws_loop(socket, st))
        .into_response()
}

async fn ws_loop(mut socket: WebSocket, st: AppState) {
    let mut rx = st.events.subscribe();

    // Initial snapshot so a fresh client renders immediately.
    if let Some(snapshot) = build_snapshot(&st).await {
        if let Ok(txt) = serde_json::to_string(&Event::Snapshot(Box::new(snapshot))) {
            if socket.send(Message::Text(txt.into())).await.is_err() {
                return;
            }
        }
    }

    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Ok(ev) => {
                    if let Ok(txt) = serde_json::to_string(&ev) {
                        if socket.send(Message::Text(txt.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("ws client lagged, dropped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
    debug!("ws client disconnected");
}

async fn build_snapshot(st: &AppState) -> Option<Snapshot> {
    Some(Snapshot {
        signals: st.tools.store.recent(200).ok()?,
        threads: st.tools.correlator.thread_views(true).ok()?,
        hints: st.tools.store.list_hints(None).ok()?,
        health: st.tools.store.source_health().ok()?,
    })
}
