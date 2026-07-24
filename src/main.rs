//! MuggleBot — a single-pane-of-glass ops-awareness agent.
//!
//! The daemon wires the whole spine together: load config, open the SQLite store,
//! build the reasoners/embedder and the correlation, grounding, live-assist, and
//! chat subsystems, run one poll loop per enabled watcher (GitHub, Slack,
//! Granola), and serve the web UI (HTTP + WebSocket) and the MCP endpoint
//! (stdio + HTTP). New signals are deduped, persisted, correlated into threads,
//! reasoned over, notified, and streamed live to the UI.

#![allow(dead_code)]

mod chat;
mod config;
mod context;
mod correlation;
mod embed;
mod event;
mod live;
mod live_engine;
mod mcp;
mod memory;
mod mitigations;
mod notify;
mod reasoner;
mod server;
mod signal;
mod store;
mod tags;
mod tools;
mod watchers;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::chat::ChatAgent;
use crate::config::Config;
use crate::context::ContextManager;
use crate::correlation::{Analyst, Correlator};
use crate::event::Event;
use crate::live_engine::LiveEngine;
use crate::mcp::McpServer;
use crate::memory::MemoryManager;
use crate::notify::Notifier;
use crate::reasoner::Reasoners;
use crate::server::AppState;
use crate::store::Store;
use crate::tools::Tools;
use crate::watchers::{
    github::GithubWatcher, granola::GranolaWatcher, slack::SlackWatcher, Watcher,
};

#[derive(Parser)]
#[command(
    name = "mugglebot",
    version,
    about = "Single-pane-of-glass ops-awareness agent"
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, env = "MUGGLEBOT_CONFIG", default_value = "config.toml")]
    config: String,
}

/// Shared handles threaded into each watcher's poll loop.
#[derive(Clone)]
struct Services {
    store: Arc<Store>,
    correlator: Arc<Correlator>,
    analyst: Arc<Analyst>,
    live: Arc<LiveEngine>,
    notifier: Arc<Notifier>,
    context: Arc<ContextManager>,
    events: broadcast::Sender<Event>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so stdout stays clean for the MCP stdio transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,mugglebot=debug")),
        )
        .init();

    let cli = Cli::parse();
    let cfg =
        Config::load(&cli.config).with_context(|| format!("loading config from {}", cli.config))?;
    let cfg = Arc::new(cfg);
    info!("MuggleBot starting (config: {})", cli.config);

    // Data dir + SQLite store.
    let data_dir = cfg.data_dir_path()?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let db_path = data_dir.join("mugglebot.sqlite");
    let store = Arc::new(Store::open(&db_path).context("opening store")?);
    info!("store: {}", db_path.display());

    // Reasoners + embedder. Reasoning rides the subscription CLI bridge (no API
    // keys); Ollama's optional key is stored in the database.
    let ollama_key = store.credential_get("ollama").unwrap_or(None);
    let reasoners = Reasoners::from_config(&cfg.reasoner, ollama_key.clone());
    // Use Ollama embeddings whenever any reasoning already runs through Ollama —
    // either ambient (local or cloud) or the on-device path used by local_only_sources.
    let embed_provider = if matches!(cfg.reasoner.ambient.as_str(), "ollama" | "ollama_local")
        || !cfg.reasoner.local_only_sources.is_empty()
    {
        "ollama"
    } else {
        "hash"
    };
    let embedder = embed::build(
        embed_provider,
        &cfg.reasoner.ollama_url,
        &cfg.reasoner.ollama_model,
        ollama_key,
    );
    info!("embedder: {embed_provider}");

    // Grounding stores.
    let memory = Arc::new(MemoryManager::new(
        store.clone(),
        embedder.clone(),
        reasoners.ambient.clone(),
        reasoners.heavy.clone(),
    ));
    let context = Arc::new(ContextManager::new(
        store.clone(),
        embedder.clone(),
        reasoners.ambient.clone(),
        reasoners.heavy.clone(),
        cfg_context_refresh(&cfg),
    ));

    // Correlation: deterministic grouping + the LLM relation graph.
    let window =
        config::parse_duration(&cfg.correlation.window).unwrap_or(Duration::from_secs(1800));
    let correlator = Arc::new(Correlator::new(store.clone(), window));
    let analyst = Arc::new(Analyst::new(
        store.clone(),
        correlator.clone(),
        reasoners.ambient.clone(),
        memory.clone(),
        context.clone(),
        cfg.correlation.dedup_threshold,
        cfg.correlation.auto_merge,
        window,
    ));

    // Notifier + live event bus.
    notify::init();
    let notifier = Arc::new(Notifier::new(
        &cfg.notifications,
        cfg.general.quiet_hours.as_deref(),
    ));
    let (events, _rx) = broadcast::channel::<Event>(1024);

    // Live assist.
    let live = Arc::new(LiveEngine::new(
        store.clone(),
        correlator.clone(),
        reasoners.heavy.clone(),
        reasoners.ambient.clone(),
        memory.clone(),
        context.clone(),
        notifier.clone(),
        events.clone(),
        config::parse_duration(&cfg.live.debounce).unwrap_or(Duration::from_secs(60)),
        config::parse_duration(&cfg.live.debounce_max).unwrap_or(Duration::from_secs(300)),
        cfg.live.red_alert,
        cfg.live.red_alert_min_confidence,
    ));

    // The shared tool surface (used by both the web API and MCP) + chat.
    let tools = Arc::new(Tools {
        store: store.clone(),
        correlator: correlator.clone(),
        analyst: analyst.clone(),
        memory: memory.clone(),
        context: context.clone(),
        reasoner: reasoners.heavy.clone(),
        config: cfg.clone(),
    });
    let chat = Arc::new(ChatAgent::new(tools.clone(), reasoners.heavy.clone()));

    // Web server (UI/API + WS).
    let app_state = AppState {
        tools: tools.clone(),
        chat,
        live: live.clone(),
        events: events.clone(),
        notifier: notifier.clone(),
        allowed_origins: Arc::new(Vec::new()), // populated by server::serve from the bound addr
        config_path: Arc::new(cli.config.clone()),
        store: store.clone(),
    };
    let ui_addr = cfg.ui.listen.clone();
    tokio::spawn(async move {
        if let Err(e) = server::serve(ui_addr, app_state).await {
            error!("web server error: {e:#}");
        }
    });

    // MCP endpoint (stdio + HTTP).
    let mcp_server = Arc::new(McpServer::new(tools.clone()));
    {
        let http_addr = cfg.mcp.http_listen.clone();
        let srv = mcp_server.clone();
        tokio::spawn(async move {
            if let Err(e) = mcp::serve_http(http_addr, srv).await {
                error!("mcp http error: {e:#}");
            }
        });
    }
    if cfg.mcp.stdio {
        let srv = mcp_server.clone();
        tokio::spawn(async move {
            if let Err(e) = mcp::serve_stdio(srv).await {
                warn!("mcp stdio ended: {e:#}");
            }
        });
    }

    // Live-assist debounce scheduler + context-library refresh scheduler.
    tokio::spawn(live.clone().run());
    {
        let context = context.clone();
        tokio::spawn(async move {
            let interval = Duration::from_secs(300);
            loop {
                tokio::time::sleep(interval).await;
                let changed = context.refresh_due().await;
                if !changed.is_empty() {
                    info!("context: refreshed {} source(s)", changed.len());
                }
            }
        });
    }
    // Managed contexts directory: <data_dir>/contexts/<tag>/<files>. Each
    // subdirectory is an automatic tag; files are watched (short poll) and
    // reloaded on change. Synced once at startup, then on a tight interval.
    {
        let context = context.clone();
        let contexts_dir = data_dir.join("contexts");
        tokio::spawn(async move {
            let interval = Duration::from_secs(15);
            loop {
                match context.sync_dir(&contexts_dir).await {
                    Ok(n) if n > 0 => info!("contexts dir: {n} file(s) synced/reloaded"),
                    Ok(_) => {}
                    Err(e) => warn!("contexts dir sync failed: {e:#}"),
                }
                // One-time summary for any tag still lacking one (folder tags,
                // manual tags). Idempotent — filled tags are skipped.
                let n = context.backfill_tag_summaries().await;
                if n > 0 {
                    info!("tags: backfilled {n} summary(ies)");
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // Build enabled watchers.
    let svc = Services {
        store: store.clone(),
        correlator: correlator.clone(),
        analyst: analyst.clone(),
        live: live.clone(),
        notifier: notifier.clone(),
        context: context.clone(),
        events: events.clone(),
    };
    let watchers = build_watchers(&cfg, &store);
    if watchers.is_empty() {
        warn!("no active watchers — running with the web UI + MCP only");
    }

    let mut handles = Vec::new();
    for w in watchers {
        handles.push(tokio::spawn(poll_loop(w, svc.clone())));
    }

    tokio::signal::ctrl_c()
        .await
        .context("waiting for shutdown signal")?;
    info!("shutting down");
    for h in handles {
        h.abort();
    }
    Ok(())
}

fn cfg_context_refresh(_cfg: &Config) -> String {
    // Context refresh cadence isn't a first-class config field yet; the library
    // default matches the design's `[context].refresh_default`.
    "6h".into()
}

/// Read a source token from the credential store. Returns `Ok(None)` when the
/// account has no stored secret.
fn token_for(store: &Store, account: &str) -> Result<Option<String>> {
    store.credential_get(account)
}

fn build_watchers(cfg: &Config, store: &Store) -> Vec<Box<dyn Watcher>> {
    let mut watchers: Vec<Box<dyn Watcher>> = Vec::new();

    if cfg.sources.github.enabled {
        match token_for(store, "github") {
            Ok(Some(token)) => match GithubWatcher::new(&cfg.sources.github, token) {
                Ok(w) => watchers.push(Box::new(w)),
                Err(e) => error!("github watcher init failed: {e:#}"),
            },
            Ok(None) => warn!(
                "github enabled but no token stored (account 'github'); skipping. \
                 Store one from the config page."
            ),
            Err(e) => error!("github credential read failed: {e:#}"),
        }
    }

    if cfg.sources.slack.enabled {
        match token_for(store, "slack") {
            Ok(Some(token)) => match SlackWatcher::new(&cfg.sources.slack, token) {
                Ok(w) => watchers.push(Box::new(w)),
                Err(e) => error!("slack watcher init failed: {e:#}"),
            },
            Ok(None) => warn!("slack enabled but no token stored (account 'slack'); skipping"),
            Err(e) => error!("slack credential read failed: {e:#}"),
        }
    }

    if cfg.sources.granola.enabled {
        match token_for(store, "granola") {
            Ok(Some(token)) => match GranolaWatcher::new(&cfg.sources.granola, token) {
                Ok(w) => watchers.push(Box::new(w)),
                Err(e) => error!("granola watcher init failed: {e:#}"),
            },
            Ok(None) => {
                warn!("granola enabled but no token stored (account 'granola'); skipping")
            }
            Err(e) => error!("granola credential read failed: {e:#}"),
        }
    }

    watchers
}

/// If a Slack signal links to a public page, fetch and summarize it into
/// `raw.link_summary` (with `raw.link_url`). First accessible URL wins; any
/// error (non-public, unreachable, no text) is logged and skipped so enrichment
/// never blocks ingest. No-op for non-Slack signals.
async fn enrich_slack_links(sig: &mut signal::Signal, context: &ContextManager) {
    if sig.source != signal::Source::Slack {
        return;
    }
    let urls: Vec<String> = sig
        .raw
        .get("urls")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for url in urls {
        match context.summarize_public_url(&url).await {
            Ok(summary) if !summary.trim().is_empty() => {
                if let Some(obj) = sig.raw.as_object_mut() {
                    obj.insert("link_url".into(), serde_json::json!(url));
                    obj.insert("link_summary".into(), serde_json::json!(summary));
                }
                return;
            }
            Ok(_) => {}
            Err(e) => debug!("slack link enrich {url}: {e:#}"),
        }
    }
}

async fn poll_loop(watcher: Box<dyn Watcher>, svc: Services) {
    let interval = watcher.interval();
    let name = watcher.name();
    info!("watcher '{name}' polling every {interval:?}");
    loop {
        match watcher.poll().await {
            Ok(batch) => {
                let mut new = 0usize;
                let mut resolved = 0usize;
                let mut refreshed = 0usize;
                let mut touched: BTreeSet<String> = BTreeSet::new();
                // Newly-ingested Slack signals to classify per-message (id + text).
                let mut slack_to_classify: Vec<(String, String)> = Vec::new();
                for sig in batch.signals {
                    let mut sig = sig;
                    // Enrich a linked-out Slack message with a one-paragraph
                    // summary of the (public) page it points at, before storing.
                    enrich_slack_links(&mut sig, &svc.context).await;
                    match svc.store.insert_signal(&sig) {
                        Ok(true) => {
                            new += 1;
                            let thread_id = match svc.correlator.ingest(&sig) {
                                Ok(id) => Some(id),
                                Err(e) => {
                                    error!("correlation ingest failed: {e:#}");
                                    None
                                }
                            };
                            // Follow (live-assist) any discussion the user is
                            // personally engaged in — they posted, were mentioned,
                            // or were asked to act — rather than all Slack traffic.
                            let engaged = sig.is_user_engaged();
                            match &thread_id {
                                // Dedup notifications against the board: one per
                                // thread state change, not one per signal.
                                Some(tid) => svc.notifier.maybe_notify_thread(tid, &sig),
                                None => svc.notifier.maybe_notify(&sig),
                            }
                            // Classify every Slack message per-signal (off the
                            // poll path — see below). An env alert (env-2…/acc-1…/
                            // org-1…) is already routed by its environment entity,
                            // so it bypasses the fuzzy tag classifier entirely.
                            let is_env_alert = sig.entities.iter().any(|e| e.kind == "environment");
                            if sig.source == signal::Source::Slack && !is_env_alert {
                                let text =
                                    format!("{} {}", sig.title, sig.body.as_deref().unwrap_or(""));
                                slack_to_classify.push((sig.id.clone(), text));
                            }
                            let _ = svc.events.send(Event::Signal(sig));
                            if let Some(tid) = thread_id {
                                if engaged {
                                    svc.live.on_activity(&tid);
                                }
                                touched.insert(tid);
                            }
                        }
                        Ok(false) => {
                            // The watcher refreshed source-provided context on
                            // an existing notification (for example, a CI log
                            // excerpt). Re-broadcast the board below so open
                            // signal details update without a page reload.
                            refreshed += 1;
                        }
                        Err(e) => error!("store insert failed: {e:#}"),
                    }
                }
                if let Some(snapshot) = batch.snapshot {
                    match snapshot.source {
                        signal::Source::GitHub => {
                            match svc
                                .store
                                .resolve_missing_github_notifications(&snapshot.active_ids)
                            {
                                Ok(signals) => {
                                    resolved = signals.len();
                                    for sig in signals {
                                        if let Some(tid) = &sig.thread {
                                            svc.notifier.clear_notified(tid);
                                        }
                                        let _ = svc.events.send(Event::Signal(sig));
                                    }
                                    if resolved > 0 {
                                        if let Ok(views) = svc.correlator.thread_views(true) {
                                            let _ = svc.events.send(Event::Board(views));
                                        }
                                    }
                                }
                                Err(e) => error!("github unread reconciliation failed: {e:#}"),
                            }
                        }
                        source => warn!("no snapshot reconciler for source '{source}'"),
                    }
                }
                let repaired = match svc.correlator.repair_orphaned_threads() {
                    Ok(count) => count,
                    Err(e) => {
                        error!("thread metadata repair failed: {e:#}");
                        0
                    }
                };
                if repaired > 0 {
                    warn!("repaired {repaired} orphaned thread(s)");
                }
                // Push the authoritative board now so newly-ingested threads appear
                // immediately — the ambient reanalyze below is async and LLM-backed,
                // so gating the board on it means new signals don't show while the
                // reasoner is slow or unavailable. Reanalyze pushes again once its
                // summaries/merges land.
                if !touched.is_empty() || refreshed > 0 || repaired > 0 {
                    if let Ok(views) = svc.correlator.thread_views(true) {
                        let _ = svc.events.send(Event::Board(views));
                    }
                }
                // Analyze a batch serially. Concurrent auto-merges can otherwise
                // each move signals into a thread another task subsequently
                // removes, leaving the signals invisible to the board.
                if !touched.is_empty() {
                    let svc2 = svc.clone();
                    tokio::spawn(async move {
                        for tid in touched {
                            if let Err(e) = svc2.analyst.reanalyze(&tid).await {
                                warn!("ambient reanalyze {tid} failed: {e:#}");
                            }
                        }
                        if let Err(e) = svc2.correlator.repair_orphaned_threads() {
                            error!("thread metadata repair failed: {e:#}");
                        }
                        // Reconcile the board — reanalyze may have auto-merged threads.
                        if let Ok(views) = svc2.correlator.thread_views(true) {
                            let _ = svc2.events.send(Event::Board(views));
                        }
                    });
                }
                // Classify each new Slack message into tags, off the poll path so
                // per-message LLM calls never stall ingest. Persist per-signal and
                // re-emit so the board shows the tags. Guarded on a non-empty
                // vocabulary — until tags exist there's nothing to route to, so we
                // skip the per-message LLM calls entirely.
                let vocab_ready = svc
                    .store
                    .list_tags()
                    .map(|t| !t.is_empty())
                    .unwrap_or(false);
                if vocab_ready && !slack_to_classify.is_empty() {
                    let svc2 = svc.clone();
                    tokio::spawn(async move {
                        for (id, text) in slack_to_classify {
                            let tags = svc2.analyst.classify_text(&text).await;
                            if tags.is_empty() {
                                continue;
                            }
                            if let Err(e) = svc2.store.set_signal_tags(&id, &tags) {
                                warn!("slack classify store {id} failed: {e:#}");
                                continue;
                            }
                            if let Ok(Some(sig)) = svc2.store.get_signal(&id) {
                                let _ = svc2.events.send(Event::Signal(sig));
                            }
                        }
                    });
                }
                let _ = svc.store.record_health(name, true, None, None);
                if new > 0 || resolved > 0 || refreshed > 0 {
                    info!("watcher '{name}': {new} new, {refreshed} refreshed, {resolved} resolved signal(s)");
                    let _ = svc
                        .events
                        .send(Event::Health(svc.store.source_health().unwrap_or_default()));
                }
            }
            Err(e) => {
                warn!("watcher '{name}' poll error: {e:#}");
                let _ = svc
                    .store
                    .record_health(name, false, Some(&format!("{e:#}")), None);
                let _ = svc
                    .events
                    .send(Event::Health(svc.store.source_health().unwrap_or_default()));
            }
        }
        tokio::time::sleep(interval).await;
    }
}
