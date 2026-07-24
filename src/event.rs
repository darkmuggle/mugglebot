//! The live event bus payload streamed to the web UI over the WebSocket.
//!
//! One adjacently-tagged enum (`{ "type": …, "data": … }`) so the frontend can
//! switch on `type` and every variant's body is a plain map. The first frame a
//! client receives is a [`Snapshot`]; everything after is an incremental event.

use serde::Serialize;

use crate::correlation::ThreadView;
use crate::live::Hint;
use crate::signal::Signal;
use crate::store::SourceHealth;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// Full board state on connect.
    Snapshot(Box<Snapshot>),
    /// A new or updated signal.
    Signal(Signal),
    /// A new or updated thread view (incremental upsert).
    Thread(ThreadView),
    /// The authoritative set of active threads — the client reconciles its board
    /// to exactly this, dropping any thread that merged, split away, or resolved.
    Board(Vec<ThreadView>),
    /// A live-assist hint/suggestion/flag.
    Hint(Hint),
    /// Per-source watcher health changed.
    Health(Vec<SourceHealth>),
    /// A high-confidence live-assist flag — flip the UI to red-alert.
    RedAlert(RedAlert),
    /// Clear red-alert (all flags dismissed).
    ClearAlert,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub signals: Vec<Signal>,
    pub threads: Vec<ThreadView>,
    pub hints: Vec<Hint>,
    pub health: Vec<SourceHealth>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedAlert {
    pub thread_id: String,
    pub hint_id: String,
    pub message: String,
}
