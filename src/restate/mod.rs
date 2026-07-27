//! The Restate service endpoint: virtual objects and (from Phase 4) workflows.
//!
//! MuggleBot serves this endpoint from the same binary as everything else, on its
//! own port. A local `restate-server` container calls into it; the daemon keeps the
//! long-lived connections (the Slack socket, the UI WebSocket, MCP stdio) because
//! those can't be poll handlers.
//!
//! **Handlers here are thin.** Each one validates, mutates object state, and calls
//! an existing free function for the real work. That is what keeps the test suite:
//! the free functions stay directly callable, so nothing needs a running server to
//! test. It is also what keeps the journal small — see [`ingress`] for why payloads
//! carry ids rather than bodies.

use anyhow::{bail, Context as _, Result};
use std::sync::Arc;
use tracing::{info, warn};

use crate::config::RestateConfig;

pub mod gate;
pub mod ingress;
pub mod objects;
pub mod ops;
pub mod pipeline;
pub mod scopes;
pub mod state;
pub mod subject_state;
pub mod workflows;

pub use ops::SubjectOps;
pub use workflows::WorkflowOps;

/// Build the endpoint, bind the objects, and serve it.
///
/// Returns once the listener stops. Failure to bind is fatal in the same way the
/// UI's listener is: a MuggleBot that can't be called by Restate would silently
/// stop ingesting.
pub async fn serve(
    cfg: RestateConfig,
    ops: Arc<SubjectOps>,
    wf: Arc<WorkflowOps>,
    ingest: Arc<pipeline::IngestOps>,
    indexer: Arc<crate::codeindex::CodeIndexer>,
) -> Result<()> {
    use restate_sdk::prelude::{Endpoint, HttpServer};

    let endpoint = Endpoint::builder()
        .bind(objects::issue::Issue::new(ops.clone()))
        .bind(objects::pull_request::PullRequest::new(ops.clone()))
        .bind(objects::slack_thread::SlackThread::new(ops.clone()))
        .bind(workflows::root_cause::RootCause::new(wf.clone()))
        .bind(workflows::issue_triage::IssueTriage::new(wf.clone()))
        .bind(workflows::rest::BrowserRead::new(wf.clone()))
        .bind(workflows::rest::PrCritique::new(wf.clone()))
        .bind(workflows::rest::PrDiff::new(wf.clone()))
        .bind(workflows::rest::RepoIndex::new(wf.clone()))
        .bind(workflows::rest::ContextIngest::new(wf.clone()))
        .bind(workflows::rest::Merge::new(wf.clone()))
        .bind(workflows::explain::Explain::new(wf.clone()))
        .bind(workflows::explain::SecondOpinion::new(wf.clone()))
        .bind(objects::watcher::Watcher::new(ingest.clone()))
        .bind(objects::scheduler::Scheduler::new(ingest.clone()))
        .bind(objects::repo_indexer::RepoIndexer::new(
            indexer.clone(),
            {
                // Resolved per tick rather than captured: the org's repo list grows, and an
                // indexer started before a repo existed must still be able to link to it.
                let store = ingest.store.clone();
                Arc::new(move || {
                    store
                        .list_repos()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| r.full_name)
                        .collect()
                })
            },
            ingest.events.clone(),
        ))
        .build();

    let addr: std::net::SocketAddr = cfg.endpoint_listen.parse().with_context(|| {
        format!(
            "parsing [restate].endpoint_listen '{}'",
            cfg.endpoint_listen
        )
    })?;
    info!("restate: serving handlers on http://{addr}");
    HttpServer::new(endpoint).listen_and_serve(addr).await;
    Ok(())
}

/// Check that this process can actually own the endpoint port, before anything registers a
/// deployment claiming that it does.
///
/// A probe rather than a handover: the SDK binds the address itself inside
/// `listen_and_serve`, so the listener opened here is dropped immediately and there is a
/// microsecond where neither holds it. That race doesn't matter for what this catches — a
/// second daemon that has held the port for minutes — and the alternative (threading a
/// pre-bound listener through the SDK) buys nothing for it.
pub fn claim_endpoint_port(cfg: &RestateConfig) -> Result<()> {
    let addr: std::net::SocketAddr = cfg.endpoint_listen.parse().with_context(|| {
        format!(
            "parsing [restate].endpoint_listen '{}'",
            cfg.endpoint_listen
        )
    })?;
    match std::net::TcpListener::bind(addr) {
        Ok(l) => {
            drop(l);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            bail!(
                "another process already holds {addr}, the Restate endpoint port. That is \
                 almost always a second MuggleBot still running — check with \
                 `lsof -nP -iTCP:{} -sTCP:LISTEN` and stop it, or set a different \
                 [restate].endpoint_listen. Starting anyway would register a deployment \
                 pointing at the other process, and every handler call would be answered by \
                 it instead of by this one.",
                addr.port()
            )
        }
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("binding the Restate endpoint port {addr}")))
        }
    }
}

/// Register this endpoint with the local Restate server.
///
/// Restate discovers handlers at registration time, so adding a handler or changing
/// a signature needs a re-register; `force` makes that idempotent. Best-effort: a
/// server that isn't up yet is a warning, not a failed boot — Tilt starts the
/// container and the binary concurrently, and the operator can re-register from the
/// Tilt UI.
pub async fn register(cfg: &RestateConfig) -> Result<()> {
    let port = cfg
        .endpoint_listen
        .rsplit(':')
        .next()
        .unwrap_or("9080")
        .to_string();
    // The server runs in a container, so it reaches the host through this alias
    // rather than through `localhost`, which would be the container itself.
    let uri = format!("http://host.docker.internal:{port}");
    let body = serde_json::json!({ "uri": uri, "force": true });
    let resp = reqwest::Client::new()
        .post(format!("{}/deployments", cfg.admin.trim_end_matches('/')))
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            info!("restate: registered deployment at {uri}");
            Ok(())
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            warn!("restate: registering {uri} failed ({status}): {text}");
            Ok(())
        }
        Err(e) => {
            warn!(
                "restate: admin API at {} unreachable ({e}); handlers are served but \
                 not registered — start the container and re-register",
                cfg.admin
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_at(listen: &str) -> RestateConfig {
        RestateConfig {
            endpoint_listen: listen.into(),
            ..Default::default()
        }
    }

    /// A second daemon on the same endpoint port must not get past boot. Left to the SDK
    /// this was silent — `listen_and_serve` discards the bind result — and the visible
    /// symptom was "no watcher named 'github'" from the *other* process, which is a error
    /// message about the wrong subsystem entirely.
    #[test]
    fn a_taken_endpoint_port_is_refused_with_the_reason() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();

        let err = claim_endpoint_port(&cfg_at(&addr.to_string()))
            .expect_err("a port someone else holds must not be claimable");
        let msg = format!("{err:#}");
        assert!(msg.contains("already holds"), "{msg}");
        // The message has to name the thing to go look at, or it is the same dead end as
        // the error it replaced.
        assert!(msg.contains(&addr.port().to_string()), "{msg}");
        assert!(msg.contains("endpoint_listen"), "{msg}");
    }

    #[test]
    fn a_free_port_is_claimable_and_stays_free_for_the_server() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        claim_endpoint_port(&cfg_at(&addr.to_string())).expect("a free port must be claimable");
        // The probe must release it — the SDK binds this address a moment later, and a
        // check that held the port would break the thing it is checking.
        std::net::TcpListener::bind(addr).expect("the probe must not keep the port");
    }

    #[test]
    fn an_unparseable_listen_address_says_which_setting_is_wrong() {
        let err = claim_endpoint_port(&cfg_at("not-an-address")).expect_err("must not parse");
        assert!(format!("{err:#}").contains("endpoint_listen"));
    }
}
