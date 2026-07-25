//! MuggleBot — a single-pane-of-glass ops-awareness agent.
//!
//! The daemon wires the whole spine together: load config, open the SQLite store,
//! build the reasoners/embedder and the correlation, grounding, live-assist, and
//! chat subsystems, run one poll loop per enabled watcher (GitHub, Slack,
//! Granola), and serve the web UI (HTTP + WebSocket) and the MCP endpoint
//! (stdio + HTTP). New signals are deduped, persisted, correlated into threads,
//! reasoned over, notified, and streamed live to the UI.

#![allow(dead_code)]

mod browser;
mod chat;
mod checkout;
mod comments;
mod config;
mod context;
mod correlation;
mod ecosystem;
mod embed;
mod event;
mod github;
mod live;
mod live_engine;
mod mcp;
mod memory;
mod mitigations;
mod notify;
mod prfix;
mod reasoner;
mod repos;
mod rootcause;
mod server;
mod signal;
mod store;
mod tags;
mod tools;
mod triage;
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
    assigned::AssignedWatcher, github::GithubWatcher, granola::GranolaWatcher, slack::SlackWatcher,
    Watcher,
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
    investigator: Arc<rootcause::Investigator>,
    browser: Arc<browser::BrowserDriver>,
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
    let reasoners = Reasoners::from_config(&cfg.reasoner, ollama_key.clone(), Some(store.clone()));
    // Use Ollama embeddings whenever any reasoning already runs through Ollama —
    // either ambient (local or cloud) or the on-device path used by local_only_sources.
    let embed_provider = if matches!(cfg.reasoner.local.as_str(), "ollama" | "ollama_local")
        || !cfg.reasoner.local_only_sources.is_empty()
    {
        "ollama"
    } else {
        "hash"
    };
    let embedder = embed::build(
        embed_provider,
        &cfg.reasoner.ollama_url,
        &cfg.reasoner.embed_model,
        ollama_key,
    );
    info!("embedder: {embed_provider}");

    // Grounding stores.
    let memory = Arc::new(MemoryManager::new(
        store.clone(),
        embedder.clone(),
        reasoners.routed.clone(),
        reasoners.routed.clone(),
    ));
    let context = Arc::new(ContextManager::new(
        store.clone(),
        embedder.clone(),
        reasoners.routed.clone(),
        reasoners.routed.clone(),
        cfg_context_refresh(&cfg),
    ));

    // Correlation: deterministic grouping + the LLM relation graph.
    let window =
        config::parse_duration(&cfg.correlation.window).unwrap_or(Duration::from_secs(1800));
    let correlator = Arc::new(Correlator::new(store.clone(), window));
    let analyst = Arc::new(Analyst::new(
        store.clone(),
        correlator.clone(),
        reasoners.routed.clone(),
        reasoners.local.clone(),
        memory.clone(),
        context.clone(),
        cfg.correlation.dedup_threshold,
        cfg.correlation.auto_merge,
        cfg.correlation.reopen_min_confidence,
        window,
    ));

    // Root-cause investigation: the README-derived repo index, and the issue/PR/
    // commit search over it. Crawling and shortlisting run on the local classifier;
    // only the final verdict reaches the cloud tier.
    let github_token = store.credential_get("github").unwrap_or(None);
    // One checkout cache, shared: the repo index reads code to characterize it and
    // triage reads code to analyze an issue, and they want the same working copies.
    let checkout_root = {
        let configured = std::path::PathBuf::from(&cfg.assigned.checkout_dir);
        if configured.is_absolute() {
            configured
        } else {
            data_dir.join(configured)
        }
    };
    let checkouts = Arc::new(checkout::CheckoutCache::new(
        checkout_root,
        github_token.clone(),
        cfg.assigned.max_checkout_mb,
        cfg.assigned.max_cache_mb,
    ));
    let repo_index = Arc::new(repos::RepoIndex::new(
        store.clone(),
        github_token.clone(),
        reasoners.local.clone(),
        Some(checkouts.clone()),
        cfg.investigation.clone(),
    ));
    let investigator = Arc::new(rootcause::Investigator::new(
        store.clone(),
        correlator.clone(),
        repo_index.clone(),
        github_token.clone(),
        reasoners.local.clone(),
        reasoners.routed.clone(),
        cfg.investigation.clone(),
    ));
    let browser_driver = Arc::new(browser::BrowserDriver::new(cfg.browser.clone()));

    // Assigned-issue triage: check the repo out, read the code with the local
    // coder model, propose patches, look for a PR that already fixes it, then
    // render the lot in plain English.
    let triager = Arc::new(triage::Triager::new(
        store.clone(),
        checkouts.clone(),
        github_token.clone(),
        reasoners.local.clone(),
        reasoners.brief.clone(),
        reasoners.routed.clone(),
        analyst.clone(),
        cfg.assigned.clone(),
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
        reasoners.routed.clone(),
        reasoners.routed.clone(),
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
        reasoner: reasoners.routed.clone(),
        config: cfg.clone(),
        investigator: investigator.clone(),
        repos: repo_index.clone(),
        browser: browser_driver.clone(),
    });
    let chat = Arc::new(ChatAgent::new(tools.clone(), reasoners.vision.clone()));

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

    // Completion-cache upkeep: expire stale answers and hold the store to its LRU
    // ceiling, so a long-running daemon doesn't accumulate a database of replies
    // nothing will ask for again.
    if cfg.reasoner.cache.enabled {
        let store = store.clone();
        let ttl =
            config::parse_duration(&cfg.reasoner.cache.ttl).unwrap_or(Duration::from_secs(86_400));
        let max_entries = cfg.reasoner.cache.max_entries;
        tokio::spawn(async move {
            loop {
                match store.prune_completions(ttl, max_entries) {
                    Ok(n) if n > 0 => debug!("cache: pruned {n} completion(s)"),
                    Ok(_) => {}
                    Err(e) => warn!("cache: prune failed: {e:#}"),
                }
                if let Ok((entries, hits)) = store.completion_cache_stats() {
                    if hits > 0 {
                        info!("cache: {entries} answer(s) stored, {hits} reuse(s) so far");
                    }
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }

    // Repo index: crawl the org's READMEs at startup so routing has something to
    // work with on the first incident, then refresh on a slow interval.
    // Conditional requests make a no-change refresh cheap and model-free.
    if cfg.investigation.enabled {
        let repos = repo_index.clone();
        let interval = config::parse_duration(&cfg.investigation.refresh_interval)
            .unwrap_or(Duration::from_secs(86_400));
        let org = cfg.investigation.org.clone();
        tokio::spawn(async move {
            if !repos.online() {
                warn!(
                    "investigation: no GitHub token stored — the repo index stays empty and \
                     root-cause routing falls back to the configured default repos"
                );
                return;
            }
            loop {
                match repos.sync().await {
                    Ok(n) => info!("repo index: {org} synced ({n} summary/summaries refreshed)"),
                    Err(e) => warn!("repo index: sync failed: {e:#}"),
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // Browser worker: drains the dashboard-investigation queue one job at a time
    // (they share a single Chrome), then re-analyzes the thread with the findings
    // and kicks off root-cause investigation now that the numbers are in hand.
    if browser_driver.enabled() {
        let worker = Arc::new(browser::BrowserWorker::new(
            store.clone(),
            browser_driver.clone(),
            {
                let svc_analyst = analyst.clone();
                let svc_investigator = investigator.clone();
                let svc_correlator = correlator.clone();
                let events = events.clone();
                Arc::new(move |inv: store::BrowserInvestigation| {
                    let Some(thread_id) = inv.thread_id.clone() else {
                        return;
                    };
                    let analyst = svc_analyst.clone();
                    let investigator = svc_investigator.clone();
                    let correlator = svc_correlator.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        if let Err(e) = analyst.reanalyze(&thread_id).await {
                            warn!("browser findings reanalyze {thread_id} failed: {e:#}");
                        }
                        if investigator.enabled() {
                            if let Err(e) = investigator.investigate(&thread_id).await {
                                debug!("root-cause after browser findings: {e:#}");
                            }
                        }
                        if let Ok(views) = correlator.thread_views(true) {
                            let _ = events.send(Event::Board(views));
                        }
                    });
                })
            },
        ));
        tokio::spawn(worker.run());
    } else {
        debug!("browser control disabled ([browser].enabled = false)");
    }

    // Triage worker: one assigned issue at a time. A clone plus several passes of
    // a 33b local model is minutes of work, so it never runs on the poll path.
    if triager.enabled() {
        let worker = Arc::new(triage::TriageWorker::new(store.clone(), triager.clone(), {
            let correlator = correlator.clone();
            let events = events.clone();
            let store = store.clone();
            Arc::new(move |t: store::IssueTriage| {
                // Re-emit the issue's signal so the open card picks up the new
                // analysis, and push the board.
                if let Some(sig) = t
                    .signal_id
                    .as_deref()
                    .and_then(|id| store.get_signal(id).ok().flatten())
                {
                    let _ = events.send(Event::Signal(sig));
                }
                if let Ok(views) = correlator.thread_views(true) {
                    let _ = events.send(Event::Board(views));
                }
            })
        }));
        tokio::spawn(worker.run());
    } else if cfg.assigned.enabled {
        warn!(
            "assigned-issue triage needs a stored GitHub credential — issues will still \
             appear on the board once one is set"
        );
    }
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
        investigator: investigator.clone(),
        browser: browser_driver.clone(),
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

    // Assignment is a standing state, not an event, so it gets its own poll —
    // an issue assigned weeks ago produces no notification but still belongs on
    // the board.
    if cfg.assigned.enabled {
        match token_for(store, "github") {
            Ok(Some(token)) => match AssignedWatcher::new(&cfg.assigned, token) {
                Ok(w) => watchers.push(Box::new(w)),
                Err(e) => error!("assigned-issues watcher init failed: {e:#}"),
            },
            Ok(None) => warn!(
                "assigned issues enabled but no github token stored; skipping. \
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

/// Queue a read-only browser investigation for the first dashboard link in a
/// Slack signal. The worker picks it up and drives the operator's signed-in
/// Chrome; queueing here keeps the poll path non-blocking, since one browser
/// session can take minutes.
fn queue_dashboard_investigation(
    sig: &mut signal::Signal,
    store: &Store,
    driver: &browser::BrowserDriver,
) {
    if sig.source != signal::Source::Slack || !driver.enabled() {
        return;
    }
    let Some(url) = rootcause::dashboard_links(sig, |u| driver.matches(u)).map(str::to_string)
    else {
        return;
    };
    let context = sig.body.as_deref().unwrap_or(&sig.title).to_string();
    let brief = driver.brief(&url, &context);
    match store.queue_browser_investigation(&sig.id, &url, &brief) {
        Ok(investigation) => {
            if let Some(raw) = sig.raw.as_object_mut() {
                raw.insert(
                    "browser_investigation_id".into(),
                    serde_json::json!(investigation.id),
                );
            }
            debug!(
                "queued browser investigation {} for {url}",
                investigation.id
            );
        }
        Err(e) => warn!(
            "queueing browser investigation for {} failed: {e:#}",
            sig.id
        ),
    }
}

/// Queue triage for a signal that represents an issue assigned to the user.
///
/// The watcher re-emits every assigned issue on every poll, so this is called
/// repeatedly for the same issue; [`Store::queue_issue_triage`] is what decides
/// whether there's anything to redo (it won't re-run a completed analysis).
fn queue_issue_triage(sig: &signal::Signal, store: &Store) {
    if !sig
        .raw
        .get("assigned_issue")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return;
    }
    let (Some(key), Some(repo), Some(number)) = (
        sig.raw.get("issue_key").and_then(|v| v.as_str()),
        sig.raw.get("repo").and_then(|v| v.as_str()),
        sig.raw.get("number").and_then(|v| v.as_u64()),
    ) else {
        return;
    };
    match store.queue_issue_triage(
        key,
        repo,
        number as i64,
        &sig.title,
        sig.url.as_deref(),
        &sig.id,
    ) {
        Ok(true) => debug!("queued triage for {key}"),
        Ok(false) => {}
        Err(e) => warn!("queueing triage for {key} failed: {e:#}"),
    }
}

/// Investigate a thread's root cause, but only when there's something worth
/// investigating.
///
/// Root-cause investigation is the most expensive thing MuggleBot does — GitHub
/// search calls, a commit-log scan, several local model passes, and one cloud
/// call. Firing it on every ingested signal would rate-limit the search API and
/// spend a cloud call on "Ben mentioned you in #eng". The gate is deliberately
/// narrow: a thread earns an investigation when it looks like something actually
/// broke, and it isn't re-investigated once it has a completed report.
async fn investigate_if_worthwhile(svc: &Services, thread_id: &str) {
    let Ok(Some(view)) = svc.correlator.thread_view(thread_id) else {
        return;
    };
    // Handled work is settled — the investigator refuses these anyway, but don't
    // even ask.
    if rootcause::is_handled(view.state) {
        return;
    }
    // Something has to have gone wrong. A review request or a passing CI run has
    // no cause to find.
    let looks_broken = view.severity >= signal::Severity::Warning
        || view.signals.iter().any(|s| {
            matches!(
                s.kind,
                signal::SignalKind::Alert | signal::SignalKind::CiFailure
            )
        });
    if !looks_broken {
        return;
    }
    // Don't redo work. A failed report is worth retrying (the failure may have
    // been a rate limit); a complete one is not, until the operator asks.
    match svc.store.get_root_cause(thread_id) {
        Ok(Some(report)) if report.status != "failed" => return,
        Ok(_) => {}
        Err(e) => {
            warn!("root-cause lookup for {thread_id} failed: {e:#}");
            return;
        }
    }
    match svc.investigator.investigate(thread_id).await {
        Ok(report) => {
            let found = report.candidates.as_array().map(Vec::len).unwrap_or(0);
            info!(
                "investigation {thread_id}: {} with {found} candidate(s)",
                report.status
            );
            // Re-summarize so the board reflects the new evidence, cited
            // `[cause:REF]`. This can't recurse: the completed report above makes
            // the next `investigate_if_worthwhile` a no-op.
            if found > 0 {
                if let Err(e) = svc.analyst.reanalyze(thread_id).await {
                    warn!("resummarize after investigating {thread_id} failed: {e:#}");
                }
            }
        }
        Err(e) => debug!("investigation {thread_id} skipped: {e:#}"),
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
                // Threads that were already handled but received new activity. These
                // are triaged on the LOCAL classifier only — see `triage_handled`.
                let mut handled_to_triage: Vec<(String, signal::Signal)> = Vec::new();
                for sig in batch.signals {
                    let mut sig = sig;
                    // Enrich a linked-out Slack message with a one-paragraph
                    // summary of the (public) page it points at, before storing.
                    enrich_slack_links(&mut sig, &svc.context).await;
                    queue_dashboard_investigation(&mut sig, &svc.store, &svc.browser);
                    // Queue triage for an assigned issue. This runs before the
                    // insert-dedup check on purpose: the signal is re-emitted every
                    // poll and only inserts once, but a triage that previously
                    // failed (or whose code has moved on) still deserves another
                    // run, and `queue_issue_triage` decides that for itself.
                    queue_issue_triage(&sig, &svc.store);
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
                            let _ = svc.events.send(Event::Signal(sig.clone()));
                            if let Some(tid) = thread_id {
                                if engaged {
                                    svc.live.on_activity(&tid);
                                }
                                // A handled thread stays out of the cloud analysis
                                // path entirely. New activity on it is instead
                                // matched locally to decide whether it should come
                                // back — so a snoozed issue that genuinely recurs
                                // isn't lost, without paying to re-reason settled
                                // work.
                                let handled = svc
                                    .correlator
                                    .thread_view(&tid)
                                    .ok()
                                    .flatten()
                                    .is_some_and(|v| rootcause::is_handled(v.state));
                                if handled {
                                    handled_to_triage.push((tid, sig));
                                } else {
                                    touched.insert(tid);
                                }
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
                            // Two watchers write GitHub signals and each is
                            // authoritative only for its own half — reconciling
                            // with the wrong listing would resolve the other's
                            // cards wholesale.
                            let reconciled = if name == "github-assigned" {
                                svc.store
                                    .resolve_missing_assigned_issues(&snapshot.active_ids)
                            } else {
                                svc.store
                                    .resolve_missing_github_notifications(&snapshot.active_ids)
                            };
                            match reconciled {
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
                        for tid in &touched {
                            if let Err(e) = svc2.analyst.reanalyze(tid).await {
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
                        // Then go looking for the cause. This runs after the merge
                        // dust settles, so an investigation isn't wasted on a thread
                        // that's about to be collapsed into another.
                        if svc2.investigator.enabled() {
                            for tid in &touched {
                                investigate_if_worthwhile(&svc2, tid).await;
                            }
                            if let Ok(views) = svc2.correlator.thread_views(true) {
                                let _ = svc2.events.send(Event::Board(views));
                            }
                        }
                    });
                }
                // Handled threads: local-only triage, off the poll path.
                if !handled_to_triage.is_empty() {
                    let svc2 = svc.clone();
                    tokio::spawn(async move {
                        let mut reopened = false;
                        for (tid, sig) in handled_to_triage {
                            match svc2.analyst.triage_handled(&tid, &sig).await {
                                Ok(true) => {
                                    reopened = true;
                                    // Now that it's live again, it earns the normal
                                    // treatment — including cloud analysis.
                                    if let Err(e) = svc2.analyst.reanalyze(&tid).await {
                                        warn!("reanalyze reopened {tid} failed: {e:#}");
                                    }
                                    svc2.notifier.clear_notified(&tid);
                                    svc2.notifier.maybe_notify_thread(&tid, &sig);
                                }
                                Ok(false) => {}
                                Err(e) => warn!("reopen triage {tid} failed: {e:#}"),
                            }
                        }
                        if reopened {
                            if let Ok(views) = svc2.correlator.thread_views(true) {
                                let _ = svc2.events.send(Event::Board(views));
                            }
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
