//! MuggleBot — a single-pane-of-glass ops-awareness agent.
//!
//! The daemon wires the whole spine together: load config, open the SQLite store,
//! build the reasoners/embedder and the correlation, grounding, live-assist, and
//! chat subsystems, run one poll loop per enabled watcher (GitHub, Slack,
//! Granola), and serve the web UI (HTTP + WebSocket) and the MCP endpoint
//! (stdio + HTTP). New signals are deduped, persisted, correlated into subjects,
//! reasoned over, notified, and streamed live to the UI.

#![allow(dead_code)]

mod agent;
mod browser;
mod chat;
mod checkout;
mod codeindex;
mod comments;
mod components;
mod config;
mod context;
mod correlation;
mod crossref;
mod dispatch;
mod ecosystem;
mod embed;
mod enrich;
mod event;
mod github;
mod live;
mod live_engine;
mod mcp;
mod memory;
mod notify;
mod prdiff;
mod prfix;
mod reasoner;
mod repos;
mod restate;
mod rootcause;
mod score;
mod secrets;
mod server;
mod signal;
mod store;
mod subject;
mod tags;
mod tools;
mod triage;
mod watchers;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::chat::ChatAgent;
use crate::config::Config;
use crate::context::ContextManager;
use crate::correlation::Analyst;
use crate::event::Event;
use crate::live_engine::LiveEngine;
use crate::mcp::McpServer;
use crate::memory::MemoryManager;
use crate::notify::Notifier;
use crate::reasoner::Reasoners;
use crate::server::AppState;
use crate::store::Store;
use crate::subject::Attributor;
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

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so stdout stays clean for the MCP stdio transport, through
    // a writer that rewrites known secret values out of the stream. Field-level
    // redaction only protects the call sites that remember to ask for it; the
    // failure we care about is a `{:?}` nobody thought about.
    let scrubber = secrets::Scrubber::new();
    tracing_subscriber::fmt()
        .with_writer(scrubber.clone())
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

    // The credential store. Read at point of use, never cached at boot, so a token
    // rotated through the config page takes effect on the next poll.
    let secrets = Arc::new(
        secrets::Secrets::open(
            store.clone(),
            cfg.secrets.encrypt,
            std::env::var("MUGGLEBOT_MASTER_KEY").ok(),
            scrubber,
        )
        .context("opening the credential store")?,
    );

    // Reasoners + embedder. Reasoning rides the subscription CLI bridge (no API
    // keys); Ollama's optional key is stored in the database.
    let ollama_key = secrets.get_opt("ollama");
    let reasoners = Reasoners::from_config(&cfg.reasoner, ollama_key.clone(), Some(store.clone()));
    // Stated at boot, because "is this thing spending money?" should be answerable from the
    // log rather than by reading the config and then the wiring. `routing` is the only
    // setting that lets an automatic pass escalate, so it is the one worth naming.
    info!(
        "models: everything on {} ({}); {} reachable only when you ask for it \
         (chat picker, 2nd opinion){}",
        cfg.reasoner.local_model,
        cfg.reasoner.local,
        cfg.reasoner.cloud_model,
        if cfg.reasoner.routing.enabled {
            " — WARNING: [reasoner.routing] is on, so hard tasks escalate on their own"
        } else {
            ""
        }
    );
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
    // Tagging: the first pass is routed (local unless the operator opts into escalation),
    // the refining pass is pinned local — a second read of the same document is worth
    // having and shouldn't be the thing that turns on a meter.
    let memory = Arc::new(MemoryManager::new(
        store.clone(),
        embedder.clone(),
        reasoners.routed.clone(),
        reasoners.local.clone(),
    ));
    let context = Arc::new(ContextManager::new(
        store.clone(),
        secrets.clone(),
        embedder.clone(),
        reasoners.routed.clone(),
        reasoners.local.clone(),
        cfg_context_refresh(&cfg),
    ));

    // Correlation: deterministic grouping + the LLM relation graph.
    let window =
        config::parse_duration(&cfg.correlation.window).unwrap_or(Duration::from_secs(1800));
    let attributor = Arc::new(Attributor::new(store.clone()));
    // Subjects and signals as virtual object state, with an in-process read model over it.
    // Constructed here and refreshed on a timer; the readers move onto it one at a time, and
    // the `subjects`/`signals` tables come out once the last of them has.
    // Characterize any repo the index already holds but has never classified. Idempotent and
    // never overwrites a human's tag, so running it every boot is what keeps it self-healing.
    match store.backfill_repo_kinds() {
        Ok(0) => {}
        Ok(n) => info!("repo index: characterized {n} repo(s) as example/docs from their names"),
        Err(e) => warn!("repo kind backfill failed: {e:#}"),
    }
    let subject_store = Arc::new(subject::store::SubjectStore::new(
        &cfg.restate,
        Arc::new(restate::ingress::Ingress::new(&cfg.restate)),
    ));
    let analyst = Arc::new(Analyst::new(
        store.clone(),
        attributor.clone(),
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
    let github_token = secrets.get_opt("github");
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
        // Bulk: characterizing 147 repos is the other thing that would sit in front of the board.
        reasoners.local_background.clone(),
        Some(checkouts.clone()),
        cfg.investigation.clone(),
    ));
    let investigator = Arc::new(rootcause::Investigator::new(
        store.clone(),
        attributor.clone(),
        repo_index.clone(),
        github_token.clone(),
        reasoners.local.clone(),
        // The ranking pass reads a shortlist, not a repository — pinned local.
        reasoners.local.clone(),
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
        reasoners.local.clone(),
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
    // The dispatch registry pushes on the same bus. Installed here rather than passed
    // around because the two halves of a dispatch — the tool that submits it and the
    // workflow handler that runs it — share a process and nothing else.
    dispatch::install(events.clone());

    // Live assist.
    let live = Arc::new(LiveEngine::new(
        store.clone(),
        attributor.clone(),
        reasoners.local.clone(),
        memory.clone(),
        context.clone(),
        notifier.clone(),
        events.clone(),
        cfg.live.red_alert,
        cfg.live.red_alert_min_confidence,
    ));

    // The shared tool surface (used by both the web API and MCP) + chat.
    // Restate is the substrate, not an option: the poll loops, the debounce and the
    // expensive pipelines are all handlers now, so there is no second execution model
    // to fall back to. A daemon that can't reach the ingress ingests nothing, which is
    // visible immediately rather than silently degraded.
    let tools_ingress = Arc::new(restate::ingress::Ingress::new(&cfg.restate));
    info!(
        "restate: ingest, analysis and workflows route through {}",
        cfg.restate.ingress
    );

    // The code index: components, commit summaries, and the dependency graph. All local
    // model work, in the `local-llm` scope, so a one-time eager index costs time rather
    // than money.
    let code_indexer = Arc::new(codeindex::CodeIndexer {
        store: store.clone(),
        checkouts: checkouts.clone(),
        github: github_token
            .as_ref()
            // Background: the indexer is the caller that produced the 403, and its lateness
            // costs far less than a watcher that stops noticing incidents.
            .and_then(|t| github::GithubClient::new(t.clone()).ok())
            .map(github::GithubClient::background),
        // Bulk: carding a component must not take the worker reserved for a notification.
        coder: reasoners.local_background.clone(),
        embedder: embedder.clone(),
    });
    // Agent sessions: a coding CLI running inside a checkout, streamed to the board.
    let agent_sessions = Arc::new(agent::AgentSessions::new(
        events.clone(),
        checkouts.clone(),
        github_token
            .as_ref()
            .and_then(|t| github::GithubClient::new(t.clone()).ok()),
    ));
    let scorer = Arc::new(score::Scorer {
        store: store.clone(),
        embedder: embedder.clone(),
    });

    // One diff reader, shared by the tool surface (the inline first read) and the `PrDiff`
    // workflow (the background warm). Local reasoner by policy: reading a diff is exactly
    // the work that shouldn't leave the machine.
    let diff_reader = Arc::new(
        prdiff::DiffReader::new(secrets.get_opt("github"), reasoners.local.clone())
            .context("building the diff reader")?,
    );
    let tools = Arc::new(Tools {
        agents: agent_sessions.clone(),
        store: store.clone(),
        ingress: tools_ingress,
        scorer: scorer.clone(),
        secrets: secrets.clone(),
        attributor: attributor.clone(),
        analyst: analyst.clone(),
        memory: memory.clone(),
        context: context.clone(),
        reasoner: reasoners.routed.clone(),
        config: cfg.clone(),
        investigator: investigator.clone(),
        repos: repo_index.clone(),
        browser: browser_driver.clone(),
        diffs: diff_reader.clone(),
    });
    let chat = Arc::new(ChatAgent::new(
        tools.clone(),
        reasoners.local.clone(),
        reasoners.vision.clone(),
    ));

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

    // ---- Restate: the execution substrate -----------------------------------
    //
    // Everything that used to be a `tokio` loop is now a handler behind this
    // endpoint: the poll loops are `Watcher` objects with durable timers, activity
    // lands on a subject object whose exclusive handler serializes it, and the
    // expensive pipelines are workflows keyed so a redundant run is free.
    let watchers = build_watchers(&cfg, &secrets);
    if watchers.is_empty() {
        warn!("no active watchers — running with the web UI + MCP only");
    }
    let ingress = Arc::new(restate::ingress::Ingress::new(&cfg.restate));
    let ingest_ops = Arc::new(restate::pipeline::IngestOps {
        store: store.clone(),
        attributor: attributor.clone(),
        analyst: analyst.clone(),
        context: context.clone(),
        browser: browser_driver.clone(),
        events: events.clone(),
        watchers: watchers.clone(),
        ingress: ingress.clone(),
        org: cfg.investigation.org.clone(),
        contexts_dir: data_dir.join("contexts"),
    });
    let ops = Arc::new(restate::SubjectOps {
        store: store.clone(),
        attributor: attributor.clone(),
        analyst: analyst.clone(),
        notifier: notifier.clone(),
        events: events.clone(),
        ingress: ingress.clone(),
        pipeline: ingest_ops.clone(),
        live: live.clone(),
        debounce: restate::objects::debounce::Debounce {
            quiet: config::parse_duration(&cfg.live.debounce).unwrap_or(Duration::from_secs(60)),
            max: config::parse_duration(&cfg.live.debounce_max).unwrap_or(Duration::from_secs(300)),
        },
    });
    let wf_ops = Arc::new(restate::WorkflowOps {
        store: store.clone(),
        attributor: attributor.clone(),
        reasoner: reasoners.routed.clone(),
        explainer: reasoners.local.clone(),
        // The one automatic-path exception, and it is not automatic: SecondOpinion runs
        // only when the operator presses the button.
        cloud: reasoners.cloud.clone(),
        investigator: investigator.clone(),
        triager: triager.clone(),
        analyst: analyst.clone(),
        repos: repo_index.clone(),
        browser: browser_driver.clone(),
        context: context.clone(),
        diffs: diff_reader.clone(),
    });
    {
        // Claim the endpoint port before anything else, and make a collision fatal.
        //
        // The SDK's `listen_and_serve` returns `()` — it swallows the bind result — so a
        // second MuggleBot on the same port used to start up quietly, register a deployment
        // pointing at a port the *first* process owns, and then arm its watchers. Restate
        // dutifully routed those handler calls to the other process, which produced
        // "no watcher named 'github'": an error about the watcher registry, thrown by a
        // daemon that had a github watcher, because it was answered by one that didn't.
        //
        // Fatal rather than a warning: an unreachable endpoint means no ingest, no analysis
        // and no workflows, so there is nothing left worth staying up for — and registering
        // anyway actively hijacks the routing of whichever process does own the port.
        restate::claim_endpoint_port(&cfg.restate)?;
        let rcfg = cfg.restate.clone();
        let ops = ops.clone();
        let wf = wf_ops.clone();
        let ingest = ingest_ops.clone();
        let indexer = code_indexer.clone();
        tokio::spawn(async move {
            if let Err(e) = restate::serve(rcfg, ops, wf, ingest, indexer).await {
                error!("restate endpoint ended: {e:#}");
            }
        });
    }

    {
        // Rebuild the read model from object state: once at boot, then on a timer.
        //
        // The timer is what makes the model a cache rather than a second truth — an
        // out-of-band write, a lost durable send, or a restart all converge here rather than
        // needing reconciliation. Started after a beat so the deployment is registered and the
        // admin API is answering.
        let subjects = subject_store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            loop {
                match subjects.refresh().await {
                    Ok(n) => debug!("subject model: {n} subject(s) from object state"),
                    // Not fatal: SQLite still backs the board until the readers move over, and
                    // a Restate that is not up yet is the normal case at boot under Tilt.
                    Err(e) => debug!("subject model refresh: {e:#}"),
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }

    // Register, apply the concurrency rule book, then arm each watcher's poll loop.
    // Ordered, and after a beat: the container and this binary start concurrently
    // under Tilt, and arming a loop before the deployment is registered would call a
    // handler Restate doesn't know about yet.
    {
        let rcfg = cfg.restate.clone();
        let names: Vec<String> = watchers.iter().map(|w| w.name().to_string()).collect();
        let mut tasks: Vec<String> = vec![
            restate::objects::scheduler::CONTEXT_REFRESH.into(),
            restate::objects::scheduler::CONTEXTS_DIR.into(),
        ];
        if cfg.investigation.enabled {
            tasks.push(restate::objects::scheduler::REPO_INDEX.into());
            if code_indexer.enabled() {
                tasks.push(restate::objects::scheduler::CODE_INDEX.into());
            } else {
                warn!(
                    "code indexing needs a stored GitHub token and `git` on PATH — issue \
                     scoring will fall back to whatever the repo index already holds"
                );
            }
        }
        if browser_driver.enabled() {
            tasks.push(restate::objects::scheduler::BROWSER_QUEUE.into());
        } else {
            debug!("browser control disabled ([browser].enabled = false)");
        }
        if triager.enabled() {
            tasks.push(restate::objects::scheduler::TRIAGE_QUEUE.into());
        } else if cfg.assigned.enabled {
            warn!(
                "assigned-issue triage needs a stored GitHub credential — issues will still \
                 appear on the board once one is set"
            );
        }
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if rcfg.register_on_boot {
                let _ = restate::register(&rcfg).await;
            }
            let _ = restate::scopes::apply_rules(&rcfg).await;
            let boot = restate::ingress::Ingress::new(&rcfg);
            for name in names {
                match boot.start_watcher(&name).await {
                    Ok(true) => info!("watcher '{name}': poll loop armed"),
                    // Already armed by a previous process — the timer is durable, so
                    // arming again would multiply the poll rate by the number of
                    // restarts.
                    Ok(false) => debug!("watcher '{name}': loop already running"),
                    Err(e) => error!("watcher '{name}': arming the poll loop failed: {e:#}"),
                }
            }
            // Recurring work — the repo index, context refresh, the managed contexts
            // tree, and the two queues — on the same durable-timer footing.
            for task in tasks {
                match boot.start_scheduler(&task).await {
                    Ok(true) => info!("scheduler '{task}': armed"),
                    Ok(false) => debug!("scheduler '{task}': already running"),
                    Err(e) => error!("scheduler '{task}': arming failed: {e:#}"),
                }
            }
        });
    }

    tokio::signal::ctrl_c()
        .await
        .context("waiting for shutdown signal")?;
    info!("shutting down");
    Ok(())
}

fn cfg_context_refresh(_cfg: &Config) -> String {
    // Context refresh cadence isn't a first-class config field yet; the library
    // default matches the design's `[context].refresh_default`.
    "6h".into()
}

fn build_watchers(cfg: &Config, secrets: &secrets::Secrets) -> Vec<Arc<dyn Watcher>> {
    let mut watchers: Vec<Arc<dyn Watcher>> = Vec::new();

    if cfg.sources.github.enabled {
        match secrets.get("github") {
            Ok(Some(token)) => match GithubWatcher::new(&cfg.sources.github, token) {
                Ok(w) => watchers.push(Arc::new(w)),
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
        match secrets.get("github") {
            Ok(Some(token)) => match AssignedWatcher::new(&cfg.assigned, token) {
                Ok(w) => watchers.push(Arc::new(w)),
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
        match secrets.get("slack") {
            Ok(Some(token)) => match SlackWatcher::new(&cfg.sources.slack, token) {
                Ok(w) => watchers.push(Arc::new(w)),
                Err(e) => error!("slack watcher init failed: {e:#}"),
            },
            Ok(None) => warn!("slack enabled but no token stored (account 'slack'); skipping"),
            Err(e) => error!("slack credential read failed: {e:#}"),
        }
    }

    if cfg.sources.granola.enabled {
        match secrets.get("granola") {
            Ok(Some(token)) => match GranolaWatcher::new(&cfg.sources.granola, token) {
                Ok(w) => watchers.push(Arc::new(w)),
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
