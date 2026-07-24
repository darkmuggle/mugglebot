//! SQLite store. One embedded, single-file store for every access pattern:
//! the append-mostly signal log, the thread **relation graph** (edges + joins),
//! the memory + context grounding stores, and semantic recall (embeddings kept
//! as `f32` BLOBs, ranked in-process — see [`crate::embed`]). Dedup on signals is
//! enforced by `UNIQUE(source, external_id)`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::context::{Context as CtxEntry, ContextSourceKind};
use crate::correlation::{ContextKind, Edge, Provenance, RelationKind, Thread, ThreadContext};
use crate::live::{FlagType, Hint, HintKind, HintState};
use crate::memory::Memory;
use crate::signal::{Entity, Severity, Signal, SignalKind, Source, State};
use crate::tags::Tag;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS signals (
    id           TEXT PRIMARY KEY,
    source       TEXT NOT NULL,
    external_id  TEXT NOT NULL,
    kind         TEXT NOT NULL,
    title        TEXT NOT NULL,
    body         TEXT,
    url          TEXT,
    actor        TEXT,
    entities     TEXT NOT NULL,
    severity     TEXT NOT NULL,
    state        TEXT NOT NULL,
    occurred_at  TEXT NOT NULL,
    ingested_at  TEXT NOT NULL,
    thread       TEXT,
    raw          TEXT NOT NULL,
    tags         TEXT NOT NULL DEFAULT '[]',
    UNIQUE(source, external_id)
);
CREATE INDEX IF NOT EXISTS idx_signals_occurred ON signals(occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_signals_source   ON signals(source);
CREATE INDEX IF NOT EXISTS idx_signals_state    ON signals(state);
CREATE INDEX IF NOT EXISTS idx_signals_thread   ON signals(thread);

CREATE TABLE IF NOT EXISTS threads (
    id               TEXT PRIMARY KEY,
    title            TEXT NOT NULL,
    summary          TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    last_reasoned_at TEXT,
    live             INTEGER NOT NULL DEFAULT 0,
    tags             TEXT NOT NULL DEFAULT '[]',
    tags_pinned      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS thread_edges (
    thread_a    TEXT NOT NULL,
    thread_b    TEXT NOT NULL,
    kind        TEXT NOT NULL,
    provenance  TEXT NOT NULL,
    confidence  REAL NOT NULL,
    rationale   TEXT NOT NULL,
    signals     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (thread_a, thread_b)
);

CREATE TABLE IF NOT EXISTS thread_context (
    id          TEXT PRIMARY KEY,
    thread_id   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    content     TEXT NOT NULL,
    summary     TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_thread_context ON thread_context(thread_id);

CREATE TABLE IF NOT EXISTS memory (
    id          TEXT PRIMARY KEY,
    text        TEXT NOT NULL,
    summary     TEXT NOT NULL,
    links       TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    embedding   BLOB,
    tags        TEXT NOT NULL DEFAULT '[]',
    tags_pinned INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS context (
    id               TEXT PRIMARY KEY,
    kind             TEXT NOT NULL,
    location         TEXT NOT NULL,
    credential       TEXT,
    header           TEXT,
    summary          TEXT,
    raw              TEXT,
    etag             TEXT,
    last_modified    TEXT,
    mtime            TEXT,
    fetched_at       TEXT,
    refresh_interval TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    embedding        BLOB,
    tags             TEXT NOT NULL DEFAULT '[]',
    tags_pinned      INTEGER NOT NULL DEFAULT 0
);

-- The tag vocabulary: one row per known tag, with a short summary the classifier
-- reads to decide which tags apply to an incoming issue.
CREATE TABLE IF NOT EXISTS tags (
    name       TEXT PRIMARY KEY,
    summary    TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS hints (
    id          TEXT PRIMARY KEY,
    thread_id   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    flag_type   TEXT,
    text        TEXT NOT NULL,
    rationale   TEXT,
    citations   TEXT NOT NULL,
    confidence  REAL NOT NULL,
    state       TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_hints_thread ON hints(thread_id);
CREATE INDEX IF NOT EXISTS idx_hints_state  ON hints(state);

CREATE TABLE IF NOT EXISTS source_health (
    source       TEXT PRIMARY KEY,
    last_poll_at TEXT,
    last_ok_at   TEXT,
    ok           INTEGER NOT NULL,
    detail       TEXT,
    cursor       TEXT
);

CREATE TABLE IF NOT EXISTS credentials (
    account TEXT PRIMARY KEY,
    secret  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chats (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    messages    TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chats_updated ON chats(updated_at DESC);

-- Cached LLM-generated mitigations per thread. Generation is slow (a reasoner
-- round-trip), so it runs in the background during reanalysis and the UI reads
-- the cache instead of blocking on it.
CREATE TABLE IF NOT EXISTS thread_mitigations (
    thread_id   TEXT PRIMARY KEY,
    json        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
"#;

/// Process-unique id fragment. Monotonic counter mixed with a startup nonce —
/// good enough to key store rows without pulling in a UUID dependency.
static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn new_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}{n:x}")
}

/// Filter for [`Store::list_signals`].
#[derive(Debug, Default, Clone)]
pub struct SignalFilter {
    pub source: Option<Source>,
    pub since: Option<DateTime<Utc>>,
    pub min_severity: Option<Severity>,
    pub state: Option<State>,
    pub limit: Option<usize>,
}

/// A persisted agent-chat conversation. `messages` is the opaque UI bubble array
/// (role, content, images, tool trace) stored as JSON — the chat endpoint stays
/// stateless, so the frontend owns the shape and we just round-trip it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredChat {
    pub id: String,
    pub title: String,
    pub messages: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
}

/// Metadata-only row for the chat list (excludes the heavy `messages` blob).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceHealth {
    pub source: String,
    pub last_poll_at: Option<String>,
    pub last_ok_at: Option<String>,
    pub ok: bool,
    pub detail: Option<String>,
    pub cursor: Option<String>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening SQLite at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store mutex poisoned")
    }

    // ---- signals ------------------------------------------------------------

    /// Insert a signal, refreshing source-provided context on duplicates while
    /// preserving local state, thread membership, and user-applied tags.
    /// Returns `true` only when the row was newly inserted.
    pub fn insert_signal(&self, s: &Signal) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO signals \
             (id, source, external_id, kind, title, body, url, actor, entities, \
              severity, state, occurred_at, ingested_at, thread, raw, tags) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                s.id,
                s.source.as_str(),
                s.external_id,
                json(&s.kind)?,
                s.title,
                s.body,
                s.url,
                s.actor,
                json(&s.entities)?,
                json(&s.severity)?,
                json(&s.state)?,
                s.occurred_at.to_rfc3339(),
                s.ingested_at.to_rfc3339(),
                s.thread,
                s.raw.to_string(),
                json(&s.tags)?,
            ],
        )?;
        if changed > 0 {
            return Ok(true);
        }
        // GitHub keeps unread notifications stable across restarts. Refreshing
        // the mutable source fields lets newly added enrichers (for example CI
        // log extraction) populate already-stored notifications without
        // resetting their triage state or correlation membership.
        conn.execute(
            "UPDATE signals SET kind=?3, title=?4, body=?5, url=?6, actor=?7, \
             entities=?8, severity=?9, occurred_at=?10, ingested_at=?11, raw=?12 \
             WHERE source=?1 AND external_id=?2",
            params![
                s.source.as_str(),
                s.external_id,
                json(&s.kind)?,
                s.title,
                s.body,
                s.url,
                s.actor,
                json(&s.entities)?,
                json(&s.severity)?,
                s.occurred_at.to_rfc3339(),
                s.ingested_at.to_rfc3339(),
                s.raw.to_string(),
            ],
        )?;
        Ok(false)
    }

    /// Most recent signals, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Signal>> {
        self.list_signals(&SignalFilter {
            limit: Some(limit),
            ..Default::default()
        })
    }

    /// Filtered signal list, newest first. Severity is filtered in-process (it's
    /// stored as a label, not an ordinal), which is fine at this volume.
    pub fn list_signals(&self, f: &SignalFilter) -> Result<Vec<Signal>> {
        let conn = self.lock();
        let mut sql = String::from(
            "SELECT id, source, external_id, kind, title, body, url, actor, entities, \
                    severity, state, occurred_at, ingested_at, thread, raw, tags FROM signals WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(src) = f.source {
            sql.push_str(" AND source = ?");
            args.push(Box::new(src.as_str().to_string()));
        }
        if let Some(state) = f.state {
            sql.push_str(" AND state = ?");
            args.push(Box::new(json(&state)?));
        }
        if let Some(since) = f.since {
            sql.push_str(" AND occurred_at >= ?");
            args.push(Box::new(since.to_rfc3339()));
        }
        sql.push_str(" ORDER BY occurred_at DESC");
        // Over-fetch when severity-filtering so the post-filter can still reach the limit.
        let hard_limit = f.limit.unwrap_or(1000);
        let fetch = if f.min_severity.is_some() {
            hard_limit * 8
        } else {
            hard_limit
        };
        sql.push_str(&format!(" LIMIT {fetch}"));

        let mut stmt = conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(params_ref.as_slice(), row_to_signal)?;
        let mut out = Vec::new();
        for r in rows {
            let s = r?;
            if let Some(min) = f.min_severity {
                if s.severity < min {
                    continue;
                }
            }
            out.push(s);
            if out.len() >= hard_limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn get_signal(&self, id: &str) -> Result<Option<Signal>> {
        let conn = self.lock();
        let sig = conn
            .query_row(
                "SELECT id, source, external_id, kind, title, body, url, actor, entities, \
                        severity, state, occurred_at, ingested_at, thread, raw, tags \
                 FROM signals WHERE id = ?1",
                [id],
                row_to_signal,
            )
            .optional()?;
        Ok(sig)
    }

    /// Set a signal's classifier tags (per-message routing labels).
    pub fn set_signal_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE signals SET tags=?2 WHERE id=?1",
            params![id, json(&tags)?],
        )?;
        Ok(())
    }

    pub fn signals_for_thread(&self, thread_id: &str) -> Result<Vec<Signal>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, source, external_id, kind, title, body, url, actor, entities, \
                    severity, state, occurred_at, ingested_at, thread, raw, tags \
             FROM signals WHERE thread = ?1 ORDER BY occurred_at ASC",
        )?;
        let rows = stmt.query_map([thread_id], row_to_signal)?;
        collect(rows)
    }

    /// Signals occurring at or after `since` — the candidate pool for grouping.
    pub fn signals_since(&self, since: DateTime<Utc>) -> Result<Vec<Signal>> {
        self.list_signals(&SignalFilter {
            since: Some(since),
            ..Default::default()
        })
    }

    /// Keyword search across title + body, newest first.
    pub fn search_signals(&self, query: &str, limit: usize) -> Result<Vec<Signal>> {
        let conn = self.lock();
        let escaped: String = query.chars().filter(|c| !matches!(c, '%' | '_')).collect();
        let like = format!("%{escaped}%");
        let mut stmt = conn.prepare(
            "SELECT id, source, external_id, kind, title, body, url, actor, entities, \
                    severity, state, occurred_at, ingested_at, thread, raw, tags \
             FROM signals WHERE title LIKE ?1 OR body LIKE ?1 \
             ORDER BY occurred_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like, limit as i64], row_to_signal)?;
        collect(rows)
    }

    pub fn set_state(&self, id: &str, state: State) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE signals SET state = ?2 WHERE id = ?1",
            params![id, json(&state)?],
        )?;
        Ok(())
    }

    /// Resolve locally active GitHub notifications that are absent from a
    /// complete snapshot of GitHub's unread notifications feed.
    pub fn resolve_missing_github_notifications(
        &self,
        active_ids: &BTreeSet<String>,
    ) -> Result<Vec<Signal>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut candidates = {
            let mut stmt = tx.prepare(
                "SELECT id, source, external_id, kind, title, body, url, actor, entities, \
                        severity, state, occurred_at, ingested_at, thread, raw, tags \
                 FROM signals WHERE source = ?1 AND state != ?2",
            )?;
            let rows = stmt.query_map(
                params![Source::GitHub.as_str(), json(&State::Resolved)?],
                row_to_signal,
            )?;
            collect(rows)?
        };

        let mut resolved = Vec::new();
        for signal in &mut candidates {
            let notification_id = signal
                .raw
                .get("thread_id")
                .and_then(|v| v.as_str())
                .or_else(|| signal.external_id.split_once('@').map(|(id, _)| id))
                .unwrap_or(&signal.external_id);
            if active_ids.contains(notification_id) {
                continue;
            }
            tx.execute(
                "UPDATE signals SET state = ?2 WHERE id = ?1",
                params![signal.id, json(&State::Resolved)?],
            )?;
            signal.state = State::Resolved;
            resolved.push(signal.clone());
        }
        tx.commit()?;
        Ok(resolved)
    }

    /// Delete every persisted board event and the derived analysis attached to
    /// those events. Configuration, credentials, memories, context sources, and
    /// saved chats are intentionally outside this reset boundary.
    ///
    /// Returns the number of deleted signals and the distinct thread ids that
    /// were affected, so the caller can reset notification dedup and broadcast
    /// the empty authoritative board.
    pub fn clear_board_events(&self) -> Result<(usize, Vec<String>)> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut threads = BTreeSet::new();
        {
            let mut stmt =
                tx.prepare("SELECT DISTINCT thread FROM signals WHERE thread IS NOT NULL")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for t in rows {
                threads.insert(t?);
            }
        }
        let cleared = tx.execute("DELETE FROM signals", [])?;
        // These tables are derived from the event/thread graph. Clearing them
        // prevents old summaries, relation pins, hints, or recommendations from
        // being attached to a later, unrelated board entry.
        tx.execute("DELETE FROM thread_edges", [])?;
        tx.execute("DELETE FROM thread_context", [])?;
        tx.execute("DELETE FROM thread_mitigations", [])?;
        tx.execute("DELETE FROM hints", [])?;
        tx.execute("DELETE FROM threads", [])?;
        tx.commit()?;
        Ok((cleared, threads.into_iter().collect()))
    }

    pub fn set_signal_thread(&self, id: &str, thread: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE signals SET thread = ?2 WHERE id = ?1",
            params![id, thread],
        )?;
        Ok(())
    }

    /// Thread ids still referenced by signals after their metadata row was
    /// removed. This should never normally happen, but lets the correlator repair
    /// an interrupted or concurrently racing merge without losing signals.
    pub fn orphaned_thread_ids(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT s.thread FROM signals s \
             LEFT JOIN threads t ON t.id = s.thread \
             WHERE s.thread IS NOT NULL AND t.id IS NULL",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        collect(rows)
    }

    // ---- threads ------------------------------------------------------------

    pub fn upsert_thread(&self, t: &Thread) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO threads (id, title, summary, created_at, updated_at, last_reasoned_at, live, tags, tags_pinned) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
             ON CONFLICT(id) DO UPDATE SET \
                title=excluded.title, summary=excluded.summary, updated_at=excluded.updated_at, \
                last_reasoned_at=excluded.last_reasoned_at, live=excluded.live, \
                tags=excluded.tags, tags_pinned=excluded.tags_pinned",
            params![
                t.id,
                t.title,
                t.summary,
                t.created_at.to_rfc3339(),
                t.updated_at.to_rfc3339(),
                t.last_reasoned_at.map(|d| d.to_rfc3339()),
                t.live as i64,
                json(&t.tags)?,
                t.tags_pinned as i64,
            ],
        )?;
        Ok(())
    }

    /// Cache a thread's generated mitigations (a JSON array). Overwrites any prior
    /// set — the newest reanalysis wins.
    pub fn set_thread_mitigations(
        &self,
        thread_id: &str,
        mitigations: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO thread_mitigations (thread_id, json, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(thread_id) DO UPDATE SET json=excluded.json, created_at=excluded.created_at",
            params![thread_id, mitigations.to_string(), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// The cached mitigations for a thread, if any have been generated.
    pub fn get_thread_mitigations(&self, thread_id: &str) -> Result<Option<serde_json::Value>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row(
                "SELECT json FROM thread_mitigations WHERE thread_id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match row {
            Some(s) => serde_json::from_str(&s).ok(),
            None => None,
        })
    }

    /// Set a thread's tags. `pinned` marks them human-authored so the classifier
    /// won't overwrite them on the next pass.
    pub fn set_thread_tags(&self, id: &str, tags: &[String], pinned: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE threads SET tags=?2, tags_pinned=?3 WHERE id=?1",
            params![id, json(&tags)?, pinned as i64],
        )?;
        Ok(())
    }

    pub fn get_thread(&self, id: &str) -> Result<Option<Thread>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, title, summary, created_at, updated_at, last_reasoned_at, live, tags, tags_pinned \
                 FROM threads WHERE id = ?1",
                [id],
                row_to_thread,
            )
            .optional()?)
    }

    pub fn list_threads(&self) -> Result<Vec<Thread>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, title, summary, created_at, updated_at, last_reasoned_at, live, tags, tags_pinned \
             FROM threads ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_thread)?;
        collect(rows)
    }

    /// Delete a thread if it has no member signals. Returns whether it was removed.
    pub fn delete_thread_if_empty(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM signals WHERE thread = ?1",
            [id],
            |r| r.get(0),
        )?;
        if count == 0 {
            conn.execute("DELETE FROM threads WHERE id = ?1", [id])?;
            conn.execute("DELETE FROM thread_context WHERE thread_id = ?1", [id])?;
            conn.execute(
                "DELETE FROM thread_edges WHERE thread_a = ?1 OR thread_b = ?1",
                [id],
            )?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn set_thread_summary(
        &self,
        id: &str,
        summary: &str,
        reasoned_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE threads SET summary=?2, last_reasoned_at=?3, updated_at=?3 WHERE id=?1",
            params![id, summary, reasoned_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn set_thread_live(&self, id: &str, live: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE threads SET live=?2 WHERE id=?1",
            params![id, live as i64],
        )?;
        Ok(())
    }

    // ---- relation graph -----------------------------------------------------

    /// Upsert an edge. A `user` pin always overwrites; an `llm` verdict never
    /// overwrites an existing `user` pin (pins win).
    pub fn put_edge(&self, e: &Edge) -> Result<()> {
        let (a, b, e) = normalize_edge(e);
        let conn = self.lock();
        if e.provenance == Provenance::Llm {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT provenance FROM thread_edges WHERE thread_a=?1 AND thread_b=?2",
                    params![a, b],
                    |r| r.get(0),
                )
                .optional()?;
            if existing.as_deref() == Some("user") {
                return Ok(());
            }
        }
        conn.execute(
            "INSERT INTO thread_edges (thread_a, thread_b, kind, provenance, confidence, rationale, signals, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
             ON CONFLICT(thread_a, thread_b) DO UPDATE SET \
                kind=excluded.kind, provenance=excluded.provenance, confidence=excluded.confidence, \
                rationale=excluded.rationale, signals=excluded.signals, created_at=excluded.created_at",
            params![
                a,
                b,
                e.kind.as_str(),
                e.provenance.as_str(),
                e.confidence,
                e.rationale,
                json(&e.signals)?,
                e.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn edges_for_thread(&self, id: &str) -> Result<Vec<Edge>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT thread_a, thread_b, kind, provenance, confidence, rationale, signals, created_at \
             FROM thread_edges WHERE thread_a=?1 OR thread_b=?1",
        )?;
        let rows = stmt.query_map([id], row_to_edge)?;
        collect(rows)
    }

    pub fn get_edge(&self, a: &str, b: &str) -> Result<Option<Edge>> {
        let (a, b) = order(a, b);
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT thread_a, thread_b, kind, provenance, confidence, rationale, signals, created_at \
                 FROM thread_edges WHERE thread_a=?1 AND thread_b=?2",
                params![a, b],
                row_to_edge,
            )
            .optional()?)
    }

    pub fn all_edges(&self) -> Result<Vec<Edge>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT thread_a, thread_b, kind, provenance, confidence, rationale, signals, created_at \
             FROM thread_edges",
        )?;
        let rows = stmt.query_map([], row_to_edge)?;
        collect(rows)
    }

    // ---- per-thread context -------------------------------------------------

    pub fn add_thread_context(&self, c: &ThreadContext) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO thread_context (id, thread_id, kind, content, summary, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                c.id,
                c.thread_id,
                c.kind.as_str(),
                c.content,
                c.summary,
                c.created_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn thread_context(&self, thread_id: &str) -> Result<Vec<ThreadContext>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, kind, content, summary, created_at \
             FROM thread_context WHERE thread_id=?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([thread_id], row_to_thread_context)?;
        collect(rows)
    }

    // ---- memory -------------------------------------------------------------

    pub fn put_memory(&self, m: &Memory, embedding: &[u8]) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO memory (id, text, summary, links, created_at, updated_at, tags, tags_pinned, embedding) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
             ON CONFLICT(id) DO UPDATE SET \
                text=excluded.text, summary=excluded.summary, links=excluded.links, \
                updated_at=excluded.updated_at, embedding=excluded.embedding",
            params![
                m.id,
                m.text,
                m.summary,
                json(&m.links)?,
                m.created_at.to_rfc3339(),
                m.updated_at.to_rfc3339(),
                json(&m.tags)?,
                m.tags_pinned as i64,
                embedding,
            ],
        )?;
        Ok(())
    }

    /// Set a memory's tags. `pinned` marks them human-authored so auto-tagging
    /// won't overwrite them.
    pub fn set_memory_tags(&self, id: &str, tags: &[String], pinned: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE memory SET tags=?2, tags_pinned=?3 WHERE id=?1",
            params![id, json(&tags)?, pinned as i64],
        )?;
        Ok(())
    }

    /// Memory entries carrying any of the given tags — the categorical routing
    /// lookup used to ground reasoning before the vector fill.
    pub fn memory_by_tags(&self, tags: &[String]) -> Result<Vec<Memory>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: BTreeSet<&str> = tags.iter().map(String::as_str).collect();
        Ok(self
            .list_memories()?
            .into_iter()
            .filter(|m| m.tags.iter().any(|t| wanted.contains(t.as_str())))
            .collect())
    }

    pub fn get_memory(&self, id: &str) -> Result<Option<Memory>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, text, summary, links, created_at, updated_at, tags, tags_pinned FROM memory WHERE id=?1",
                [id],
                row_to_memory,
            )
            .optional()?)
    }

    pub fn list_memories(&self) -> Result<Vec<Memory>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, text, summary, links, created_at, updated_at, tags, tags_pinned FROM memory ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_memory)?;
        collect(rows)
    }

    pub fn delete_memory(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM memory WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn all_memory_embeddings(&self) -> Result<Vec<(Memory, Vec<u8>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, text, summary, links, created_at, updated_at, tags, tags_pinned, embedding FROM memory",
        )?;
        let rows = stmt.query_map([], |row| {
            let mem = row_to_memory(row)?;
            let blob: Option<Vec<u8>> = row.get(8)?;
            Ok((mem, blob.unwrap_or_default()))
        })?;
        collect(rows)
    }

    // ---- context library ----------------------------------------------------

    /// Upsert a context source. When `embedding` is `None` the stored embedding
    /// is preserved (a metadata-only update); `Some` replaces it.
    pub fn put_context(&self, c: &CtxEntry, embedding: Option<&[u8]>) -> Result<()> {
        let conn = self.lock();
        if let Some(emb) = embedding {
            conn.execute(
                "INSERT INTO context (id, kind, location, credential, header, summary, raw, etag, \
                    last_modified, mtime, fetched_at, refresh_interval, created_at, tags, tags_pinned, embedding) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) \
                 ON CONFLICT(id) DO UPDATE SET \
                    kind=excluded.kind, location=excluded.location, credential=excluded.credential, \
                    header=excluded.header, summary=excluded.summary, raw=excluded.raw, etag=excluded.etag, \
                    last_modified=excluded.last_modified, mtime=excluded.mtime, fetched_at=excluded.fetched_at, \
                    refresh_interval=excluded.refresh_interval, tags=excluded.tags, \
                    tags_pinned=excluded.tags_pinned, embedding=excluded.embedding",
                params![
                    c.id, c.kind.as_str(), c.location, c.credential, c.header, c.summary, c.raw,
                    c.etag, c.last_modified, c.mtime, c.fetched_at.map(|d| d.to_rfc3339()),
                    c.refresh_interval, c.created_at.to_rfc3339(), json(&c.tags)?, c.tags_pinned as i64, emb,
                ],
            )?;
        } else {
            conn.execute(
                "INSERT INTO context (id, kind, location, credential, header, summary, raw, etag, \
                    last_modified, mtime, fetched_at, refresh_interval, created_at, tags, tags_pinned) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) \
                 ON CONFLICT(id) DO UPDATE SET \
                    kind=excluded.kind, location=excluded.location, credential=excluded.credential, \
                    header=excluded.header, summary=excluded.summary, raw=excluded.raw, etag=excluded.etag, \
                    last_modified=excluded.last_modified, mtime=excluded.mtime, fetched_at=excluded.fetched_at, \
                    refresh_interval=excluded.refresh_interval, tags=excluded.tags, tags_pinned=excluded.tags_pinned",
                params![
                    c.id, c.kind.as_str(), c.location, c.credential, c.header, c.summary, c.raw,
                    c.etag, c.last_modified, c.mtime, c.fetched_at.map(|d| d.to_rfc3339()),
                    c.refresh_interval, c.created_at.to_rfc3339(), json(&c.tags)?, c.tags_pinned as i64,
                ],
            )?;
        }
        Ok(())
    }

    /// Set a context's tags. `pinned` marks them human-authored so auto-tagging
    /// won't overwrite them.
    pub fn set_context_tags(&self, id: &str, tags: &[String], pinned: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE context SET tags=?2, tags_pinned=?3 WHERE id=?1",
            params![id, json(&tags)?, pinned as i64],
        )?;
        Ok(())
    }

    /// Context sources carrying any of the given tags — the categorical routing
    /// lookup used to ground reasoning before the vector fill.
    pub fn context_by_tags(&self, tags: &[String]) -> Result<Vec<CtxEntry>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: BTreeSet<&str> = tags.iter().map(String::as_str).collect();
        Ok(self
            .list_context()?
            .into_iter()
            .filter(|c| c.tags.iter().any(|t| wanted.contains(t.as_str())))
            .collect())
    }

    pub fn get_context(&self, id: &str) -> Result<Option<CtxEntry>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, kind, location, credential, header, summary, raw, etag, last_modified, \
                        mtime, fetched_at, refresh_interval, created_at, tags, tags_pinned FROM context WHERE id=?1",
                [id],
                row_to_context,
            )
            .optional()?)
    }

    pub fn list_context(&self) -> Result<Vec<CtxEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, location, credential, header, summary, raw, etag, last_modified, \
                    mtime, fetched_at, refresh_interval, created_at, tags, tags_pinned FROM context ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_context)?;
        collect(rows)
    }

    pub fn delete_context(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM context WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn all_context_embeddings(&self) -> Result<Vec<(CtxEntry, Vec<u8>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, location, credential, header, summary, raw, etag, last_modified, \
                    mtime, fetched_at, refresh_interval, created_at, tags, tags_pinned, embedding FROM context",
        )?;
        let rows = stmt.query_map([], |row| {
            let ctx = row_to_context(row)?;
            let blob: Option<Vec<u8>> = row.get(15)?;
            Ok((ctx, blob.unwrap_or_default()))
        })?;
        collect(rows)
    }

    // ---- tag vocabulary -----------------------------------------------------

    /// The tag vocabulary with summaries — what the classifier reads to decide
    /// which tags apply to an incoming issue.
    pub fn list_tags(&self) -> Result<Vec<Tag>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT name, summary, created_at FROM tags ORDER BY name ASC")?;
        let rows = stmt.query_map([], row_to_tag)?;
        collect(rows)
    }

    pub fn get_tag(&self, name: &str) -> Result<Option<Tag>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT name, summary, created_at FROM tags WHERE name=?1",
                [name],
                row_to_tag,
            )
            .optional()?)
    }

    /// Register a tag if new; fill a blank summary but never clobber an existing
    /// (possibly human-edited) one. Used when auto-tagging coins a tag.
    pub fn ensure_tag(&self, name: &str, summary: &str, now: DateTime<Utc>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tags (name, summary, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(name) DO UPDATE SET \
                summary = CASE WHEN tags.summary = '' THEN excluded.summary ELSE tags.summary END",
            params![name, summary, now.to_rfc3339()],
        )?;
        Ok(())
    }

    /// Set (overwrite) a tag's summary, registering the tag if needed.
    pub fn set_tag_summary(&self, name: &str, summary: &str, now: DateTime<Utc>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tags (name, summary, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(name) DO UPDATE SET summary=excluded.summary",
            params![name, summary, now.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn delete_tag(&self, name: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM tags WHERE name=?1", [name])?;
        Ok(())
    }

    /// Rewrite a tag across all tagged content — contexts, memories, threads, and
    /// signals. `into = Some(x)` renames/merges `from`→`x` (de-duplicated, pin
    /// flags preserved); `into = None` strips the tag. Returns the rows changed.
    /// The vocabulary row itself is managed by the caller (delete/ensure).
    pub fn rewrite_tag_in_content(&self, from: &str, into: Option<&str>) -> Result<usize> {
        let mut changed = 0;
        for c in self.list_context()? {
            if c.tags.iter().any(|t| t == from) {
                self.set_context_tags(&c.id, &remap_tags(&c.tags, from, into), c.tags_pinned)?;
                changed += 1;
            }
        }
        for m in self.list_memories()? {
            if m.tags.iter().any(|t| t == from) {
                self.set_memory_tags(&m.id, &remap_tags(&m.tags, from, into), m.tags_pinned)?;
                changed += 1;
            }
        }
        for t in self.list_threads()? {
            if t.tags.iter().any(|x| x == from) {
                self.set_thread_tags(&t.id, &remap_tags(&t.tags, from, into), t.tags_pinned)?;
                changed += 1;
            }
        }
        for s in self.signals_with_tag(from)? {
            self.set_signal_tags(&s.id, &remap_tags(&s.tags, from, into))?;
            changed += 1;
        }
        Ok(changed)
    }

    /// Signals carrying `tag` (matched against the stored JSON array). Tags are
    /// kebab-case alphanumerics, so the substring match has no false positives.
    fn signals_with_tag(&self, tag: &str) -> Result<Vec<Signal>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, source, external_id, kind, title, body, url, actor, entities, \
                    severity, state, occurred_at, ingested_at, thread, raw, tags \
             FROM signals WHERE tags LIKE ?1",
        )?;
        let pattern = format!("%\"{tag}\"%");
        let rows = stmt.query_map([pattern], row_to_signal)?;
        collect(rows)
    }

    // ---- hints (live assist) ------------------------------------------------

    pub fn put_hint(&self, h: &Hint) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO hints (id, thread_id, kind, flag_type, text, rationale, citations, confidence, state, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
             ON CONFLICT(id) DO UPDATE SET \
                kind=excluded.kind, flag_type=excluded.flag_type, text=excluded.text, \
                rationale=excluded.rationale, citations=excluded.citations, confidence=excluded.confidence, \
                state=excluded.state",
            params![
                h.id,
                h.thread_id,
                h.kind.as_str(),
                h.flag_type.map(|f| f.as_str()),
                h.text,
                h.rationale,
                json(&h.citations)?,
                h.confidence,
                h.state.as_str(),
                h.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_hint(&self, id: &str) -> Result<Option<Hint>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, thread_id, kind, flag_type, text, rationale, citations, confidence, state, created_at \
                 FROM hints WHERE id=?1",
                [id],
                row_to_hint,
            )
            .optional()?)
    }

    /// Active hints, optionally scoped to one thread.
    pub fn list_hints(&self, thread_id: Option<&str>) -> Result<Vec<Hint>> {
        let conn = self.lock();
        let mut out = Vec::new();
        if let Some(tid) = thread_id {
            let mut stmt = conn.prepare(
                "SELECT id, thread_id, kind, flag_type, text, rationale, citations, confidence, state, created_at \
                 FROM hints WHERE thread_id=?1 AND state='active' ORDER BY created_at DESC",
            )?;
            for r in stmt.query_map([tid], row_to_hint)? {
                out.push(r?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, thread_id, kind, flag_type, text, rationale, citations, confidence, state, created_at \
                 FROM hints WHERE state='active' ORDER BY created_at DESC",
            )?;
            for r in stmt.query_map([], row_to_hint)? {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn set_hint_state(&self, id: &str, state: HintState) -> Result<()> {
        self.lock().execute(
            "UPDATE hints SET state=?2 WHERE id=?1",
            params![id, state.as_str()],
        )?;
        Ok(())
    }

    /// Clear a thread's active hints before a fresh live-assist pass re-populates them.
    pub fn clear_active_hints(&self, thread_id: &str) -> Result<()> {
        self.lock().execute(
            "DELETE FROM hints WHERE thread_id=?1 AND state='active'",
            [thread_id],
        )?;
        Ok(())
    }

    // ---- source health ------------------------------------------------------

    pub fn record_health(
        &self,
        source: &str,
        ok: bool,
        detail: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO source_health (source, last_poll_at, last_ok_at, ok, detail, cursor) \
             VALUES (?1, ?2, CASE WHEN ?3 THEN ?2 ELSE NULL END, ?3, ?4, ?5) \
             ON CONFLICT(source) DO UPDATE SET \
                last_poll_at=?2, \
                last_ok_at=CASE WHEN ?3 THEN ?2 ELSE source_health.last_ok_at END, \
                ok=?3, detail=?4, \
                cursor=COALESCE(?5, source_health.cursor)",
            params![source, now, ok as i64, detail, cursor],
        )?;
        Ok(())
    }

    pub fn source_health(&self) -> Result<Vec<SourceHealth>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT source, last_poll_at, last_ok_at, ok, detail, cursor FROM source_health ORDER BY source",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SourceHealth {
                source: r.get(0)?,
                last_poll_at: r.get(1)?,
                last_ok_at: r.get(2)?,
                ok: r.get::<_, i64>(3)? != 0,
                detail: r.get(4)?,
                cursor: r.get(5)?,
            })
        })?;
        collect(rows)
    }

    // ---- credentials --------------------------------------------------------

    /// Fetch a stored secret by account name. Returns `Ok(None)` when absent.
    pub fn credential_get(&self, account: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT secret FROM credentials WHERE account = ?1",
            params![account],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Store (or overwrite) a secret by account name.
    pub fn credential_set(&self, account: &str, secret: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO credentials (account, secret) VALUES (?1, ?2) \
             ON CONFLICT(account) DO UPDATE SET secret = excluded.secret",
            params![account, secret],
        )?;
        Ok(())
    }

    /// Delete a secret. Missing entries are treated as success.
    pub fn credential_delete(&self, account: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM credentials WHERE account = ?1",
            params![account],
        )?;
        Ok(())
    }

    // ---- agent chats --------------------------------------------------------

    /// Chat conversations, newest activity first (metadata only).
    pub fn list_chats(&self) -> Result<Vec<ChatSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, title, created_at, updated_at FROM chats ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ChatSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })?;
        collect(rows)
    }

    pub fn get_chat(&self, id: &str) -> Result<Option<StoredChat>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, title, messages, created_at, updated_at FROM chats WHERE id = ?1",
            [id],
            |r| {
                let raw: String = r.get(2)?;
                Ok(StoredChat {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    messages: serde_json::from_str(&raw).map_err(|e| conv_err(2, e.to_string()))?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Insert or update a chat. `created_at` is preserved across updates; both the
    /// row's `updated_at` and the chat's ordering key move to now.
    pub fn upsert_chat(&self, id: &str, title: &str, messages: &serde_json::Value) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO chats (id, title, messages, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?4) \
             ON CONFLICT(id) DO UPDATE SET \
                title=excluded.title, messages=excluded.messages, updated_at=excluded.updated_at",
            params![id, title, messages.to_string(), now],
        )?;
        Ok(())
    }

    /// Delete a chat. Missing ids are treated as success.
    pub fn delete_chat(&self, id: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM chats WHERE id = ?1", [id])?;
        Ok(())
    }
}

// ---- helpers ----------------------------------------------------------------

fn json<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}

fn collect<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn order<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn normalize_edge(e: &Edge) -> (String, String, Edge) {
    let (a, b) = order(&e.thread_a, &e.thread_b);
    let mut norm = e.clone();
    norm.thread_a = a.to_string();
    norm.thread_b = b.to_string();
    (a.to_string(), b.to_string(), norm)
}

fn row_to_signal(row: &Row) -> rusqlite::Result<Signal> {
    let source_str: String = row.get(1)?;
    let source = Source::parse(&source_str)
        .ok_or_else(|| conv_err(1, format!("unknown source '{source_str}'")))?;
    Ok(Signal {
        id: row.get(0)?,
        source,
        external_id: row.get(2)?,
        kind: from_json::<SignalKind>(row, 3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        url: row.get(6)?,
        actor: row.get(7)?,
        entities: from_json::<Vec<Entity>>(row, 8)?,
        severity: from_json::<Severity>(row, 9)?,
        state: from_json::<State>(row, 10)?,
        occurred_at: parse_ts(row, 11)?,
        ingested_at: parse_ts(row, 12)?,
        thread: row.get(13)?,
        raw: {
            let s: String = row.get(14)?;
            serde_json::from_str(&s).map_err(|e| conv_err(14, e.to_string()))?
        },
        tags: from_json::<Vec<String>>(row, 15)?,
    })
}

fn row_to_thread(row: &Row) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: row.get(0)?,
        title: row.get(1)?,
        summary: row.get(2)?,
        created_at: parse_ts(row, 3)?,
        updated_at: parse_ts(row, 4)?,
        last_reasoned_at: parse_ts_opt(row, 5)?,
        live: row.get::<_, i64>(6)? != 0,
        tags: from_json::<Vec<String>>(row, 7)?,
        tags_pinned: row.get::<_, i64>(8)? != 0,
    })
}

fn row_to_edge(row: &Row) -> rusqlite::Result<Edge> {
    let kind_s: String = row.get(2)?;
    let prov_s: String = row.get(3)?;
    Ok(Edge {
        thread_a: row.get(0)?,
        thread_b: row.get(1)?,
        kind: RelationKind::parse(&kind_s)
            .ok_or_else(|| conv_err(2, format!("bad relation kind '{kind_s}'")))?,
        provenance: match prov_s.as_str() {
            "user" => Provenance::User,
            _ => Provenance::Llm,
        },
        confidence: row.get(4)?,
        rationale: row.get(5)?,
        signals: from_json::<Vec<String>>(row, 6)?,
        created_at: parse_ts(row, 7)?,
    })
}

fn row_to_thread_context(row: &Row) -> rusqlite::Result<ThreadContext> {
    let kind_s: String = row.get(2)?;
    Ok(ThreadContext {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        kind: ContextKind::parse(&kind_s)
            .ok_or_else(|| conv_err(2, format!("bad context kind '{kind_s}'")))?,
        content: row.get(3)?,
        summary: row.get(4)?,
        created_at: parse_ts(row, 5)?,
    })
}

/// Apply a tag rewrite to one tag list: replace `from` with `into` (or drop it
/// when `into` is `None`), de-duplicating while preserving order.
fn remap_tags(tags: &[String], from: &str, into: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let mapped = if t == from {
            into.map(str::to_string)
        } else {
            Some(t.clone())
        };
        if let Some(m) = mapped {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

fn row_to_tag(row: &Row) -> rusqlite::Result<Tag> {
    Ok(Tag {
        name: row.get(0)?,
        summary: row.get(1)?,
        created_at: parse_ts(row, 2)?,
    })
}

fn row_to_memory(row: &Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        text: row.get(1)?,
        summary: row.get(2)?,
        links: from_json::<Vec<String>>(row, 3)?,
        created_at: parse_ts(row, 4)?,
        updated_at: parse_ts(row, 5)?,
        tags: from_json::<Vec<String>>(row, 6)?,
        tags_pinned: row.get::<_, i64>(7)? != 0,
    })
}

fn row_to_context(row: &Row) -> rusqlite::Result<CtxEntry> {
    let kind_s: String = row.get(1)?;
    Ok(CtxEntry {
        id: row.get(0)?,
        kind: ContextSourceKind::parse(&kind_s)
            .ok_or_else(|| conv_err(1, format!("bad context source kind '{kind_s}'")))?,
        location: row.get(2)?,
        credential: row.get(3)?,
        header: row.get(4)?,
        summary: row.get(5)?,
        raw: row.get(6)?,
        etag: row.get(7)?,
        last_modified: row.get(8)?,
        mtime: row.get(9)?,
        fetched_at: parse_ts_opt(row, 10)?,
        refresh_interval: row.get(11)?,
        created_at: parse_ts(row, 12)?,
        tags: from_json::<Vec<String>>(row, 13)?,
        tags_pinned: row.get::<_, i64>(14)? != 0,
    })
}

fn row_to_hint(row: &Row) -> rusqlite::Result<Hint> {
    let kind_s: String = row.get(2)?;
    let flag_s: Option<String> = row.get(3)?;
    let state_s: String = row.get(8)?;
    Ok(Hint {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        kind: HintKind::parse(&kind_s)
            .ok_or_else(|| conv_err(2, format!("bad hint kind '{kind_s}'")))?,
        flag_type: flag_s.and_then(|s| FlagType::parse(&s)),
        text: row.get(4)?,
        rationale: row.get(5)?,
        citations: from_json::<Vec<String>>(row, 6)?,
        confidence: row.get(7)?,
        state: HintState::parse(&state_s)
            .ok_or_else(|| conv_err(8, format!("bad hint state '{state_s}'")))?,
        created_at: parse_ts(row, 9)?,
    })
}

fn from_json<T: serde::de::DeserializeOwned>(row: &Row, idx: usize) -> rusqlite::Result<T> {
    let s: String = row.get(idx)?;
    serde_json::from_str(&s).map_err(|e| conv_err(idx, e.to_string()))
}

fn parse_ts(row: &Row, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = row.get(idx)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| conv_err(idx, e.to_string()))
}

fn parse_ts_opt(row: &Row, idx: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let s: Option<String> = row.get(idx)?;
    match s {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(&s)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|e| conv_err(idx, e.to_string())),
    }
}

fn conv_err(idx: usize, msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(StoreDecodeError(msg)))
}

#[derive(Debug)]
struct StoreDecodeError(String);

impl std::fmt::Display for StoreDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store decode error: {}", self.0)
    }
}

impl std::error::Error for StoreDecodeError {}

#[cfg(test)]
impl Store {
    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// Idempotent column additions for databases created before a column existed.
/// New tables come from [`SCHEMA`] (all `IF NOT EXISTS`); only added columns on
/// pre-existing tables need this. `ALTER TABLE ADD COLUMN` is a no-op-safe here
/// because we guard on `PRAGMA table_info`.
fn migrate(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "threads", "tags", "TEXT NOT NULL DEFAULT '[]'")?;
    add_column_if_missing(conn, "threads", "tags_pinned", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "context", "tags", "TEXT NOT NULL DEFAULT '[]'")?;
    add_column_if_missing(conn, "context", "tags_pinned", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "memory", "tags", "TEXT NOT NULL DEFAULT '[]'")?;
    add_column_if_missing(conn, "memory", "tags_pinned", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "signals", "tags", "TEXT NOT NULL DEFAULT '[]'")?;
    Ok(())
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    if !existing.iter().any(|c| c == column) {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Entity, Severity, SignalKind, Source, State};
    use chrono::Utc;

    fn sample(ext: &str) -> Signal {
        Signal {
            id: Signal::make_id(Source::GitHub, ext),
            source: Source::GitHub,
            external_id: ext.into(),
            kind: SignalKind::Mention,
            title: "hi".into(),
            body: Some("body".into()),
            url: Some("https://example.com".into()),
            actor: None,
            entities: vec![Entity::new("repo", "o/r")],
            severity: Severity::Warning,
            state: State::Unseen,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            thread: None,
            raw: serde_json::json!({ "k": "v" }),
            tags: Vec::new(),
        }
    }

    #[test]
    fn tags_roundtrip_and_lookup() {
        use crate::context::{Context as Ctx, ContextSourceKind};
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();

        // Vocabulary: ensure_tag fills a blank summary but won't clobber a set one.
        store.ensure_tag("database", "db runbooks", now).unwrap();
        store
            .ensure_tag("database", "SHOULD NOT OVERWRITE", now)
            .unwrap();
        assert_eq!(
            store.get_tag("database").unwrap().unwrap().summary,
            "db runbooks"
        );
        store.set_tag_summary("database", "edited", now).unwrap();
        assert_eq!(
            store.get_tag("database").unwrap().unwrap().summary,
            "edited"
        );

        let ctx = Ctx {
            id: "ctx/1".into(),
            kind: ContextSourceKind::File,
            location: "r.md".into(),
            credential: None,
            header: None,
            tags: vec!["database".into(), "postgres".into()],
            tags_pinned: true,
            summary: Some("s".into()),
            raw: Some("b".into()),
            etag: None,
            last_modified: None,
            mtime: None,
            fetched_at: None,
            refresh_interval: "6h".into(),
            created_at: now,
        };
        store.put_context(&ctx, None).unwrap();
        let got = store.get_context("ctx/1").unwrap().unwrap();
        assert_eq!(got.tags, vec!["database".to_string(), "postgres".into()]);
        assert!(got.tags_pinned);

        // by-tag lookup
        let hits = store.context_by_tags(&["postgres".to_string()]).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(store
            .context_by_tags(&["missing".to_string()])
            .unwrap()
            .is_empty());
        assert!(store.context_by_tags(&[]).unwrap().is_empty());

        // thread tags round-trip
        let t = Thread {
            id: "thr/1".into(),
            title: "t".into(),
            summary: None,
            created_at: now,
            updated_at: now,
            last_reasoned_at: None,
            live: false,
            tags: vec![],
            tags_pinned: false,
        };
        store.upsert_thread(&t).unwrap();
        store
            .set_thread_tags("thr/1", &["database".to_string()], true)
            .unwrap();
        let gt = store.get_thread("thr/1").unwrap().unwrap();
        assert_eq!(gt.tags, vec!["database".to_string()]);
        assert!(gt.tags_pinned);

        // memory tags round-trip + by-tag lookup
        let mem = crate::memory::Memory {
            id: "mem/1".into(),
            text: "restart the primary".into(),
            summary: "db recovery".into(),
            links: vec![],
            tags: vec![],
            tags_pinned: false,
            created_at: now,
            updated_at: now,
        };
        store.put_memory(&mem, &[]).unwrap();
        store
            .set_memory_tags("mem/1", &["database".to_string()], true)
            .unwrap();
        let gm = store.get_memory("mem/1").unwrap().unwrap();
        assert_eq!(gm.tags, vec!["database".to_string()]);
        assert!(gm.tags_pinned);
        assert_eq!(
            store
                .memory_by_tags(&["database".to_string()])
                .unwrap()
                .len(),
            1
        );
        assert!(store.memory_by_tags(&[]).unwrap().is_empty());

        // signal tags round-trip
        store.insert_signal(&sample("sig-tagged")).unwrap();
        let sid = Signal::make_id(Source::GitHub, "sig-tagged");
        store
            .set_signal_tags(&sid, &["database".to_string()])
            .unwrap();
        let gs = store.get_signal(&sid).unwrap().unwrap();
        assert_eq!(gs.tags, vec!["database".to_string()]);

        // --- merge (rename) `database` → `postgres` across all content ---
        let moved = store
            .rewrite_tag_in_content("database", Some("postgres"))
            .unwrap();
        assert_eq!(moved, 4, "context, thread, memory, signal all rewritten");
        // Context already had `postgres`, so the merge de-duplicates.
        assert_eq!(
            store.get_context("ctx/1").unwrap().unwrap().tags,
            vec!["postgres".to_string()]
        );
        assert_eq!(
            store.get_thread("thr/1").unwrap().unwrap().tags,
            vec!["postgres".to_string()]
        );
        assert_eq!(
            store.get_memory("mem/1").unwrap().unwrap().tags,
            vec!["postgres".to_string()]
        );
        assert_eq!(
            store.get_signal(&sid).unwrap().unwrap().tags,
            vec!["postgres".to_string()]
        );
        // Pin flags survive the rewrite.
        assert!(store.get_context("ctx/1").unwrap().unwrap().tags_pinned);

        // --- delete strips the label everywhere ---
        let stripped = store.rewrite_tag_in_content("postgres", None).unwrap();
        assert_eq!(stripped, 4);
        assert!(store.get_context("ctx/1").unwrap().unwrap().tags.is_empty());
        assert!(store.get_signal(&sid).unwrap().unwrap().tags.is_empty());
    }

    #[test]
    fn insert_dedups_and_roundtrips() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store.insert_signal(&sample("1")).unwrap(),
            "first insert is new"
        );
        assert!(
            !store.insert_signal(&sample("1")).unwrap(),
            "duplicate (source, external_id) is ignored"
        );
        assert!(store.insert_signal(&sample("2")).unwrap());

        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        let s = &recent[0];
        assert_eq!(s.source, Source::GitHub);
        assert_eq!(s.severity, Severity::Warning);
        assert_eq!(s.entities, vec![Entity::new("repo", "o/r")]);
        assert_eq!(s.raw, serde_json::json!({ "k": "v" }));
    }

    #[test]
    fn set_state_updates() {
        let store = Store::open_in_memory().unwrap();
        store.insert_signal(&sample("1")).unwrap();
        store
            .set_state(&Signal::make_id(Source::GitHub, "1"), State::Acknowledged)
            .unwrap();
        let recent = store.recent(10).unwrap();
        assert!(matches!(recent[0].state, State::Acknowledged));
    }

    #[test]
    fn clear_board_events_deletes_signals_and_threads() {
        let store = Store::open_in_memory().unwrap();

        // Signals from three sources, on two threads. All should be resolved —
        // the reset clears the whole board, not just some sources.
        let mut gh = sample("gh1");
        gh.thread = Some("thread-x".into());
        let mut slack = sample("sl1");
        slack.source = Source::Slack;
        slack.id = Signal::make_id(Source::Slack, "sl1");
        slack.thread = Some("thread-x".into());
        let mut granola = sample("gr1");
        granola.source = Source::Granola;
        granola.id = Signal::make_id(Source::Granola, "gr1");
        granola.thread = Some("thread-y".into());
        for s in [&gh, &slack, &granola] {
            store.insert_signal(s).unwrap();
        }

        let (cleared, mut threads) = store.clear_board_events().unwrap();
        assert_eq!(cleared, 3, "every signal is deleted regardless of source");
        threads.sort();
        assert_eq!(
            threads,
            vec!["thread-x".to_string(), "thread-y".to_string()]
        );

        // The event rows and their board-level thread records are gone. A source
        // can subsequently re-ingest a still-active upstream notification.
        assert!(store.recent(10).unwrap().is_empty());
        assert!(store.list_threads().unwrap().is_empty());
        assert!(
            store.insert_signal(&gh).unwrap(),
            "re-ingest is a new event"
        );

        // The reset is idempotent once the re-ingested event is cleared too.
        let (again, _) = store.clear_board_events().unwrap();
        assert_eq!(again, 1);
        let (empty, _) = store.clear_board_events().unwrap();
        assert_eq!(empty, 0);
    }

    #[test]
    fn github_unread_snapshot_resolves_missing_notifications() {
        let store = Store::open_in_memory().unwrap();
        let mut active = sample("1@2026-07-24T10:00:00Z");
        active.raw = serde_json::json!({ "thread_id": "1" });
        let mut read = sample("2@2026-07-24T10:00:00Z");
        read.raw = serde_json::json!({ "thread_id": "2" });
        store.insert_signal(&active).unwrap();
        store.insert_signal(&read).unwrap();

        let active_ids = BTreeSet::from(["1".to_string()]);
        let resolved = store
            .resolve_missing_github_notifications(&active_ids)
            .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].external_id, read.external_id);
        assert_eq!(
            store.get_signal(&read.id).unwrap().unwrap().state,
            State::Resolved
        );
        assert_eq!(
            store.get_signal(&active.id).unwrap().unwrap().state,
            State::Unseen
        );
    }

    #[test]
    fn severity_filter_and_get() {
        let store = Store::open_in_memory().unwrap();
        store.insert_signal(&sample("1")).unwrap();
        let mut low = sample("2");
        low.severity = Severity::Info;
        store.insert_signal(&low).unwrap();

        let warns = store
            .list_signals(&SignalFilter {
                min_severity: Some(Severity::Warning),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].external_id, "1");

        let got = store
            .get_signal(&Signal::make_id(Source::GitHub, "1"))
            .unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn edges_respect_user_pins() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let user_edge = Edge {
            thread_a: "t/b".into(),
            thread_b: "t/a".into(), // deliberately reversed to test normalization
            kind: RelationKind::Distinct,
            provenance: Provenance::User,
            confidence: 1.0,
            rationale: "user says so".into(),
            signals: vec![],
            created_at: now,
        };
        store.put_edge(&user_edge).unwrap();
        // An LLM verdict must not overwrite the user pin.
        let llm_edge = Edge {
            thread_a: "t/a".into(),
            thread_b: "t/b".into(),
            kind: RelationKind::Same,
            provenance: Provenance::Llm,
            confidence: 0.9,
            rationale: "looks same".into(),
            signals: vec![],
            created_at: now,
        };
        store.put_edge(&llm_edge).unwrap();
        let e = store.get_edge("t/a", "t/b").unwrap().unwrap();
        assert_eq!(e.kind, RelationKind::Distinct);
        assert_eq!(e.provenance, Provenance::User);
    }

    #[test]
    fn health_records_and_preserves_last_ok() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_health("github", true, None, Some("cursor-1"))
            .unwrap();
        let h = &store.source_health().unwrap()[0];
        assert!(h.ok);
        assert!(h.last_ok_at.is_some());
        assert_eq!(h.cursor.as_deref(), Some("cursor-1"));

        // A failure keeps the prior last_ok_at and cursor, records the error.
        store
            .record_health("github", false, Some("boom"), None)
            .unwrap();
        let h = &store.source_health().unwrap()[0];
        assert!(!h.ok);
        assert_eq!(h.detail.as_deref(), Some("boom"));
        assert!(
            h.last_ok_at.is_some(),
            "last success timestamp is preserved"
        );
        assert_eq!(
            h.cursor.as_deref(),
            Some("cursor-1"),
            "cursor preserved when None"
        );
    }

    #[test]
    fn chats_roundtrip_list_and_delete() {
        let store = Store::open_in_memory().unwrap();
        let msgs = serde_json::json!([{ "role": "user", "content": "hi", "images": [] }]);
        store.upsert_chat("c1", "hi there", &msgs).unwrap();
        store
            .upsert_chat("c2", "second", &serde_json::json!([]))
            .unwrap();

        let list = store.list_chats().unwrap();
        assert_eq!(list.len(), 2);

        let got = store.get_chat("c1").unwrap().unwrap();
        assert_eq!(got.title, "hi there");
        assert_eq!(got.messages, msgs);
        assert!(!got.created_at.is_empty());

        // Update preserves created_at, refreshes the title.
        store.upsert_chat("c1", "renamed", &msgs).unwrap();
        let updated = store.get_chat("c1").unwrap().unwrap();
        assert_eq!(updated.title, "renamed");
        assert_eq!(updated.created_at, got.created_at);

        store.delete_chat("c1").unwrap();
        assert!(store.get_chat("c1").unwrap().is_none());
        assert_eq!(store.list_chats().unwrap().len(), 1);
    }

    #[test]
    fn memory_recall_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let m = Memory {
            id: "mem/1".into(),
            text: "restart the pod when the pool is exhausted".into(),
            summary: "pool exhaustion → restart".into(),
            links: vec!["thr/1".into()],
            created_at: now,
            updated_at: now,
            tags: Vec::new(),
            tags_pinned: false,
        };
        let vec = crate::embed::HashEmbedder::embed_sync(&m.text);
        store.put_memory(&m, &crate::embed::to_blob(&vec)).unwrap();
        let all = store.all_memory_embeddings().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0.summary, "pool exhaustion → restart");
        assert!(!all[0].1.is_empty());
    }
}
