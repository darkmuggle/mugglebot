//! The subject virtual objects: `Issue`, `PullRequest`, `SlackThread`.
//!
//! One object type per rank, keyed by upstream identity. Three properties are the
//! reason this is a virtual object rather than a table plus a mutex:
//!
//! 1. **Serialized writes per key, for free.** Restate runs at most one
//!    write-access handler per key at a time, so two watchers ingesting activity
//!    about `restatedev/restate#412` in the same second cannot interleave. The
//!    read-modify-write on links, counters, and debounce state needs no lock and no
//!    "who won" reconciliation — the concurrency model forbids the race.
//! 2. **State already keyed by the thing it describes**, so there is no
//!    cache-coherency question between a map and a table.
//! 3. **Durable timers on the entity** (Phase 5): the re-analysis debounce survives
//!    a restart, which in-process `tokio::time` does not.
//!
//! Shared (read-only) handlers matter as much: `get` runs concurrently with the
//! exclusive writers, so the board reading two hundred subjects never queues behind
//! an in-progress analysis.
//!
//! Three types rather than one generic `Subject` because they will diverge — a PR
//! carries CI state and reviews, an issue carries assignment, a Slack thread carries
//! its channel — and because the model is then legible in the Restate UI.

use restate_sdk::prelude::*;

use crate::subject::{SubjectKey, SubjectRank};

pub mod debounce;
pub mod issue;
pub mod pull_request;
pub mod repo_indexer;
pub mod scheduler;
pub mod slack_thread;
pub mod watcher;

/// Write the subject record into this object's state.
///
/// The durable half of a [`crate::subject::store::SubjectStore`] write. The read model is
/// updated by the caller before this is sent, so by the time this runs the value is already
/// being served — this is what makes it survive a restart.
pub(crate) async fn put_subject(
    ctx: &ObjectContext<'_>,
    subject: crate::subject::Subject,
) -> HandlerResult<()> {
    ctx.set(crate::restate::subject_state::SUBJECT, Json(subject));
    Ok(())
}

/// Write one signal into this object's state, keyed by the signal's id.
///
/// Keyed by id rather than appended to a list, so a re-delivery of the same signal overwrites
/// instead of duplicating — which is the property the `UNIQUE(source, external_id, version)`
/// index used to provide, expressed as the shape of the state rather than as a constraint.
pub(crate) async fn put_signal(
    ctx: &ObjectContext<'_>,
    signal: crate::signal::Signal,
) -> HandlerResult<()> {
    ctx.set(
        &crate::restate::subject_state::signal_key(&signal.id),
        Json(signal),
    );
    Ok(())
}

/// Remove one signal from this object's state. The other half of a merge: the winner gets a
/// `put_signal`, the loser this.
pub(crate) async fn drop_signal(ctx: &ObjectContext<'_>, id: String) -> HandlerResult<()> {
    ctx.clear(&crate::restate::subject_state::signal_key(&id));
    Ok(())
}

/// Dispatch `record` to whichever of the three subject objects owns this key.
///
/// Matched on the rank rather than on a name string: routing through
/// `&'static str` meant a typo in one arm fell through to the Slack-thread client
/// silently, which would attribute a GitHub issue's activity to a conversation.
pub(crate) fn send_record(ctx: &ObjectContext<'_>, key: &SubjectKey, signal_id: &str) {
    let signal_id = signal_id.to_string();
    match key.rank() {
        SubjectRank::Issue => {
            ctx.object_client::<issue::IssueClient>(key.to_string())
                .record(signal_id)
                .send();
        }
        SubjectRank::PullRequest => {
            ctx.object_client::<pull_request::PullRequestClient>(key.to_string())
                .record(signal_id)
                .send();
        }
        SubjectRank::SlackThread => {
            ctx.object_client::<slack_thread::SlackThreadClient>(key.to_string())
                .record(signal_id)
                .send();
        }
    }
}

/// State keys used inside the objects. Small and hot by design: bodies, artifacts,
/// and embeddings live in SQLite and are referenced from here.
pub mod state {
    /// When the debounced analysis should run (Phase 5).
    pub const DEBOUNCE_DEADLINE: &str = "debounce_deadline";
    /// First activity in the current debounce window, for the hard cap.
    pub const FIRST_ACTIVITY: &str = "first_activity";
    /// A pull request's summarized diff, as [`crate::prdiff::StoredDiff`] JSON.
    ///
    /// On the object because the diff is a fact about *this* pull request and is read far
    /// more often than it changes — from the PR's card, from the issue it attempts, and
    /// again after clicking in. See [`crate::prdiff`] for what is kept and what is trimmed.
    pub const DIFF: &str = "diff";
    /// How many signals this subject has recorded — a ranking hint. The
    /// authoritative count is the SQL query; see AGENTS.md on reconciling the two.
    pub const SIGNAL_COUNT: &str = "signal_count";
}
