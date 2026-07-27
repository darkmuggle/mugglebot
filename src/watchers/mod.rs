//! Signal sources. Each watcher authenticates, fetches, and normalizes into the
//! common [`Signal`] — nothing source-specific leaks past here. Watchers are
//! independent and fault-isolated: one being down must not stall the others.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeSet;
use std::time::Duration;

use crate::signal::{Signal, Source};

pub mod assigned;
pub mod github;
pub mod granola;
pub mod slack;

/// A complete upstream view that can be reconciled against locally active
/// signals. Incremental/cursor polls leave this absent.
pub struct SourceSnapshot {
    pub source: Source,
    pub active_ids: BTreeSet<String>,
}

pub struct PollBatch {
    pub signals: Vec<Signal>,
    pub snapshot: Option<SourceSnapshot>,
}

impl PollBatch {
    pub fn incremental(signals: Vec<Signal>) -> Self {
        Self {
            signals,
            snapshot: None,
        }
    }
}

#[async_trait]
pub trait Watcher: Send + Sync {
    fn name(&self) -> &'static str;

    /// How long to wait between polls. (Push-based watchers can return a long
    /// interval and drive their own stream instead.)
    fn interval(&self) -> Duration;

    /// Fetch the latest signals since the last poll, normalized to [`Signal`].
    /// Must be idempotent — re-emitting a signal is fine, the store dedups on
    /// `(source, external_id, version)`.
    async fn poll(&self) -> Result<PollBatch>;

    /// This watcher's resume point, as an opaque string.
    ///
    /// Stored in the `Watcher` virtual object's state so a restart resumes instead
    /// of re-reading from the top (a replay) or from now (a gap). Watchers whose
    /// dedup is content-based can leave this `None`.
    fn cursor(&self) -> Option<String> {
        None
    }

    /// Restore a cursor saved by a previous process. Called once before the first
    /// poll; must tolerate a cursor written by an older version of the watcher —
    /// a malformed one means "start fresh", never a crash.
    fn restore_cursor(&self, _cursor: &str) {}
}
