//! The generic-mitigations catalog (Phase 3).
//!
//! Per the design principle _mitigate generically, understand later_: a fixed
//! catalog of fast, reversible, low-risk first moves. `suggest_mitigations`
//! matches a thread's signals against these by keyword overlap and returns
//! ranked candidates — **suggestions only, never executed**.

use serde::Serialize;

use crate::signal::{Signal, Source};

#[derive(Debug, Clone, Serialize)]
pub struct Mitigation {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// True for every entry — the catalog is reversible-by-construction. Surfaced
    /// so a client can show it without special-casing.
    pub reversible: bool,
    #[serde(skip)]
    keywords: &'static [&'static str],
}

#[derive(Debug, Clone, Serialize)]
pub struct MitigationMatch {
    #[serde(flatten)]
    pub mitigation: Mitigation,
    pub score: f64,
    /// Signal ids whose text triggered the match — the citation for this suggestion.
    pub cited_signals: Vec<String>,
}

/// The catalog. Ordered roughly least- to most-invasive.
pub const CATALOG: &[Mitigation] = &[
    Mitigation {
        id: "fix-build",
        name: "Fix the failing build / check",
        description: "A CI build or check is failing on the branch, not in production. Read the \
                      failing job's log and fix the specific error — a missing module, type error, \
                      or failing test — then push. Fix forward; roll back only if a green build must \
                      ship urgently. This is not an incident mitigation.",
        reversible: true,
        keywords: &[
            "workflow run",
            "check",
            "compile",
            "typescript",
            "tsc",
            "error ts",
            "cannot find module",
            "lint",
            "test failed",
            "tests failed",
            "exit code",
            "npm err",
            "build failed",
        ],
    },
    Mitigation {
        id: "rollback",
        name: "Roll back the recent change",
        description: "Revert the most recent deploy/config change touching the affected service. \
                      The fastest reversal when the timeline points at a recent rollout.",
        reversible: true,
        keywords: &[
            "deploy",
            "release",
            "rollout",
            "regression",
            "version",
            "ci",
            "build",
            "revert",
            "canary",
        ],
    },
    Mitigation {
        id: "data-rollback",
        name: "Roll back data / migration",
        description: "Restore from a recent snapshot or reverse a schema/data migration when the \
                      fault correlates with a data change rather than code.",
        reversible: true,
        keywords: &[
            "migration",
            "schema",
            "data",
            "corruption",
            "database",
            "backup",
            "snapshot",
            "restore",
        ],
    },
    Mitigation {
        id: "drain-redirect",
        name: "Drain / redirect traffic",
        description: "Shift traffic away from the unhealthy instance, region, or shard to healthy \
                      capacity while you investigate.",
        reversible: true,
        keywords: &[
            "region",
            "instance",
            "shard",
            "node",
            "traffic",
            "load",
            "failover",
            "unhealthy",
            "latency",
            "5xx",
            "timeout",
        ],
    },
    Mitigation {
        id: "quarantine",
        name: "Quarantine the bad actor",
        description: "Isolate the offending job, tenant, or request pattern (feature-flag off, \
                      disable the endpoint) so it stops affecting everyone else.",
        reversible: true,
        keywords: &[
            "tenant", "job", "endpoint", "flag", "feature", "abuse", "runaway", "hot", "noisy",
        ],
    },
    Mitigation {
        id: "upsize",
        name: "Upsize / add capacity",
        description:
            "Scale up or out when the signal is saturation — CPU, memory, connections, or \
                      queue depth — to buy headroom before diagnosing the leak.",
        reversible: true,
        keywords: &[
            "cpu",
            "memory",
            "oom",
            "saturation",
            "capacity",
            "queue",
            "connection",
            "pool",
            "exhausted",
            "throttle",
            "quota",
        ],
    },
    Mitigation {
        id: "degrade",
        name: "Degrade gracefully",
        description:
            "Shed non-critical load: disable expensive features, serve cached/stale data, \
                      or drop to read-only to keep the core path up.",
        reversible: true,
        keywords: &[
            "overload",
            "cache",
            "stale",
            "read-only",
            "degrade",
            "shed",
            "expensive",
            "slow",
        ],
    },
    Mitigation {
        id: "block-list",
        name: "Block the source",
        description: "Rate-limit or block the offending IP, user, or query at the edge when the \
                      fault is externally driven.",
        reversible: true,
        keywords: &[
            "attack", "ddos", "spike", "ip", "rate", "limit", "block", "flood", "spam", "bot",
        ],
    },
];

/// A completed CI run is evidence, not an incident. The last known CI outcome
/// in the chronological timeline is authoritative: a green run after a red one
/// closes that failure instead of leaving stale remediation on the board.
pub fn is_successful_ci_only(signals: &[Signal]) -> bool {
    !signals.is_empty()
        && signals
            .iter()
            .all(|s| s.source == Source::GitHub && is_ci_signal(s))
        && signals
            .iter()
            .max_by_key(|s| s.occurred_at)
            .is_some_and(|s| ci_outcome(s).as_deref() == Some("success"))
}

fn is_ci_signal(s: &Signal) -> bool {
    s.raw
        .get("subject_type")
        .and_then(|v| v.as_str())
        .is_some_and(|kind| kind == "CheckSuite")
        || s.title.to_ascii_lowercase().contains("workflow run")
}

fn ci_outcome(s: &Signal) -> Option<String> {
    s.raw
        .get("ci_outcome")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| {
            let title = s.title.to_ascii_lowercase();
            if title.contains("succeeded") {
                Some("success".into())
            } else if title.contains("failed") || title.contains("failure") {
                Some("failure".into())
            } else {
                None
            }
        })
}

fn is_ci_only_with_failure(signals: &[Signal]) -> bool {
    !signals.is_empty()
        && signals.iter().all(is_ci_signal)
        && signals
            .iter()
            .any(|s| ci_outcome(s).as_deref() == Some("failure"))
}

/// Render the complete, chronologically ordered timeline evidence given to the
/// mitigation reasoner. Each event carries its outcome/state, source, actor,
/// entities, deep links, and body/log details; generated actions must cite these
/// event ids rather than relying on a thread title or a stale summary.
pub fn timeline_evidence(signals: &[Signal]) -> String {
    let mut ordered: Vec<&Signal> = signals.iter().collect();
    ordered.sort_by_key(|s| s.occurred_at);
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, s)| {
            let outcome = ci_outcome(s).unwrap_or_else(|| "n/a".into());
            let actor = s.actor.as_deref().unwrap_or("n/a");
            let url = s.url.as_deref().unwrap_or("n/a");
            let entities = s
                .entities
                .iter()
                .map(|e| format!("{}:{}", e.kind, e.value))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Event {} [sig:{}]\n  time: {}\n  source: {} · kind: {:?} · state: {:?} · severity: {:?} · outcome: {}\n  actor: {}\n  entities: {}\n  url: {}\n  title: {}\n  details:\n{}\n",
                index + 1,
                s.id,
                s.occurred_at.to_rfc3339(),
                s.source,
                s.kind,
                s.state,
                s.severity,
                outcome,
                actor,
                if entities.is_empty() { "n/a" } else { &entities },
                url,
                s.title,
                s.body.as_deref().unwrap_or("(none)")
            )
        })
        .collect()
}

/// Rank the catalog against a thread's signals. Returns matches with a nonzero
/// score, best first. Each match cites the signals whose text triggered it.
pub fn suggest(signals: &[Signal]) -> Vec<MitigationMatch> {
    if is_successful_ci_only(signals) {
        return Vec::new();
    }
    let mut out: Vec<MitigationMatch> = CATALOG
        .iter()
        // A CI-only thread with a failure needs one narrow, concrete suggestion:
        // fix that check. It is not proof of a production regression.
        .filter(|m| !is_ci_only_with_failure(signals) || m.id == "fix-build")
        .filter_map(|m| {
            let mut score = 0.0;
            let mut cited = Vec::new();
            for s in signals {
                let hay = haystack(s);
                let hits = m.keywords.iter().filter(|k| hay.contains(*k)).count();
                if hits > 0 {
                    score += hits as f64;
                    cited.push(s.id.clone());
                }
            }
            if score > 0.0 {
                Some(MitigationMatch {
                    mitigation: m.clone(),
                    score,
                    cited_signals: cited,
                })
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn haystack(s: &Signal) -> String {
    let mut h = format!("{} {}", s.title, s.body.as_deref().unwrap_or(""));
    for e in &s.entities {
        h.push(' ');
        h.push_str(&e.value);
    }
    h.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Severity, SignalKind, Source, State};
    use chrono::Utc;

    fn sig(title: &str, body: &str) -> Signal {
        Signal {
            id: format!("t/{title}"),
            source: Source::Slack,
            external_id: title.into(),
            kind: SignalKind::Alert,
            title: title.into(),
            body: Some(body.into()),
            url: None,
            actor: None,
            entities: vec![],
            severity: Severity::Critical,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: serde_json::Value::Null,
            tags: Vec::new(),
        }
    }

    #[test]
    fn saturation_signal_suggests_upsize() {
        let sigs = vec![sig("DB alert", "connection pool exhausted, cpu saturation")];
        let matches = suggest(&sigs);
        assert_eq!(matches[0].mitigation.id, "upsize");
        assert!(!matches[0].cited_signals.is_empty());
    }

    #[test]
    fn ci_compile_failure_suggests_fix_build_not_rollback() {
        let sigs = vec![sig(
            "PR Checks (npm) workflow run failed",
            "CI failure log:\nerror TS2307: Cannot find module './restate-version'\n\
             Error: Process completed with exit code 2.",
        )];
        let matches = suggest(&sigs);
        assert_eq!(
            matches[0].mitigation.id, "fix-build",
            "compile failure should suggest fixing the build, not rolling back"
        );
    }

    #[test]
    fn successful_ci_does_not_suggest_incident_mitigations() {
        let mut success = sig(
            "Data Plane Images workflow run succeeded for main branch",
            "CI/CD log tail: all tests passed",
        );
        success.source = Source::GitHub;
        success.raw = serde_json::json!({
            "subject_type": "CheckSuite",
            "ci_outcome": "success"
        });
        assert!(is_successful_ci_only(&[success.clone()]));
        assert!(suggest(&[success]).is_empty());
    }

    #[test]
    fn ci_failure_excludes_production_rollbacks() {
        let mut failure = sig(
            "Data Plane Images workflow run failed for main branch",
            "CI failure log: Process completed with exit code 1",
        );
        failure.source = Source::GitHub;
        failure.raw = serde_json::json!({
            "subject_type": "CheckSuite",
            "ci_outcome": "failure"
        });
        let matches = suggest(&[failure]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].mitigation.id, "fix-build");
    }

    #[test]
    fn later_success_closes_an_earlier_ci_failure() {
        let mut failure = sig("Checks workflow run failed", "exit code 1");
        failure.source = Source::GitHub;
        failure.occurred_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        failure.raw = serde_json::json!({ "subject_type": "CheckSuite", "ci_outcome": "failure" });
        let mut success = failure.clone();
        success.id = "github/success".into();
        success.title = "Checks workflow run succeeded".into();
        success.occurred_at = chrono::Utc::now();
        success.raw = serde_json::json!({ "subject_type": "CheckSuite", "ci_outcome": "success" });
        assert!(is_successful_ci_only(&[failure, success]));
    }

    #[test]
    fn unrelated_signal_yields_nothing() {
        let sigs = vec![sig("lunch", "who wants tacos")];
        assert!(suggest(&sigs).is_empty());
    }
}
