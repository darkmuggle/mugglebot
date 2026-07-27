//! What the AI is doing right now, per subject.
//!
//! Every expensive pass is a Restate workflow submitted with `send`: the tool call
//! returns as soon as the ingress accepts it, long before the pass runs. That is the
//! right shape — a root-cause investigation takes minutes and holding an HTTP request
//! open for it would make the UI hostage to it — but it left the operator with no way
//! to tell the three outcomes apart:
//!
//! - **queued** behind a vqueue concurrency limit, about to run;
//! - **refused** as a duplicate, because this exact key already ran (the answer is
//!   already on screen, and pressing the button again is *supposed* to be free);
//! - **failed**, either at the ingress or inside the handler.
//!
//! All three looked identical: the button flashed and nothing changed. "I need to see
//! visual indicators that AI dispatches work" is that gap, and a per-button spinner
//! would only have covered the first case — the one that was already working.
//!
//! So this is a process-wide registry rather than per-caller state. Two reasons:
//!
//! 1. **The submitter and the runner are different call stacks.** A tool handler
//!    submits; the workflow handler runs minutes later, driven by Restate. They share
//!    a process (the SDK endpoint is served by this binary) but nothing else, and
//!    `Ingress` is constructed fresh in half a dozen places — so there is no object
//!    both halves could hang this off.
//! 2. **Failures have to outlive the thing that failed.** A workflow that dies on a
//!    terminal error leaves no artifact anywhere; the whole point is that the failure
//!    is still on screen afterwards, with its message.
//!
//! Deliberately in memory and deliberately lossy: this is a liveness display, not an
//! audit trail. The invocation journal is the durable record, and `list_workflows`
//! reads it from Restate, which is the authority.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::event::Event;

/// How many finished activities to keep per subject.
///
/// Enough that a sequence of actions stays legible ("triage, then root cause, then a
/// second opinion") without the strip becoming a log. In-flight activities are never
/// evicted by this — only completed ones.
const KEEP_FINISHED: usize = 6;

/// Total cap across all subjects, as a backstop.
///
/// A board with hundreds of subjects each holding a handful of finished rows is still
/// tiny, but an unbounded process-lifetime structure is how a daemon that runs for
/// weeks develops a memory leak nobody can explain.
const KEEP_TOTAL: usize = 400;

/// Where a dispatch has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    /// Accepted by the ingress, not yet started — usually waiting on a vqueue slot.
    Queued,
    /// The handler is executing.
    Running,
    /// Finished successfully.
    Done,
    /// Restate refused the key because this exact work already ran. Not an error: the
    /// result is already stored, and this is what makes a redundant press free. Shown
    /// because "nothing happened" and "nothing *needed* to happen" are different
    /// answers to the same button press.
    Duplicate,
    /// It failed. `detail` carries the message.
    Failed,
}

impl DispatchState {
    /// Whether this state is a resting one. Used for retention: unfinished work is
    /// never evicted, however old it looks.
    fn is_final(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// One AI dispatch, as the UI sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Dispatch {
    /// `{workflow}/{key}` — stable across the submit → run → finish transitions, which
    /// is what lets the client patch a row rather than accumulate three of them.
    pub id: String,
    /// The subject this belongs to, or `""` for work that isn't subject-scoped (a repo
    /// index, a scheduler tick).
    pub subject: String,
    /// The workflow name, verbatim (`RootCause`, `SecondOpinion`, `IssueTriage`). The UI
    /// prettifies it; keeping the real name here means a log line and a strip row can be
    /// matched up.
    pub kind: String,
    pub state: DispatchState,
    /// The failure message, or the note explaining a duplicate.
    pub detail: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

static BOARD: LazyLock<Mutex<VecDeque<Dispatch>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// The WS sender, installed once at boot.
///
/// A `OnceLock` rather than a parameter threaded through every call site: the recording
/// points are deep inside an ingress method and four workflow handlers, none of which
/// otherwise know the event bus exists. Absent (in tests, and before boot finishes) the
/// registry still records — it just doesn't push.
static EVENTS: OnceLock<broadcast::Sender<Event>> = OnceLock::new();

/// Publish dispatch changes on the live bus. Called once from `main`.
pub fn install(events: broadcast::Sender<Event>) {
    let _ = EVENTS.set(events);
}

/// Record a state transition and push it to the UI.
///
/// Upserts on `id`: a submit records `Queued`, the handler flips it to `Running`, and
/// the same row ends as `Done` or `Failed`. A transition arriving for an unknown id
/// (the daemon restarted while a workflow was in flight, and Restate resumed it) is
/// inserted rather than dropped — the resumed work is real and belongs on screen.
pub fn record(kind: &str, key: &str, state: DispatchState, detail: Option<String>) {
    let (subject, _) = crate::restate::workflows::split_versioned(key);
    let id = format!("{kind}/{key}");
    let now = Utc::now();
    let updated = {
        let Ok(mut board) = BOARD.lock() else {
            // A poisoned mutex means some other thread panicked mid-update. A liveness
            // display is not worth propagating that into the caller's path.
            return;
        };
        let entry = match board.iter_mut().find(|d| d.id == id) {
            Some(existing) => {
                // A duplicate submission arriving while the *first* one is still in flight
                // is not "already done" — the work is running right now, and downgrading
                // the row would tell the operator the opposite of the truth. Seen
                // immediately in practice: the boot sweep submits a triage, a catch-up tick
                // re-submits it thirty seconds later, and Restate answers
                // `PreviouslyAccepted` because the original invocation is the live one.
                if state == DispatchState::Duplicate && !existing.state.is_final() {
                    existing.clone()
                } else {
                    // Re-running a row that already finished (an explicit redo, or a Restate
                    // retry after a transient error) starts its clock again, so the strip
                    // shows how long *this* attempt has been going.
                    if state == DispatchState::Running && existing.state.is_final() {
                        existing.started_at = now;
                        existing.finished_at = None;
                    }
                    existing.state = state;
                    if detail.is_some() {
                        existing.detail = detail;
                    }
                    if state.is_final() {
                        existing.finished_at = Some(now);
                    }
                    existing.clone()
                }
            }
            None => {
                let fresh = Dispatch {
                    id,
                    subject: subject.to_string(),
                    kind: kind.to_string(),
                    state,
                    detail,
                    started_at: now,
                    finished_at: state.is_final().then_some(now),
                };
                board.push_back(fresh.clone());
                fresh
            }
        };
        prune(&mut board, &entry.subject);
        entry
    };
    if let Some(events) = EVENTS.get() {
        let _ = events.send(Event::Dispatch(Box::new(updated)));
    }
}

/// Convenience wrappers, so a call site reads as what happened.
pub fn queued(kind: &str, key: &str) {
    record(kind, key, DispatchState::Queued, None);
}

pub fn running(kind: &str, key: &str) {
    record(kind, key, DispatchState::Running, None);
}

pub fn done(kind: &str, key: &str) {
    record(kind, key, DispatchState::Done, None);
}

pub fn duplicate(kind: &str, key: &str, note: &str) {
    record(kind, key, DispatchState::Duplicate, Some(note.to_string()));
}

pub fn failed(kind: &str, key: &str, detail: impl std::fmt::Display) {
    record(
        kind,
        key,
        DispatchState::Failed,
        Some(format!("{detail}").chars().take(400).collect()),
    );
}

/// Drop the oldest *finished* rows once a subject (or the process) holds too many.
fn prune(board: &mut VecDeque<Dispatch>, subject: &str) {
    let mut finished: Vec<usize> = board
        .iter()
        .enumerate()
        .filter(|(_, d)| d.subject == subject && d.state.is_final())
        .map(|(i, _)| i)
        .collect();
    while finished.len() > KEEP_FINISHED {
        board.remove(finished.remove(0));
        // Indices after the removed one shift down by one.
        for i in finished.iter_mut() {
            *i -= 1;
        }
    }
    while board.len() > KEEP_TOTAL {
        // Evict the oldest finished row anywhere; never an in-flight one, because that
        // would make the display claim work stopped when it hasn't.
        match board.iter().position(|d| d.state.is_final()) {
            Some(i) => {
                board.remove(i);
            }
            None => break,
        }
    }
}

/// Everything currently known, newest first. The snapshot a connecting client gets.
pub fn all() -> Vec<Dispatch> {
    let Ok(board) = BOARD.lock() else {
        return Vec::new();
    };
    let mut out: Vec<Dispatch> = board.iter().cloned().collect();
    out.reverse();
    out
}

/// One subject's dispatches, newest first.
pub fn for_subject(subject: &str) -> Vec<Dispatch> {
    let Ok(board) = BOARD.lock() else {
        return Vec::new();
    };
    let mut out: Vec<Dispatch> = board
        .iter()
        .filter(|d| d.subject == subject)
        .cloned()
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
pub fn reset_for_tests() {
    if let Ok(mut board) = BOARD.lock() {
        board.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One sequential test rather than several: the registry is process-wide by design,
    /// so parallel tests would interleave writes into it and fail each other.
    #[test]
    fn the_registry_tracks_a_dispatch_through_its_states() {
        reset_for_tests();

        // The subject key is derived from the workflow key, so a versioned key
        // (`{subject}@{watermark}`) still lands on the subject's strip. Subject keys
        // contain `@` themselves in GitHub signal ids, which is why this splits on the
        // *first* one.
        queued("RootCause", "o/r#412@gh:notif@1");
        let rows = for_subject("o/r#412");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, DispatchState::Queued);
        assert_eq!(rows[0].kind, "RootCause");
        assert!(rows[0].finished_at.is_none());

        // Running, then failing, is the same row throughout — otherwise one button
        // press would draw three.
        running("RootCause", "o/r#412@gh:notif@1");
        assert_eq!(for_subject("o/r#412").len(), 1);
        failed("RootCause", "o/r#412@gh:notif@1", "no such repo");
        let rows = for_subject("o/r#412");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, DispatchState::Failed);
        assert_eq!(rows[0].detail.as_deref(), Some("no such repo"));
        assert!(rows[0].finished_at.is_some());

        // A duplicate carries its note: "nothing happened" and "nothing needed to
        // happen" are different answers, and the second one is not an error.
        duplicate(
            "Explain",
            "o/r#412@w2",
            "already explained at this watermark",
        );
        let rows = for_subject("o/r#412");
        assert_eq!(rows.len(), 2, "newest first");
        assert_eq!(rows[0].state, DispatchState::Duplicate);
        assert_eq!(rows[0].kind, "Explain");

        // A transition for an id nobody recorded — the daemon restarted and Restate
        // resumed the workflow — is inserted, not dropped.
        running("IssueTriage", "o/r#99@sha");
        assert_eq!(for_subject("o/r#99").len(), 1);

        // A re-submission while the first one is still running answers
        // `PreviouslyAccepted` — but the work *is* in flight, so the row must not claim
        // it is already done.
        duplicate("IssueTriage", "o/r#99@sha", "already run at this key");
        assert_eq!(
            for_subject("o/r#99")[0].state,
            DispatchState::Running,
            "a duplicate submission must not downgrade live work"
        );

        // Retention drops the oldest finished row and keeps in-flight work.
        reset_for_tests();
        queued("Slow", "o/r#1@x");
        for i in 0..KEEP_FINISHED + 3 {
            let key = format!("o/r#1@k{i}");
            done("Fast", &key);
        }
        let rows = for_subject("o/r#1");
        assert_eq!(
            rows.len(),
            KEEP_FINISHED + 1,
            "in-flight work is not evicted"
        );
        assert!(
            rows.iter().any(|d| d.state == DispatchState::Queued),
            "the queued row survived: {rows:?}"
        );

        // Un-scoped work (no subject) is still recorded, under the empty subject, so it
        // shows up in `all()` for a global view without polluting a subject's strip.
        reset_for_tests();
        queued("RepoIndex", "restatedev/restate");
        assert!(for_subject("restatedev/restate").len() == 1);
        assert_eq!(all().len(), 1);
    }
}
