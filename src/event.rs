//! The live event bus payload streamed to the web UI over the WebSocket.
//!
//! One adjacently-tagged enum (`{ "type": …, "data": … }`) so the frontend can
//! switch on `type` and every variant's body is a plain map. The first frame a
//! client receives is a [`Snapshot`]; everything after is an incremental event.

use serde::Serialize;

use crate::dispatch::Dispatch;
use crate::live::Hint;
use crate::signal::Signal;
use crate::store::SourceHealth;
use crate::subject::SubjectView;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// Full board state on connect.
    Snapshot(Box<Snapshot>),
    /// A new or updated signal.
    Signal(Signal),
    /// A new or updated subject view (incremental upsert).
    Subject(Box<SubjectView>),
    /// The authoritative set of active subjects — the client reconciles its board to
    /// exactly this, dropping any subject that merged away or was handled.
    Board(Vec<SubjectView>),
    /// A live-assist hint/suggestion/flag.
    Hint(Hint),
    /// Per-source watcher health changed.
    Health(Vec<SourceHealth>),
    /// A high-confidence live-assist flag — flip the UI to red-alert.
    RedAlert(RedAlert),
    /// Clear red-alert (all flags dismissed).
    ClearAlert,
    /// One line of an agent session's output — text, thinking, a tool call, or the end.
    ///
    /// Streamed rather than collected because watching an agent work in a repository is the
    /// point: a transcript that arrives when it finishes tells you what it concluded, and a
    /// stream tells you whether it is on the right track while there is still time to stop it.
    AgentChunk(Box<AgentChunk>),
    /// One repo's code-index progress moved.
    ///
    /// Pushed rather than polled because indexing is slow and bursty: a component card lands
    /// every few minutes, and a panel that polls for it either wastes a Datafusion query every
    /// few seconds or shows a number that is up to a poll-interval stale. The event carries the
    /// whole row so the client patches one repo rather than re-reading the org.
    IndexProgress(Box<IndexProgressEvent>),
    /// One AI dispatch changed state — submitted, started, finished, refused as a
    /// duplicate, or failed.
    ///
    /// Pushed for the same reason the index progress is: the expensive passes are
    /// `send`-submitted workflows, so the HTTP call that starts one returns long before
    /// it runs. Without this the operator presses a button, the screen flashes, and
    /// queued / already-done / broken are indistinguishable.
    Dispatch(Box<Dispatch>),
}

/// What kind of thing an agent just emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// The session started; `native_session_id` carries the CLI's own id where it mints one.
    Started,
    /// Prose addressed to the operator.
    Text,
    /// Reasoning. Shown separately because it is the part worth watching and the part you
    /// should not act on.
    Thinking,
    /// A tool call — the name, or the command.
    Tool,
    /// The turn finished, with its cost where the CLI reports one.
    Result,
    /// Something on stderr. Usually "not logged in" or a rejected flag.
    Error,
    /// The process ended.
    Exited,
}

/// One streamed line from an agent session.
#[derive(Debug, Clone, Serialize)]
pub struct AgentChunk {
    pub session_id: String,
    pub repo: String,
    /// `claude` or `codex`.
    pub tool: String,
    pub kind: ChunkKind,
    pub text: String,
    /// The tool call this came from, when a subagent produced it. Set by
    /// `--forward-subagent-text`, and what makes a subagent's thinking attributable rather than
    /// looking like the main agent talking.
    pub subagent_of: Option<String>,
    /// The CLI's own session id, where it differs from ours (Codex mints a `thread_id`).
    pub native_session_id: Option<String>,
    /// Reported at the end of a turn. Surfaced because these sessions are the one thing here
    /// that spends money by design.
    pub cost_usd: Option<f64>,
    /// This is a continuation of the block before it, not a new one.
    ///
    /// Streaming arrives token by token, so a client that started a line per chunk would render
    /// one word per row. The client appends to the previous chunk when this is set and the kind
    /// matches.
    #[serde(default)]
    pub delta: bool,
}

impl AgentChunk {
    /// A chunk with only what the parser knows; the runner fills in the session, repo and tool.
    pub fn partial(kind: ChunkKind, text: String, subagent_of: Option<String>) -> Self {
        Self {
            session_id: String::new(),
            repo: String::new(),
            tool: String::new(),
            kind,
            text,
            subagent_of,
            native_session_id: None,
            cost_usd: None,
            delta: false,
        }
    }
}

/// One repo's indexing progress, as the board shows it.
///
/// Absolute figures rather than per-tick deltas: the client patches a row by replacing it, and a
/// delta would need someone to accumulate — which is the second account of one fact that
/// publishing progress is meant to remove.
#[derive(Debug, Clone, Serialize)]
pub struct IndexProgressEvent {
    pub repo: String,
    pub components: u64,
    pub commits_cached: i64,
    pub commits_summarized: i64,
    pub dep_edges: usize,
    /// How far back history has been walked, RFC3339. `None` means the walk hasn't started —
    /// a different state from "nothing left to do", and indistinguishable without it.
    pub history_back_to: Option<String>,
    /// The newest cached commit — the repo's last activity as the index has seen it.
    pub last_commit: Option<String>,
    /// Whether this repo's index is finished: history walked, every component carded, every
    /// cached commit summarized.
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub signals: Vec<Signal>,
    pub subjects: Vec<SubjectView>,
    pub hints: Vec<Hint>,
    pub health: Vec<SourceHealth>,
    /// In-flight and recent AI dispatches, so a client that connects mid-pass sees the
    /// work already running instead of an idle-looking board.
    pub dispatches: Vec<Dispatch>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedAlert {
    pub subject_key: String,
    pub hint_id: String,
    pub message: String,
}
