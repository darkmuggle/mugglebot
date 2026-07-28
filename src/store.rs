//! SQLite store. One embedded, single-file store for every access pattern:
//! the append-mostly signal log, the subject **relation graph** (edges + joins),
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
use crate::correlation::{ContextKind, Edge, Provenance, RelationKind, SubjectContext};
use crate::live::{FlagType, Hint, HintKind, HintState};
use crate::memory::Memory;
use crate::signal::{ResolutionKey, Severity, Signal, SignalKind, Source};
use crate::subject::{Handled, Subject, SubjectKey, SubjectRank};
use crate::tags::Tag;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS signals (
    id            TEXT PRIMARY KEY,
    source        TEXT NOT NULL,
    external_id   TEXT NOT NULL,
    -- Upstream version of a mutable event. Part of the dedup key, so "the same
    -- notification, changed" stays distinct from "the same event again".
    version       TEXT,
    kind          TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT,
    url           TEXT,
    actor         TEXT,
    keys          TEXT NOT NULL,
    severity      TEXT NOT NULL,
    -- Gone upstream (no longer unread, issue closed). A fact about the source, not
    -- operator triage — that lives on the subject.
    upstream_gone INTEGER NOT NULL DEFAULT 0,
    occurred_at   TEXT NOT NULL,
    ingested_at   TEXT NOT NULL,
    -- The subject that owns this signal; NULL is the unattributed lane.
    subject       TEXT,
    raw           TEXT NOT NULL,
    tags          TEXT NOT NULL DEFAULT '[]',
    UNIQUE(source, external_id, version)
);
CREATE INDEX IF NOT EXISTS idx_signals_occurred ON signals(occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_signals_source   ON signals(source);
CREATE INDEX IF NOT EXISTS idx_signals_gone     ON signals(upstream_gone);
CREATE INDEX IF NOT EXISTS idx_signals_subject  ON signals(subject);

-- Subjects: the durable pieces of work, keyed by upstream identity
-- (owner/repo#412, owner/repo!987, channel/thread_ts). From Phase 2 this table is
-- the board projection of the matching Restate virtual object's state — the object
-- is addressable only by key, and the board is a cross-key query.
CREATE TABLE IF NOT EXISTS subjects (
    key              TEXT PRIMARY KEY,
    rank             TEXT NOT NULL,
    title            TEXT NOT NULL,
    summary          TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    last_reasoned_at TEXT,
    live             INTEGER NOT NULL DEFAULT 0,
    tags             TEXT NOT NULL DEFAULT '[]',
    tags_pinned      INTEGER NOT NULL DEFAULT 0,
    -- Operator triage, per subject rather than per signal.
    handled          TEXT NOT NULL DEFAULT 'open',
    snoozed_until    TEXT,
    -- Merged away into this canonical subject; activity forwards there.
    same_as          TEXT,
    -- Deterministic merge key within the Slack rank (an environment id).
    merge_key        TEXT
);
CREATE INDEX IF NOT EXISTS idx_subjects_merge ON subjects(merge_key);

-- The hierarchy: a PR under the issue it closes. Recorded the moment a signal names
-- both, which is often before either card exists — a closing keyword is a fact about
-- GitHub, not a property of a row we happen to have created.
CREATE TABLE IF NOT EXISTS subject_links (
    child      TEXT PRIMARY KEY,
    parent     TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_subject_links_parent ON subject_links(parent);

-- A human correction to the ranked climb, per signal. Kept so a re-ingest of the
-- same upstream event can't silently undo it: `subject` NULL means "this belongs to
-- nothing", which is a decision, not an absence.
CREATE TABLE IF NOT EXISTS attribution_pins (
    signal_id  TEXT PRIMARY KEY,
    subject    TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS subject_edges (
    subject_a   TEXT NOT NULL,
    subject_b   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    provenance  TEXT NOT NULL,
    confidence  REAL NOT NULL,
    rationale   TEXT NOT NULL,
    signals     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (subject_a, subject_b)
);

CREATE TABLE IF NOT EXISTS subject_context (
    id          TEXT PRIMARY KEY,
    subject_key TEXT NOT NULL,
    kind        TEXT NOT NULL,
    content     TEXT NOT NULL,
    summary     TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_subject_context ON subject_context(subject_key);

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
    subject_key   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    flag_type   TEXT,
    text        TEXT NOT NULL,
    rationale   TEXT,
    citations   TEXT NOT NULL,
    confidence  REAL NOT NULL,
    state       TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_hints_subject ON hints(subject_key);
CREATE INDEX IF NOT EXISTS idx_hints_state  ON hints(state);

CREATE TABLE IF NOT EXISTS source_health (
    source       TEXT PRIMARY KEY,
    last_poll_at TEXT,
    last_ok_at   TEXT,
    ok           INTEGER NOT NULL,
    detail       TEXT,
    cursor       TEXT
);

-- Credentials: source tokens, model API keys, authed-context secrets. This is the
-- credential store — there is no Keychain involved, deliberately (see AGENTS.md →
-- "Secrets in SQLite"). `value` is a BLOB so a sealed value needs no text
-- encoding: byte 0 is a format tag (0x00 plaintext UTF-8, 0x01 AES-256-GCM with a
-- 12-byte nonce ahead of the ciphertext).
CREATE TABLE IF NOT EXISTS secrets (
    name       TEXT PRIMARY KEY,
    value      BLOB NOT NULL,
    updated_at TEXT NOT NULL
);

-- Small key/value side table for store-level facts that aren't domain data: the
-- KDF salt, schema notes. Not a config store — config is the TOML.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS chats (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    messages    TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chats_updated ON chats(updated_at DESC);


-- An authenticated, read-only browser investigation of a dashboard link found in
-- a signal. Queued at ingest, claimed by the browser worker, which drives Chrome
-- through the agent bridge and writes its findings back here.
CREATE TABLE IF NOT EXISTS browser_investigations (
    id          TEXT PRIMARY KEY,
    signal_id   TEXT NOT NULL UNIQUE,
    url         TEXT NOT NULL,
    prompt      TEXT NOT NULL,
    status      TEXT NOT NULL,
    findings    TEXT,
    error       TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_browser_investigations_signal ON browser_investigations(signal_id);
CREATE INDEX IF NOT EXISTS idx_browser_investigations_status ON browser_investigations(status);

-- The repo index: one row per repository in the watched org, characterized from
-- its **code**. This is the routing table that turns a Slack symptom into the
-- handful of repos worth searching for a cause.
--
-- `summary` is derived by reading the checked-out tree, not the README: a README
-- describes what a project wants to be, which is often out of date, aspirational,
-- or absent, whereas the layout and the source say what it actually does.
-- `indexed_sha` is the commit the characterization was built from — the cache key,
-- so a repo is only re-read when its code has actually moved.
CREATE TABLE IF NOT EXISTS repo_index (
    full_name    TEXT PRIMARY KEY,
    description  TEXT,
    topics       TEXT NOT NULL DEFAULT '[]',
    language     TEXT,
    archived     INTEGER NOT NULL DEFAULT 0,
    pushed_at    TEXT,
    readme_etag  TEXT,
    readme       TEXT,
    summary      TEXT,
    indexed_sha  TEXT,
    digest       TEXT,
    -- What kind of repo this is: 'code', 'example', 'docs'. NULL means nobody has said and
    -- the name gave no clue, so it is treated as code until someone tags it.
    --
    -- Stored rather than derived on read *because* it is operator-taggable: a derivation would
    -- overwrite the tag on the next crawl. `kind_pinned` is what protects a human's answer from
    -- the name-matching heuristic.
    kind         TEXT,
    kind_pinned  INTEGER NOT NULL DEFAULT 0,
    fetched_at   TEXT NOT NULL
);

-- Open pull requests that may already fix an issue — possibly written by somebody
-- else, which is exactly the case you want to know about before starting work.
-- Each row is one (issue, PR) pairing plus the local model's read of it: what the
-- PR actually does, a critique of whether it really fixes the issue, and any other
-- issues it would also resolve.
CREATE TABLE IF NOT EXISTS issue_pr_fixes (
    issue_key   TEXT NOT NULL,
    pr_repo     TEXT NOT NULL,
    pr_number   INTEGER NOT NULL,
    pr_title    TEXT NOT NULL,
    pr_url      TEXT,
    pr_author   TEXT,
    pr_state    TEXT,
    files       TEXT NOT NULL DEFAULT '[]',
    -- `fixes` | `partial` | `related` | `unrelated`.
    verdict     TEXT NOT NULL,
    confidence  REAL NOT NULL DEFAULT 0,
    implementation TEXT,
    critique    TEXT,
    -- A distilled summary of the PR's review discussion, from the merit-scored
    -- comments the critique already reads. Stored so the board can show what
    -- reviewers actually said without re-fetching or re-judging it.
    conversation TEXT,
    also_fixes  TEXT NOT NULL DEFAULT '[]',
    -- Which tier produced the analysis — local, or an escalation.
    analyzed_by TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (issue_key, pr_repo, pr_number)
);
CREATE INDEX IF NOT EXISTS idx_issue_pr_fixes_issue ON issue_pr_fixes(issue_key);

-- Cached commit history per repo. The root-cause pass scans this window instead
-- of re-pulling the log on every investigation; `since`/`until` bound what the
-- cache actually covers so a wider request knows to re-fetch.
CREATE TABLE IF NOT EXISTS repo_commits (
    full_name   TEXT NOT NULL,
    sha         TEXT NOT NULL,
    author      TEXT,
    committed_at TEXT NOT NULL,
    message     TEXT NOT NULL,
    url         TEXT,
    files       TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (full_name, sha)
);
CREATE INDEX IF NOT EXISTS idx_repo_commits_time ON repo_commits(full_name, committed_at DESC);

-- What window of each repo's log the commit cache actually holds.
CREATE TABLE IF NOT EXISTS repo_commit_windows (
    full_name  TEXT PRIMARY KEY,
    since      TEXT NOT NULL,
    fetched_at TEXT NOT NULL
);

-- Cached GitHub issue/PR search results, keyed by the exact query. Symptom
-- searches repeat across re-analyses of the same thread; this keeps the search
-- API (30 req/min) out of the hot path.
-- ---------------------------------------------------------------------------------
-- The code index: what each repo is, what its components do, and what each commit
-- changed — summarized once and embedded, so "which repo and component is this issue
-- likely about?" is a retrieval rather than a crawl.
--
-- These live in SQLite and not in the `RepoIndexer` object's state, deliberately. The
-- whole point is to rank thousands of summaries against one issue, and object state is
-- addressable only by key — there is no cross-key query, so scoring from it would mean
-- loading every repo's every commit. It is also the record rather than work in flight:
-- rebuilding it costs thousands of local model calls, and `data/restate` gets wiped
-- whenever vqueues are toggled. The object owns the *indexing*; this owns the *index*.

-- One row per commit, keyed by sha — which is immutable, so a summary is computed
-- exactly once and is valid forever.
CREATE TABLE IF NOT EXISTS commit_summaries (
    full_name   TEXT NOT NULL,
    sha         TEXT NOT NULL,
    -- What the change does, in behavioural terms: the unit a symptom is matched against.
    summary     TEXT NOT NULL,
    -- Components the commit touched, derived from its changed paths.
    components  TEXT NOT NULL DEFAULT '[]',
    -- `f32` little-endian, as elsewhere; ranked in-process by cosine similarity.
    embedding   BLOB,
    -- Which tier wrote it, so a re-index on a better model is identifiable.
    model       TEXT,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (full_name, sha)
);
CREATE INDEX IF NOT EXISTS idx_commit_summaries_repo ON commit_summaries(full_name);

-- One row per component: a module root inside a repo. This is the granularity an
-- engineer actually acts on — "which component" is a more useful answer than "which
-- repo", and a far more useful one than "which commit" when the change is old.
CREATE TABLE IF NOT EXISTS component_summaries (
    full_name   TEXT NOT NULL,
    -- Repo-relative directory, e.g. `crates/partition-processor`.
    path        TEXT NOT NULL,
    -- `PURPOSE:` — what this component runs.
    purpose     TEXT,
    -- `SYMPTOMS:` — the terms that should route an incident here.
    symptoms    TEXT,
    -- File-type/module digest the summary was derived from.
    digest      TEXT,
    embedding   BLOB,
    -- The commit the summary was built from; unchanged code is not re-summarized.
    indexed_sha TEXT,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (full_name, path)
);
CREATE INDEX IF NOT EXISTS idx_component_summaries_repo ON component_summaries(full_name);

-- The dependency graph, from manifests that are actually present (see `ecosystem`).
-- What it buys over search: an issue in `restate-cloud` whose symptom matches a change
-- in `restate` scores *through the edge*, with the hop named in the rationale.
CREATE TABLE IF NOT EXISTS repo_deps (
    from_repo   TEXT NOT NULL,
    -- The resolved repo this depends on, when the dependency maps to one we index.
    to_repo     TEXT NOT NULL,
    -- The declared dependency name, kept as the citation.
    dep_name    TEXT NOT NULL,
    -- The manifest it came from, so the edge is checkable.
    source      TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (from_repo, to_repo, dep_name)
);
CREATE INDEX IF NOT EXISTS idx_repo_deps_to ON repo_deps(to_repo);

CREATE TABLE IF NOT EXISTS repo_issue_cache (
    query      TEXT PRIMARY KEY,
    results    TEXT NOT NULL,
    fetched_at TEXT NOT NULL
);

-- Cached model completions, keyed by a hash of the whole request (tier, system
-- prompt, messages, limits). Persisted rather than in-memory so a restart doesn't
-- re-buy every answer the daemon already paid for.
CREATE TABLE IF NOT EXISTS completion_cache (
    key        TEXT PRIMARY KEY,
    label      TEXT NOT NULL,
    response   TEXT NOT NULL,
    hits       INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    used_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_completion_cache_used ON completion_cache(used_at);

-- Triage for one issue assigned to the operator: what the local coder model made
-- of it after reading the actual source, the candidate patches it proposed, and
-- the plain-English rendering of both. Keyed by `owner/repo#number` so it survives
-- re-correlation and thread merges.
CREATE TABLE IF NOT EXISTS issue_triage (
    issue_key       TEXT PRIMARY KEY,
    repo            TEXT NOT NULL,
    number          INTEGER NOT NULL,
    title           TEXT NOT NULL,
    url             TEXT,
    signal_id       TEXT,
    status          TEXT NOT NULL,
    head_sha        TEXT,
    checkout        TEXT,
    files           TEXT NOT NULL DEFAULT '[]',
    characterization TEXT,
    patches         TEXT NOT NULL DEFAULT '[]',
    plain_summary   TEXT,
    error           TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_issue_triage_signal ON issue_triage(signal_id);
CREATE INDEX IF NOT EXISTS idx_issue_triage_status ON issue_triage(status);

-- A distilled explanation of one subject *and everything under it*: the PRs
-- attempting an issue, their critiques and review conversations, the root cause, the
-- triage, the attached context. Produced by the `Explain` workflow and keyed by the
-- watermark it was built from, so a stale explanation is visibly stale rather than
-- quietly wrong.
CREATE TABLE IF NOT EXISTS subject_explanations (
    subject_key TEXT NOT NULL,
    -- Who wrote it: 'local' for the automatic explanation, 'cloud' for one the operator
    -- explicitly asked a cloud model for. Part of the key, so the two coexist and can be
    -- read side by side — the whole point of asking for a second opinion is comparing it
    -- to the first.
    produced_by TEXT NOT NULL,
    -- The newest attributed signal id at the time of writing.
    watermark   TEXT NOT NULL,
    markdown    TEXT NOT NULL,
    -- What went into it, for the citation strip: 'pr_critiques', 'root_cause', …
    sources     TEXT NOT NULL DEFAULT '[]',
    -- What the dossier check removed before this was stored, as displayable lines. Empty is
    -- the good case; non-empty is shown, because an explanation that had claims taken out
    -- of it is one to read more carefully.
    removed     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    PRIMARY KEY (subject_key, produced_by)
);

-- The root-cause report for one subject: the ranked issue/PR/commit/code candidates the
-- investigator believes contributed, with its citations.
CREATE TABLE IF NOT EXISTS subject_root_cause (
    subject_key   TEXT PRIMARY KEY,
    status      TEXT NOT NULL,
    symptoms    TEXT NOT NULL DEFAULT '[]',
    repos       TEXT NOT NULL DEFAULT '[]',
    candidates  TEXT NOT NULL DEFAULT '[]',
    verdict     TEXT,
    error       TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
"#;

/// Column list for [`row_to_browser_investigation`]. The subject id is joined off
/// the signal rather than stored, so a merge that re-homes signals moves the
/// investigation with them.
const BROWSER_SELECT: &str = "SELECT b.id, b.signal_id, s.subject, b.url, b.prompt, b.status, \
     b.findings, b.error, b.attempts, b.created_at, b.updated_at \
     FROM browser_investigations b";

/// Column list for [`row_to_repo`].
const REPO_SELECT: &str = "SELECT full_name, description, topics, language, archived, pushed_at, \
     readme_etag, readme, summary, indexed_sha, digest, fetched_at, kind, kind_pinned \
     FROM repo_index";

/// Column list for [`row_to_pr_fix`].
const PR_FIX_SELECT: &str = "SELECT issue_key, pr_repo, pr_number, pr_title, pr_url, pr_author, \
     pr_state, files, verdict, confidence, implementation, critique, conversation, \
     also_fixes, analyzed_by, created_at, updated_at FROM issue_pr_fixes";

/// Column list for [`row_to_issue_triage`].
const TRIAGE_SELECT: &str = "SELECT issue_key, repo, number, title, url, signal_id, status, \
     head_sha, checkout, files, characterization, patches, plain_summary, error, created_at, \
     updated_at FROM issue_triage";

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
    /// Filter on whether the signal is still present upstream. Operator triage is
    /// a subject-level filter, not a signal-level one.
    pub upstream_gone: Option<bool>,
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

/// A read-only browser investigation of a dashboard link: queued at ingest, run
/// by the browser worker against the operator's authenticated Chrome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowserInvestigation {
    pub id: String,
    pub signal_id: String,
    pub subject_key: Option<String>,
    pub url: String,
    pub prompt: String,
    /// `pending` → `running` → `completed` | `failed`.
    pub status: String,
    pub findings: Option<String>,
    pub error: Option<String>,
    pub attempts: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// One repository in the watched org, characterized from its **code** — the
/// routing table that turns a symptom into the repos worth searching.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoEntry {
    pub full_name: String,
    pub description: Option<String>,
    pub topics: Vec<String>,
    pub language: Option<String>,
    pub archived: bool,
    pub pushed_at: Option<String>,
    #[serde(skip_serializing)]
    pub readme_etag: Option<String>,
    #[serde(skip_serializing)]
    pub readme: Option<String>,
    pub summary: Option<String>,
    /// The commit `summary` was built from — the cache key for re-characterizing.
    pub indexed_sha: Option<String>,
    /// The structural digest of the tree the model was shown. Kept so a stale
    /// characterization can be explained, and re-used without re-walking.
    #[serde(skip_serializing)]
    pub digest: Option<String>,
    /// What kind of repo this is, when known. See [`RepoKind`].
    #[serde(default)]
    pub kind: Option<RepoKind>,
    /// The kind was set by a human and the name-matching heuristic must not overwrite it.
    #[serde(default)]
    pub kind_pinned: bool,
    pub fetched_at: String,
}

/// What a repository is *for*, which decides how much attention it deserves.
///
/// The distinction is practical rather than taxonomic: an issue about a demo is rarely an
/// incident, docs have no runtime behaviour to break, and grouping the board's 147 repos by this
/// makes the twenty that can actually page you legible among the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoKind {
    /// Production code. The default, because assuming something matters is the safe error.
    Code,
    /// Examples, demos, templates, playgrounds.
    Example,
    /// Documentation, websites, specs.
    Docs,
}

impl RepoKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoKind::Code => "code",
            RepoKind::Example => "example",
            RepoKind::Docs => "docs",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "code" => Some(RepoKind::Code),
            "example" | "examples" | "demo" | "demos" => Some(RepoKind::Example),
            "docs" | "doc" | "documentation" => Some(RepoKind::Docs),
            _ => None,
        }
    }

    /// Guess from a repo's name and topics, or `None` when nothing in them says.
    ///
    /// Deliberately only the unambiguous cases. A guess that has to reach — "sdk" or "tools"
    /// might be a demo — would mis-file production code as a toy, and the operator would have to
    /// notice and correct something they never asked for. `None` means "you tell me", and until
    /// they do it is treated as code, because that is the assumption that fails safe.
    pub fn guess(full_name: &str, topics: &[String]) -> Option<Self> {
        let name = full_name
            .rsplit('/')
            .next()
            .unwrap_or(full_name)
            .to_ascii_lowercase();
        // Word-ish boundaries, so `demos` and `sdk-examples` match while `redemption` does not.
        let has = |needle: &str| {
            name.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == needle || w == format!("{needle}s"))
        };
        if has("example") || has("demo") || has("sample") || has("template") || has("playground") {
            return Some(RepoKind::Example);
        }
        if has("doc") || has("docs") || has("documentation") || has("website") || has("handbook") {
            return Some(RepoKind::Docs);
        }
        // Topics are author-declared and cheap to honour when the name is silent.
        let topical = |needle: &str| topics.iter().any(|t| t.eq_ignore_ascii_case(needle));
        if topical("example") || topical("examples") || topical("demo") || topical("sample") {
            return Some(RepoKind::Example);
        }
        if topical("documentation") || topical("docs") {
            return Some(RepoKind::Docs);
        }
        None
    }
}

/// An open pull request that may already fix an issue, with the local model's read
/// of it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PrFix {
    pub issue_key: String,
    pub pr_repo: String,
    pub pr_number: i64,
    pub pr_title: String,
    pub pr_url: Option<String>,
    pub pr_author: Option<String>,
    pub pr_state: Option<String>,
    pub files: Vec<String>,
    /// `fixes` | `partial` | `related` | `unrelated`.
    pub verdict: String,
    pub confidence: f64,
    /// What the PR actually does, in implementation terms.
    pub implementation: Option<String>,
    /// Whether it genuinely addresses the issue, and what it misses.
    pub critique: Option<String>,
    /// What reviewers actually said, distilled from the merit-scored discussion.
    pub conversation: Option<String>,
    /// Other issue references this PR would also resolve.
    pub also_fixes: Vec<String>,
    /// The tier that produced this analysis (`local`, `brief`, `mid`).
    pub analyzed_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl PrFix {
    /// `owner/repo#number`.
    pub fn reference(&self) -> String {
        format!("{}#{}", self.pr_repo, self.pr_number)
    }
}

/// A cached commit from a repo's log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitEntry {
    pub full_name: String,
    pub sha: String,
    pub author: Option<String>,
    pub committed_at: DateTime<Utc>,
    pub message: String,
    pub url: Option<String>,
    pub files: Vec<String>,
}

impl CommitEntry {
    /// First line of the commit message — what the ranking prompt reads.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }

    /// Short sha, as GitHub renders it.
    pub fn short_sha(&self) -> &str {
        let n = self.sha.len().min(8);
        &self.sha[..n]
    }
}

/// Triage for one issue assigned to the operator.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IssueTriage {
    /// `owner/repo#number`.
    pub issue_key: String,
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub url: Option<String>,
    pub signal_id: Option<String>,
    /// `pending` → `running` → `complete` | `failed`.
    pub status: String,
    /// The commit the triage actually read, so a later checkout can tell whether
    /// the analysis is stale.
    pub head_sha: Option<String>,
    pub checkout: Option<String>,
    /// Source files the model was shown — the citation for its reasoning.
    pub files: Vec<String>,
    pub characterization: Option<String>,
    pub patches: serde_json::Value,
    pub plain_summary: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// One commit's summary, as the scorer sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitSummary {
    pub full_name: String,
    pub sha: String,
    pub summary: String,
    pub components: Vec<String>,
}

/// How far the code index has got with one repo. Everything the progress panel shows for a
/// row, so a repaint is one query rather than one per repo per facet.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoIndexProgress {
    pub full_name: String,
    pub summary: Option<String>,
    pub language: Option<String>,
    pub archived: bool,
    /// The commit the repo card was built from.
    pub indexed_sha: Option<String>,
    pub components: i64,
    /// Commits fetched into the local cache — the denominator for summarizing.
    pub commits_cached: i64,
    pub commits_summarized: i64,
    pub depends_on: i64,
    pub depended_on_by: i64,
    /// How far back history has been walked. `None` means the walk hasn't started, which is
    /// a different state from "0 commits to do" and reads identically without this field.
    pub history_back_to: Option<String>,
    /// What kind of repo this is — code, example, or docs. `None` means nobody has said and
    /// the name gave no clue.
    pub kind: Option<RepoKind>,
    /// Whether a human set the kind.
    pub kind_pinned: bool,
    /// The newest commit in the local cache — the repo's last activity, as far as the index has
    /// seen. Distinct from `history_back_to`, which is the *oldest*: the walk runs backwards
    /// from HEAD, so the two ends answer different questions and were being confused for each
    /// other on the board.
    pub last_commit: Option<String>,
}

/// One commit summary with enough of its commit to be recognizable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitSummaryRow {
    pub sha: String,
    pub summary: String,
    pub components: Vec<String>,
    pub model: Option<String>,
    /// First line of the commit message, for orientation — the summary is behavioural and
    /// deliberately doesn't restate it.
    pub subject: Option<String>,
    pub author: Option<String>,
    pub committed_at: Option<String>,
    pub url: Option<String>,
}

/// A commit the index already holds, looked up because something pointed at it.
///
/// Carries the **commit message** rather than just the behavioural summary: when a reviewer
/// says "this was fixed in a1b2c3d", the message — and for a merge, the PR title it carries —
/// is the thing that says whether it was. The summary is the index's reading of the diff and
/// rides along when it exists.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegistryCommit {
    pub full_name: String,
    pub sha: String,
    pub author: Option<String>,
    pub committed_at: String,
    pub message: String,
    pub url: Option<String>,
    /// The index's behavioural summary, when this commit has been summarized.
    pub summary: Option<String>,
}

/// One component of a repo — a module root, which is the granularity an engineer acts
/// on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ComponentSummary {
    pub full_name: String,
    pub path: String,
    pub purpose: Option<String>,
    pub symptoms: Option<String>,
    pub digest: Option<String>,
    pub indexed_sha: Option<String>,
}

/// A dependency edge between two indexed repos.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoDep {
    pub from_repo: String,
    pub to_repo: String,
    pub dep_name: String,
    pub source: String,
}

/// A distilled explanation of a subject and everything under it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Explanation {
    pub subject_key: String,
    /// The newest attributed signal at the time of writing. The board compares this
    /// against the subject's current watermark to show whether the explanation still
    /// describes what's there.
    pub watermark: String,
    pub markdown: String,
    /// [`EXPLAIN_LOCAL`] or [`EXPLAIN_CLOUD`].
    pub produced_by: String,
    pub sources: Vec<String>,
    /// Claims the dossier check removed. Shown to the operator: an explanation that needed
    /// correcting is one to read more carefully.
    pub removed: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// The automatic explanation, written by the local model.
pub const EXPLAIN_LOCAL: &str = "local";
/// The explanation a cloud model wrote because the operator asked for one.
pub const EXPLAIN_CLOUD: &str = "cloud";

/// The stored root-cause report for a subject.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RootCauseReport {
    pub subject_key: String,
    /// `running` → `complete` | `failed`.
    pub status: String,
    pub symptoms: Vec<String>,
    pub repos: Vec<String>,
    pub candidates: serde_json::Value,
    pub verdict: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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
        check_compatible(&conn, path)?;
        conn.execute_batch(SCHEMA)?;
        add_columns(&conn)?;
        // Stamped after the schema is in place, so a half-applied schema (a disk error
        // mid-batch) doesn't leave a database claiming to be current.
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )?;
        // This file holds every ingested signal body *and* the credential store, so
        // it is the sensitive artifact — not just the tokens inside it. WAL mode
        // means two sidecar files carry the same content; all three get 0600.
        restrict_permissions(path);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store mutex poisoned")
    }

    // ---- signals ------------------------------------------------------------

    /// Insert a signal, refreshing source-provided context on duplicates while
    /// preserving local state, subject membership, and user-applied tags.
    /// Returns `true` only when the row was newly inserted.
    pub fn insert_signal(&self, s: &Signal) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO signals \
             (id, source, external_id, version, kind, title, body, url, actor, keys, \
              severity, upstream_gone, occurred_at, ingested_at, subject, raw, tags) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                s.id,
                s.source.as_str(),
                s.external_id,
                s.version,
                json(&s.kind)?,
                s.title,
                s.body,
                s.url,
                s.actor,
                json(&s.keys)?,
                json(&s.severity)?,
                s.upstream_gone as i64,
                s.occurred_at.to_rfc3339(),
                s.ingested_at.to_rfc3339(),
                s.subject,
                s.raw.to_string(),
                json(&s.tags)?,
            ],
        )?;
        if changed > 0 {
            return Ok(true);
        }
        // GitHub keeps unread notifications stable across restarts. Refreshing the
        // mutable source fields lets newly added enrichers (a CI log excerpt, say)
        // populate an already-stored signal without resetting its attribution.
        //
        // Scoped to `(source, external_id, version)` — the same key the unique index
        // uses. Matching on `(source, external_id)` alone would hit *every version* of
        // one notification thread and overwrite the older ones' content with the
        // newest, quietly rewriting the timeline.
        conn.execute(
            "UPDATE signals SET kind=?4, title=?5, body=?6, url=?7, actor=?8, \
             keys=?9, severity=?10, occurred_at=?11, ingested_at=?12, raw=?13, \
             upstream_gone=0 \
             WHERE source=?1 AND external_id=?2 AND version IS ?3",
            params![
                s.source.as_str(),
                s.external_id,
                s.version,
                json(&s.kind)?,
                s.title,
                s.body,
                s.url,
                s.actor,
                json(&s.keys)?,
                json(&s.severity)?,
                s.occurred_at.to_rfc3339(),
                s.ingested_at.to_rfc3339(),
                s.raw.to_string(),
            ],
        )?;
        Ok(false)
    }

    /// Signals that resolved to no subject, newest first.
    ///
    /// The unattributed lane. These are deliberately *not* given subjects of their own:
    /// minting one per unresolvable event is exactly how a board fills with
    /// near-identical one-signal cards. They stay visible here instead, so a CI failure
    /// on a commit with no PR is findable rather than lost.
    pub fn unattributed_signals(&self, limit: usize) -> Result<Vec<Signal>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{SIGNAL_SELECT} WHERE subject IS NULL AND upstream_gone = 0 \
             ORDER BY occurred_at DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], row_to_signal)?;
        collect(rows)
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
        let mut sql = format!("{SIGNAL_SELECT} WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(src) = f.source {
            sql.push_str(" AND source = ?");
            args.push(Box::new(src.as_str().to_string()));
        }
        if let Some(gone) = f.upstream_gone {
            sql.push_str(" AND upstream_gone = ?");
            args.push(Box::new(gone as i64));
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
                &format!("{SIGNAL_SELECT} WHERE id = ?1"),
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

    pub fn signals_for_subject(&self, subject_key: &str) -> Result<Vec<Signal>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{SIGNAL_SELECT} WHERE subject = ?1 ORDER BY occurred_at ASC"
        ))?;
        let rows = stmt.query_map([subject_key], row_to_signal)?;
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
        let mut stmt = conn.prepare(&format!(
            "{SIGNAL_SELECT} WHERE title LIKE ?1 OR body LIKE ?1 \
             ORDER BY occurred_at DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![like, limit as i64], row_to_signal)?;
        collect(rows)
    }

    /// Prefix marking a signal as coming from the assigned-issues watcher rather
    /// than the notifications feed.
    ///
    /// This is *only* a reconciliation scope now: each GitHub watcher is
    /// authoritative for its own listing, and neither listing contains the other's
    /// ids. It used to also keep one watcher's snapshot from resolving the other's
    /// cards — a hazard that no longer exists, because both watchers key their
    /// signals to the same subject by upstream identity rather than to a synthetic
    /// per-watcher subject.
    pub const ASSIGNED_PREFIX: &'static str = "assigned/";

    /// Resolve locally active GitHub notifications that are absent from a
    /// complete snapshot of GitHub's unread notifications feed.
    pub fn resolve_missing_github_notifications(
        &self,
        active_ids: &BTreeSet<String>,
    ) -> Result<Vec<Signal>> {
        self.resolve_missing(active_ids, false, |signal| signal.external_id.clone())
    }

    /// Resolve assigned-issue cards that are absent from a complete listing of
    /// issues currently assigned to the user — i.e. the issue was closed, or
    /// somebody else took it.
    pub fn resolve_missing_assigned_issues(
        &self,
        active_ids: &BTreeSet<String>,
    ) -> Result<Vec<Signal>> {
        self.resolve_missing(active_ids, true, |signal| signal.external_id.clone())
    }

    /// Shared reconciliation. `assigned` selects which half of the GitHub signals
    /// this snapshot is authoritative for.
    fn resolve_missing(
        &self,
        active_ids: &BTreeSet<String>,
        assigned: bool,
        key_of: impl Fn(&Signal) -> String,
    ) -> Result<Vec<Signal>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut candidates = {
            let mut stmt = tx.prepare(&format!(
                "{SIGNAL_SELECT} WHERE source = ?1 AND upstream_gone = 0"
            ))?;
            let rows = stmt.query_map(params![Source::GitHub.as_str()], row_to_signal)?;
            collect(rows)?
        };

        let mut resolved = Vec::new();
        for signal in &mut candidates {
            // Only reconcile the half this snapshot actually covers.
            if signal.external_id.starts_with(Self::ASSIGNED_PREFIX) != assigned {
                continue;
            }
            if active_ids.contains(&key_of(signal)) {
                continue;
            }
            tx.execute(
                "UPDATE signals SET upstream_gone = 1 WHERE id = ?1",
                params![signal.id],
            )?;
            signal.upstream_gone = true;
            resolved.push(signal.clone());
        }
        tx.commit()?;
        Ok(resolved)
    }

    /// Delete every persisted board event and the derived analysis attached to
    /// those events. Configuration, credentials, memories, context sources, and
    /// saved chats are intentionally outside this reset boundary.
    ///
    /// Returns the number of deleted signals and the distinct subject keys that
    /// were affected, so the caller can reset notification dedup and broadcast the
    /// empty authoritative board.
    pub fn clear_board_events(&self) -> Result<(usize, Vec<String>)> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut threads = BTreeSet::new();
        {
            let mut stmt =
                tx.prepare("SELECT DISTINCT subject FROM signals WHERE subject IS NOT NULL")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for t in rows {
                threads.insert(t?);
            }
        }
        let cleared = tx.execute("DELETE FROM signals", [])?;
        // Everything derived from the event/subject graph goes with it. Clearing these
        // prevents old summaries, relation pins, hints, or recommendations from being
        // attached to a later, unrelated board entry.
        //
        // The rule, because getting this list wrong is invisible until it misleads someone:
        // **anything keyed by a signal or a subject is cleared.** Subject keys are stable
        // upstream identities (`owner/repo#412`), so the next poll re-ingests the same
        // notification and mints the *same key* — and anything left behind under it reappears
        // instantly on a card the operator believes is fresh.
        tx.execute("DELETE FROM subject_edges", [])?;
        tx.execute("DELETE FROM subject_context", [])?;
        tx.execute("DELETE FROM hints", [])?;
        // The parent/child hierarchy is derived by attribution, not chosen by anyone: a stale
        // link would file a fresh PR card under an issue the reset removed.
        tx.execute("DELETE FROM subject_links", [])?;
        // Root causes and explanations are the most misleading things to leave behind — they
        // read as conclusions about work that is, as far as the board is concerned, new.
        tx.execute("DELETE FROM subject_root_cause", [])?;
        tx.execute("DELETE FROM subject_explanations", [])?;
        // Keyed by signal id, and every signal has just gone: these are orphans.
        tx.execute("DELETE FROM browser_investigations", [])?;
        tx.execute("DELETE FROM subjects", [])?;
        tx.commit()?;
        Ok((cleared, threads.into_iter().collect()))
    }

    pub fn set_signal_subject(&self, id: &str, subject: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE signals SET subject = ?2 WHERE id = ?1",
            params![id, subject],
        )?;
        Ok(())
    }

    /// Pin a signal's attribution, overriding the ranked climb. `None` pins it to
    /// the unattributed lane.
    pub fn pin_attribution(
        &self,
        signal_id: &str,
        subject: Option<&crate::subject::SubjectKey>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO attribution_pins (signal_id, subject, created_at) VALUES (?1,?2,?3) \
             ON CONFLICT(signal_id) DO UPDATE SET subject=excluded.subject, \
             created_at=excluded.created_at",
            params![
                signal_id,
                subject.map(|k| k.as_str()),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// The pinned attribution for a signal: `Some(None)` is "pinned to nothing",
    /// `None` is "no pin". The distinction is the whole point.
    #[allow(clippy::option_option)]
    pub fn attribution_pin(&self, signal_id: &str) -> Result<Option<Option<String>>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT subject FROM attribution_pins WHERE signal_id = ?1",
            params![signal_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Mark a signal gone upstream (or back). Distinct from operator triage.
    pub fn set_upstream_gone(&self, id: &str, gone: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE signals SET upstream_gone = ?2 WHERE id = ?1",
            params![id, gone as i64],
        )?;
        Ok(())
    }

    /// Signals whose subject has no row. Under the old synthetic-thread model this
    /// happened whenever a merge was interrupted mid-way; a subject keyed by its own
    /// upstream identity can't be orphaned from itself, so this now only catches a
    /// hand-deleted row — and re-creates it from the signals rather than losing them.
    pub fn orphaned_subject_keys(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT s.subject FROM signals s \
             LEFT JOIN subjects t ON t.key = s.subject \
             WHERE s.subject IS NOT NULL AND t.key IS NULL",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        collect(rows)
    }

    // ---- subjects -----------------------------------------------------------

    pub fn upsert_subject(&self, t: &Subject) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO subjects (key, rank, title, summary, created_at, updated_at, \
                last_reasoned_at, live, tags, tags_pinned, handled, snoozed_until, \
                same_as, merge_key) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) \
             ON CONFLICT(key) DO UPDATE SET \
                title=excluded.title, summary=excluded.summary, updated_at=excluded.updated_at, \
                last_reasoned_at=excluded.last_reasoned_at, live=excluded.live, \
                tags=excluded.tags, tags_pinned=excluded.tags_pinned, \
                handled=excluded.handled, snoozed_until=excluded.snoozed_until, \
                same_as=excluded.same_as, \
                merge_key=COALESCE(excluded.merge_key, subjects.merge_key)",
            params![
                t.key.as_str(),
                t.rank.as_str(),
                t.title,
                t.summary,
                t.created_at.to_rfc3339(),
                t.updated_at.to_rfc3339(),
                t.last_reasoned_at.map(|d| d.to_rfc3339()),
                t.live as i64,
                json(&t.tags)?,
                t.tags_pinned as i64,
                t.handled.as_str(),
                t.snoozed_until.map(|d| d.to_rfc3339()),
                t.same_as.as_ref().map(|k| k.as_str()),
                None::<String>,
            ],
        )?;
        Ok(())
    }

    /// Set the deterministic Slack-rank merge key (an environment id). Separate from
    /// the upsert because it is set once, at creation, and must not be clobbered by
    /// a later metadata refresh.
    pub fn set_subject_merge_key(&self, key: &str, merge_key: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE subjects SET merge_key = ?2 WHERE key = ?1",
            params![key, merge_key],
        )?;
        Ok(())
    }

    /// The Slack-rank subject already grouped under `merge_key`, if any. Two alerts
    /// about one customer environment collapse through this without asking the LLM.
    pub fn subject_by_merge_key(&self, merge_key: &str) -> Result<Option<Subject>> {
        let conn = self.lock();
        conn.query_row(
            &format!(
                "{SUBJECT_SELECT} WHERE s.merge_key = ?1 AND s.same_as IS NULL \
                 ORDER BY s.created_at ASC LIMIT 1"
            ),
            params![merge_key],
            row_to_subject,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Subjects filed under `key` — the PRs attempting an issue.
    pub fn subject_children(&self, key: &str) -> Result<Vec<crate::subject::SubjectKey>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT child FROM subject_links WHERE parent = ?1 ORDER BY child")?;
        let rows = stmt.query_map(params![key], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            if let Ok(k) = crate::subject::SubjectKey::parse(&r?) {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// File a PR under the issue it closes. Recorded whether or not either subject
    /// has a row yet, because the closing keyword is a fact about GitHub — and the
    /// signal that reveals it usually belongs to the *issue*, so waiting for the PR's
    /// card to exist means never recording it.
    pub fn set_subject_parent(&self, child: &str, parent: Option<&str>) -> Result<()> {
        let conn = self.lock();
        match parent {
            Some(parent) => conn.execute(
                "INSERT INTO subject_links (child, parent, created_at) VALUES (?1,?2,?3) \
                 ON CONFLICT(child) DO UPDATE SET parent=excluded.parent",
                params![child, parent, Utc::now().to_rfc3339()],
            )?,
            None => conn.execute("DELETE FROM subject_links WHERE child = ?1", params![child])?,
        };
        Ok(())
    }

    /// Merge `key` away into `canonical`: point it there **and move its signals**.
    ///
    /// Both halves are required. The board hides a subject with `same_as` set, so
    /// setting the pointer without re-pointing the signals doesn't collapse two cards
    /// into one — it hides one card and every signal on it.
    ///
    /// One transaction, because a merge that applied half of itself would leave
    /// activity attributed to a subject nothing displays.
    pub fn merge_subject_into(&self, key: &str, canonical: &str) -> Result<usize> {
        if key == canonical {
            return Ok(0);
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE subjects SET same_as = ?2 WHERE key = ?1",
            params![key, canonical],
        )?;
        let moved = tx.execute(
            "UPDATE signals SET subject = ?2 WHERE subject = ?1",
            params![key, canonical],
        )?;
        tx.commit()?;
        Ok(moved)
    }

    /// Undo a merge pointer, leaving the signals where they are.
    pub fn clear_subject_same_as(&self, key: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE subjects SET same_as = NULL WHERE key = ?1",
            params![key],
        )?;
        Ok(())
    }

    /// Operator triage for a subject.
    pub fn set_handled(
        &self,
        key: &str,
        handled: Handled,
        snoozed_until: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE subjects SET handled = ?2, snoozed_until = ?3, updated_at = ?4 WHERE key = ?1",
            params![
                key,
                handled.as_str(),
                snoozed_until.map(|d| d.to_rfc3339()),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    // ---- browser investigations ---------------------------------------------

    /// Idempotently queue a browser investigation for one signal's dashboard link.
    pub fn queue_browser_investigation(
        &self,
        signal_id: &str,
        url: &str,
        prompt: &str,
    ) -> Result<BrowserInvestigation> {
        let now = Utc::now().to_rfc3339();
        let id = format!("binv/{}", new_id());
        let conn = self.lock();
        conn.execute(
            "INSERT INTO browser_investigations (id, signal_id, url, prompt, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5) \
             ON CONFLICT(signal_id) DO NOTHING",
            params![id, signal_id, url, prompt, now],
        )?;
        Self::browser_investigation_for_signal_locked(&conn, signal_id)?
            .context("browser investigation was not persisted")
    }

    pub fn browser_investigations_for_subject(
        &self,
        subject_key: &str,
    ) -> Result<Vec<BrowserInvestigation>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{BROWSER_SELECT} JOIN signals s ON s.id=b.signal_id \
             WHERE s.subject=?1 ORDER BY b.created_at ASC"
        ))?;
        let rows = stmt.query_map([subject_key], row_to_browser_investigation)?;
        collect(rows)
    }

    pub fn get_browser_investigation(&self, id: &str) -> Result<Option<BrowserInvestigation>> {
        self.lock()
            .query_row(
                &format!("{BROWSER_SELECT} LEFT JOIN signals s ON s.id=b.signal_id WHERE b.id=?1"),
                [id],
                row_to_browser_investigation,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Investigation ids waiting for the browser.
    ///
    /// A plain read, not a claim: the `BrowserRead` workflow id *is* the claim now, so
    /// the `status` column stopped being a queue — which also ended the failure where
    /// a worker that died left rows marked `running` forever.
    pub fn list_browser_investigations_pending(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id FROM browser_investigations WHERE status IN ('pending', 'running') \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        collect(rows)
    }

    pub fn complete_browser_investigation(
        &self,
        id: &str,
        findings: &str,
    ) -> Result<BrowserInvestigation> {
        self.finish_browser_investigation(id, "completed", Some(findings.trim()), None)
    }

    pub fn fail_browser_investigation(
        &self,
        id: &str,
        error: &str,
    ) -> Result<BrowserInvestigation> {
        self.finish_browser_investigation(id, "failed", None, Some(error))
    }

    fn finish_browser_investigation(
        &self,
        id: &str,
        status: &str,
        findings: Option<&str>,
        error: Option<&str>,
    ) -> Result<BrowserInvestigation> {
        let conn = self.lock();
        conn.execute(
            "UPDATE browser_investigations SET status=?2, findings=COALESCE(?3, findings), \
             error=?4, updated_at=?5 WHERE id=?1",
            params![id, status, findings, error, Utc::now().to_rfc3339()],
        )?;
        conn.query_row(
            &format!("{BROWSER_SELECT} LEFT JOIN signals s ON s.id=b.signal_id WHERE b.id=?1"),
            [id],
            row_to_browser_investigation,
        )
        .context("browser investigation not found")
    }

    fn browser_investigation_for_signal_locked(
        conn: &Connection,
        signal_id: &str,
    ) -> Result<Option<BrowserInvestigation>> {
        conn.query_row(
            &format!(
                "{BROWSER_SELECT} LEFT JOIN signals s ON s.id=b.signal_id WHERE b.signal_id=?1"
            ),
            [signal_id],
            row_to_browser_investigation,
        )
        .optional()
        .map_err(Into::into)
    }

    // ---- repo index ---------------------------------------------------------

    /// Upsert a repo's index row.
    ///
    /// `recharacterized` distinguishes "we re-read the code and this is a new
    /// analysis" from "we only refreshed GitHub metadata". Cheap metadata refreshes
    /// happen on every sync; the characterization is expensive and must survive
    /// them, so it (and the sha it was built from) is only overwritten when the
    /// caller actually produced a new one.
    pub fn put_repo(&self, repo: &RepoEntry, recharacterized: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO repo_index \
             (full_name, description, topics, language, archived, pushed_at, readme_etag, readme, \
              summary, indexed_sha, digest, fetched_at, kind) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?14) \
             ON CONFLICT(full_name) DO UPDATE SET \
               description=excluded.description, topics=excluded.topics, language=excluded.language, \
               archived=excluded.archived, pushed_at=excluded.pushed_at, fetched_at=excluded.fetched_at, \
               readme_etag =CASE WHEN ?13 THEN excluded.readme_etag ELSE readme_etag END, \
               readme      =CASE WHEN ?13 THEN excluded.readme      ELSE readme      END, \
               summary     =CASE WHEN ?13 THEN excluded.summary     ELSE summary     END, \
               indexed_sha =CASE WHEN ?13 THEN excluded.indexed_sha ELSE indexed_sha END, \
               digest      =CASE WHEN ?13 THEN excluded.digest      ELSE digest      END, \
               -- A pinned kind is the operator's answer and the crawl must not overwrite it.
               kind        =CASE WHEN kind_pinned THEN kind ELSE excluded.kind END",
            params![
                repo.full_name,
                repo.description,
                json(&repo.topics)?,
                repo.language,
                repo.archived as i64,
                repo.pushed_at,
                repo.readme_etag,
                repo.readme,
                repo.summary,
                repo.indexed_sha,
                repo.digest,
                repo.fetched_at,
                recharacterized,
                repo.kind.as_ref().map(|k| k.as_str()),
            ],
        )?;
        Ok(())
    }

    // ---- PR fixes -----------------------------------------------------------

    pub fn put_pr_fix(&self, f: &PrFix) -> Result<()> {
        self.lock().execute(
            "INSERT INTO issue_pr_fixes \
             (issue_key, pr_repo, pr_number, pr_title, pr_url, pr_author, pr_state, files, \
              verdict, confidence, implementation, critique, conversation, also_fixes, \
              analyzed_by, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16) \
             ON CONFLICT(issue_key, pr_repo, pr_number) DO UPDATE SET \
               pr_title=excluded.pr_title, pr_url=excluded.pr_url, pr_author=excluded.pr_author, \
               pr_state=excluded.pr_state, files=excluded.files, verdict=excluded.verdict, \
               confidence=excluded.confidence, implementation=excluded.implementation, \
               critique=excluded.critique, conversation=excluded.conversation, \
               also_fixes=excluded.also_fixes, \
               analyzed_by=excluded.analyzed_by, updated_at=excluded.updated_at",
            params![
                f.issue_key,
                f.pr_repo,
                f.pr_number,
                f.pr_title,
                f.pr_url,
                f.pr_author,
                f.pr_state,
                json(&f.files)?,
                f.verdict,
                f.confidence,
                f.implementation,
                f.critique,
                f.conversation,
                json(&f.also_fixes)?,
                f.analyzed_by,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Candidate fixing PRs for an issue, most-convincing first.
    pub fn pr_fixes_for_issue(&self, issue_key: &str) -> Result<Vec<PrFix>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{PR_FIX_SELECT} WHERE issue_key=?1 \
             ORDER BY CASE verdict WHEN 'fixes' THEN 0 WHEN 'partial' THEN 1 \
                                   WHEN 'related' THEN 2 ELSE 3 END, confidence DESC"
        ))?;
        let rows = stmt.query_map([issue_key], row_to_pr_fix)?;
        collect(rows)
    }

    /// Every issue that one pull request has been judged to fix.
    ///
    /// When a single PR resolves several issues, those issues are one piece of work
    /// wearing several numbers — showing them as separate cards is duplication, so
    /// the caller lumps them together.
    pub fn issues_fixed_by_pr(&self, pr_repo: &str, pr_number: i64) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT issue_key FROM issue_pr_fixes              WHERE pr_repo=?1 AND pr_number=?2 AND verdict='fixes'              ORDER BY issue_key",
        )?;
        let rows = stmt.query_map(params![pr_repo, pr_number], |r| r.get::<_, String>(0))?;
        collect(rows)
    }

    /// The subject an issue's signals belong to, if any.
    pub fn subject_for_issue(&self, issue_key: &str) -> Result<Option<String>> {
        self.lock()
            .query_row(
                "SELECT s.subject FROM issue_triage t JOIN signals s ON s.id = t.signal_id \
                 WHERE t.issue_key = ?1 AND s.subject IS NOT NULL LIMIT 1",
                [issue_key],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Forget an issue's PR analysis, so a re-triage re-derives it rather than
    /// showing conclusions about pull requests that may since have merged.
    pub fn clear_pr_fixes(&self, issue_key: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM issue_pr_fixes WHERE issue_key=?1", [issue_key])?;
        Ok(())
    }

    pub fn list_repos(&self) -> Result<Vec<RepoEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!("{REPO_SELECT} ORDER BY full_name ASC"))?;
        let rows = stmt.query_map([], row_to_repo)?;
        collect(rows)
    }

    pub fn get_repo(&self, full_name: &str) -> Result<Option<RepoEntry>> {
        self.lock()
            .query_row(
                &format!("{REPO_SELECT} WHERE full_name=?1"),
                [full_name],
                row_to_repo,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Drop repos that vanished from the org listing, so a renamed or deleted
    /// repository stops being offered as a routing target.
    pub fn prune_repos(&self, keep: &BTreeSet<String>) -> Result<usize> {
        let conn = self.lock();
        let existing: Vec<String> = {
            let mut stmt = conn.prepare("SELECT full_name FROM repo_index")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            collect(rows)?
        };
        let mut removed = 0;
        for name in existing.iter().filter(|n| !keep.contains(*n)) {
            conn.execute("DELETE FROM repo_index WHERE full_name=?1", [name])?;
            removed += 1;
        }
        Ok(removed)
    }

    // ---- commit cache -------------------------------------------------------

    pub fn put_commits(&self, commits: &[CommitEntry]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for c in commits {
            tx.execute(
                "INSERT INTO repo_commits (full_name, sha, author, committed_at, message, url, files) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7) \
                 ON CONFLICT(full_name, sha) DO UPDATE SET \
                   author=excluded.author, committed_at=excluded.committed_at, \
                   message=excluded.message, url=excluded.url, \
                   files=CASE WHEN excluded.files='[]' THEN files ELSE excluded.files END",
                params![
                    c.full_name,
                    c.sha,
                    c.author,
                    c.committed_at.to_rfc3339(),
                    c.message,
                    c.url,
                    json(&c.files)?,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn commits_since(
        &self,
        full_name: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<CommitEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT full_name, sha, author, committed_at, message, url, files FROM repo_commits \
             WHERE full_name=?1 AND committed_at >= ?2 ORDER BY committed_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![full_name, since.to_rfc3339(), limit as i64],
            row_to_commit,
        )?;
        collect(rows)
    }

    /// The oldest commit timestamp the cache holds for this repo, if any — used
    /// to decide whether a requested window is already covered.
    pub fn commit_window(&self, full_name: &str) -> Result<Option<DateTime<Utc>>> {
        let raw: Option<String> = self
            .lock()
            .query_row(
                "SELECT since FROM repo_commit_windows WHERE full_name=?1",
                [full_name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)))
    }

    pub fn set_commit_window(&self, full_name: &str, since: DateTime<Utc>) -> Result<()> {
        self.lock().execute(
            "INSERT INTO repo_commit_windows (full_name, since, fetched_at) VALUES (?1,?2,?3) \
             ON CONFLICT(full_name) DO UPDATE SET \
               since=MIN(since, excluded.since), fetched_at=excluded.fetched_at",
            params![full_name, since.to_rfc3339(), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    // ---- issue-search cache -------------------------------------------------

    /// A cached issue/PR search, if it's still within `ttl`.
    pub fn get_issue_search(
        &self,
        query: &str,
        ttl: std::time::Duration,
    ) -> Result<Option<serde_json::Value>> {
        let row: Option<(String, String)> = self
            .lock()
            .query_row(
                "SELECT results, fetched_at FROM repo_issue_cache WHERE query=?1",
                [query],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((results, fetched_at)) = row else {
            return Ok(None);
        };
        let fresh = DateTime::parse_from_rfc3339(&fetched_at)
            .ok()
            .is_some_and(|t| {
                Utc::now().signed_duration_since(t.with_timezone(&Utc))
                    < chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(1))
            });
        if !fresh {
            return Ok(None);
        }
        Ok(serde_json::from_str(&results).ok())
    }

    pub fn put_issue_search(&self, query: &str, results: &serde_json::Value) -> Result<()> {
        self.lock().execute(
            "INSERT INTO repo_issue_cache (query, results, fetched_at) VALUES (?1,?2,?3) \
             ON CONFLICT(query) DO UPDATE SET results=excluded.results, fetched_at=excluded.fetched_at",
            params![query, results.to_string(), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    // ---- completion cache ---------------------------------------------------

    /// A cached completion, if one exists and is still inside `ttl`.
    ///
    /// A hit also bumps `hits`/`used_at`, so pruning can evict what nothing is
    /// actually reusing rather than merely what's old.
    pub fn get_completion(&self, key: &str, ttl: std::time::Duration) -> Result<Option<String>> {
        let conn = self.lock();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT response, created_at FROM completion_cache WHERE key=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((response, created_at)) = row else {
            return Ok(None);
        };
        let fresh = DateTime::parse_from_rfc3339(&created_at)
            .ok()
            .is_some_and(|t| {
                Utc::now().signed_duration_since(t.with_timezone(&Utc))
                    < chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(24))
            });
        if !fresh {
            conn.execute("DELETE FROM completion_cache WHERE key=?1", [key])?;
            return Ok(None);
        }
        conn.execute(
            "UPDATE completion_cache SET hits=hits+1, used_at=?2 WHERE key=?1",
            params![key, Utc::now().to_rfc3339()],
        )?;
        Ok(Some(response))
    }

    pub fn put_completion(&self, key: &str, label: &str, response: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.lock().execute(
            "INSERT INTO completion_cache (key, label, response, hits, created_at, used_at) \
             VALUES (?1,?2,?3,0,?4,?4) \
             ON CONFLICT(key) DO UPDATE SET response=excluded.response, created_at=excluded.created_at, \
               used_at=excluded.used_at",
            params![key, label, response, now],
        )?;
        Ok(())
    }

    /// Drop expired entries, then evict least-recently-used down to `max_entries`.
    /// Returns how many rows went away.
    pub fn prune_completions(&self, ttl: std::time::Duration, max_entries: usize) -> Result<usize> {
        let cutoff = (Utc::now()
            - chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(24)))
        .to_rfc3339();
        let conn = self.lock();
        let mut removed = conn.execute(
            "DELETE FROM completion_cache WHERE created_at < ?1",
            [cutoff],
        )?;
        if max_entries > 0 {
            removed += conn.execute(
                "DELETE FROM completion_cache WHERE key IN ( \
                   SELECT key FROM completion_cache ORDER BY used_at DESC LIMIT -1 OFFSET ?1)",
                [max_entries as i64],
            )?;
        }
        Ok(removed)
    }

    /// `(entries, total hits)` — how much prior work the cache is saving.
    pub fn completion_cache_stats(&self) -> Result<(i64, i64)> {
        self.lock()
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(hits), 0) FROM completion_cache",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(Into::into)
    }

    // ---- assigned-issue triage ----------------------------------------------

    pub fn put_issue_triage(&self, t: &IssueTriage) -> Result<()> {
        self.lock().execute(
            "INSERT INTO issue_triage \
             (issue_key, repo, number, title, url, signal_id, status, head_sha, checkout, files, \
              characterization, patches, plain_summary, error, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?15) \
             ON CONFLICT(issue_key) DO UPDATE SET \
               title=excluded.title, url=excluded.url, \
               signal_id=COALESCE(excluded.signal_id, signal_id), \
               status=excluded.status, head_sha=excluded.head_sha, checkout=excluded.checkout, \
               files=excluded.files, characterization=excluded.characterization, \
               patches=excluded.patches, plain_summary=excluded.plain_summary, \
               error=excluded.error, updated_at=excluded.updated_at",
            params![
                t.issue_key,
                t.repo,
                t.number,
                t.title,
                t.url,
                t.signal_id,
                t.status,
                t.head_sha,
                t.checkout,
                json(&t.files)?,
                t.characterization,
                t.patches.to_string(),
                t.plain_summary,
                t.error,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_issue_triage(&self, issue_key: &str) -> Result<Option<IssueTriage>> {
        self.lock()
            .query_row(
                &format!("{TRIAGE_SELECT} WHERE issue_key=?1"),
                [issue_key],
                row_to_issue_triage,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Triage rows for the issues on one subject, matched through their signals.
    pub fn issue_triage_for_subject(&self, subject_key: &str) -> Result<Vec<IssueTriage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{TRIAGE_SELECT} WHERE signal_id IN (SELECT id FROM signals WHERE subject=?1) \
             ORDER BY updated_at DESC"
        ))?;
        let rows = stmt.query_map([subject_key], row_to_issue_triage)?;
        collect(rows)
    }

    pub fn list_issue_triage(&self) -> Result<Vec<IssueTriage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!("{TRIAGE_SELECT} ORDER BY updated_at DESC"))?;
        let rows = stmt.query_map([], row_to_issue_triage)?;
        collect(rows)
    }

    /// Queue an issue for triage unless it already has usable analysis. Returns
    /// whether it was (re-)queued.
    ///
    /// A completed triage is only redone when the code has actually moved on: the
    /// issue text and the source it was read against are what the analysis depends
    /// on, and neither changes just because the daemon restarted.
    pub fn queue_issue_triage(
        &self,
        issue_key: &str,
        repo: &str,
        number: i64,
        title: &str,
        url: Option<&str>,
        signal_id: &str,
    ) -> Result<bool> {
        let existing = self.get_issue_triage(issue_key)?;
        if let Some(t) = &existing {
            if matches!(t.status.as_str(), "running" | "pending" | "complete") {
                // Keep the signal link fresh even when we don't re-queue, so the
                // board can always find the analysis.
                if t.signal_id.as_deref() != Some(signal_id) {
                    self.lock().execute(
                        "UPDATE issue_triage SET signal_id=?2, updated_at=?3 WHERE issue_key=?1",
                        params![issue_key, signal_id, Utc::now().to_rfc3339()],
                    )?;
                }
                return Ok(false);
            }
        }
        let now = Utc::now().to_rfc3339();
        self.put_issue_triage(&IssueTriage {
            issue_key: issue_key.to_string(),
            repo: repo.to_string(),
            number,
            title: title.to_string(),
            url: url.map(str::to_string),
            signal_id: Some(signal_id.to_string()),
            status: "pending".into(),
            head_sha: existing.as_ref().and_then(|t| t.head_sha.clone()),
            checkout: existing.as_ref().and_then(|t| t.checkout.clone()),
            files: Vec::new(),
            characterization: None,
            patches: serde_json::json!([]),
            plain_summary: None,
            error: None,
            created_at: existing
                .map(|t| t.created_at)
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        })?;
        Ok(true)
    }

    /// Force a completed triage back into the queue (the "re-triage" action, and
    /// the path taken when new commits land).
    pub fn retriage_issue(&self, issue_key: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE issue_triage SET status='pending', error=NULL, updated_at=?2 WHERE issue_key=?1",
            params![issue_key, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    // ---- root-cause reports -------------------------------------------------

    pub fn put_root_cause(&self, r: &RootCauseReport) -> Result<()> {
        self.lock().execute(
            "INSERT INTO subject_root_cause \
             (subject_key, status, symptoms, repos, candidates, verdict, error, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8) \
             ON CONFLICT(subject_key) DO UPDATE SET \
               status=excluded.status, symptoms=excluded.symptoms, repos=excluded.repos, \
               candidates=excluded.candidates, verdict=excluded.verdict, error=excluded.error, \
               updated_at=excluded.updated_at",
            params![
                r.subject_key,
                r.status,
                json(&r.symptoms)?,
                json(&r.repos)?,
                r.candidates.to_string(),
                r.verdict,
                r.error,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_root_cause(&self, subject_key: &str) -> Result<Option<RootCauseReport>> {
        self.lock()
            .query_row(
                "SELECT subject_key, status, symptoms, repos, candidates, verdict, error, created_at, updated_at \
                 FROM subject_root_cause WHERE subject_key=?1",
                [subject_key],
                row_to_root_cause,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Move a report from one subject to another — called when subjects merge, so
    /// the surviving subject keeps the investigation rather than losing it with
    /// the subject that was collapsed.
    pub fn move_root_cause(&self, from: &str, to: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE OR REPLACE subject_root_cause SET subject_key=?2 WHERE subject_key=?1",
            params![from, to],
        )?;
        Ok(())
    }

    /// Set a subject's tags. `pinned` marks them human-authored so the classifier
    /// won't overwrite them on the next pass.
    pub fn set_subject_tags(&self, id: &str, tags: &[String], pinned: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE subjects SET tags=?2, tags_pinned=?3 WHERE key=?1",
            params![id, json(&tags)?, pinned as i64],
        )?;
        Ok(())
    }

    pub fn get_subject(&self, id: &str) -> Result<Option<Subject>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!("{SUBJECT_SELECT} WHERE s.key = ?1"),
                [id],
                row_to_subject,
            )
            .optional()?)
    }

    pub fn list_subjects(&self) -> Result<Vec<Subject>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!("{SUBJECT_SELECT} ORDER BY s.updated_at DESC"))?;
        let rows = stmt.query_map([], row_to_subject)?;
        collect(rows)
    }

    /// Delete a subject if it has no member signals. Returns whether it was removed.
    pub fn delete_subject_if_empty(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        // A merged-away subject with no signals is not an empty subject — it is the
        // forwarding tombstone. Deleting it drops the `same_as` pointer, so the *next*
        // message addressed to it would mint a fresh subject instead of forwarding, and
        // the board would grow a second card for work already filed elsewhere. That
        // failure surfaces one message after the merge, nowhere near the merge.
        let merged_away: bool = conn
            .query_row(
                "SELECT same_as IS NOT NULL FROM subjects WHERE key = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if merged_away {
            return Ok(false);
        }
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM signals WHERE subject = ?1",
            [id],
            |r| r.get(0),
        )?;
        if count == 0 {
            conn.execute("DELETE FROM subjects WHERE key = ?1", [id])?;
            conn.execute("DELETE FROM subject_context WHERE subject_key = ?1", [id])?;
            conn.execute(
                "DELETE FROM subject_edges WHERE subject_a = ?1 OR subject_b = ?1",
                [id],
            )?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn set_subject_summary(
        &self,
        id: &str,
        summary: &str,
        reasoned_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE subjects SET summary=?2, last_reasoned_at=?3, updated_at=?3 WHERE key=?1",
            params![id, summary, reasoned_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn set_subject_live(&self, id: &str, live: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE subjects SET live=?2 WHERE key=?1",
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
                    "SELECT provenance FROM subject_edges WHERE subject_a=?1 AND subject_b=?2",
                    params![a, b],
                    |r| r.get(0),
                )
                .optional()?;
            if existing.as_deref() == Some("user") {
                return Ok(());
            }
        }
        conn.execute(
            "INSERT INTO subject_edges (subject_a, subject_b, kind, provenance, confidence, rationale, signals, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
             ON CONFLICT(subject_a, subject_b) DO UPDATE SET \
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

    pub fn edges_for_subject(&self, id: &str) -> Result<Vec<Edge>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT subject_a, subject_b, kind, provenance, confidence, rationale, signals, created_at \
             FROM subject_edges WHERE subject_a=?1 OR subject_b=?1",
        )?;
        let rows = stmt.query_map([id], row_to_edge)?;
        collect(rows)
    }

    pub fn get_edge(&self, a: &str, b: &str) -> Result<Option<Edge>> {
        let (a, b) = order(a, b);
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT subject_a, subject_b, kind, provenance, confidence, rationale, signals, created_at \
                 FROM subject_edges WHERE subject_a=?1 AND subject_b=?2",
                params![a, b],
                row_to_edge,
            )
            .optional()?)
    }

    pub fn all_edges(&self) -> Result<Vec<Edge>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT subject_a, subject_b, kind, provenance, confidence, rationale, signals, created_at \
             FROM subject_edges",
        )?;
        let rows = stmt.query_map([], row_to_edge)?;
        collect(rows)
    }

    // ---- per-subject context -------------------------------------------------

    pub fn add_subject_context(&self, c: &SubjectContext) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO subject_context (id, subject_key, kind, content, summary, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                c.id,
                c.subject_key,
                c.kind.as_str(),
                c.content,
                c.summary,
                c.created_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn subject_context(&self, subject_key: &str) -> Result<Vec<SubjectContext>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, subject_key, kind, content, summary, created_at \
             FROM subject_context WHERE subject_key=?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([subject_key], row_to_subject_context)?;
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

    /// Rewrite a tag across all tagged content — contexts, memories, subjects, and
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
        for t in self.list_subjects()? {
            if t.tags.iter().any(|x| x == from) {
                self.set_subject_tags(
                    t.key.as_str(),
                    &remap_tags(&t.tags, from, into),
                    t.tags_pinned,
                )?;
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
        let mut stmt = conn.prepare(&format!("{SIGNAL_SELECT} WHERE tags LIKE ?1"))?;
        let pattern = format!("%\"{tag}\"%");
        let rows = stmt.query_map([pattern], row_to_signal)?;
        collect(rows)
    }

    // ---- hints (live assist) ------------------------------------------------

    pub fn put_hint(&self, h: &Hint) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO hints (id, subject_key, kind, flag_type, text, rationale, citations, confidence, state, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
             ON CONFLICT(id) DO UPDATE SET \
                kind=excluded.kind, flag_type=excluded.flag_type, text=excluded.text, \
                rationale=excluded.rationale, citations=excluded.citations, confidence=excluded.confidence, \
                state=excluded.state",
            params![
                h.id,
                h.subject_key,
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
                "SELECT id, subject_key, kind, flag_type, text, rationale, citations, confidence, state, created_at \
                 FROM hints WHERE id=?1",
                [id],
                row_to_hint,
            )
            .optional()?)
    }

    /// Active hints, optionally scoped to one subject.
    pub fn list_hints(&self, subject_key: Option<&str>) -> Result<Vec<Hint>> {
        let conn = self.lock();
        let mut out = Vec::new();
        if let Some(tid) = subject_key {
            let mut stmt = conn.prepare(
                "SELECT id, subject_key, kind, flag_type, text, rationale, citations, confidence, state, created_at \
                 FROM hints WHERE subject_key=?1 AND state='active' ORDER BY created_at DESC",
            )?;
            for r in stmt.query_map([tid], row_to_hint)? {
                out.push(r?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, subject_key, kind, flag_type, text, rationale, citations, confidence, state, created_at \
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

    /// Clear a subject's active hints before a fresh live-assist pass re-populates them.
    pub fn clear_active_hints(&self, subject_key: &str) -> Result<()> {
        self.lock().execute(
            "DELETE FROM hints WHERE subject_key=?1 AND state='active'",
            [subject_key],
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

    // ---- the code index -----------------------------------------------------

    /// Store a commit's summary. Keyed by sha, so this is written once per commit.
    pub fn put_commit_summary(
        &self,
        full_name: &str,
        sha: &str,
        summary: &str,
        components: &[String],
        embedding: Option<&[u8]>,
        model: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO commit_summaries (full_name, sha, summary, components, embedding, \
                model, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7) \
             ON CONFLICT(full_name, sha) DO UPDATE SET summary=excluded.summary, \
                components=excluded.components, embedding=excluded.embedding, \
                model=excluded.model",
            params![
                full_name,
                sha,
                summary,
                json(&components)?,
                embedding,
                model,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Shas in `full_name` that have no summary yet, oldest first.
    ///
    /// Oldest first on purpose: indexing runs in bounded batches, and a cause precedes
    /// its symptom — so the commits most likely to explain something are the ones
    /// already in the window, not the ones arriving now.
    pub fn commits_needing_summary(
        &self,
        full_name: &str,
        limit: usize,
    ) -> Result<Vec<CommitEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.full_name, c.sha, c.author, c.committed_at, c.message, c.url, c.files \
             FROM repo_commits c \
             LEFT JOIN commit_summaries s ON s.full_name = c.full_name AND s.sha = c.sha \
             WHERE c.full_name = ?1 AND s.sha IS NULL \
             ORDER BY c.committed_at ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![full_name, limit as i64], row_to_commit)?;
        collect(rows)
    }

    /// How much of a repo's commit window is summarized — the progress the board shows
    /// while a one-time index is still running.
    pub fn commit_index_progress(&self, full_name: &str) -> Result<(i64, i64)> {
        let conn = self.lock();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM repo_commits WHERE full_name = ?1",
            params![full_name],
            |r| r.get(0),
        )?;
        let done: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commit_summaries WHERE full_name = ?1",
            params![full_name],
            |r| r.get(0),
        )?;
        Ok((done, total))
    }

    /// Every commit summary with an embedding, for the semantic pass. `repos` empty
    /// means the whole index.
    pub fn commit_summary_embeddings(
        &self,
        repos: &[String],
    ) -> Result<Vec<(CommitSummary, Vec<u8>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT full_name, sha, summary, components, embedding FROM commit_summaries \
             WHERE embedding IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                CommitSummary {
                    full_name: r.get(0)?,
                    sha: r.get(1)?,
                    summary: r.get(2)?,
                    components: from_json::<Vec<String>>(r, 3)?,
                },
                r.get::<_, Vec<u8>>(4)?,
            ))
        })?;
        let all: Vec<(CommitSummary, Vec<u8>)> = collect(rows)?;
        Ok(if repos.is_empty() {
            all
        } else {
            all.into_iter()
                .filter(|(c, _)| repos.contains(&c.full_name))
                .collect()
        })
    }

    /// Commit summaries whose text or changed paths match `term`, for the lexical pass.
    pub fn search_commit_summaries(&self, term: &str, limit: usize) -> Result<Vec<CommitSummary>> {
        let conn = self.lock();
        let like = format!("%{term}%");
        let mut stmt = conn.prepare(
            "SELECT s.full_name, s.sha, s.summary, s.components FROM commit_summaries s \
             JOIN repo_commits c ON c.full_name = s.full_name AND c.sha = s.sha \
             WHERE s.summary LIKE ?1 OR c.message LIKE ?1 OR c.files LIKE ?1 \
             ORDER BY c.committed_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like, limit as i64], |r| {
            Ok(CommitSummary {
                full_name: r.get(0)?,
                sha: r.get(1)?,
                summary: r.get(2)?,
                components: from_json::<Vec<String>>(r, 3)?,
            })
        })?;
        collect(rows)
    }

    pub fn put_component_summary(
        &self,
        c: &ComponentSummary,
        embedding: Option<&[u8]>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO component_summaries (full_name, path, purpose, symptoms, digest, \
                embedding, indexed_sha, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
             ON CONFLICT(full_name, path) DO UPDATE SET purpose=excluded.purpose, \
                symptoms=excluded.symptoms, digest=excluded.digest, \
                embedding=excluded.embedding, indexed_sha=excluded.indexed_sha",
            params![
                c.full_name,
                c.path,
                c.purpose,
                c.symptoms,
                c.digest,
                embedding,
                c.indexed_sha,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn components_for_repo(&self, full_name: &str) -> Result<Vec<ComponentSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT full_name, path, purpose, symptoms, digest, indexed_sha \
             FROM component_summaries WHERE full_name = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![full_name], row_to_component)?;
        collect(rows)
    }

    /// Per-repo index progress for the whole watched org, in one query.
    ///
    /// One query rather than a loop of `commit_index_progress` + `components_for_repo` +
    /// `repo_deps` per repo: at 147 repos that is 600 round trips behind a single mutex, on a
    /// panel that repaints whenever anything changes.
    pub fn index_progress_all(&self) -> Result<Vec<RepoIndexProgress>> {
        let conn = self.lock();
        // Column order is load-bearing — the mapper below reads by index — so the two are kept
        // adjacent and numbered in the comment. Inserting a column in the middle silently
        // shifts every field after it, which is a class of bug the compiler cannot catch.
        let mut stmt = conn.prepare(
            "SELECT r.full_name,                                              -- 0
                    r.summary,                                                -- 1
                    r.language,                                               -- 2
                    r.archived,                                               -- 3
                    r.indexed_sha,                                            -- 4
                    r.kind,                                                   -- 5
                    r.kind_pinned,                                            -- 6
                    (SELECT COUNT(*) FROM component_summaries c
                      WHERE c.full_name = r.full_name),                       -- 7
                    (SELECT COUNT(*) FROM repo_commits k
                      WHERE k.full_name = r.full_name),                       -- 8
                    (SELECT COUNT(*) FROM commit_summaries s
                      WHERE s.full_name = r.full_name),                       -- 9
                    (SELECT COUNT(*) FROM repo_deps d
                      WHERE d.from_repo = r.full_name),                       -- 10
                    (SELECT COUNT(*) FROM repo_deps d
                      WHERE d.to_repo = r.full_name),                         -- 11
                    -- The actual oldest cached commit, not the walk cursor: once the walk
                    -- reaches the root, the cursor is parked at an epoch completion sentinel
                    -- which must never be presented as repository history.
                    (SELECT MIN(k.committed_at) FROM repo_commits k
                      WHERE k.full_name = r.full_name),                       -- 12
                    (SELECT MAX(k.committed_at) FROM repo_commits k
                      WHERE k.full_name = r.full_name)                        -- 13
             FROM repo_index r ORDER BY r.full_name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RepoIndexProgress {
                full_name: r.get(0)?,
                summary: r.get(1)?,
                language: r.get(2)?,
                archived: r.get::<_, i64>(3)? != 0,
                indexed_sha: r.get(4)?,
                kind: r
                    .get::<_, Option<String>>(5)?
                    .and_then(|k| RepoKind::parse(&k)),
                kind_pinned: r.get::<_, i64>(6)? != 0,
                components: r.get(7)?,
                commits_cached: r.get(8)?,
                commits_summarized: r.get(9)?,
                depends_on: r.get(10)?,
                depended_on_by: r.get(11)?,
                history_back_to: r.get(12)?,
                last_commit: r.get(13)?,
            })
        })?;
        collect(rows)
    }

    /// Fill in the kind for repos that have none, from the name-and-topics guess.
    ///
    /// Needed because the guess is applied when the crawl *writes* a row, so repos already in the
    /// index keep a NULL kind until their next crawl — which for a daily cadence means the
    /// grouping looks broken for a day after the feature ships.
    ///
    /// Only ever fills a NULL on an unpinned row: it cannot overwrite a human's tag, and it
    /// cannot change its mind about one it already guessed. That makes it safe to run at every
    /// boot, which is also what makes it self-healing rather than a one-shot to remember.
    ///
    /// Returns how many rows it filled.
    pub fn backfill_repo_kinds(&self) -> Result<usize> {
        let repos = self.list_repos()?;
        let mut filled = 0usize;
        for repo in repos {
            if repo.kind.is_some() || repo.kind_pinned {
                continue;
            }
            let Some(kind) = RepoKind::guess(&repo.full_name, &repo.topics) else {
                continue;
            };
            self.put_repo_kind_guess(&repo.full_name, kind)?;
            filled += 1;
        }
        Ok(filled)
    }

    /// Record a *guessed* kind — from the keyword heuristic or the local model.
    ///
    /// Deliberately not `set_repo_kind`, which pins. A guess must stay revisable: the next crawl
    /// may know better, and the operator's answer has to be able to win without a fight.
    pub fn put_repo_kind_guess(&self, full_name: &str, kind: RepoKind) -> Result<()> {
        self.lock().execute(
            "UPDATE repo_index SET kind = ?2 WHERE full_name = ?1 AND kind_pinned = 0",
            params![full_name, kind.as_str()],
        )?;
        Ok(())
    }

    /// Set a repo's kind as a human decision, pinning it against the crawl's guess.
    ///
    /// Pinned because the alternative is worse than useless: an operator corrects a demo repo
    /// that was mis-guessed as code, and the next crawl silently reverts it.
    pub fn set_repo_kind(&self, full_name: &str, kind: RepoKind) -> Result<()> {
        let n = self.lock().execute(
            "UPDATE repo_index SET kind = ?2, kind_pinned = 1 WHERE full_name = ?1",
            params![full_name, kind.as_str()],
        )?;
        if n == 0 {
            anyhow::bail!("{full_name} is not in the repo index");
        }
        Ok(())
    }

    /// Drop a human's kind, handing the repo back to the name-matching guess.
    pub fn clear_repo_kind(&self, full_name: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE repo_index SET kind = NULL, kind_pinned = 0 WHERE full_name = ?1",
            params![full_name],
        )?;
        Ok(())
    }

    /// The newest cached commit for one repo.
    ///
    /// Its own query rather than a filter over [`Self::index_progress_all`]: that reads every
    /// repo in the org, and this is called once per indexer tick.
    pub fn last_commit_at(&self, full_name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT MAX(committed_at) FROM repo_commits WHERE full_name = ?1",
            params![full_name],
            |r| r.get(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// A repo's commit summaries, newest first. The drill-down for "what has it actually
    /// read?", which is the only way to tell a thin index from a wrong one.
    pub fn commit_summaries_for_repo(
        &self,
        full_name: &str,
        limit: usize,
    ) -> Result<Vec<CommitSummaryRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT s.sha, s.summary, s.components, s.model, s.created_at, \
                    c.message, c.author, c.committed_at, c.url \
             FROM commit_summaries s \
             LEFT JOIN repo_commits c ON c.full_name = s.full_name AND c.sha = s.sha \
             WHERE s.full_name = ?1 \
             ORDER BY COALESCE(c.committed_at, s.created_at) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![full_name, limit as i64], |r| {
            let message: Option<String> = r.get(5)?;
            Ok(CommitSummaryRow {
                sha: r.get(0)?,
                summary: r.get(1)?,
                components: from_json::<Vec<String>>(r, 2)?,
                model: r.get(3)?,
                subject: message
                    .as_deref()
                    .and_then(|m| m.lines().next())
                    .map(|l| l.trim().to_string()),
                author: r.get(6)?,
                committed_at: r.get(7)?,
                url: r.get(8)?,
            })
        })?;
        collect(rows)
    }

    /// Look a commit up by sha, or by any unambiguous prefix of one.
    ///
    /// `repo` is a preference, not a filter: a reviewer writing "fixed in a1b2c3d" usually means
    /// this repo, but the whole point of following the reference is that they might not — so a
    /// sha that exists only in another indexed repo still resolves, and the caller is told which
    /// repo it came from.
    pub fn commit_by_sha(
        &self,
        repo: Option<&str>,
        sha_prefix: &str,
    ) -> Result<Option<RegistryCommit>> {
        let prefix = sha_prefix.trim().to_ascii_lowercase();
        // A one-character prefix would match most of the index. Seven is git's own default
        // abbreviation and the shortest thing anyone actually writes down.
        if prefix.len() < 7 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.full_name, c.sha, c.author, c.committed_at, c.message, c.url, s.summary \
             FROM repo_commits c \
             LEFT JOIN commit_summaries s ON s.full_name = c.full_name AND s.sha = c.sha \
             WHERE c.sha LIKE ?1 || '%' \
             ORDER BY (c.full_name = ?2) DESC, c.committed_at DESC LIMIT 1",
        )?;
        stmt.query_row(
            params![prefix, repo.unwrap_or_default()],
            row_to_registry_commit,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The commit a pull request landed as, if the index has walked past it.
    ///
    /// Both merge styles, and both are anchored so `#412` cannot match inside `#4120`: GitHub's
    /// merge commits open `Merge pull request #N from …`, and squash merges end their subject
    /// with `(#N)`. A PR that was never merged — or that landed before the history walk reached
    /// back this far — simply isn't here, which is a different answer from "it didn't fix it".
    ///
    /// The repo is required rather than optional: `#42` means something different in every
    /// repository, and searching the whole index for one would answer with a stranger's commit.
    pub fn commit_for_pull(&self, repo: &str, number: u64) -> Result<Option<RegistryCommit>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.full_name, c.sha, c.author, c.committed_at, c.message, c.url, s.summary \
             FROM repo_commits c \
             LEFT JOIN commit_summaries s ON s.full_name = c.full_name AND s.sha = c.sha \
             WHERE c.full_name = ?2 \
               AND (c.message LIKE '%(#' || ?1 || ')%' \
                    OR c.message LIKE 'Merge pull request #' || ?1 || ' from%') \
             ORDER BY c.committed_at DESC LIMIT 1",
        )?;
        stmt.query_row(params![number.to_string(), repo], row_to_registry_commit)
            .optional()
            .map_err(Into::into)
    }

    pub fn component_embeddings(&self) -> Result<Vec<(ComponentSummary, Vec<u8>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT full_name, path, purpose, symptoms, digest, indexed_sha, embedding \
             FROM component_summaries WHERE embedding IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((row_to_component(r)?, r.get::<_, Vec<u8>>(6)?)))?;
        collect(rows)
    }

    /// Replace a repo's outgoing dependency edges. Whole-set replacement because a
    /// manifest that drops a dependency must drop the edge with it.
    pub fn put_repo_deps(&self, from_repo: &str, edges: &[(String, String, String)]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM repo_deps WHERE from_repo = ?1",
            params![from_repo],
        )?;
        let now = Utc::now().to_rfc3339();
        for (to_repo, dep_name, source) in edges {
            tx.execute(
                "INSERT OR REPLACE INTO repo_deps (from_repo, to_repo, dep_name, source, \
                    created_at) VALUES (?1,?2,?3,?4,?5)",
                params![from_repo, to_repo, dep_name, source, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Edges out of `repo` (what it depends on) and into it (what depends on it).
    ///
    /// Both directions matter: a symptom in a consumer can be caused by a change in a
    /// dependency, and a symptom in a library shows up in whatever uses it.
    pub fn repo_deps(&self, repo: &str) -> Result<(Vec<RepoDep>, Vec<RepoDep>)> {
        let conn = self.lock();
        let read = |sql: &str| -> Result<Vec<RepoDep>> {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(params![repo], |r| {
                Ok(RepoDep {
                    from_repo: r.get(0)?,
                    to_repo: r.get(1)?,
                    dep_name: r.get(2)?,
                    source: r.get(3)?,
                })
            })?;
            collect(rows)
        };
        let out = read(
            "SELECT from_repo, to_repo, dep_name, source FROM repo_deps WHERE from_repo = ?1",
        )?;
        let inbound =
            read("SELECT from_repo, to_repo, dep_name, source FROM repo_deps WHERE to_repo = ?1")?;
        Ok((out, inbound))
    }

    // ---- explanations -------------------------------------------------------

    /// Store a distilled explanation, replacing any previous one for this subject.
    pub fn put_explanation(
        &self,
        subject_key: &str,
        watermark: &str,
        markdown: &str,
        produced_by: &str,
        sources: &[String],
        removed: &[String],
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO subject_explanations (subject_key, produced_by, watermark, markdown, \
                sources, removed, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7) \
             ON CONFLICT(subject_key, produced_by) DO UPDATE SET watermark=excluded.watermark, \
                markdown=excluded.markdown, sources=excluded.sources, \
                removed=excluded.removed, created_at=excluded.created_at",
            params![
                subject_key,
                produced_by,
                watermark,
                markdown,
                json(&sources)?,
                json(&removed)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// One subject's explanation by a specific author ([`EXPLAIN_LOCAL`] / [`EXPLAIN_CLOUD`]).
    pub fn get_explanation(
        &self,
        subject_key: &str,
        produced_by: &str,
    ) -> Result<Option<Explanation>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT subject_key, produced_by, watermark, markdown, sources, removed, created_at \
             FROM subject_explanations WHERE subject_key = ?1 AND produced_by = ?2",
            params![subject_key, produced_by],
            row_to_explanation,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The oldest commit actually cached for one repo.
    ///
    /// This is deliberately distinct from [`Self::commit_window`]: once a full-history walk
    /// reaches the root, that cursor is parked at an epoch sentinel to record completion.
    /// Operator-facing progress must keep showing the real root commit's date.
    pub fn oldest_commit_at(&self, full_name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT MIN(committed_at) FROM repo_commits WHERE full_name = ?1",
            [full_name],
            |r| r.get(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// Every explanation of a subject, local first so the board's default is the one that
    /// didn't cost anything.
    pub fn explanations(&self, subject_key: &str) -> Result<Vec<Explanation>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT subject_key, produced_by, watermark, markdown, sources, removed, created_at \
             FROM subject_explanations WHERE subject_key = ?1 \
             ORDER BY CASE produced_by WHEN 'local' THEN 0 ELSE 1 END, created_at DESC",
        )?;
        let rows = stmt.query_map(params![subject_key], row_to_explanation)?;
        collect(rows)
    }

    // ---- secrets ------------------------------------------------------------
    //
    // Raw byte access only. Sealing, unsealing, and the write-only API shape live
    // in `secrets::Secrets` — the store deliberately doesn't know whether a value
    // is plaintext, so there is exactly one place that can decrypt.

    /// Fetch a stored secret's raw bytes. Returns `Ok(None)` when absent.
    pub fn secret_raw(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM secrets WHERE name = ?1",
            params![name],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Store (or overwrite) a secret's raw bytes, stamping `updated_at`.
    pub fn secret_put_raw(&self, name: &str, value: &[u8]) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO secrets (name, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(name) DO UPDATE SET value = excluded.value, \
             updated_at = excluded.updated_at",
            params![name, value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Delete a secret. Missing entries are treated as success.
    pub fn secret_delete(&self, name: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM secrets WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// Names of every stored secret, with when each was last written. Never
    /// values — this is what the config page and MCP are allowed to see.
    pub fn secret_names(&self) -> Result<Vec<(String, DateTime<Utc>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT name, updated_at FROM secrets ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, parse_ts(r, 1)?)))?;
        collect(rows)
    }

    /// Every stored secret's raw bytes, for the one caller that needs them all at
    /// once: re-sealing on a key change, and registering values with the log
    /// scrubber.
    pub fn secrets_raw(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT name, value FROM secrets")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        collect(rows)
    }

    // ---- meta ---------------------------------------------------------------

    pub fn meta_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.lock();
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .optional()
        .map_err(Into::into)
    }

    pub fn meta_put(&self, key: &str, value: &[u8]) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
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
    let (a, b) = order(&e.subject_a, &e.subject_b);
    let mut norm = e.clone();
    norm.subject_a = a.to_string();
    norm.subject_b = b.to_string();
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
        version: row.get(3)?,
        kind: from_json::<SignalKind>(row, 4)?,
        title: row.get(5)?,
        body: row.get(6)?,
        url: row.get(7)?,
        actor: row.get(8)?,
        keys: from_json::<Vec<ResolutionKey>>(row, 9)?,
        severity: from_json::<Severity>(row, 10)?,
        upstream_gone: row.get::<_, i64>(11)? != 0,
        occurred_at: parse_ts(row, 12)?,
        ingested_at: parse_ts(row, 13)?,
        subject: row.get(14)?,
        raw: {
            let s: String = row.get(15)?;
            serde_json::from_str(&s).map_err(|e| conv_err(15, e.to_string()))?
        },
        tags: from_json::<Vec<String>>(row, 16)?,
    })
}

/// Column list for [`row_to_signal`], shared so the two never drift. It was inlined at
/// six call sites; adding `version` to five of them and missing the sixth is the kind of
/// mistake that shows up as a decode error at runtime rather than a compile error.
const SIGNAL_SELECT: &str = "SELECT id, source, external_id, version, kind, title, body, \
     url, actor, keys, severity, upstream_gone, occurred_at, ingested_at, subject, raw, tags \
     FROM signals";

/// Column list for [`row_to_subject`], shared so the two never drift.
const SUBJECT_SELECT: &str = "SELECT s.key, s.rank, s.title, s.summary, s.created_at, \
     s.updated_at, s.last_reasoned_at, s.live, s.tags, s.tags_pinned, s.handled, \
     s.snoozed_until, s.same_as, l.parent \
     FROM subjects s LEFT JOIN subject_links l ON l.child = s.key";

fn row_to_subject(row: &Row) -> rusqlite::Result<Subject> {
    let raw_key: String = row.get(0)?;
    let key = SubjectKey::parse(&raw_key)
        .map_err(|e| conv_err(0, format!("bad subject key '{raw_key}': {e}")))?;
    let rank_s: String = row.get(1)?;
    let handled_s: String = row.get(10)?;
    Ok(Subject {
        rank: SubjectRank::parse(&rank_s).unwrap_or_else(|| key.rank()),
        key,
        title: row.get(2)?,
        summary: row.get(3)?,
        created_at: parse_ts(row, 4)?,
        updated_at: parse_ts(row, 5)?,
        last_reasoned_at: parse_ts_opt(row, 6)?,
        live: row.get::<_, i64>(7)? != 0,
        tags: from_json::<Vec<String>>(row, 8)?,
        tags_pinned: row.get::<_, i64>(9)? != 0,
        handled: Handled::parse(&handled_s)
            .ok_or_else(|| conv_err(10, format!("bad handled state '{handled_s}'")))?,
        snoozed_until: parse_ts_opt(row, 11)?,
        same_as: row
            .get::<_, Option<String>>(12)?
            .and_then(|k| SubjectKey::parse(&k).ok()),
        parent: row
            .get::<_, Option<String>>(13)?
            .and_then(|k| SubjectKey::parse(&k).ok()),
        merge_key: row.get(14).ok().flatten(),
    })
}

fn row_to_edge(row: &Row) -> rusqlite::Result<Edge> {
    let kind_s: String = row.get(2)?;
    let prov_s: String = row.get(3)?;
    Ok(Edge {
        subject_a: row.get(0)?,
        subject_b: row.get(1)?,
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

fn row_to_subject_context(row: &Row) -> rusqlite::Result<SubjectContext> {
    let kind_s: String = row.get(2)?;
    Ok(SubjectContext {
        id: row.get(0)?,
        subject_key: row.get(1)?,
        kind: ContextKind::parse(&kind_s)
            .ok_or_else(|| conv_err(2, format!("bad context kind '{kind_s}'")))?,
        content: row.get(3)?,
        summary: row.get(4)?,
        created_at: parse_ts(row, 5)?,
    })
}

fn row_to_browser_investigation(row: &Row) -> rusqlite::Result<BrowserInvestigation> {
    Ok(BrowserInvestigation {
        id: row.get(0)?,
        signal_id: row.get(1)?,
        subject_key: row.get(2)?,
        url: row.get(3)?,
        prompt: row.get(4)?,
        status: row.get(5)?,
        findings: row.get(6)?,
        error: row.get(7)?,
        attempts: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn row_to_repo(row: &Row) -> rusqlite::Result<RepoEntry> {
    Ok(RepoEntry {
        full_name: row.get(0)?,
        description: row.get(1)?,
        topics: from_json::<Vec<String>>(row, 2)?,
        language: row.get(3)?,
        archived: row.get::<_, i64>(4)? != 0,
        pushed_at: row.get(5)?,
        readme_etag: row.get(6)?,
        readme: row.get(7)?,
        summary: row.get(8)?,
        indexed_sha: row.get(9)?,
        digest: row.get(10)?,
        fetched_at: row.get(11)?,
        kind: row
            .get::<_, Option<String>>(12)?
            .and_then(|k| RepoKind::parse(&k)),
        kind_pinned: row.get::<_, i64>(13).unwrap_or(0) != 0,
    })
}

fn row_to_pr_fix(row: &Row) -> rusqlite::Result<PrFix> {
    Ok(PrFix {
        issue_key: row.get(0)?,
        pr_repo: row.get(1)?,
        pr_number: row.get(2)?,
        pr_title: row.get(3)?,
        pr_url: row.get(4)?,
        pr_author: row.get(5)?,
        pr_state: row.get(6)?,
        files: from_json::<Vec<String>>(row, 7)?,
        verdict: row.get(8)?,
        confidence: row.get(9)?,
        implementation: row.get(10)?,
        critique: row.get(11)?,
        conversation: row.get(12)?,
        also_fixes: from_json::<Vec<String>>(row, 13)?,
        analyzed_by: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn row_to_component(row: &Row) -> rusqlite::Result<ComponentSummary> {
    Ok(ComponentSummary {
        full_name: row.get(0)?,
        path: row.get(1)?,
        purpose: row.get(2)?,
        symptoms: row.get(3)?,
        digest: row.get(4)?,
        indexed_sha: row.get(5)?,
    })
}

fn row_to_registry_commit(row: &Row) -> rusqlite::Result<RegistryCommit> {
    Ok(RegistryCommit {
        full_name: row.get(0)?,
        sha: row.get(1)?,
        author: row.get(2)?,
        committed_at: row.get(3)?,
        message: row.get(4)?,
        url: row.get(5)?,
        summary: row.get(6)?,
    })
}

/// Columns added to tables that already exist, applied idempotently.
///
/// `CREATE TABLE IF NOT EXISTS` **silently skips a table that is already there**, so a column
/// added to a table in [`SCHEMA`] never reaches an existing database — and then every query
/// naming it fails with `no such column`. That is not a hypothetical: adding `kind` to
/// `repo_index` broke every indexer tick on a database holding 147 repos, 462 component cards
/// and 718 commit summaries.
///
/// This is not a migration framework and must not become one. It handles exactly the case that
/// needs no data transformation — **a new nullable column, or one with a default** — because
/// that case is otherwise unserviceable without discarding the database. Anything that needs
/// data moved, a type changed, or a constraint added is not this: bump [`SCHEMA_VERSION`] and
/// let [`check_compatible`] refuse the database with an explanation.
///
/// Guarded on `PRAGMA table_info` rather than relying on the error, so a real failure is not
/// swallowed as "already applied".
fn add_columns(conn: &Connection) -> Result<()> {
    // (table, column, definition)
    const ADDED: &[(&str, &str, &str)] = &[
        ("repo_index", "kind", "TEXT"),
        ("repo_index", "kind_pinned", "INTEGER NOT NULL DEFAULT 0"),
    ];
    for (table, column, def) in ADDED {
        if has_column(conn, table, column)? {
            continue;
        }
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {def}"),
            [],
        )
        .with_context(|| format!("adding {table}.{column}"))?;
        tracing::info!("store: added {table}.{column}");
    }
    Ok(())
}

/// Whether a table already has a column. `false` when the table itself is absent, which is the
/// right answer: [`SCHEMA`] will have created it with the column already in place.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Bumped whenever [`SCHEMA`] changes in a way an existing database can't satisfy.
///
/// **Which of the two mechanisms applies:** a new nullable column (or one with a default) goes
/// in [`add_columns`] and the version stays put, because such a database *can* satisfy the new
/// schema once the column is there. Anything else — a column that needs data moved into it, a
/// changed type, a new constraint, a renamed table — bumps this, and the database is refused
/// with an explanation.
///
/// Getting that choice wrong is silent in the worst way. Adding `kind` to `repo_index` without
/// either mechanism left the column existing only for fresh databases, and every indexer tick on
/// an existing one failed with `no such column` — while the version check happily reported the
/// database as current.
///
/// There is deliberately **no migration path** — this is a local, rebuildable cache of
/// upstream state, and carrying migrations for it would cost more than re-fetching. So the
/// only thing a version buys is a clear refusal, which is the entire point: without it, an
/// out-of-date database fails somewhere arbitrary in the middle of `execute_batch` and
/// reports whichever statement happened to trip first.
const SCHEMA_VERSION: i64 = 2;

/// Refuse an incompatible database with a sentence instead of a stack of SQL.
///
/// The failure this exists to replace: `CREATE TABLE IF NOT EXISTS signals` silently skips a
/// pre-existing `signals` of an older shape, and then `CREATE INDEX … ON signals(subject)`
/// fails with `no such column`. rusqlite reports that as the message plus *the whole
/// remainder of the batch* and a byte offset into it — a thousand characters of unrelated
/// DDL, with the offset landing in whichever table the reader's terminal stopped scrolling
/// at. It reads as "this CREATE TABLE is malformed", which is both wrong and unfixable.
fn check_compatible(conn: &Connection, path: &Path) -> Result<()> {
    // An empty file is a fresh database, which is always compatible.
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if tables == 0 {
        return Ok(());
    }

    // `meta` may itself not exist on a database old enough to predate it; absent is a
    // version of 0, which is never current.
    let found: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if found == SCHEMA_VERSION {
        return Ok(());
    }

    let what = if found == 0 {
        "an older MuggleBot".to_string()
    } else {
        format!("MuggleBot schema v{found}")
    };
    let verb = if found > SCHEMA_VERSION {
        "newer than"
    } else {
        "older than"
    };
    anyhow::bail!(
        "{} was created by {what} and its schema is {verb} this build's (v{SCHEMA_VERSION}). \
         There is no migration path: the database is a local cache of GitHub, Slack and \
         Granola state, and it is rebuilt by re-polling. Move it aside and restart:\n\
         \n    mv {} {}.bak\n\
         \nStored credentials go with it, so re-enter them on the config page \
         (or copy the `secrets` table across with sqlite3 first).",
        path.display(),
        path.display(),
        path.display(),
    )
}

fn row_to_explanation(r: &Row) -> rusqlite::Result<Explanation> {
    Ok(Explanation {
        subject_key: r.get(0)?,
        produced_by: r.get(1)?,
        watermark: r.get(2)?,
        markdown: r.get(3)?,
        sources: from_json::<Vec<String>>(r, 4)?,
        removed: from_json::<Vec<String>>(r, 5)?,
        created_at: parse_ts(r, 6)?,
    })
}

fn row_to_commit(row: &Row) -> rusqlite::Result<CommitEntry> {
    Ok(CommitEntry {
        full_name: row.get(0)?,
        sha: row.get(1)?,
        author: row.get(2)?,
        committed_at: parse_ts(row, 3)?,
        message: row.get(4)?,
        url: row.get(5)?,
        files: from_json::<Vec<String>>(row, 6)?,
    })
}

fn row_to_issue_triage(row: &Row) -> rusqlite::Result<IssueTriage> {
    Ok(IssueTriage {
        issue_key: row.get(0)?,
        repo: row.get(1)?,
        number: row.get(2)?,
        title: row.get(3)?,
        url: row.get(4)?,
        signal_id: row.get(5)?,
        status: row.get(6)?,
        head_sha: row.get(7)?,
        checkout: row.get(8)?,
        files: from_json::<Vec<String>>(row, 9)?,
        characterization: row.get(10)?,
        patches: {
            let s: String = row.get(11)?;
            serde_json::from_str(&s).map_err(|e| conv_err(11, e.to_string()))?
        },
        plain_summary: row.get(12)?,
        error: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

fn row_to_root_cause(row: &Row) -> rusqlite::Result<RootCauseReport> {
    Ok(RootCauseReport {
        subject_key: row.get(0)?,
        status: row.get(1)?,
        symptoms: from_json::<Vec<String>>(row, 2)?,
        repos: from_json::<Vec<String>>(row, 3)?,
        candidates: {
            let s: String = row.get(4)?;
            serde_json::from_str(&s).map_err(|e| conv_err(4, e.to_string()))?
        },
        verdict: row.get(5)?,
        error: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
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
        subject_key: row.get(1)?,
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
        add_columns(&conn)?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// Tighten the DB (and its WAL sidecars) to owner-only. Best-effort: a failure
/// here means the filesystem doesn't support it, which is not a reason to refuse
/// to start — but it is worth a warning, which the caller emits.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                path.to_path_buf()
            } else {
                let mut s = path.as_os_str().to_owned();
                s.push(suffix);
                std::path::PathBuf::from(s)
            };
            if p.exists() {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{ResolutionKey, Severity, SignalKind, Source};
    use chrono::Utc;

    fn sample(ext: &str) -> Signal {
        Signal {
            id: Signal::make_id(Source::GitHub, ext, None),
            source: Source::GitHub,
            external_id: ext.into(),
            kind: SignalKind::Mention,
            title: "hi".into(),
            body: Some("body".into()),
            url: Some("https://example.com".into()),
            actor: None,
            keys: vec![ResolutionKey::new("repo", "o/r")],
            severity: Severity::Warning,
            version: None,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
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

        // subject tags round-trip
        let key = SubjectKey::issue("o/r", 1);
        let t = Subject {
            rank: key.rank(),
            key: key.clone(),
            title: "t".into(),
            summary: None,
            created_at: now,
            updated_at: now,
            last_reasoned_at: None,
            live: false,
            tags: vec![],
            tags_pinned: false,
            handled: Handled::Open,
            snoozed_until: None,
            same_as: None,
            parent: None,
            merge_key: None,
        };
        store.upsert_subject(&t).unwrap();
        store
            .set_subject_tags(key.as_str(), &["database".to_string()], true)
            .unwrap();
        let gt = store.get_subject(key.as_str()).unwrap().unwrap();
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
        let sid = Signal::make_id(Source::GitHub, "sig-tagged", None);
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
            store.get_subject(key.as_str()).unwrap().unwrap().tags,
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
        assert_eq!(s.keys, vec![ResolutionKey::new("repo", "o/r")]);
        assert_eq!(s.raw, serde_json::json!({ "k": "v" }));
    }

    /// Triage is per subject; "gone upstream" is per signal. Conflating the two was
    /// what made the old `state` column mean two unrelated things.
    #[test]
    fn triage_is_per_subject_and_upstream_absence_is_per_signal() {
        let store = Store::open_in_memory().unwrap();
        let s = sample("1");
        store.insert_signal(&s).unwrap();
        let key = SubjectKey::issue("o/r", 1);
        store
            .upsert_subject(&Subject::new(key.clone(), &s, Utc::now()))
            .unwrap();

        store
            .set_handled(key.as_str(), Handled::Acknowledged, None)
            .unwrap();
        assert_eq!(
            store.get_subject(key.as_str()).unwrap().unwrap().handled,
            Handled::Acknowledged
        );
        assert!(
            !store.recent(10).unwrap()[0].upstream_gone,
            "acknowledging work does not make the notification disappear upstream"
        );

        store.set_upstream_gone(&s.id, true).unwrap();
        assert!(store.recent(10).unwrap()[0].upstream_gone);
    }

    #[test]
    fn clear_board_events_deletes_signals_and_subjects() {
        let store = Store::open_in_memory().unwrap();

        // Signals from three sources, on two subjects. All should be resolved —
        // the reset clears the whole board, not just some sources.
        let mut gh = sample("gh1");
        gh.subject = Some("o/r#1".into());
        let mut slack = sample("sl1");
        slack.source = Source::Slack;
        slack.id = Signal::make_id(Source::Slack, "sl1", None);
        slack.subject = Some("o/r#1".into());
        let mut granola = sample("gr1");
        granola.source = Source::Granola;
        granola.id = Signal::make_id(Source::Granola, "gr1", None);
        granola.subject = Some("o/r#2".into());
        for s in [&gh, &slack, &granola] {
            store.insert_signal(s).unwrap();
        }

        let (cleared, mut subjects) = store.clear_board_events().unwrap();
        assert_eq!(cleared, 3, "every signal is deleted regardless of source");
        subjects.sort();
        assert_eq!(subjects, vec!["o/r#1".to_string(), "o/r#2".to_string()]);

        // The event rows and their board-level subject records are gone. A source
        // can subsequently re-ingest a still-active upstream notification.
        assert!(store.recent(10).unwrap().is_empty());
        assert!(store.list_subjects().unwrap().is_empty());
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

    /// The backfill fills gaps and nothing else.
    ///
    /// It runs at every boot, so "only ever fills a NULL on an unpinned row" is what stops it
    /// from being a process that slowly overwrites the operator's answers.
    #[test]
    fn the_kind_backfill_fills_gaps_without_overwriting_anything() {
        let store = Store::open_in_memory().unwrap();
        let put = |name: &str, topics: Vec<String>| {
            store
                .put_repo(
                    &RepoEntry {
                        full_name: name.into(),
                        description: None,
                        topics,
                        language: None,
                        archived: false,
                        pushed_at: None,
                        readme_etag: None,
                        readme: None,
                        summary: None,
                        indexed_sha: None,
                        digest: None,
                        kind: None,
                        kind_pinned: false,
                        fetched_at: Utc::now().to_rfc3339(),
                    },
                    false,
                )
                .unwrap();
        };
        put("o/ai-examples", vec![]);
        put("o/docs-restate", vec![]);
        put("o/restate", vec![]);
        // A human called this one code, against what the name suggests.
        put("o/loan-demo", vec![]);
        store.set_repo_kind("o/loan-demo", RepoKind::Code).unwrap();

        assert_eq!(store.backfill_repo_kinds().unwrap(), 2);
        assert_eq!(
            store.get_repo("o/ai-examples").unwrap().unwrap().kind,
            Some(RepoKind::Example)
        );
        assert_eq!(
            store.get_repo("o/docs-restate").unwrap().unwrap().kind,
            Some(RepoKind::Docs)
        );
        // Nothing in the name says, so it stays unset and is treated as code.
        assert_eq!(store.get_repo("o/restate").unwrap().unwrap().kind, None);
        // The human's answer stands, even though the name says otherwise.
        let pinned = store.get_repo("o/loan-demo").unwrap().unwrap();
        assert_eq!(pinned.kind, Some(RepoKind::Code));
        assert!(pinned.kind_pinned);

        // Idempotent: a second run has nothing left to fill.
        assert_eq!(store.backfill_repo_kinds().unwrap(), 0);
    }

    /// A column added to an existing table has to reach an existing database.
    ///
    /// `CREATE TABLE IF NOT EXISTS` skips a table that is already there, so without
    /// [`add_columns`] a new column exists only for fresh databases — and every query naming it
    /// fails with `no such column`. Adding `kind` to `repo_index` did exactly that to a database
    /// holding 147 repos and 718 commit summaries.
    #[test]
    fn a_new_column_reaches_a_database_that_predates_it() {
        let dir = std::env::temp_dir().join("mugglebot-add-column-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("older.sqlite");

        // A `repo_index` from before `kind` existed, with a row in it.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE repo_index (
                     full_name TEXT PRIMARY KEY, description TEXT, topics TEXT NOT NULL DEFAULT '[]',
                     language TEXT, archived INTEGER NOT NULL DEFAULT 0, pushed_at TEXT,
                     readme_etag TEXT, readme TEXT, summary TEXT, indexed_sha TEXT, digest TEXT,
                     fetched_at TEXT NOT NULL);
                 INSERT INTO repo_index (full_name, fetched_at) VALUES ('o/r', '2026-01-01');
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');",
            )
            .unwrap();
        }

        // Opening it adds the columns rather than failing, and the row survives — the whole
        // point is not having to discard the database over an additive change.
        let store = Store::open(&path).expect("an additive change must not refuse the database");
        let got = store
            .get_repo("o/r")
            .expect("the query naming the new column must work")
            .expect("the existing row survives");
        assert_eq!(got.kind, None, "the added column defaults to unset");
        assert!(!got.kind_pinned);

        // And it is usable, not merely present.
        store.set_repo_kind("o/r", RepoKind::Example).unwrap();
        assert_eq!(
            store.get_repo("o/r").unwrap().unwrap().kind,
            Some(RepoKind::Example)
        );

        // Idempotent: opening again must not try to add them twice.
        drop(store);
        let reopened = Store::open(&path).expect("reopen");
        assert_eq!(
            reopened.get_repo("o/r").unwrap().unwrap().kind,
            Some(RepoKind::Example),
            "a second open must not disturb the data"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn column_detection_handles_an_absent_table() {
        let conn = Connection::open_in_memory().unwrap();
        // No table: `false` is right, because SCHEMA will create it with the column in place.
        assert!(!has_column(&conn, "repo_index", "kind").unwrap());
        conn.execute_batch("CREATE TABLE repo_index (full_name TEXT, kind TEXT)")
            .unwrap();
        assert!(has_column(&conn, "repo_index", "kind").unwrap());
        assert!(!has_column(&conn, "repo_index", "kind_pinned").unwrap());
    }

    /// The name-matching guess covers the unambiguous cases and declines the rest.    /// The name-matching guess covers the unambiguous cases and declines the rest.
    ///
    /// Declining matters more than reaching: a guess that mis-files production code as a demo
    /// makes the operator notice and correct something they never asked for, while `None` simply
    /// means "you tell me" and is treated as code until they do.
    #[test]
    fn a_repo_kind_is_guessed_only_when_the_name_is_unambiguous() {
        let g = |n: &str| RepoKind::guess(n, &[]);
        assert_eq!(g("o/sdk-examples"), Some(RepoKind::Example));
        assert_eq!(g("o/ai-examples"), Some(RepoKind::Example));
        assert_eq!(g("o/demo"), Some(RepoKind::Example));
        assert_eq!(g("o/demos-private"), Some(RepoKind::Example));
        assert_eq!(g("o/rust-template"), Some(RepoKind::Example));
        assert_eq!(g("o/docs-restate"), Some(RepoKind::Docs));
        assert_eq!(g("o/website"), Some(RepoKind::Docs));

        // Real code, and nothing in the name says otherwise.
        assert_eq!(g("o/restate"), None);
        assert_eq!(g("o/restate-cloud"), None);
        assert_eq!(g("o/sdk-python"), None);
        // A substring is not a word: these must not be mistaken for demos or docs.
        assert_eq!(g("o/redemption"), None);
        assert_eq!(g("o/docker-images"), None);
        assert_eq!(g("o/exampled-things"), None);

        // Author-declared topics are honoured when the name is silent.
        assert_eq!(
            RepoKind::guess("o/playground-svc", &["example".into()]),
            Some(RepoKind::Example)
        );
        assert_eq!(
            RepoKind::guess("o/handbook-src", &["documentation".into()]),
            Some(RepoKind::Docs)
        );
    }

    /// A human's tag survives the crawl. Without the pin, an operator's correction is silently
    /// reverted the next time the org is listed — which is worse than never offering the tag.
    #[test]
    fn a_pinned_kind_is_not_overwritten_by_the_crawl() {
        let store = Store::open_in_memory().unwrap();
        let entry = |kind| RepoEntry {
            full_name: "o/tools".into(),
            description: None,
            topics: vec![],
            language: None,
            archived: false,
            pushed_at: None,
            readme_etag: None,
            readme: None,
            summary: None,
            indexed_sha: None,
            digest: None,
            kind,
            kind_pinned: false,
            fetched_at: Utc::now().to_rfc3339(),
        };
        store.put_repo(&entry(None), false).unwrap();

        // The operator says it is a demo.
        store.set_repo_kind("o/tools", RepoKind::Example).unwrap();
        let got = store.get_repo("o/tools").unwrap().unwrap();
        assert_eq!(got.kind, Some(RepoKind::Example));
        assert!(got.kind_pinned);

        // A later crawl guesses nothing and must not clear it.
        store.put_repo(&entry(None), false).unwrap();
        let after = store.get_repo("o/tools").unwrap().unwrap();
        assert_eq!(
            after.kind,
            Some(RepoKind::Example),
            "the crawl overwrote a human's answer"
        );

        // Clearing hands it back to the guess.
        store.clear_repo_kind("o/tools").unwrap();
        let cleared = store.get_repo("o/tools").unwrap().unwrap();
        assert_eq!(cleared.kind, None);
        assert!(!cleared.kind_pinned);

        // Tagging something that isn't indexed is an error rather than a silent no-op.
        assert!(store.set_repo_kind("o/absent", RepoKind::Docs).is_err());
    }

    /// A board reset must leave nothing keyed by a subject behind    /// A board reset must leave nothing keyed by a subject behind, and must leave the code
    /// index alone.
    ///
    /// Both halves matter and they pull in opposite directions. Subject keys are stable upstream
    /// identities, so the next poll re-mints the *same* key — anything left under it reappears on
    /// a card the operator believes is fresh, which is exactly what the reset is for. But the
    /// code index is keyed by *repo*, cost hours of GPU, and has nothing to do with the board;
    /// clearing it would make a reset unaffordable.
    #[test]
    fn a_reset_clears_everything_subject_keyed_and_nothing_repo_keyed() {
        let store = Store::open_in_memory().unwrap();
        let key = "o/r#412";

        let mut sig = sample("gh1");
        sig.subject = Some(key.into());
        store.insert_signal(&sig).unwrap();

        // Derived analysis hanging off the subject.
        store
            .put_explanation(key, &sig.id, "the pool saturates", EXPLAIN_LOCAL, &[], &[])
            .unwrap();
        store
            .put_root_cause(&RootCauseReport {
                subject_key: key.into(),
                status: "complete".into(),
                symptoms: vec!["pool".into()],
                repos: vec!["o/r".into()],
                candidates: serde_json::json!([]),
                verdict: Some("likely the retry change".into()),
                error: None,
                created_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
            })
            .unwrap();
        store.set_subject_parent("o/r!987", Some(key)).unwrap();

        // Code index: keyed by repo, and expensive.
        store
            .put_repo(
                &RepoEntry {
                    full_name: "o/r".into(),
                    description: None,
                    topics: vec![],
                    language: None,
                    archived: false,
                    pushed_at: None,
                    readme_etag: None,
                    readme: None,
                    summary: Some("a card".into()),
                    indexed_sha: Some("abc".into()),
                    digest: None,
                    kind: None,
                    kind_pinned: false,
                    fetched_at: Utc::now().to_rfc3339(),
                },
                false,
            )
            .unwrap();
        store
            .put_component_summary(
                &ComponentSummary {
                    full_name: "o/r".into(),
                    path: "crates/pool".into(),
                    purpose: Some("the pool".into()),
                    symptoms: None,
                    digest: None,
                    indexed_sha: None,
                },
                None,
            )
            .unwrap();
        store
            .put_commit_summary("o/r", "aaa", "stops leaking", &[], None, None)
            .unwrap();
        store
            .put_repo_deps(
                "o/r",
                &[("o/lib".into(), "lib".into(), "Cargo.toml".into())],
            )
            .unwrap();

        store.clear_board_events().unwrap();

        // Nothing subject-keyed may survive — the re-ingested card must start blank.
        assert!(store.explanations(key).unwrap().is_empty(), "explanation");
        assert!(store.get_root_cause(key).unwrap().is_none(), "root cause");
        assert!(store.subject_children(key).unwrap().is_empty(), "hierarchy");
        assert!(store.get_subject(key).unwrap().is_none(), "subject");
        assert!(store.recent(10).unwrap().is_empty(), "signals");

        // ...and the code index is untouched. A reset that cost the index would be one nobody
        // could afford to press.
        assert!(store.get_repo("o/r").unwrap().is_some(), "repo card");
        assert_eq!(store.components_for_repo("o/r").unwrap().len(), 1);
        assert_eq!(store.commit_index_progress("o/r").unwrap().0, 1);
        assert_eq!(store.repo_deps("o/r").unwrap().0.len(), 1);
    }

    fn assigned_signal(ext: &str) -> Signal {
        let mut s = sample(ext);
        s.external_id = format!("assigned/{ext}");
        s.id = Signal::make_id(Source::GitHub, &s.external_id, None);
        s
    }

    /// Two GitHub watchers each reconcile against their own complete listing.
    /// Neither listing contains the other's ids, so an unscoped reconciler would
    /// resolve every card the other watcher owns.
    #[test]
    fn each_github_snapshot_only_resolves_its_own_half() {
        let store = Store::open_in_memory().unwrap();
        let notification = sample("notif-1");
        let assigned = assigned_signal("restatedev/restate#412");
        store.insert_signal(&notification).unwrap();
        store.insert_signal(&assigned).unwrap();

        // The notifications feed still lists its own item; it says nothing about
        // assignments and must not touch them.
        let active: BTreeSet<String> = [notification.external_id.clone()].into();
        let resolved = store.resolve_missing_github_notifications(&active).unwrap();
        assert!(resolved.is_empty(), "nothing should have been resolved");
        assert!(
            !store
                .get_signal(&assigned.id)
                .unwrap()
                .unwrap()
                .upstream_gone,
            "the assigned card must survive a notifications reconcile"
        );

        // Likewise the assigned listing must not resolve notification cards.
        let active: BTreeSet<String> = [assigned.external_id.clone()].into();
        assert!(store
            .resolve_missing_assigned_issues(&active)
            .unwrap()
            .is_empty());
        assert!(
            !store
                .get_signal(&notification.id)
                .unwrap()
                .unwrap()
                .upstream_gone
        );

        // An emptied assigned listing means the issue was closed or reassigned.
        let resolved = store
            .resolve_missing_assigned_issues(&BTreeSet::new())
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, assigned.id);
        assert!(
            !store
                .get_signal(&notification.id)
                .unwrap()
                .unwrap()
                .upstream_gone,
            "the notification card is still untouched"
        );
    }

    #[test]
    fn triage_is_queued_once_and_redone_only_on_demand() {
        let store = Store::open_in_memory().unwrap();
        let sig = assigned_signal("restatedev/restate#412");
        store.insert_signal(&sig).unwrap();
        let key = "restatedev/restate#412";
        let queue = || {
            store
                .queue_issue_triage(key, "restatedev/restate", 412, "Pool leak", None, &sig.id)
                .unwrap()
        };

        assert!(queue(), "first sighting queues");
        // The watcher re-emits every poll; that must not re-queue work.
        assert!(!queue(), "already pending");

        // The scheduler submits an `IssueTriage` workflow and the workflow marks the row
        // `running`; there is no claim step any more — the workflow id is the claim.
        let mut running = store.get_issue_triage(key).unwrap().unwrap();
        assert_eq!(running.issue_key, key);
        running.status = "running".into();
        store.put_issue_triage(&running).unwrap();
        assert!(!queue(), "running work is not re-queued");

        let mut done = running;
        done.status = "complete".into();
        done.characterization = Some("The pool never shrinks.".into());
        done.patches = serde_json::json!([{ "id": "patch-0", "title": "Bound the pool" }]);
        done.head_sha = Some("abc1234".into());
        store.put_issue_triage(&done).unwrap();
        assert!(!queue(), "a completed analysis is not silently redone");

        // Explicitly asking is what re-runs it.
        store.retriage_issue(key).unwrap();
        let requeued = store.get_issue_triage(key).unwrap().unwrap();
        assert_eq!(
            requeued.status, "pending",
            "back in the queue the scheduler reads"
        );
        // Prior analysis is preserved until the new run overwrites it.
        assert_eq!(requeued.head_sha.as_deref(), Some("abc1234"));
    }

    #[test]
    fn failed_triage_is_retried_on_the_next_sighting() {
        let store = Store::open_in_memory().unwrap();
        let sig = assigned_signal("restatedev/restate#9");
        store.insert_signal(&sig).unwrap();
        let key = "restatedev/restate#9";
        store
            .queue_issue_triage(key, "restatedev/restate", 9, "t", None, &sig.id)
            .unwrap();
        let mut failed = store.get_issue_triage(key).unwrap().unwrap();
        failed.status = "failed".into();
        failed.error = Some("git timed out".into());
        store.put_issue_triage(&failed).unwrap();

        assert!(
            store
                .queue_issue_triage(key, "restatedev/restate", 9, "t", None, &sig.id)
                .unwrap(),
            "a transient failure should be retried"
        );
    }

    #[test]
    fn triage_is_reachable_from_its_subject() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = assigned_signal("restatedev/restate#77");
        sig.subject = Some("restatedev/restate#412".into());
        store.insert_signal(&sig).unwrap();
        store
            .queue_issue_triage(
                "restatedev/restate#77",
                "restatedev/restate",
                77,
                "Leak",
                Some("https://github.com/restatedev/restate/issues/77"),
                &sig.id,
            )
            .unwrap();

        let found = store
            .issue_triage_for_subject("restatedev/restate#412")
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].number, 77);
        assert!(store
            .issue_triage_for_subject("thr/other")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn repo_index_keeps_summaries_across_unchanged_refreshes() {
        let store = Store::open_in_memory().unwrap();
        let entry = |summary: Option<&str>, etag: Option<&str>, desc: &str| RepoEntry {
            full_name: "restatedev/restate".into(),
            description: Some(desc.into()),
            topics: vec!["runtime".into()],
            language: Some("Rust".into()),
            archived: false,
            pushed_at: Some("2026-07-01T00:00:00Z".into()),
            readme_etag: etag.map(str::to_string),
            readme: Some("# Restate".into()),
            summary: summary.map(str::to_string),
            indexed_sha: None,
            digest: None,
            kind: None,
            kind_pinned: false,
            fetched_at: Utc::now().to_rfc3339(),
        };
        store
            .put_repo(
                &entry(Some("PURPOSE: the runtime"), Some("etag-1"), "v1"),
                true,
            )
            .unwrap();

        // A 304 refresh updates metadata but must not blank the (LLM-produced)
        // summary or the ETag that got us the 304.
        store.put_repo(&entry(None, None, "v2"), false).unwrap();
        let stored = store.get_repo("restatedev/restate").unwrap().unwrap();
        assert_eq!(stored.summary.as_deref(), Some("PURPOSE: the runtime"));
        assert_eq!(stored.readme_etag.as_deref(), Some("etag-1"));
        assert_eq!(stored.description.as_deref(), Some("v2"));

        // A real re-read replaces it.
        store
            .put_repo(&entry(Some("PURPOSE: updated"), Some("etag-2"), "v3"), true)
            .unwrap();
        let stored = store.get_repo("restatedev/restate").unwrap().unwrap();
        assert_eq!(stored.summary.as_deref(), Some("PURPOSE: updated"));
        assert_eq!(stored.readme_etag.as_deref(), Some("etag-2"));

        // A repo that left the org stops being a routing target.
        let keep: BTreeSet<String> = BTreeSet::new();
        assert_eq!(store.prune_repos(&keep).unwrap(), 1);
        assert!(store.list_repos().unwrap().is_empty());
    }

    #[test]
    fn commit_cache_widens_its_window_and_keeps_known_files() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let commit = |sha: &str, files: Vec<String>| CommitEntry {
            full_name: "restatedev/restate".into(),
            sha: sha.into(),
            author: Some("octocat".into()),
            committed_at: now,
            message: "fix: bound the pool".into(),
            url: None,
            files,
        };
        store
            .put_commits(&[commit("aaa", vec!["src/pool.rs".into()])])
            .unwrap();
        // Re-fetching without file detail must not erase the files we already know.
        store.put_commits(&[commit("aaa", vec![])]).unwrap();
        let cached = store
            .commits_since("restatedev/restate", now - chrono::Duration::hours(1), 10)
            .unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].files, vec!["src/pool.rs"]);

        // The window only ever widens, so a narrow later request can't shrink the
        // recorded coverage and cause a re-fetch of already-cached history.
        let wide = now - chrono::Duration::hours(72);
        let narrow = now - chrono::Duration::hours(4);
        store.set_commit_window("restatedev/restate", wide).unwrap();
        store
            .set_commit_window("restatedev/restate", narrow)
            .unwrap();
        assert_eq!(
            store
                .commit_window("restatedev/restate")
                .unwrap()
                .unwrap()
                .timestamp(),
            wide.timestamp()
        );
    }

    #[test]
    fn issue_search_cache_expires() {
        let store = Store::open_in_memory().unwrap();
        let results = serde_json::json!([{ "number": 1 }]);
        store.put_issue_search("pool repo:a/b", &results).unwrap();
        assert_eq!(
            store
                .get_issue_search("pool repo:a/b", std::time::Duration::from_secs(600))
                .unwrap(),
            Some(results)
        );
        // A zero TTL makes every entry stale — the freshness check is real.
        assert!(store
            .get_issue_search("pool repo:a/b", std::time::Duration::ZERO)
            .unwrap()
            .is_none());
        assert!(store
            .get_issue_search("never searched", std::time::Duration::from_secs(600))
            .unwrap()
            .is_none());
    }

    #[test]
    fn root_cause_report_round_trips_and_moves_on_merge() {
        let store = Store::open_in_memory().unwrap();
        let report = RootCauseReport {
            subject_key: "thr/a".into(),
            status: "complete".into(),
            symptoms: vec!["pool exhausted".into()],
            repos: vec!["restatedev/restate".into()],
            candidates: serde_json::json!([{ "reference": "restatedev/restate#12" }]),
            verdict: Some("Likely the pool ceiling change.".into()),
            error: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        store.put_root_cause(&report).unwrap();
        let stored = store.get_root_cause("thr/a").unwrap().unwrap();
        assert_eq!(stored.symptoms, vec!["pool exhausted"]);
        assert_eq!(stored.candidates[0]["reference"], "restatedev/restate#12");

        // A merge must not lose the investigation with the collapsed subject.
        store.move_root_cause("thr/a", "thr/b").unwrap();
        assert!(store.get_root_cause("thr/a").unwrap().is_none());
        assert_eq!(
            store.get_root_cause("thr/b").unwrap().unwrap().verdict,
            report.verdict
        );
    }

    #[test]
    fn browser_investigation_round_trips_findings() {
        let store = Store::open_in_memory().unwrap();
        let mut signal = sample("grafana");
        signal.subject = Some("o/r#7".into());
        store.insert_signal(&signal).unwrap();

        let queued = store
            .queue_browser_investigation(
                &signal.id,
                "https://example.grafana.net/alerting/123",
                "Use @Chrome read-only.",
            )
            .unwrap();
        assert_eq!(queued.status, "pending");
        // Queueing the same signal is idempotent.
        let duplicate = store
            .queue_browser_investigation(&signal.id, "https://ignored", "ignored")
            .unwrap();
        assert_eq!(queued.id, duplicate.id);

        let complete = store
            .complete_browser_investigation(&queued.id, "CPU saturated on restate-0.")
            .unwrap();
        assert_eq!(complete.status, "completed");
        assert_eq!(complete.subject_key.as_deref(), Some("o/r#7"));
        assert_eq!(
            complete.findings.as_deref(),
            Some("CPU saturated on restate-0.")
        );
    }

    /// The snapshot lists bare upstream notification ids, so reconciliation compares
    /// against `external_id` — which is the bare id now that the version has its own
    /// column. This test previously encoded the old composite `id@version` form and
    /// passed only because the reconciler carried a fallback that split it back apart.
    #[test]
    fn github_unread_snapshot_resolves_missing_notifications() {
        let store = Store::open_in_memory().unwrap();
        let mut active = sample("1");
        active.version = Some("2026-07-24T10:00:00Z".into());
        active.id = Signal::make_id(Source::GitHub, "1", active.version.as_deref());
        let mut read = sample("2");
        read.version = Some("2026-07-24T10:00:00Z".into());
        read.id = Signal::make_id(Source::GitHub, "2", read.version.as_deref());
        store.insert_signal(&active).unwrap();
        store.insert_signal(&read).unwrap();

        let active_ids = BTreeSet::from(["1".to_string()]);
        let resolved = store
            .resolve_missing_github_notifications(&active_ids)
            .unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].external_id, read.external_id);
        assert!(store.get_signal(&read.id).unwrap().unwrap().upstream_gone);
        assert!(!store.get_signal(&active.id).unwrap().unwrap().upstream_gone);
    }

    /// A notification thread has one row per version, so refreshing the newest must not
    /// rewrite the older ones. Before the version moved into its own column the refresh
    /// matched on `(source, external_id)`, which quietly overwrote the whole history of
    /// a thread with its latest state.
    #[test]
    fn refreshing_one_version_leaves_the_others_alone() {
        let store = Store::open_in_memory().unwrap();
        let mut v1 = sample("n1");
        v1.version = Some("v1".into());
        v1.id = Signal::make_id(Source::GitHub, "n1", Some("v1"));
        v1.title = "first state".into();
        let mut v2 = sample("n1");
        v2.version = Some("v2".into());
        v2.id = Signal::make_id(Source::GitHub, "n1", Some("v2"));
        v2.title = "second state".into();
        assert!(store.insert_signal(&v1).unwrap());
        assert!(
            store.insert_signal(&v2).unwrap(),
            "a new version is a new event"
        );

        // Re-ingest v2 with enriched content, as a later poll would.
        let mut v2_enriched = v2.clone();
        v2_enriched.body = Some("now with a CI log excerpt".into());
        assert!(
            !store.insert_signal(&v2_enriched).unwrap(),
            "same version, refreshed"
        );

        assert_eq!(
            store.get_signal(&v1.id).unwrap().unwrap().title,
            "first state",
            "the earlier version keeps its own content"
        );
        assert_eq!(
            store.get_signal(&v2.id).unwrap().unwrap().body.as_deref(),
            Some("now with a CI log excerpt")
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
            .get_signal(&Signal::make_id(Source::GitHub, "1", None))
            .unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn edges_respect_user_pins() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now();
        let user_edge = Edge {
            subject_a: "t/b".into(),
            subject_b: "t/a".into(), // deliberately reversed to test normalization
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
            subject_a: "t/a".into(),
            subject_b: "t/b".into(),
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
    /// The progress panel's whole dataset in one query. Asserted because the counts are
    /// correlated subqueries against four tables, and a wrong join reads as a plausible
    /// number rather than an error.
    #[test]
    fn index_progress_counts_each_facet_against_the_right_repo() {
        let store = Store::open_in_memory().unwrap();
        for name in ["o/app", "o/lib", "o/untouched"] {
            store
                .put_repo(
                    &RepoEntry {
                        full_name: name.into(),
                        description: None,
                        topics: vec![],
                        language: Some("rust".into()),
                        archived: false,
                        pushed_at: None,
                        readme_etag: None,
                        readme: None,
                        summary: None,
                        indexed_sha: None,
                        digest: None,
                        kind: None,
                        kind_pinned: false,
                        fetched_at: Utc::now().to_rfc3339(),
                    },
                    false,
                )
                .unwrap();
        }
        store
            .put_component_summary(
                &ComponentSummary {
                    full_name: "o/app".into(),
                    path: "crates/a".into(),
                    purpose: Some("does a".into()),
                    symptoms: None,
                    digest: None,
                    indexed_sha: None,
                },
                None,
            )
            .unwrap();
        store
            .put_commits(&[CommitEntry {
                full_name: "o/app".into(),
                sha: "aaa".into(),
                author: Some("alice".into()),
                committed_at: Utc::now(),
                message: "fix the leak\n\nmore detail".into(),
                url: Some("https://example/aaa".into()),
                files: vec!["src/pool.rs".into()],
            }])
            .unwrap();
        store
            .put_commit_summary(
                "o/app",
                "aaa",
                "stops leaking on the error path",
                &[],
                None,
                None,
            )
            .unwrap();
        // A second cached commit with no summary, so done < total is visible.
        store
            .put_commits(&[CommitEntry {
                full_name: "o/app".into(),
                sha: "bbb".into(),
                author: None,
                committed_at: Utc::now(),
                message: "unrelated".into(),
                url: None,
                files: vec![],
            }])
            .unwrap();
        store
            .put_repo_deps(
                "o/app",
                &[("o/lib".into(), "lib".into(), "Cargo.toml".into())],
            )
            .unwrap();
        store
            .set_commit_window("o/app", Utc::now() - chrono::Duration::days(30))
            .unwrap();

        let all = store.index_progress_all().unwrap();
        let by = |n: &str| {
            all.iter()
                .find(|r| r.full_name == n)
                .unwrap_or_else(|| panic!("{n} missing"))
        };

        let app = by("o/app");
        assert_eq!(app.components, 1);
        assert_eq!(app.commits_cached, 2);
        assert_eq!(app.commits_summarized, 1);
        assert_eq!(app.depends_on, 1);
        assert_eq!(app.depended_on_by, 0);
        assert!(app.history_back_to.is_some());

        // The edge belongs to `o/app` outbound and `o/lib` inbound — and to neither in the
        // other direction. This is the case the panel flags: a repo the graph points at with
        // nothing indexed inside it.
        let lib = by("o/lib");
        assert_eq!(lib.depended_on_by, 1);
        assert_eq!(lib.depends_on, 0);
        assert_eq!(lib.components, 0);

        // A repo with no index presence at all reports zeroes, not the other repos' counts.
        let none = by("o/untouched");
        assert_eq!(
            (
                none.components,
                none.commits_cached,
                none.commits_summarized,
                none.depends_on,
                none.depended_on_by
            ),
            (0, 0, 0, 0, 0)
        );
        // Never fetched is distinguishable from nothing-left-to-do.
        assert!(none.history_back_to.is_none());
    }

    #[test]
    fn commit_summaries_carry_enough_of_the_commit_to_be_recognizable() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_commits(&[CommitEntry {
                full_name: "o/app".into(),
                sha: "aaa1111".into(),
                author: Some("alice".into()),
                committed_at: Utc::now(),
                message: "fix the pool leak\n\nlonger body that must not be shown".into(),
                url: Some("https://example/aaa1111".into()),
                files: vec!["src/pool.rs".into()],
            }])
            .unwrap();
        store
            .put_commit_summary(
                "o/app",
                "aaa1111",
                "stops leaking on the error path",
                &["crates/pool".into()],
                None,
                Some("local"),
            )
            .unwrap();

        let rows = store.commit_summaries_for_repo("o/app", 10).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.summary, "stops leaking on the error path");
        // First line only: the summary is behavioural and deliberately doesn't restate the
        // message, so showing the whole body next to it would bury it.
        assert_eq!(r.subject.as_deref(), Some("fix the pool leak"));
        assert_eq!(r.author.as_deref(), Some("alice"));
        assert_eq!(r.components, vec!["crates/pool".to_string()]);
        assert_eq!(r.model.as_deref(), Some("local"));
        assert!(r.url.is_some());

        // A summary whose commit is no longer cached still renders — the sha and the summary
        // are the durable half, and dropping the row would make the count and the list
        // disagree.
        store
            .put_commit_summary(
                "o/app",
                "orphan",
                "summary with no commit row",
                &[],
                None,
                None,
            )
            .unwrap();
        let rows = store.commit_summaries_for_repo("o/app", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|r| r.sha == "orphan" && r.subject.is_none()));
    }

    /// An out-of-date database must be refused by name, not by failing somewhere arbitrary
    /// inside `execute_batch`.
    ///
    /// The failure this replaces: `CREATE TABLE IF NOT EXISTS signals` skips a pre-existing
    /// `signals` of an older shape, so `CREATE INDEX … ON signals(upstream_gone)` fails with
    /// `no such column`, and rusqlite renders that as the message plus the entire remainder
    /// of the batch and a byte offset into it. What the operator sees is a screenful of DDL
    /// for whichever table their terminal stopped at — a real report of this landed on
    /// `subject_root_cause`, which was neither the cause nor even mentioned in the error.
    #[test]
    fn an_older_database_is_refused_by_name_rather_than_failing_mid_schema() {
        let dir = std::env::temp_dir().join("mugglebot-schema-compat-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite");

        // A pre-rewrite database: `signals` without the columns the current schema indexes.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE signals (id TEXT PRIMARY KEY, source TEXT NOT NULL);
                 CREATE TABLE threads (id TEXT PRIMARY KEY);",
            )
            .unwrap();
        }

        let msg = match Store::open(&path) {
            Ok(_) => panic!("an older database must be refused"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains("older MuggleBot"),
            "must name the cause, got: {msg}"
        );
        assert!(msg.contains("no migration path"), "{msg}");
        // The path and the way out, because "incompatible" with no next step is a dead end.
        assert!(msg.contains("old.sqlite"), "{msg}");
        assert!(msg.contains("mv "), "{msg}");
        // And emphatically NOT the old failure mode.
        assert!(
            !msg.contains("CREATE TABLE"),
            "must not dump the schema at the operator: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A database this build created must reopen, and reopen again — the version stamp is
    /// only useful if the happy path is unaffected by it.
    #[test]
    fn a_current_database_reopens_cleanly() {
        let dir = std::env::temp_dir().join("mugglebot-schema-reopen-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("current.sqlite");

        for attempt in 1..=3 {
            let store = Store::open(&path).unwrap_or_else(|e| panic!("open {attempt}: {e:#}"));
            // Usable, not merely openable.
            store
                .put_explanation("o/r#1", "sig-1", "text", EXPLAIN_LOCAL, &[], &[])
                .unwrap();
            drop(store);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two explanations of one subject must coexist: the local one MuggleBot wrote on its
    /// own, and the cloud one somebody asked for. A single-row table would make asking for
    /// a second opinion destroy the answer being compared against.
    #[test]
    fn a_second_opinion_sits_beside_the_local_explanation_rather_than_replacing_it() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_explanation("o/r#412", "sig-9", "local read", EXPLAIN_LOCAL, &[], &[])
            .unwrap();
        store
            .put_explanation(
                "o/r#412",
                "sig-9",
                "cloud read",
                EXPLAIN_CLOUD,
                &["pr_critiques".into()],
                &["1 link removed (not in the dossier)".into()],
            )
            .unwrap();

        let all = store.explanations("o/r#412").unwrap();
        assert_eq!(all.len(), 2, "both must survive");
        // Local first: it is what MuggleBot actually concluded, and it cost nothing.
        assert_eq!(all[0].produced_by, EXPLAIN_LOCAL);
        assert_eq!(all[0].markdown, "local read");
        assert_eq!(all[1].produced_by, EXPLAIN_CLOUD);
        // The removals ride with the explanation they were removed from, not globally —
        // the local answer here was clean and must not inherit the cloud one's note.
        assert!(all[0].removed.is_empty());
        assert_eq!(all[1].removed.len(), 1);

        // Re-explaining replaces that author's row and leaves the other alone.
        store
            .put_explanation("o/r#412", "sig-11", "local, again", EXPLAIN_LOCAL, &[], &[])
            .unwrap();
        let all = store.explanations("o/r#412").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].markdown, "local, again");
        assert_eq!(all[0].watermark, "sig-11");
        assert_eq!(
            all[1].markdown, "cloud read",
            "a fresh local pass must not wipe the second opinion"
        );

        let one = store
            .get_explanation("o/r#412", EXPLAIN_CLOUD)
            .unwrap()
            .expect("addressable by author");
        assert_eq!(one.markdown, "cloud read");
    }

    // ---- the registry lookups that follow a "fixed elsewhere" reference ----------

    fn commit(repo: &str, sha: &str, message: &str) -> CommitEntry {
        CommitEntry {
            full_name: repo.into(),
            sha: sha.into(),
            author: Some("alice".into()),
            committed_at: Utc::now(),
            message: message.into(),
            url: None,
            files: vec!["src/pool.rs".into()],
        }
    }

    #[test]
    fn a_commit_resolves_from_an_abbreviated_sha() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_commits(&[commit(
                "o/r",
                "a1b2c3d4e5f6a7b8c9d0",
                "Drain the pool on terminal errors",
            )])
            .unwrap();

        let found = store
            .commit_by_sha(Some("o/r"), "a1b2c3d")
            .unwrap()
            .expect("git's own abbreviation length must resolve");
        assert_eq!(found.message, "Drain the pool on terminal errors");

        // A reference into a repo we did not expect still resolves: the point of following it
        // is that the fix may not be where you assumed.
        assert!(store
            .commit_by_sha(Some("other/repo"), "a1b2c3d")
            .unwrap()
            .is_some());
        // Too short to mean anything, and not a sha at all.
        assert!(store.commit_by_sha(None, "a1b2").unwrap().is_none());
        assert!(store.commit_by_sha(None, "zzzzzzz").unwrap().is_none());
    }

    /// Both merge styles, and neither may match a longer number: `#412` inside `#4120` would
    /// hand the judge a stranger's commit as the fix.
    #[test]
    fn a_pull_request_resolves_to_the_commit_it_landed_as() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_commits(&[
                commit("o/r", "aaaaaaa1", "Bound the pool (#412)"),
                commit(
                    "o/r",
                    "bbbbbbb2",
                    "Merge pull request #500 from fork/branch\n\nFix retries",
                ),
                commit("o/r", "ccccccc3", "Unrelated (#4120)"),
                commit("other/repo", "ddddddd4", "Something else (#412)"),
            ])
            .unwrap();

        let squashed = store
            .commit_for_pull("o/r", 412)
            .unwrap()
            .expect("squash merge");
        assert_eq!(squashed.sha, "aaaaaaa1");
        let merged = store
            .commit_for_pull("o/r", 500)
            .unwrap()
            .expect("merge commit");
        assert_eq!(merged.sha, "bbbbbbb2");
        assert!(
            store.commit_for_pull("o/r", 41).unwrap().is_none(),
            "#41 must not match inside #412 or #4120"
        );
        assert_eq!(
            store
                .commit_for_pull("other/repo", 412)
                .unwrap()
                .unwrap()
                .sha,
            "ddddddd4",
            "#412 means a different commit in a different repo"
        );
        assert!(store
            .commit_for_pull("never/indexed", 412)
            .unwrap()
            .is_none());
    }
}
