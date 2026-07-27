//! What the subject objects actually do.
//!
//! The three objects (`Issue`, `PullRequest`, `SlackThread`) differ in the upstream
//! facts they carry, not in how they record activity or re-analyze. So the bodies
//! live here once, as ordinary async functions over ordinary handles — callable from
//! a test with no Restate server anywhere in sight.
//!
//! The handlers in [`super::objects`] are wrappers: they validate the key, mutate
//! object state, and call one of these.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::correlation::Analyst;
use crate::event::Event;
use crate::notify::Notifier;
use crate::store::Store;
use crate::subject::{Attributor, Handled, SubjectKey};

pub struct SubjectOps {
    pub store: Arc<Store>,
    pub attributor: Arc<Attributor>,
    pub analyst: Arc<Analyst>,
    pub notifier: Arc<Notifier>,
    pub events: broadcast::Sender<Event>,
    /// Re-analysis debounce window, from `[live]`.
    pub debounce: crate::restate::objects::debounce::Debounce,
    /// Submits the `RootCause` workflow when an analysis pass decides a subject
    /// looks broken enough to be worth one.
    pub ingress: Arc<crate::restate::ingress::Ingress>,
    /// The gate + watermark logic, shared with the ingest pipeline.
    pub pipeline: Arc<crate::restate::pipeline::IngestOps>,
    /// Live assist: hints, suggestions, and flags on the operator's own messages.
    pub live: Arc<crate::live_engine::LiveEngine>,
}

/// What happened to a recorded signal, from the handler's point of view.
///
/// Serializable because it crosses a `ctx.run` boundary: the step's result is journalled,
/// so a retry of the surrounding handler reuses the outcome the first attempt saw rather
/// than re-attributing the signal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Recorded {
    /// Landed here. Arm the debounce.
    Analyze,
    /// Landed here, but this subject is handled: settled work, muted, and excluded from
    /// the analysis path. The local reopen classifier has already had its look.
    Muted,
    /// Landed on a *different* subject — this one was merged away, and activity
    /// addressed to it forwards to the canonical one. Nothing to arm here; the canonical
    /// object gets its own `record`.
    Forwarded(SubjectKey),
}

impl SubjectOps {
    /// Attribute a stored signal to this subject and update the board.
    ///
    /// Takes a signal **id**, not a signal: the body is already in SQLite, and a 200KB
    /// raw notification payload passed as a handler argument would be 200KB in the
    /// invocation journal, replayed on every retry.
    pub async fn record(&self, key: &SubjectKey, signal_id: &str) -> Result<Recorded> {
        let Some(signal) = self.store.get_signal(signal_id)? else {
            // A signal that vanished between submission and handling is not an error
            // worth retrying forever — the ingress may have replayed an old invocation
            // after a board reset.
            warn!("record on {key}: no signal {signal_id}");
            return Ok(Recorded::Muted);
        };

        // The attribution write happens *here*, inside the exclusive handler for this
        // key, and nowhere else. That is what makes "two watchers cannot interleave on
        // one issue" true rather than aspirational.
        //
        // `attach` follows a merge pointer, so the signal can land on a different
        // subject than the one this handler is keyed to — a Slack thread demoted under
        // an issue, say. The canonical key is what everything below uses.
        let canonical = self
            .attributor
            .attach(&signal)?
            .unwrap_or_else(|| key.clone());

        // One notification per subject state change, not one per signal. The check and
        // the record of having notified are the same critical section, which is exactly
        // what a per-key exclusive handler gives us.
        self.notifier
            .maybe_notify_subject(canonical.as_str(), &signal);
        let _ = self.events.send(Event::Signal(signal.clone()));

        // The signal can land somewhere other than this handler's key: `attach` follows
        // a merge pointer, so a Slack thread demoted under an issue forwards activity
        // still addressed to it. Arming the debounce here would then schedule an analysis
        // of a subject whose signals have all moved away — it would summarize nothing,
        // and the canonical subject would never be analyzed at all.
        if canonical != *key {
            self.broadcast(&canonical)?;
            return Ok(Recorded::Forwarded(canonical));
        }

        let handled = self
            .store
            .get_subject(canonical.as_str())?
            .is_some_and(|s| s.handled.is_handled());
        if handled {
            let reopened = self
                .analyst
                .triage_handled(canonical.as_str(), &signal)
                .await
                .unwrap_or(false);
            self.broadcast(&canonical)?;
            // A reopened subject earns the normal treatment again, including the
            // debounced pass it was excluded from while muted.
            return Ok(if reopened {
                Recorded::Analyze
            } else {
                Recorded::Muted
            });
        }
        self.broadcast(&canonical)?;
        Ok(Recorded::Analyze)
    }

    /// The debounced analysis pass. Refuses handled work — settled subjects never
    /// reach a cloud model.
    ///
    /// Ends by deciding whether to go looking for a cause. This runs *after* the
    /// analysis rather than alongside it so an investigation isn't wasted on a subject
    /// that the same pass is about to merge into another.
    pub async fn analyze(&self, key: &SubjectKey) -> Result<()> {
        // A timer armed before this subject was merged away still fires. There is
        // nothing here to analyze — the signals moved to the canonical subject, which
        // armed its own pass — so this is a no-op rather than a pass that summarizes an
        // empty subject.
        if let Some(subject) = self.store.get_subject(key.as_str())? {
            if let Some(canonical) = &subject.same_as {
                debug!("analyze {key}: merged into {canonical}; nothing to do");
                return Ok(());
            }
        }
        if let Err(e) = self.analyst.reanalyze(key.as_str()).await {
            warn!("analyze {key} failed: {e:#}");
        }
        // The live-assist pass shares this timer rather than keeping its own. It used
        // to have a separate in-memory scheduler with the same debounce window, which
        // meant two independent things could disagree about when a subject was quiet.
        if self
            .store
            .get_subject(key.as_str())?
            .is_some_and(|s| s.live)
        {
            if let Err(e) = self.live.analyze_thread(key.as_str()).await {
                warn!("live pass for {key} failed: {e:#}");
            }
        }
        self.broadcast(key)?;
        self.maybe_investigate(key).await;
        self.warm_diffs(key).await;
        Ok(())
    }

    /// Keep the diffs of this subject's pull requests stored and current.
    ///
    /// A diff used to be read on click and thrown away, so opening an issue's attempt paid a
    /// GitHub call and a local model pass every time — and clicking into the issue lost it
    /// altogether. Now the `PullRequest` object holds it, and this is what fills it in before
    /// anybody asks.
    ///
    /// Safe to call on every pass because the workflow key carries the watermark: a PR that
    /// nothing has happened to is a refused submission, not a second read. The set is bounded
    /// by the board — pull requests the operator is actually in — which is the distinction
    /// that makes storing diffs affordable at all where diffing all 147 repositories would
    /// not be.
    async fn warm_diffs(&self, key: &SubjectKey) {
        // Each entry carries its own freshness token, and which token depends on what the PR
        // *is* here. A PR on the board is a subject, so its newest signal id is the token —
        // the same watermark Explain and RootCause key on. An attempt found by the PR-fix
        // finder may not be a subject at all, and would have the watermark of a subject with
        // no signals ("empty") forever: one read and never again, however much the PR moved.
        // For those the token is the judgment's own `updated_at`, which moves when the finder
        // re-judges the PR, which is when its diff has changed.
        let mut targets: Vec<(String, String)> = Vec::new();
        if key.rank() == crate::subject::SubjectRank::PullRequest {
            targets.push((key.to_string(), self.pipeline.watermark(key.as_str())));
        }
        // The attempts on an issue — the case that was losing the diff on click-in.
        if let Ok(fixes) = self.store.pr_fixes_for_issue(key.as_str()) {
            for f in fixes.into_iter().take(crate::prdiff::MAX_DIFF_PRS) {
                let pr = crate::prdiff::pr_key(&f.pr_repo, f.pr_number);
                let token = match self.store.get_subject(&pr) {
                    Ok(Some(_)) => self.pipeline.watermark(&pr),
                    _ => f.updated_at.clone(),
                };
                targets.push((pr, token));
            }
        }
        for (pr, token) in targets {
            let wf_key = format!("{pr}@{token}");
            match self
                .ingress
                .submit_workflow(
                    "PrDiff",
                    &wf_key,
                    Some(crate::restate::workflows::rest::PrDiff::SCOPE),
                )
                .await
            {
                Ok(true) => debug!("reading the diff for {pr} (at {token})"),
                Ok(false) => debug!("{pr} diff already stored at {token}"),
                Err(e) => warn!("submitting PrDiff for {pr} failed: {e:#}"),
            }
        }
    }

    /// Submit `RootCause` when the subject looks broken and hasn't been investigated
    /// at this watermark.
    ///
    /// Both halves of "don't redo work" are load-bearing: the gate keeps the most
    /// expensive thing MuggleBot does off "Ben mentioned you in #eng", and the
    /// watermark in the workflow key means a subject with a current report costs a
    /// refused submission rather than a second investigation.
    async fn maybe_investigate(&self, key: &SubjectKey) {
        if !self.pipeline.worth_investigating(key.as_str()) {
            return;
        }
        let watermark = self.pipeline.watermark(key.as_str());
        let wf_key = format!("{key}@{watermark}");
        match self
            .ingress
            .submit_workflow(
                "RootCause",
                &wf_key,
                Some(crate::restate::workflows::root_cause::SCOPE),
            )
            .await
        {
            Ok(true) => debug!("investigating {key} (watermark {watermark})"),
            Ok(false) => debug!("{key} already investigated at {watermark}"),
            Err(e) => warn!("submitting RootCause for {key} failed: {e:#}"),
        }
    }

    /// Operator triage.
    pub async fn triage(
        &self,
        key: &SubjectKey,
        handled: Handled,
        until: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()> {
        self.store.set_handled(key.as_str(), handled, until)?;
        // Triaging resets notification dedup, so genuinely new activity can notify
        // again rather than being suppressed as "already seen".
        self.notifier.clear_notified(key.as_str());
        self.broadcast(key)?;
        Ok(())
    }

    /// Pin this subject's routing tags (human-authored; the classifier won't
    /// overwrite them) and re-analyze under the new constraint.
    pub async fn set_tags(&self, key: &SubjectKey, tags: Vec<String>) -> Result<()> {
        let names = crate::tags::normalize_tags(tags);
        let now = chrono::Utc::now();
        for n in &names {
            self.store.ensure_tag(n, "", now)?;
        }
        self.store.set_subject_tags(key.as_str(), &names, true)?;
        self.analyze(key).await
    }

    /// Mark this subject merged away. Activity recorded on it from now on forwards
    /// to the canonical subject (see [`Attributor::attach`]).
    pub async fn mark_same_as(&self, key: &SubjectKey, canonical: &SubjectKey) -> Result<()> {
        if key == canonical {
            return Ok(());
        }
        // Pointer and signals together, in one transaction: a merge that applied half
        // of itself would leave activity attributed to a subject the board hides.
        let moved = self
            .store
            .merge_subject_into(key.as_str(), canonical.as_str())?;
        debug!("{key} merged into {canonical}, moving {moved} signal(s)");
        self.attributor
            .refresh_subject_metadata(canonical.as_str())?;
        self.broadcast(canonical)?;
        debug!("{key} merged into {canonical}");
        Ok(())
    }

    /// File this subject under a parent issue.
    pub async fn link_parent(&self, key: &SubjectKey, parent: &SubjectKey) -> Result<()> {
        self.store
            .set_subject_parent(key.as_str(), Some(parent.as_str()))?;
        // A branch or pull request appearing under a handled issue un-handles it. Somebody has
        // started work on something the operator marked as dealt with, which is exactly the
        // state they would want back on the board — and unlike a comment, opening a PR is not
        // something anyone does as chatter.
        if self
            .store
            .get_subject(parent.as_str())?
            .is_some_and(|s| s.handled.is_handled())
        {
            self.store
                .set_handled(parent.as_str(), Handled::Open, None)?;
            self.notifier.clear_notified(parent.as_str());
            tracing::warn!("subject {parent}: reopened — {key} was filed under it");
            self.broadcast(parent)?;
        }
        self.broadcast(key)?;
        Ok(())
    }

    /// Push the subject and the authoritative active board to the UI.
    fn broadcast(&self, key: &SubjectKey) -> Result<()> {
        if let Some(view) = self.attributor.subject_view(key.as_str())? {
            let _ = self.events.send(Event::Subject(Box::new(view)));
        }
        if let Ok(views) = self.attributor.subject_views(true) {
            let _ = self.events.send(Event::Board(views));
        }
        Ok(())
    }
}
