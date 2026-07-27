//! Per-signal enrichment, applied before a signal is stored.
//!
//! Each of these runs on the ingest path and must never block it: an unreachable
//! link, a dashboard that won't load, or a triage queue that's full are all
//! "carry on without it" rather than "fail the poll". Extracted from the old
//! `poll_loop`, where they were interleaved with insertion and attribution and so
//! could not be retried independently.

use tracing::{debug, warn};

use crate::browser::BrowserDriver;
use crate::context::ContextManager;
use crate::signal;
use crate::store::Store;

/// If a Slack signal links to a public page, fetch and summarize it into
/// `raw.link_summary` (with `raw.link_url`). First accessible URL wins; any
/// error (non-public, unreachable, no text) is logged and skipped so enrichment
/// never blocks ingest. No-op for non-Slack signals.
pub async fn slack_links(sig: &mut signal::Signal, context: &ContextManager) {
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
pub fn queue_dashboard_investigation(
    sig: &mut signal::Signal,
    store: &Store,
    driver: &BrowserDriver,
) {
    if sig.source != signal::Source::Slack || !driver.enabled() {
        return;
    }
    let Some(url) =
        crate::rootcause::dashboard_links(sig, |u| driver.matches(u)).map(str::to_string)
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
pub fn queue_issue_triage(sig: &signal::Signal, store: &Store) {
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
