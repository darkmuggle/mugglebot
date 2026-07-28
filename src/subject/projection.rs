//! The board projection: turning stored subjects and signals into what the UI and
//! MCP read.
//!
//! This is a *read* path over SQLite, deliberately. Restate virtual-object state is
//! addressable only by key, and the board is a cross-key query — "every subject
//! needing attention, ranked, filtered by source and severity". So subject handlers
//! write their state through to these tables (Phase 2) and every list view stays a
//! `SELECT`.
//!
//! Derived values — attention, severity, decorations — are computed here on every
//! read rather than stored. A stored "needs attention" flag drifts the moment a
//! subject is acknowledged elsewhere, and a stored "AI done" flag lies after a
//! failed pass. Computing them from the artifacts that actually exist means the
//! badge can't disagree with the panel underneath it.

use anyhow::Result;
use chrono::Utc;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::{union_keys, Attention, Decorations, Handled, Subject, SubjectKey, SubjectView};
use crate::signal::{Severity, Signal};
use crate::store::Store;

pub struct Board {
    pub(crate) store: Arc<Store>,
}

impl Board {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Recompute a subject's deterministic title/summary and bump `updated_at`,
    /// preserving any LLM summary already recorded (only fills a blank one).
    pub fn refresh_metadata(&self, key: &SubjectKey) -> Result<()> {
        let signals = self.store.signals_for_subject(key.as_str())?;
        if signals.is_empty() {
            self.store.delete_subject_if_empty(key.as_str())?;
            return Ok(());
        }
        let Some(mut subject) = self.store.get_subject(key.as_str())? else {
            return Ok(());
        };
        subject.updated_at = Utc::now();
        subject.title = super::title_from(&signals[0]);
        if subject.summary.is_none() || subject.last_reasoned_at.is_none() {
            subject.summary = Some(deterministic_summary(&signals));
        }
        self.store.upsert_subject(&subject)?;
        Ok(())
    }

    pub fn view(&self, key: &SubjectKey) -> Result<Option<SubjectView>> {
        let Some(subject) = self.store.get_subject(key.as_str())? else {
            return Ok(None);
        };
        let signals = self.store.signals_for_subject(key.as_str())?;
        let keys = union_keys(&signals);
        let severity = signals
            .iter()
            .map(|s| s.severity)
            .max()
            .unwrap_or(Severity::Info);
        let edges = self.store.edges_for_subject(key.as_str())?;
        let context = self.store.subject_context(key.as_str())?;
        let children = self.store.subject_children(key.as_str())?;
        // The PR critiques are keyed by the issue, so an issue view carries its
        // attempts and a PR view carries none of its own — which is the nesting the
        // board renders.
        let pull_requests = self.store.pr_fixes_for_issue(key.as_str())?;
        let explanations = self.store.explanations(key.as_str())?;
        // Passed in rather than re-queried: `attention` needs the same critiques to
        // count judged PRs and attribute their cost, and the board renders every subject
        // on every push.
        let attention = self.attention(&subject, &signals, severity, &pull_requests)?;
        // Only a *reasoned* summary can produce a headline. `deterministic_summary` is a
        // preview of the newest event body — fine as a placeholder in the detail view,
        // but presenting a chat message's first sentence as "what this needs from you"
        // would be a claim the board hasn't earned.
        let headline = subject
            .last_reasoned_at
            .and_then(|_| super::headline_from(subject.summary.as_deref()));
        Ok(Some(SubjectView {
            subject,
            headline,
            signals,
            keys,
            severity,
            edges,
            context,
            children,
            pull_requests,
            explanations,
            attention,
        }))
    }

    /// Every subject as a view, newest activity first.
    ///
    /// `active_only` drops handled work and anything merged away: a subject with
    /// `same_as` set forwards its activity to the canonical one, so showing both
    /// would be showing the same work twice.
    pub fn views(&self, active_only: bool) -> Result<Vec<SubjectView>> {
        let mut out = Vec::new();
        for s in self.store.list_subjects()? {
            if active_only
                && (s.same_as.is_some()
                    || matches!(s.handled, Handled::Resolved | Handled::Snoozed))
            {
                continue;
            }
            if let Some(view) = self.view(&s.key)? {
                // Everything upstream is gone: the issue was closed, the PR merged, the
                // notification cleared. Not operator triage — a fact about the source — and the
                // board used to keep showing it, because this filter only looked at `same_as` and
                // `handled`. A merged PR sitting on the board forever is exactly the noise the
                // reconciler's `upstream_gone` flag was already recording and nothing was reading.
                //
                // Guarded on non-empty, because "no signals" is a subject mid-creation rather than
                // one whose work is finished.
                if active_only
                    && !view.signals.is_empty()
                    && view.signals.iter().all(|sig| sig.upstream_gone)
                {
                    continue;
                }
                out.push(view);
            }
        }
        Ok(out)
    }

    /// Derive the two things the board actually reports: whether this needs the
    /// operator, and what the AI has made of it.
    fn attention(
        &self,
        subject: &Subject,
        signals: &[Signal],
        severity: Severity,
        pull_requests: &[crate::store::PrFix],
    ) -> Result<Attention> {
        let key = subject.key.as_str();
        let mut decorated = Decorations {
            // `last_reasoned_at` distinguishes a real grounded summary from the
            // deterministic one-liner every subject gets for free — but a pass that
            // stored the prompt back at us set that timestamp too, so the text has to
            // agree that it's a summary before the facet claims one exists.
            summary: subject.last_reasoned_at.is_some()
                && super::is_usable_summary(subject.summary.as_deref().unwrap_or_default()),
            tags: !subject.tags.is_empty(),
            dashboard: self
                .store
                .browser_investigations_for_subject(key)?
                .iter()
                .any(|i| i.findings.as_deref().is_some_and(|f| !f.trim().is_empty())),
            root_cause: self.store.get_root_cause(key)?.map(|r| r.status.clone()),
            ..Default::default()
        };
        let triage_rows = self.store.issue_triage_for_subject(key)?;
        decorated.triage = triage_rows.first().map(|t| t.status.clone());
        decorated.prs_judged = pull_requests.len();
        // The tier that answered is recorded per judgment, so cost attribution is real
        // rather than assumed.
        for fix in pull_requests {
            match fix.analyzed_by.as_deref() {
                Some("local") | None => decorated.local_passes += 1,
                _ => decorated.cloud_passes += 1,
            }
        }
        // Tag classification, triage, and root-cause searching are on-device by
        // policy; summaries go through the routed tier.
        if decorated.tags {
            decorated.local_passes += 1;
        }
        if decorated.triage.as_deref() == Some("complete") {
            decorated.local_passes += 1;
        }
        if decorated.root_cause.as_deref() == Some("complete") {
            decorated.local_passes += 1;
        }
        if decorated.summary {
            decorated.cloud_passes += 1;
        }
        if decorated.dashboard {
            decorated.cloud_passes += 1;
        }

        // Handled work never asks for attention — that is what handling it meant.
        let (needed, reason) = if matches!(subject.handled, Handled::Resolved | Handled::Snoozed) {
            (false, None)
        } else if self
            .store
            .list_hints(Some(key))?
            .iter()
            .any(|h| matches!(h.kind, crate::live::HintKind::Flag))
        {
            (
                true,
                Some("live-assist flagged something you said".to_string()),
            )
        } else if severity >= Severity::Critical {
            (true, Some("critical".to_string()))
        } else if severity >= Severity::Warning {
            (true, Some("warning".to_string()))
        } else if signals.iter().any(|s| s.is_user_engaged()) {
            (true, Some("you're in this one".to_string()))
        } else if triage_rows.iter().any(|t| t.status != "complete") {
            (true, Some("assigned to you".to_string()))
        } else {
            // Informational, or already acknowledged: on the board, not asking.
            (false, None)
        };
        Ok(Attention {
            needed,
            reason,
            decorated,
        })
    }
}

pub fn deterministic_summary(signals: &[Signal]) -> String {
    // Lead with real content — the newest message — rather than a key dump, which
    // is useless for a chat message. This is only the fallback headline until the
    // LLM writes a proper summary; keep it a single readable line.
    if let Some(s) = signals.iter().max_by_key(|s| s.occurred_at) {
        let body = s.body.as_deref().unwrap_or("").trim();
        let text = if body.is_empty() {
            s.title.trim()
        } else {
            body
        };
        if !text.is_empty() {
            // Collapse newlines/runs of whitespace into a one-line preview.
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut preview: String = flat.chars().take(200).collect();
            if flat.chars().count() > 200 {
                preview.push('…');
            }
            if signals.len() > 1 {
                return format!("{preview} · +{} more event(s)", signals.len() - 1);
            }
            return preview;
        }
    }
    // Content-less signals: fall back to a source count.
    let mut sources: BTreeSet<&str> = BTreeSet::new();
    for s in signals {
        sources.insert(s.source.as_str());
    }
    let src_str = sources.into_iter().collect::<Vec<_>>().join("/");
    format!("{} event(s) from {}.", signals.len(), src_str)
}
