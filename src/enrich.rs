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
use crate::grafana;
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

/// Queue one dashboard investigation for a Slack alert, on the best tier available.
///
/// **Grafana first when it is configured.** The alert's links are parsed rather than
/// pattern-matched, because a Grafana notification carries three of them and they are not
/// interchangeable: the rule view holds the query and the threshold, the `/d/` link holds
/// the time range and the tenant, and `/alerting/silence/new` must never be followed at
/// all. Across 25 consecutive real alerts every message carried all three. The parsed
/// links become the investigation's prompt, so the workflow needs no second look at the
/// signal to know what to ask for.
///
/// **The browser otherwise**, on the dashboard link — and specifically not on whichever
/// URL happened to appear first. The previous "first URL containing grafana" resolved to
/// the alert *rule page* in every one of those 25 alerts, which is the rule's definition
/// rather than its graph.
///
/// Queueing rather than reading keeps the poll path non-blocking: a Grafana query takes
/// seconds and a browser session can take minutes.
pub fn queue_dashboard_investigation(
    sig: &mut signal::Signal,
    store: &Store,
    driver: &BrowserDriver,
    grafana: &grafana::Reader,
) {
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
    let links = grafana::parse_links(urls.iter().map(String::as_str), grafana.host_hint());

    let (method, url, prompt) = if grafana.ready() && links.actionable() {
        let target = links
            .dashboard_url
            .clone()
            .unwrap_or_else(|| sig.url.clone().unwrap_or_default());
        let Ok(payload) = serde_json::to_string(&links) else {
            return;
        };
        ("grafana", target, payload)
    } else if driver.enabled() {
        // Prefer the graph over the rule page, and never the silence form.
        let Some(url) = links.dashboard_url.clone().or_else(|| {
            crate::rootcause::dashboard_links(sig, |u| driver.matches(u)).map(str::to_string)
        }) else {
            return;
        };
        let context = sig.body.as_deref().unwrap_or(&sig.title).to_string();
        let brief = driver.brief(&url, &context);
        ("browser", url, brief)
    } else {
        return;
    };

    match store.queue_browser_investigation(&sig.id, &url, &prompt, method) {
        Ok(investigation) => {
            if let Some(raw) = sig.raw.as_object_mut() {
                raw.insert(
                    "browser_investigation_id".into(),
                    serde_json::json!(investigation.id),
                );
            }
            debug!(
                "queued {method} investigation {} for {url}",
                investigation.id
            );
        }
        Err(e) => warn!(
            "queueing {method} investigation for {} failed: {e:#}",
            sig.id
        ),
    }
}

/// Record which Restate Cloud ids this signal's text mentions.
///
/// Scanning only — the `rcc` lookups happen on the scheduler's sweep, not here. A signal can
/// name half a hierarchy, each id is a subprocess, and the poll path is the one place in
/// MuggleBot where a few seconds of shelling out would be paid on every message in the
/// channel. What this leaves behind is a set of ids the resolver picks up within a tick.
///
/// Idempotent, and deliberately outside the insert-dedup check for the same reason
/// [`queue_issue_triage`] is: a signal that was ingested before the operator configured
/// `rcc` should still get its ids on the next poll.
pub fn cloud_ids(sig: &signal::Signal, store: &Store) {
    let mut text = sig.title.clone();
    if let Some(body) = &sig.body {
        text.push('\n');
        text.push_str(body);
    }
    let ids = crate::cloud::scan(&text);
    if ids.is_empty() {
        return;
    }
    match store.link_signal_cloud_ids(&sig.id, &ids) {
        Ok(()) => debug!("cloud: {} mentions {}", sig.id, ids.join(", ")),
        Err(e) => warn!("recording cloud ids for {} failed: {e:#}", sig.id),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::signal::{Severity, Signal, SignalKind, Source};
    use chrono::Utc;
    use std::sync::Arc;

    /// The three links a real Grafana alert carries, in the order Grafana emits them.
    const RULE: &str = "https://restateprod.grafana.net/alerting/grafana/aeb9f2c1x/view";
    const SILENCE: &str =
        "https://restateprod.grafana.net/alerting/silence/new?alertmanager=grafana";
    const DASH: &str = "https://restateprod.grafana.net/d/abc123def/tenant-overview\
                        ?from=1754300000000&to=1754310000000&var-environment=env-9xk2";

    fn alert() -> Signal {
        Signal {
            id: Signal::make_id(Source::Slack, "alert-1", None),
            source: Source::Slack,
            external_id: "alert-1".into(),
            kind: SignalKind::Alert,
            title: "[FIRING:1] TenantStorageHigh Restate Cloud Alerts us".into(),
            body: Some("Tenant storage above 90%".into()),
            url: Some(RULE.into()),
            actor: None,
            keys: vec![],
            severity: Severity::Warning,
            version: None,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: serde_json::json!({ "urls": [RULE, SILENCE, DASH] }),
            tags: vec![],
        }
    }

    fn reader(enabled: bool) -> grafana::Reader {
        let cfg = config::Grafana {
            enabled,
            base_url: "https://restateprod.grafana.net".into(),
            ..Default::default()
        };
        let token = enabled.then(|| "glsa_test".to_string());
        grafana::Reader::new(cfg, token, Arc::new(crate::reasoner::MockReasoner::new("")))
    }

    fn driver(enabled: bool) -> BrowserDriver {
        BrowserDriver::new(config::Browser {
            enabled,
            ..Default::default()
        })
    }

    fn queued(store: &Store, sig: &Signal) -> crate::store::BrowserInvestigation {
        store
            .browser_investigations_for_subject(sig.subject.as_deref().unwrap_or_default())
            .unwrap()
            .into_iter()
            .next()
            .or_else(|| store.get_browser_investigation_for_signal(&sig.id).unwrap())
            .expect("an investigation was queued")
    }

    /// Grafana wins when it is configured, and what it is handed is the *parsed links* —
    /// so the workflow has the rule UID, the window, and the tenant without re-reading
    /// the signal.
    #[test]
    fn a_configured_grafana_takes_the_alert_and_gets_the_parsed_links() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        store.insert_signal(&sig).unwrap();
        queue_dashboard_investigation(&mut sig, &store, &driver(true), &reader(true));

        let inv = queued(&store, &sig);
        assert_eq!(inv.method, "grafana");
        let links: grafana::Links = serde_json::from_str(&inv.prompt).unwrap();
        assert_eq!(links.rule_uid.as_deref(), Some("aeb9f2c1x"));
        assert_eq!(links.from.as_deref(), Some("1754300000000"));
        assert_eq!(
            links.vars.get("environment").map(String::as_str),
            Some("env-9xk2")
        );
    }

    /// The bug this replaced. "First URL containing grafana" resolved to the alert *rule
    /// page* in all 25 real alerts examined — the rule's definition, not its graph.
    #[test]
    fn without_grafana_the_browser_gets_the_dashboard_not_the_rule_page() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        store.insert_signal(&sig).unwrap();
        queue_dashboard_investigation(&mut sig, &store, &driver(true), &reader(false));

        let inv = queued(&store, &sig);
        assert_eq!(inv.method, "browser");
        assert!(inv.url.contains("/d/"), "got {}", inv.url);
        assert!(!inv.url.contains("/alerting/"), "got {}", inv.url);
    }

    /// Never, on either tier, whatever the ordering.
    #[test]
    fn the_silence_form_is_never_what_gets_queued() {
        for (grafana_on, browser_on) in [(true, true), (false, true)] {
            let store = Store::open_in_memory().unwrap();
            let mut sig = alert();
            // Grafana listing silence first is a template change away.
            sig.raw = serde_json::json!({ "urls": [SILENCE, RULE, DASH] });
            store.insert_signal(&sig).unwrap();
            queue_dashboard_investigation(
                &mut sig,
                &store,
                &driver(browser_on),
                &reader(grafana_on),
            );
            let inv = queued(&store, &sig);
            assert!(
                !inv.url.contains("silence") && !inv.prompt.contains("silence/new"),
                "grafana={grafana_on}: queued {}",
                inv.url
            );
        }
    }

    /// Both tiers off is a no-op, not a row that nothing will ever read.
    #[test]
    fn nothing_is_queued_when_neither_tier_is_available() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        store.insert_signal(&sig).unwrap();
        queue_dashboard_investigation(&mut sig, &store, &driver(false), &reader(false));
        assert!(store
            .get_browser_investigation_for_signal(&sig.id)
            .unwrap()
            .is_none());
    }

    /// The ingest path records the mention and stops there. Anything that shelled out here
    /// would be paid on every message in the channel.
    #[test]
    fn a_mentioned_environment_is_recorded_from_the_title_and_the_body() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        sig.title = "acc_10vk05bJNjt1nLXvId4zh14 escalation".into();
        sig.body = Some("env_201kpvffchrm1sq5p0qph42dqev is 500ing".into());
        store.insert_signal(&sig).unwrap();
        cloud_ids(&sig, &store);

        let mut stale = store
            .stale_cloud_ids(
                chrono::Duration::hours(6),
                chrono::Duration::minutes(10),
                50,
            )
            .unwrap();
        stale.sort();
        assert_eq!(
            stale,
            vec![
                "acc_10vk05bJNjt1nLXvId4zh14",
                "env_201kpvffchrm1sq5p0qph42dqev"
            ]
        );
    }

    /// A watcher re-emits every signal on every poll, so this runs repeatedly on the same
    /// message. It must not accumulate rows, and it must not fail the second time.
    #[test]
    fn re_ingesting_the_same_message_records_the_mention_once() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        sig.body = Some("env_201kpvffchrm1sq5p0qph42dqev again".into());
        store.insert_signal(&sig).unwrap();
        cloud_ids(&sig, &store);
        cloud_ids(&sig, &store);
        assert_eq!(
            store
                .stale_cloud_ids(
                    chrono::Duration::hours(6),
                    chrono::Duration::minutes(10),
                    50
                )
                .unwrap()
                .len(),
            1
        );
    }

    /// The poll path's own protection: a message full of `env_vars` talk is not a queue of
    /// subprocesses.
    #[test]
    fn a_message_with_no_platform_ids_queues_nothing() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        sig.title = "env_vars are unset in the key_management job".into();
        sig.body = Some("see org_name in the acc_id column".into());
        store.insert_signal(&sig).unwrap();
        cloud_ids(&sig, &store);
        assert!(store
            .stale_cloud_ids(
                chrono::Duration::hours(6),
                chrono::Duration::minutes(10),
                50
            )
            .unwrap()
            .is_empty());
    }

    /// A watcher re-emits the same alert on every poll. One question, asked once.
    #[test]
    fn re_ingesting_the_same_alert_does_not_queue_a_second_read() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        store.insert_signal(&sig).unwrap();
        queue_dashboard_investigation(&mut sig, &store, &driver(true), &reader(true));
        let first = queued(&store, &sig).id;
        queue_dashboard_investigation(&mut sig, &store, &driver(true), &reader(true));
        assert_eq!(queued(&store, &sig).id, first);
    }

    /// A Slack message with no Grafana link at all reaches neither tier.
    #[test]
    fn an_ordinary_slack_message_is_not_an_investigation() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = alert();
        sig.raw = serde_json::json!({ "urls": ["https://github.com/restatedev/restate/pull/1"] });
        store.insert_signal(&sig).unwrap();
        queue_dashboard_investigation(&mut sig, &store, &driver(true), &reader(true));
        assert!(store
            .get_browser_investigation_for_signal(&sig.id)
            .unwrap()
            .is_none());
    }
}
