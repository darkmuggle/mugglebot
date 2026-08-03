//! Submitting work *into* Restate from the daemon.
//!
//! The watchers, the UI, and the MCP tools all live in the daemon rather than inside
//! a handler, so they reach the objects and workflows through the ingress. Two things
//! this module exists to get right:
//!
//! **The idempotency key.** Every ingest carries `{source}:{external_id}:{version}`.
//! Restate dedups the *invocation*: a re-poll that re-sees the same notification, a
//! watcher restart replaying its cursor, or a retry after a half-finished ingest
//! resolves to the original invocation instead of a second one — and an in-flight
//! duplicate attaches to the running one rather than racing it. That is a guarantee
//! a unique index cannot give, because the index catches the duplicate row *after*
//! the side effects have run twice.
//!
//! **Ids, not bodies.** Payloads are signal ids and subject keys. A 200KB raw
//! notification passed through the ingress is 200KB in the invocation journal,
//! replayed on every retry — so the body goes to SQLite first and the handler reads
//! it back.

use anyhow::{bail, Context as _, Result};
use serde::Serialize;
use tracing::debug;

use crate::config::RestateConfig;

pub struct Ingress {
    client: reqwest::Client,
    base: String,
    /// The admin API, for invocation introspection.
    admin: String,
}

impl Ingress {
    pub fn new(cfg: &RestateConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: cfg.ingress.trim_end_matches('/').to_string(),
            admin: cfg.admin.trim_end_matches('/').to_string(),
        }
    }

    /// Fire-and-forget a handler on a virtual object.
    ///
    /// `idempotency_key` is what makes ingest exactly-once. Passing `None` means "no
    /// dedup" — the ingress assigns a random key — which is right for an explicit
    /// operator action ("re-analyze this now") and wrong for anything a poll loop
    /// can re-emit.
    pub async fn send_object(
        &self,
        object: &str,
        key: &str,
        handler: &str,
        idempotency_key: Option<&str>,
        payload: &impl Serialize,
    ) -> Result<String> {
        let url = format!("{}/{object}/{}/{handler}/send", self.base, urlencoding(key));
        let mut req = self.client.post(&url).json(payload);
        if let Some(k) = idempotency_key {
            req = req.header("idempotency-key", k);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("submitting {object}/{key}/{handler}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("ingress {object}/{handler} returned {status}: {body}");
        }
        debug!("ingress: sent {object}/{key}/{handler}");
        Ok(body)
    }

    /// Call a handler and wait for its result.
    ///
    /// The awaited counterpart of [`Self::send_object`], for *reading* an object: a shared
    /// handler answering from state is a single ingress round trip, which is what makes
    /// "the object owns this fact" cheap enough to put on a click path. The alternative —
    /// the admin SQL `state` table — is a Datafusion scan, right for a board-wide sweep and
    /// wrong for one key.
    ///
    /// No idempotency key: a read has nothing to dedup.
    ///
    /// **Sends no body**, so this only addresses handlers that take no input — which the
    /// read handlers do, because everything they need is the object key. Sending `null` with
    /// a JSON content-type is not equivalent to sending nothing: the ingress answers
    /// `input validation error: Expected body and content-type to be empty`, which arrives
    /// as a failed call and reads as "there is nothing stored". [`Self::submit_workflow`]
    /// carries the same warning for the same reason.
    pub async fn call_object(&self, object: &str, key: &str, handler: &str) -> Result<String> {
        let url = format!("{}/{object}/{}/{handler}", self.base, urlencoding(key));
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("calling {object}/{key}/{handler}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("ingress {object}/{handler} returned {status}: {body}");
        }
        Ok(body)
    }

    /// Submit a workflow, returning `false` when Restate refuses because this key
    /// already ran.
    ///
    /// That refusal is the feature: `IssueTriage` keyed `{issue}@{sha}` means
    /// re-triaging an issue whose code hasn't moved costs nothing and returns the
    /// previous analysis. The caller decides whether that's a success ("already
    /// done") or a reason to bump an explicit redo suffix.
    /// The workflows take no input: everything they need is in the key. Sending a
    /// body — even `null` with a JSON content-type — is rejected by the ingress as an
    /// input validation error, so this deliberately sends none.
    pub async fn submit_workflow(
        &self,
        workflow: &str,
        key: &str,
        scope: Option<&str>,
    ) -> Result<bool> {
        // A scope is carried by the *path*, not a header: matching invocations are
        // held in that scope's virtual queue until a slot frees. Without one the
        // invocation is unscoped and no concurrency limit applies to it, which is the
        // failure mode that looks like "why is the laptop on fire".
        let url = match scope {
            Some(scope) => format!(
                "{}/restate/scope/{scope}/send/{workflow}/{}/run",
                self.base,
                urlencoding(key)
            ),
            None => format!("{}/{workflow}/{}/run/send", self.base, urlencoding(key)),
        };
        let resp = match self.client.post(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                // The ingress being unreachable is the one failure the operator most needs
                // to see and the one least likely to reach them: it happens before any
                // handler exists to report it.
                crate::dispatch::failed(workflow, key, format!("submitting: {e}"));
                return Err(e).with_context(|| format!("submitting {workflow}/{key}"));
            }
        };
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // A key collision is *not* an HTTP error: the ingress answers 200 with
        // `{"status":"PreviouslyAccepted"}`. Reading it out of the body rather than
        // off the status code is the difference between "this was free" and thinking
        // every redundant submission started real work.
        let already_ran =
            body.contains("PreviouslyAccepted") || status == reqwest::StatusCode::CONFLICT;
        if already_ran {
            debug!("ingress: {workflow}/{key} already ran; nothing to redo");
            crate::dispatch::duplicate(workflow, key, "already run at this key — nothing to redo");
            return Ok(false);
        }
        if status.is_success() {
            debug!("ingress: submitted {workflow}/{key}");
            // Queued, not running: with a scope the invocation waits for a vqueue slot,
            // and that wait is exactly the interval the operator used to spend wondering
            // whether the button did anything.
            crate::dispatch::queued(workflow, key);
            return Ok(true);
        }
        crate::dispatch::failed(workflow, key, format!("ingress returned {status}: {body}"));
        bail!("ingress {workflow}/{key} returned {status}: {body}");
    }

    /// Arm a watcher's poll loop. Returns whether this call armed it, or found it
    /// already running — the loop's timer is durable, so re-arming on every restart
    /// would multiply the poll rate.
    ///
    /// A blocking call rather than a send: "did the loop start?" is exactly the thing
    /// the operator needs to know at boot, and a send would report success for a
    /// watcher that immediately failed to resolve.
    pub async fn start_watcher(&self, name: &str) -> Result<bool> {
        let url = format!("{}/Watcher/{}/start", self.base, urlencoding(name));
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("arming watcher '{name}'"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("arming watcher '{name}' returned {status}: {body}");
        }
        Ok(body.trim() == "true")
    }

    /// Answer a pending human gate on a blocked invocation.
    ///
    /// Approval resolves the durable promise the handler is awaiting. Rejection resolves
    /// it as `false`, which the handler turns into a `TerminalError` carrying the reason
    /// — a rejected action must not be retried, and Restate retries anything that isn't
    /// terminal.
    pub async fn resolve_gate(
        &self,
        invocation_id: &str,
        approve: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        let url = format!(
            "{}/restate/invocation/{}/promise/{}/resolve",
            self.base,
            urlencoding(invocation_id),
            crate::restate::gate::APPROVED
        );
        let resp = self
            .client
            .post(&url)
            .json(&approve)
            .send()
            .await
            .with_context(|| format!("resolving the gate on {invocation_id}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "resolving the gate on {invocation_id} returned {status}: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        tracing::info!(
            "gate on {invocation_id}: {}{}",
            if approve { "approved" } else { "declined" },
            reason.map(|r| format!(" ({r})")).unwrap_or_default()
        );
        Ok(())
    }

    /// Arm one repo's indexing loop. Same idempotence as the other loops.
    /// Run one repo's indexer tick **now**, rather than waiting for its timer.
    ///
    /// `start` is the wrong verb for this: it is idempotent-by-staleness and deliberately
    /// refuses when a timer is already armed, which is every repo in steady state — so the
    /// push sweep would poke and nothing would happen. Sent rather than called, so the sweep
    /// does not wait on a fetch-and-summarize it only needed to trigger.
    ///
    /// Targets `poke`, **not** `tick`. `tick` arms the next timer as its first act, so calling
    /// it out of band forks the loop — the poked tick schedules a successor alongside the chain
    /// already running, and every later poke adds another. `poke` does the same work and leaves
    /// the timer alone, which is also the right meaning: a push is a reason to index now, not a
    /// reason to index more often from now on.
    pub async fn poke_repo_indexer(&self, repo: &str) -> Result<()> {
        let url = format!("{}/RepoIndexer/{}/poke/send", self.base, urlencoding(repo));
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("poking the indexer for {repo}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("poking the indexer for {repo} returned {status}: {body}");
        }
        Ok(())
    }

    pub async fn start_repo_indexer(&self, repo: &str) -> Result<bool> {
        let url = format!("{}/RepoIndexer/{}/start", self.base, urlencoding(repo));
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("arming the indexer for {repo}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("arming the indexer for {repo} returned {status}: {body}");
        }
        Ok(body.trim() == "true")
    }

    /// Query Restate's invocation introspection.
    ///
    /// Read through the admin SQL endpoint rather than kept in our own table: the
    /// server is the authority on what is running, and a mirror would be a second
    /// thing to get out of date.
    pub async fn invocations(&self, subject: Option<&str>) -> Result<serde_json::Value> {
        let filter = match subject {
            // The subject key is part of the target string (`Issue/o/r#412/record`),
            // so a LIKE over the target is the whole filter. Escaped, because a key
            // legitimately contains `_` and `%` would otherwise be a wildcard.
            Some(s) => format!(
                " WHERE target LIKE '%{}%'",
                s.replace('\'', "''").replace('%', "").replace('_', "\\_")
            ),
            None => String::new(),
        };
        let query = format!(
            "SELECT target, status, scope, completion_result, completion_failure, \
             created_at, completed_at FROM sys_invocation{filter} \
             ORDER BY created_at DESC LIMIT 50"
        );
        let url = format!("{}/query", self.admin.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("accept", "application/json")
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await
            .context("querying Restate invocations")?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "invocation query returned {status}: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        Ok(resp.json().await.unwrap_or(serde_json::json!([])))
    }

    /// Arm a recurring scheduler task. Same idempotence as [`Self::start_watcher`].
    pub async fn start_scheduler(&self, task: &str) -> Result<bool> {
        let url = format!("{}/Scheduler/{}/start", self.base, urlencoding(task));
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("arming scheduler '{task}'"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("arming scheduler '{task}' returned {status}: {body}");
        }
        Ok(body.trim() == "true")
    }
}

/// Percent-encode a subject key for a URL path segment.
///
/// Subject keys contain `/` (`owner/repo#412`, `channel/ts`), which would otherwise
/// split the path and address a handler that doesn't exist.
fn urlencoding(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_keys_survive_the_url_path() {
        // The `/` and `#` in a subject key would otherwise split the path or start a
        // fragment, addressing a handler that doesn't exist.
        assert_eq!(
            urlencoding("restatedev/restate#412"),
            "restatedev%2Frestate%23412"
        );
        assert_eq!(
            urlencoding("C02ABC/1721822400.001"),
            "C02ABC%2F1721822400.001"
        );
        assert_eq!(urlencoding("o/r!987"), "o%2Fr%21987");
    }
}
