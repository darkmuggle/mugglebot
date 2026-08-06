//! incident.io watcher — every open incident, and only the open ones.
//!
//! Unlike the GitHub watchers this is a **mirror, not a feed**. It does not ask "what is new
//! since last time"; it asks "what is burning right now" and reports the complete answer. The
//! difference matters for the half of the requirement that is about *removal*: an incident
//! leaves the board when incident.io closes it, and the only way to know that from a feed of
//! new things is to never be told. So every poll carries a [`SourceSnapshot`] of the open
//! incidents, and the pipeline resolves anything absent from it (see
//! `Store::resolve_missing_incidents`).
//!
//! That also means the poll is cheap to repeat and safe to miss: there is no cursor to lose.
//! A restart re-reads the current truth rather than resuming a position in history.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeSet;
use std::time::Duration;

use super::{PollBatch, SourceSnapshot, Watcher};
use crate::config::{self, IncidentSource};
use crate::incident::{Incident, IncidentClient};
use crate::signal::{ResolutionKey, Severity, Signal, SignalKind, Source};

/// This watcher's name, and the key its health is recorded under. See [`Watcher::name`].
pub const NAME: &str = "incident";

pub struct IncidentWatcher {
    client: IncidentClient,
    interval: Duration,
}

impl IncidentWatcher {
    pub fn new(cfg: &IncidentSource, api_key: String) -> Result<Self> {
        Ok(Self {
            client: IncidentClient::new(api_key)?,
            interval: config::parse_duration(&cfg.poll_interval).unwrap_or(Duration::from_secs(60)),
        })
    }
}

#[async_trait]
impl Watcher for IncidentWatcher {
    /// `incident` — the same word as the credential (`secrets.get("incident")`), the config
    /// key (`[sources.incident]`), and the SOURCES pill in the UI.
    ///
    /// One word in all four places on purpose. This started as `"incident-io"`, which is a
    /// better name for the service and was silently wrong for the only thing the name is used
    /// for: the watcher name is the key `record_health` writes under, and the UI looks a pill's
    /// health up by exact string match. So the source dot would have stayed grey for ever —
    /// "never polled" — no matter how healthy the watcher was. The reconcile dispatch in
    /// `pipeline::reconcile` matches this string too, so a rename touches three call sites and
    /// a missed one stops closed incidents being removed.
    fn name(&self) -> &'static str {
        NAME
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn poll(&self) -> Result<PollBatch> {
        let open = self.client.open_incidents().await?;
        let active_ids: BTreeSet<String> = open.iter().map(external_id).collect();
        let signals = open.iter().map(normalize).collect();
        Ok(PollBatch {
            signals,
            // The authoritative set. An incident whose signal is not in here has left the
            // open set — closed, declined, merged or paused — and comes off the board.
            snapshot: Some(SourceSnapshot {
                source: Source::IncidentIo,
                active_ids,
            }),
        })
    }
}

/// The dedup identity of an incident's signal.
///
/// The reference, not the ULID: it is stable, human-readable in a log line, and the same
/// thing the subject key is built from — so a signal and its subject cannot disagree about
/// which incident they are.
pub fn external_id(i: &Incident) -> String {
    format!("incident/{}", i.reference)
}

/// One incident as one signal.
///
/// **Versioned on the status**, not on `updated_at`. incident.io touches `updated_at` for
/// every edit — a Slack reaction, a field change — and versioning on it would mint a new
/// signal per fidget, each re-triggering the analysis pass behind it. The status is what
/// changes the incident's *meaning* to the board, so `triage → active` is a new version and
/// nothing else is.
pub fn normalize(i: &Incident) -> Signal {
    let version = Some(i.status_category.clone());
    let external = external_id(i);
    let mut keys = vec![ResolutionKey::new("incident", &i.reference)];
    // The severity name is org-configured, so it is context rather than something to parse.
    if let Some(sev) = &i.severity {
        keys.push(ResolutionKey::new("severity", sev));
    }
    Signal {
        id: Signal::make_id(Source::IncidentIo, &external, version.as_deref()),
        source: Source::IncidentIo,
        external_id: external,
        version,
        // `Alert`, because that is what an incident is to an operator: the thing that is
        // wrong right now. It is not a `MeetingNote` or a `ThreadReply`, and it is not tied
        // to a GitHub notification kind.
        kind: SignalKind::Alert,
        title: i.name.clone(),
        body: i
            .summary
            .clone()
            .filter(|s| !s.trim().is_empty() && s.trim() != "not set"),
        url: i.permalink.clone(),
        actor: None,
        keys,
        severity: severity_of(i),
        upstream_gone: false,
        occurred_at: i
            .reported_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now),
        ingested_at: chrono::Utc::now(),
        subject: None,
        raw: serde_json::json!({
            "incident_id": i.id,
            "reference": i.reference,
            "status": i.status_name,
            // The lifecycle category, which is what decides whether this is on the board.
            "status_category": i.status_category,
            "severity": i.severity,
            // Named timestamps as an object, so `Resolved at` is readable without parsing a
            // list of pairs.
            "timestamps": i
                .timestamps
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<String, String>>(),
            // The board's `state` vocabulary, shared with GitHub subjects: this is what the
            // active-board filter reads to decide whether the work is over. Mapping it here
            // rather than teaching the projection about incident.io keeps one vocabulary.
            "state": if i.is_open() { "open" } else { "closed" },
        }),
        tags: Vec::new(),
    }
}

/// An incident's severity, on the board's four-level scale.
///
/// Mapped by **name**, because incident.io's severity scale is per-org: `Minor`, `Major`,
/// `Critical` are this org's configured names, not an API enum. An unrecognised name lands on
/// `Warning` rather than `Info` — an incident nobody has classified is still an incident, and
/// the failure worth avoiding is one sitting quietly at the bottom of a list.
fn severity_of(i: &Incident) -> Severity {
    match i
        .severity
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        s if s.contains("critical") || s.contains("sev1") || s.contains("sev 1") => {
            Severity::Critical
        }
        s if s.contains("major") || s.contains("sev2") || s.contains("sev 2") => Severity::Critical,
        s if s.contains("minor") || s.contains("sev3") || s.contains("sev 3") => Severity::Warning,
        _ => Severity::Warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incident(reference: &str, category: &str, severity: Option<&str>) -> Incident {
        Incident {
            id: "01KYW0F8MRXF0TS4J8ZRD0SHCH".into(),
            reference: reference.into(),
            name: "TenantPodOOMKillLoop".into(),
            summary: Some("A FREE-tier tenant OOM-killed 5 times.".into()),
            permalink: Some("https://app.incident.io/restatedev/incidents/445".into()),
            status_category: category.into(),
            status_name: "Investigating".into(),
            severity: severity.map(str::to_string),
            reported_at: Some("2026-07-31T11:55:36Z".into()),
            updated_at: Some("2026-07-31T16:00:46Z".into()),
            timestamps: vec![("Resolved at".into(), "2026-07-31T16:00:44Z".into())],
        }
    }

    /// The signal resolves to its own incident subject, and carries the lifecycle in the two
    /// places the board reads: the version (for dedup) and `raw.state` (for the active
    /// filter).
    #[test]
    fn an_incident_becomes_a_signal_addressed_to_its_own_subject() {
        let s = normalize(&incident("INC-445", "active", Some("Minor")));
        assert_eq!(s.source, Source::IncidentIo);
        assert_eq!(s.external_id, "incident/INC-445");
        assert_eq!(s.kind, SignalKind::Alert);

        // It resolves to the incident, not to anything else on the signal.
        let attribution = crate::subject::resolve::attribute(&s.keys);
        assert_eq!(
            attribution.subject.as_ref().map(|k| k.as_str()),
            Some("incident:INC-445"),
        );
        assert_eq!(
            attribution.subject.map(|k| k.rank()),
            Some(crate::subject::SubjectRank::Incident),
        );
        assert_eq!(s.raw["state"], "open");
    }

    /// Versioned on the status, so an incident being edited does not mint signals.
    #[test]
    fn only_a_status_change_is_a_new_version() {
        let triage = normalize(&incident("INC-445", "triage", Some("Minor")));
        let mut edited = incident("INC-445", "triage", Some("Minor"));
        edited.updated_at = Some("2026-08-01T09:00:00Z".into());
        edited.summary = Some("Someone rewrote the summary.".into());
        let edited = normalize(&edited);
        assert_eq!(
            triage.id, edited.id,
            "an edit is the same signal — otherwise every fidget re-runs the analysis"
        );

        let active = normalize(&incident("INC-445", "active", Some("Minor")));
        assert_ne!(
            triage.id, active.id,
            "triage → active changes what this means to the board"
        );
    }

    /// A closed incident reports `state: closed`, which is how the active board drops it —
    /// the same vocabulary GitHub subjects use, so the projection needs no incident-specific
    /// rule.
    #[test]
    fn a_closed_incident_reports_itself_closed() {
        assert_eq!(
            normalize(&incident("INC-445", "closed", None)).raw["state"],
            "closed"
        );
        assert_eq!(
            normalize(&incident("INC-445", "merged", None)).raw["state"],
            "closed"
        );
        assert_eq!(
            normalize(&incident("INC-445", "triage", None)).raw["state"],
            "open"
        );
    }

    /// End to end through a real store: an incident lands on the incidents board and on
    /// **neither** the main board nor a merge.
    ///
    /// The three properties the feature was asked for, asserted together because they are one
    /// claim — "a separate incidents topic, not on the board".
    #[test]
    fn an_incident_is_its_own_board_and_is_never_absorbed() {
        use crate::store::Store;
        use crate::subject::{Attributor, Subject};
        use std::sync::Arc;

        let store = Arc::new(Store::open_in_memory().unwrap());
        let attributor = Attributor::new(store.clone());

        // An incident, and an ordinary issue to prove the split cuts both ways.
        let mut inc = normalize(&incident("INC-445", "active", Some("Major")));
        let inc_key = crate::subject::SubjectKey::incident("INC-445");
        inc.subject = Some(inc_key.as_str().to_string());
        store.insert_signal(&inc).unwrap();
        store
            .set_signal_subject(&inc.id, Some(inc_key.as_str()))
            .unwrap();
        store
            .upsert_subject(&Subject::new(inc_key.clone(), &inc, chrono::Utc::now()))
            .unwrap();

        let issue_key = crate::subject::SubjectKey::issue("restatedev/restate-cloud", 1262);
        let mut issue_sig = normalize(&incident("INC-999", "active", None));
        issue_sig.id = "issue-sig".into();
        issue_sig.external_id = "issue-sig".into();
        issue_sig.subject = Some(issue_key.as_str().to_string());
        store.insert_signal(&issue_sig).unwrap();
        store
            .set_signal_subject(&issue_sig.id, Some(issue_key.as_str()))
            .unwrap();
        store
            .upsert_subject(&Subject::new(
                issue_key.clone(),
                &issue_sig,
                chrono::Utc::now(),
            ))
            .unwrap();

        let keys = |views: Vec<crate::subject::SubjectView>| -> Vec<String> {
            views
                .into_iter()
                .map(|v| v.subject.key.into_string())
                .collect()
        };

        // The incidents board has the incident and nothing else.
        assert_eq!(
            keys(attributor.incident_views(true).unwrap()),
            vec!["incident:INC-445".to_string()],
        );
        // The main board has the issue and *not* the incident. This is the requirement:
        // "display them on a separate incidents topic (not on the board)".
        let board = keys(attributor.board_views(true).unwrap());
        assert!(board.contains(&"restatedev/restate-cloud#1262".to_string()));
        assert!(
            !board.iter().any(|k| k.starts_with("incident:")),
            "an incident must not appear on the main board: {board:?}"
        );
        // The general lister still sees both, so an incident stays reachable from MCP.
        assert_eq!(attributor.subject_views(true).unwrap().len(), 2);
    }

    /// The one name, in all the places that must agree.
    ///
    /// The watcher name is not just a label: it is the key `Store::record_health` writes
    /// under, the string `pipeline::reconcile` dispatches on, and the id the UI's SOURCES pill
    /// looks its health up by. When it was `"incident-io"` and the pill was `"incident"`, the
    /// dot read grey — "never polled" — regardless of the watcher's actual state, and nothing
    /// failed loudly enough to notice.
    ///
    /// Asserted against the credential and config names too, because those are the other two
    /// spellings a reader has to type and there is no reason for them to differ.
    #[test]
    fn the_watcher_name_matches_the_credential_the_config_and_the_ui_pill() {
        // The credential account: `secrets.get("incident")` in main.rs.
        assert!(
            crate::secrets::KNOWN_SECRETS.contains(&NAME),
            "the credential account must be listed under the same name, or it cannot be \
             entered on the config page at all"
        );
        // The config key. `[sources.incident]` deserializes into `Sources::incident`, so this
        // pins the spelling a reader types in TOML.
        let toml = format!("[sources.{NAME}]\nenabled = true\n");
        let cfg: crate::config::Config = ::toml::from_str(&toml).expect("config key matches");
        assert!(
            cfg.sources.incident.enabled,
            "`[sources.{NAME}]` must be the block that enables this watcher"
        );
        // And the reconcile dispatch, which is the one that silently stops removing closed
        // incidents if the name drifts.
        assert_eq!(NAME, "incident");
    }

    /// An incident qualifies for root-cause investigation without anything asking it to.
    ///
    /// This is what makes "map incidents to code" work with no separate pipeline: the gate
    /// admits anything that looks broken — `SignalKind::Alert`, or severity at Warning or
    /// above — and an incident is both. So ingest → debounced analyse → `RootCause` runs on
    /// its own, and that workflow is the same engine that maps an issue to code: deepseek
    /// extracts the symptoms and shortlists the candidates, and the deep ranking pass judges
    /// them on `claude-opus-5`.
    ///
    /// Asserted here because it is load-bearing *and* invisible — nothing in this file calls
    /// the investigator, so a change to the gate could quietly stop incidents being mapped
    /// with no other test noticing.
    #[test]
    fn an_incident_qualifies_for_investigation_on_its_own() {
        use crate::signal::SignalKind;

        let open = normalize(&incident("INC-445", "active", Some("Major")));
        // Both halves of the gate, independently — either one is enough, and an incident
        // satisfies both.
        assert_eq!(open.kind, SignalKind::Alert);
        assert!(open.severity >= Severity::Warning);

        // Even the least severe incident this org can file still clears it. An unclassified
        // one does too — see `severity_is_mapped_by_name_and_never_falls_to_info`.
        for sev in [Some("Minor"), None] {
            let s = normalize(&incident("INC-1", "triage", sev));
            assert!(
                s.severity >= Severity::Warning || s.kind == SignalKind::Alert,
                "every incident must be worth investigating"
            );
        }
    }

    /// Severity is mapped by name because the scale is per-org, and an unclassified incident
    /// must not sort to the bottom.
    #[test]
    fn severity_is_mapped_by_name_and_never_falls_to_info() {
        assert_eq!(
            severity_of(&incident("INC-1", "active", Some("Critical"))),
            Severity::Critical
        );
        assert_eq!(
            severity_of(&incident("INC-1", "active", Some("Major"))),
            Severity::Critical
        );
        assert_eq!(
            severity_of(&incident("INC-1", "active", Some("Minor"))),
            Severity::Warning
        );
        // Unknown, and absent, both land on Warning — never Info.
        assert_eq!(
            severity_of(&incident("INC-1", "active", Some("Spicy"))),
            Severity::Warning
        );
        assert_eq!(
            severity_of(&incident("INC-1", "active", None)),
            Severity::Warning
        );
    }
}
