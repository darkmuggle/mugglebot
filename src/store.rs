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
use crate::persona::{
    Evidence, EvidenceKind, Facet, Identity, IdentityProvenance, Persona, PredictedPoint,
    Prediction, PredictionKind, Profile, Removed, Stats, Trait,
};
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

-- A Restate Cloud entity (`env_…`, `acc_…`, a Nuon install) reduced to the fields worth
-- showing, cached from `rcc`/`nuon`. See `src/cloud.rs`.
--
-- A cache, not a record: every row is reconstructible by re-running the CLI, and `fetched_at`
-- is what decides whether to. Rows are curated field lists rather than raw virtual-object
-- state precisely because this table is read into model prompts — see the note on
-- `cloud::summarize` about which credential-bearing fields that keeps out.
CREATE TABLE IF NOT EXISTS cloud_entities (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    name        TEXT,
    status      TEXT,
    -- JSON arrays: `[{label, value}]` and `[id]`.
    fields      TEXT NOT NULL,
    related     TEXT NOT NULL,
    -- The ids this one contains, as opposed to the ones it belongs to: an account's people,
    -- an organization's members and groups. Stored rather than derived because it is the only
    -- record that a `usr_` exists — nothing lists users, so the estate walk finds them by
    -- reading this back. See `cloud::Entity::members`.
    members     TEXT NOT NULL DEFAULT '[]',
    -- Why the lookup failed, when it did. Cached briefly so an expired token retries soon.
    error       TEXT,
    fetched_at  TEXT NOT NULL
);

-- Which ids a signal's text actually mentioned. Only the mentions — the ancestors reached by
-- following `cloud_entities.related` are derived on read, so this table stays a statement
-- about the signal rather than a second copy of the hierarchy that can drift from it.
CREATE TABLE IF NOT EXISTS signal_cloud_ids (
    signal_id  TEXT NOT NULL,
    entity_id  TEXT NOT NULL,
    PRIMARY KEY (signal_id, entity_id)
);
CREATE INDEX IF NOT EXISTS idx_signal_cloud_ids_entity ON signal_cloud_ids(entity_id);

-- One reviewer's pass over one branch at one commit. Several rows per branch: the model's own
-- read plus a pass per persona, each on whichever model the operator picked — see
-- `src/ghreview.rs`.
--
-- Keyed on the *sha*, not the branch name, because a review is about a commit: when the branch
-- moves the pass does not follow it, which makes it stale rather than wrong.
CREATE TABLE IF NOT EXISTS branch_review_passes (
    id              TEXT PRIMARY KEY,
    branch_key      TEXT NOT NULL,
    repo            TEXT NOT NULL,
    head            TEXT NOT NULL,
    base            TEXT NOT NULL,
    head_sha        TEXT NOT NULL,
    -- `model` or `persona:<slug>`.
    reviewer        TEXT NOT NULL,
    provider        TEXT NOT NULL,
    model           TEXT NOT NULL,
    status          TEXT NOT NULL,
    recommendation  TEXT,
    summary         TEXT,
    -- The placed comments, as JSON. Read back whole by one view; nothing queries inside them.
    comments        TEXT,
    -- Hidden from the diff, kept on the record: a dismissed review is evidence about the
    -- reviewer, and re-running one you dismissed should be a decision rather than something
    -- that happens because the record was thrown away.
    dismissed       INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE(branch_key, head_sha, reviewer, provider, model)
);
CREATE INDEX IF NOT EXISTS idx_branch_passes_key ON branch_review_passes(branch_key);

-- The diff a branch's reviewers read, cached per commit.
--
-- Its own table rather than a column on a pass: a panel of five reviewers reads one diff, and
-- the view still needs it to render comments against after the pass that fetched it has been
-- dismissed. Keyed on the sha, so a branch that moves gets a new row rather than silently
-- reusing the old code.
CREATE TABLE IF NOT EXISTS branch_diffs (
    branch_key  TEXT NOT NULL,
    head_sha    TEXT NOT NULL,
    report      TEXT NOT NULL,
    fetched_at  TEXT NOT NULL,
    PRIMARY KEY (branch_key, head_sha)
);
CREATE INDEX IF NOT EXISTS idx_branch_passes_status ON branch_review_passes(status);

-- A whole source, quiet until a time. "Not Slack, not now" — see `src/quiet.rs`.
--
-- Expiry is a column that gets read, never a timer that gets armed: nothing has to survive a
-- restart, and there is no scheduled cancellation that can fail to run and leave a source
-- silenced past its snooze.
CREATE TABLE IF NOT EXISTS source_snoozes (
    source     TEXT PRIMARY KEY,
    until      TEXT NOT NULL,
    -- Severities BELOW this are held back; this and above land anyway. Normally `critical`,
    -- so a snooze can never be what hides an outage.
    floor      TEXT NOT NULL,
    set_at     TEXT NOT NULL
);

-- Notifications the tidier decided did not need you. An audit trail, not a queue: the row
-- is written before the API call, so a crash leaves a record of what was about to happen
-- rather than a gap, and `cleared` says whether it actually went.
--
-- This table exists because the action is outward-facing and awkward to undo. "What did it
-- take out of my inbox" is a question you have precisely when the notification is no longer
-- there to look at.
CREATE TABLE IF NOT EXISTS tidied_notifications (
    thread_id  TEXT PRIMARY KEY,
    reason     TEXT NOT NULL,
    rule       TEXT NOT NULL,
    repo       TEXT NOT NULL,
    title      TEXT NOT NULL,
    url        TEXT,
    cleared    INTEGER NOT NULL DEFAULT 0,
    at         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tidied_at ON tidied_notifications(at DESC);

-- One hand-requested Slack thread analysis. Keyed by (channel, thread_ts) so pasting the
-- same link twice is the same row: the analysis costs two metered model calls, and the
-- answer only changes when the thread does.
CREATE TABLE IF NOT EXISTS thread_analyses (
    id           TEXT PRIMARY KEY,
    channel      TEXT NOT NULL,
    thread_ts    TEXT NOT NULL,
    url          TEXT NOT NULL,
    -- Reply count at request time. Part of the workflow key, so new replies make a
    -- re-analysis a *different* invocation rather than a refused duplicate.
    reply_count  INTEGER NOT NULL DEFAULT 0,
    status       TEXT NOT NULL,
    -- The whole `thread::Verdict` as JSON: the thread as read, both analyses, and the
    -- agreement between them. One blob because nothing queries inside it — this is read
    -- back whole, by one screen.
    verdict      TEXT,
    error        TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    UNIQUE(channel, thread_ts)
);
CREATE INDEX IF NOT EXISTS idx_thread_analyses_status ON thread_analyses(status);

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
    -- The reviewers' actual verdict: `approved`, `changes_requested`, `commented`, or
    -- NULL when nobody has reviewed. Distinct from `verdict` above, which is
    -- MuggleBot's own judgment of whether the PR fixes the issue — this one is what
    -- the humans said, and it is what the board reports instead of asking for
    -- attention on a change somebody has already signed off.
    review_state TEXT,
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

-- ---- personas ---------------------------------------------------------------
--
-- The second axis: subjects are what work is about, personas are who it is with. See
-- `crate::persona` for why a persona is not a subject and does not violate the rule that
-- `person` is never one.
--
-- All of it is in SQLite rather than object state, and the reason is the strongest form of
-- the standard one: a profile is the *expensive* artifact — hundreds of local model passes
-- over months of somebody's writing — and `data/restate` is wiped whenever vqueues are
-- toggled. The `Persona` object holds only its harvest cadence.
--
-- These tables need no schema-version bump: `CREATE TABLE IF NOT EXISTS` creates a *missing*
-- table on an existing database, and it is only a pre-existing table that it silently skips.
-- (That distinction is what `add_columns` exists for; see the note there.)
CREATE TABLE IF NOT EXISTS personas (
    slug               TEXT PRIMARY KEY,
    display_name       TEXT NOT NULL,
    -- What they do, and the operator's own notes. Asserted rather than inferred, so they
    -- bypass trait verification entirely and are fed to the model verbatim.
    role               TEXT,
    notes              TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    harvested_at       TEXT,
    profiled_at        TEXT,
    -- Newest evidence id at the last profile pass. The `PersonaProfile` workflow is keyed on
    -- it, so re-distilling a persona nothing new has been seen of is a refused key.
    evidence_watermark TEXT,
    -- How far back the GitHub history walk has reached, RFC3339. Stored rather than derived
    -- from the oldest evidence held: a barren window leaves that unchanged, so a derived
    -- cursor stalls the walk forever while looking merely slow.
    harvest_cursor     TEXT,
    harvest_complete   INTEGER NOT NULL DEFAULT 0,
    -- Why the last pass came back with less than it looks like it should have: a deferred
    -- GitHub budget, an unset org, items that could not be read. Persisted rather than
    -- returned, because the pass runs in a Restate handler minutes after whatever asked for
    -- it, so its return value reaches nobody. NULL is the good case.
    --
    -- Without this, a harvest that made no requests at all and a colleague who genuinely
    -- writes nothing both render as "0 ev", which is how the feature came to look like a
    -- button that does nothing.
    harvest_note       TEXT
);

-- `(source, handle)` is the primary key, not `(persona, source, handle)`. That is the
-- constraint that stops one GitHub login belonging to two personas — which would build two
-- profiles from the same evidence and give the operator two confident, differing answers
-- about the same human.
CREATE TABLE IF NOT EXISTS persona_identities (
    source     TEXT NOT NULL,
    -- Lowercased, because that is what lookups compare.
    handle     TEXT NOT NULL,
    persona    TEXT NOT NULL,
    -- As the operator typed it, for display.
    display    TEXT NOT NULL,
    -- `operator` | `exact` | `proposed`. Only the first two are ever read for evidence.
    provenance TEXT NOT NULL,
    rationale  TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (source, handle)
);
CREATE INDEX IF NOT EXISTS idx_persona_identities_persona ON persona_identities(persona);

-- Verbatim excerpts of things the person wrote. The citation store: every trait points here,
-- and a trait citing nothing is dropped rather than shown.
CREATE TABLE IF NOT EXISTS persona_evidence (
    id          TEXT PRIMARY KEY,
    persona     TEXT NOT NULL,
    source      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    subject_key TEXT,
    url         TEXT,
    excerpt     TEXT NOT NULL,
    context     TEXT,
    -- APPROVED / CHANGES_REQUESTED / COMMENTED. Counted, never modelled.
    state       TEXT,
    occurred_at TEXT NOT NULL,
    ingested_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_persona_evidence_persona ON persona_evidence(persona, occurred_at);
CREATE INDEX IF NOT EXISTS idx_persona_evidence_subject ON persona_evidence(subject_key);

-- One falsifiable claim about behaviour. The id is derived from the claim text (see
-- `persona::profile::trait_id`) so a re-profile that reaches the same conclusion keeps the
-- same id — otherwise every stored prediction loses its citations on the next refresh.
CREATE TABLE IF NOT EXISTS persona_traits (
    id               TEXT PRIMARY KEY,
    persona          TEXT NOT NULL,
    facet            TEXT NOT NULL,
    claim            TEXT NOT NULL,
    confidence       REAL NOT NULL,
    evidence         TEXT NOT NULL DEFAULT '[]',
    counter_evidence TEXT NOT NULL DEFAULT '[]',
    created_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_persona_traits_persona ON persona_traits(persona);

-- What verification refused, and why. Kept for the same reason as
-- `subject_explanations.removed`: a filter nobody can see is a filter nobody can debug, and
-- on a first pass this list is routinely longer than the profile.
CREATE TABLE IF NOT EXISTS persona_removed (
    persona    TEXT NOT NULL,
    facet      TEXT NOT NULL,
    claim      TEXT NOT NULL,
    why        TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_persona_removed_persona ON persona_removed(persona);

-- What a persona is predicted to do about one subject. Keyed `(persona, subject, kind)`, so
-- re-predicting replaces rather than accumulates — and `watermark` records what it was built
-- from, so a prediction older than the subject is visibly stale rather than quietly wrong.
CREATE TABLE IF NOT EXISTS persona_predictions (
    persona        TEXT NOT NULL,
    subject_key    TEXT NOT NULL,
    kind           TEXT NOT NULL,
    watermark      TEXT NOT NULL,
    would_engage   INTEGER NOT NULL,
    confidence     REAL NOT NULL,
    recommendation TEXT,
    summary        TEXT NOT NULL,
    points         TEXT NOT NULL DEFAULT '[]',
    caveats        TEXT NOT NULL DEFAULT '[]',
    produced_by    TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    PRIMARY KEY (persona, subject_key, kind)
);
CREATE INDEX IF NOT EXISTS idx_persona_predictions_subject ON persona_predictions(subject_key);

-- Things the operator knows about a person that no excerpt can supply.
--
-- "Owns the release process", "prefers async review", "is on sabbatical until March", a link to
-- their team's charter. **Asserted, not inferred**, and therefore deliberately outside the trait
-- model: `persona::verify` exists to stop the *model* making unfalsifiable claims, and applying
-- it to something the operator stated would be the filter second-guessing its own author.
--
-- Distinct from `personas.notes`, which is the one-line who-they-are shown beside their name.
-- These are a list, they accumulate, and each one is individually removable — which is what
-- makes them usable for the thing that actually happens: learning one more fact about somebody
-- every few weeks.
CREATE TABLE IF NOT EXISTS persona_context (
    id         TEXT PRIMARY KEY,
    persona    TEXT NOT NULL,
    -- `text` (used verbatim) or `url` (fetched and summarized through the context library).
    kind       TEXT NOT NULL,
    content    TEXT NOT NULL,
    -- For a URL, what was read from it. NULL for text, whose content is already the summary.
    summary    TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_persona_context_persona ON persona_context(persona);

-- The Slack workspace directory, cached.
--
-- Slack identifies people by opaque id, and the normalized `raw` a signal carries keeps only
-- channel/ts/flags — so the signal log knows *that* `U06T7445RHD` said something and nothing
-- about who that is. Fine for attribution; useless for letting the operator say "model Pavel".
--
-- Refreshed lazily on the operator paths that need it (proposing, linking) rather than on a
-- timer: a workspace directory changes when somebody joins, and reading it on a schedule would
-- spend a request per interval to learn nothing most of the time.
CREATE TABLE IF NOT EXISTS slack_users (
    id           TEXT PRIMARY KEY,
    -- The `@handle`, without the `@`.
    name         TEXT NOT NULL,
    real_name    TEXT,
    display_name TEXT,
    -- Only present when the token has `users:read.email`. Absent is normal.
    email        TEXT,
    is_bot       INTEGER NOT NULL DEFAULT 0,
    deleted      INTEGER NOT NULL DEFAULT 0,
    -- Every string worth matching a typed name or a GitHub login against, as a JSON array.
    -- Stored rather than recomputed so a lookup is one indexed scan instead of a table walk.
    aliases      TEXT NOT NULL DEFAULT '[]',
    updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_slack_users_name ON slack_users(name);
"#;

/// Column list for [`row_to_browser_investigation`]. The subject id is joined off
/// the signal rather than stored, so a merge that re-homes signals moves the
/// investigation with them.
const BROWSER_SELECT: &str = "SELECT b.id, b.signal_id, s.subject, b.url, b.prompt, b.status, \
     b.findings, b.error, b.attempts, b.created_at, b.updated_at, \
     COALESCE(b.method, 'browser'), b.evidence \
     FROM browser_investigations b";

/// Column list for [`row_to_cloud_entity`].
const CLOUD_SELECT: &str = "SELECT id, kind, name, status, fields, related, error, fetched_at, \
     members FROM cloud_entities";

/// Column list for [`row_to_branch_pass`].
const BRANCH_PASS_SELECT: &str = "SELECT id, branch_key, repo, head, base, head_sha, reviewer, \
     provider, model, status, recommendation, summary, comments, dismissed, error, created_at, \
     updated_at FROM branch_review_passes";

/// Column list for [`row_to_thread_analysis`].
const THREAD_SELECT: &str = "SELECT id, channel, thread_ts, url, reply_count, status, verdict, \
     error, created_at, updated_at FROM thread_analyses";

/// Column list for [`row_to_repo`].
const REPO_SELECT: &str = "SELECT full_name, description, topics, language, archived, pushed_at, \
     readme_etag, readme, summary, indexed_sha, digest, fetched_at, kind, kind_pinned \
     FROM repo_index";

/// Column list for [`row_to_pr_fix`].
const PR_FIX_SELECT: &str = "SELECT issue_key, pr_repo, pr_number, pr_title, pr_url, pr_author, \
     pr_state, files, verdict, confidence, implementation, critique, conversation, \
     review_state, also_fixes, analyzed_by, created_at, updated_at FROM issue_pr_fixes";

/// Column list for [`row_to_issue_triage`].
const TRIAGE_SELECT: &str = "SELECT issue_key, repo, number, title, url, signal_id, status, \
     head_sha, checkout, files, characterization, patches, plain_summary, error, created_at, \
     updated_at FROM issue_triage";

/// Column list for [`row_to_persona_prediction`].
const PREDICTION_SELECT: &str = "SELECT persona, subject_key, kind, watermark, would_engage, \
     confidence, recommendation, summary, points, caveats, produced_by, created_at \
     FROM persona_predictions";

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
    /// Which tier is on this question: `grafana` (the HTTP API, real series) or
    /// `browser` (the operator's signed-in Chrome, a rendered page). Rows written before
    /// the Grafana tier existed read as `browser`, which is what they were.
    pub method: String,
    /// The series the conclusion was drawn from, as JSON — `grafana` only. This is what
    /// makes the conclusion checkable rather than merely plausible.
    pub evidence: Option<String>,
}

/// One notification the tidier cleared, or would have.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TidiedNotification {
    pub thread_id: String,
    /// GitHub's notification reason — `ci_activity`, `subscribed`, …
    pub reason: String,
    /// Which rule fired: `ci_activity` or `merged`.
    pub rule: String,
    pub repo: String,
    pub title: String,
    pub url: Option<String>,
    /// False in a dry run, and false when the API call failed — so this never claims
    /// something happened that did not.
    pub cleared: bool,
    pub at: String,
}

/// One hand-requested Slack thread analysis.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ThreadAnalysis {
    pub id: String,
    pub channel: String,
    pub thread_ts: String,
    pub url: String,
    pub reply_count: i64,
    /// `pending` → `running` → `completed` | `failed`.
    pub status: String,
    /// The `thread::Verdict`, as JSON. Passed to the UI untouched.
    pub verdict: Option<String>,
    pub error: Option<String>,
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
    /// The reviewers' verdict — `approved`, `changes_requested`, `commented` — or
    /// `None` when nobody has reviewed.
    ///
    /// Not to be confused with `verdict`, which is MuggleBot's own reading of whether
    /// this PR fixes the issue. This is the humans'.
    pub review_state: Option<String>,
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
        // WAL lets a reader and a writer coexist, but not two writers: the second gets
        // `SQLITE_BUSY` immediately unless told to wait. That used to be unreachable — one
        // daemon owned the file — and stopped being so when the maintenance subcommands
        // arrived, since `mugglebot cloud-warm` writes a couple of thousand rows alongside a
        // running daemon. Waiting is the correct answer for every one of those collisions:
        // they are short transactions, and the alternative is a spurious failure mid-sweep.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
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

    /// Resolve incidents absent from a complete listing of incident.io's **open** set.
    ///
    /// The removal half of "all open incidents tracked, resolved ones removed". Unlike the
    /// GitHub reconcilers this needs no half-selection: the incident source has exactly one
    /// listing and it is authoritative for all of it.
    pub fn resolve_missing_incidents(&self, active_ids: &BTreeSet<String>) -> Result<Vec<Signal>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut candidates = {
            let mut stmt = tx.prepare(&format!(
                "{SIGNAL_SELECT} WHERE source = ?1 AND upstream_gone = 0"
            ))?;
            let rows = stmt.query_map(params![Source::IncidentIo.as_str()], row_to_signal)?;
            collect(rows)?
        };
        let mut resolved = Vec::new();
        for signal in &mut candidates {
            if active_ids.contains(&signal.external_id) {
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
        // The mentions go; `cloud_entities` stays. That table is keyed by a platform id
        // rather than by a signal, and re-reading an account from `rcc` to learn what it
        // already knows is a subprocess per id for no new information.
        tx.execute("DELETE FROM signal_cloud_ids", [])?;
        tx.execute("DELETE FROM thread_analyses", [])?;
        tx.execute("DELETE FROM tidied_notifications", [])?;
        tx.execute("DELETE FROM source_snoozes", [])?;
        tx.execute("DELETE FROM branch_review_passes", [])?;
        tx.execute("DELETE FROM branch_diffs", [])?;
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

    /// Operator triage for many subjects at once, in one transaction.
    ///
    /// Not a loop over [`Self::set_handled`], for two reasons that both showed up as soon as
    /// the board grew a bulk action. Forty-one separate writes is forty-one lock acquisitions
    /// against a file a live daemon is also writing; and each one is separately visible, so a
    /// board broadcast landing mid-loop renders a half-triaged board. One transaction makes
    /// the whole triage a single fact.
    ///
    /// Returns how many rows actually changed — which can be fewer than `keys`, since a key
    /// the caller was looking at may have been merged away or deleted in between.
    pub fn set_handled_bulk(
        &self,
        keys: &[String],
        handled: Handled,
        snoozed_until: Option<DateTime<Utc>>,
    ) -> Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let now = Utc::now().to_rfc3339();
        let until = snoozed_until.map(|d| d.to_rfc3339());
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut changed = 0usize;
        {
            let mut stmt = tx.prepare(
                "UPDATE subjects SET handled = ?2, snoozed_until = ?3, updated_at = ?4 \
                 WHERE key = ?1",
            )?;
            for key in keys {
                changed += stmt.execute(params![key, handled.as_str(), until, now])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Return subjects whose snooze has run out to the board.
    ///
    /// The half of "snooze until" that was missing. `snoozed_until` has always been *written*
    /// — by the per-subject snooze, and by a source snooze taking its subjects off the board —
    /// and never read: `Projection::views` drops anything whose `handled` is `Snoozed` and
    /// looks no further. So every snooze with an expiry was in practice indefinite, and the
    /// only ways back were un-snoozing by hand or a new signal the operator was personally in.
    /// A duration the operator picks has to mean something, and this is what makes it mean it.
    ///
    /// A *sweep* rather than a check on read, because `handled` is what the notifier, the
    /// correlation engine and the root-cause guard all consult. Waking only when somebody
    /// looks at the board would leave those three silently suppressing work whose snooze had
    /// expired hours earlier. Writing the state back keeps every consumer honest without
    /// teaching any of them about expiry.
    ///
    /// Idempotent and restart-safe, so a tick that is late or lost costs latency and nothing
    /// else — the row still says when it was due. Subjects snoozed with **no** expiry are
    /// untouched: "off the board until it moves" is a different thing to have asked for.
    pub fn wake_lapsed_snoozes(&self) -> Result<Vec<String>> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT key FROM subjects \
             WHERE handled = 'snoozed' AND snoozed_until IS NOT NULL AND snoozed_until <= ?1",
        )?;
        let due: Vec<String> = collect(stmt.query_map([&now], |r| r.get::<_, String>(0))?)?;
        if due.is_empty() {
            return Ok(due);
        }
        // Back to `open`, not `seen`: the subject was taken off the board without being read,
        // and returning it as already-seen would hide it in the same breath as un-hiding it.
        conn.execute(
            "UPDATE subjects SET handled = 'open', snoozed_until = NULL, updated_at = ?1 \
             WHERE handled = 'snoozed' AND snoozed_until IS NOT NULL AND snoozed_until <= ?1",
            [&now],
        )?;
        Ok(due)
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
    /// Queue one dashboard investigation for a signal, on the named tier.
    ///
    /// `ON CONFLICT DO NOTHING` on the unique `signal_id` is the point: a watcher re-emits
    /// the same alert on every poll, and one question about one alert should be asked once.
    /// Escalating to another tier is [`Self::escalate_investigation_to_browser`], a
    /// transition on this row rather than a second row.
    pub fn queue_browser_investigation(
        &self,
        signal_id: &str,
        url: &str,
        prompt: &str,
        method: &str,
    ) -> Result<BrowserInvestigation> {
        let now = Utc::now().to_rfc3339();
        let id = format!("binv/{}", new_id());
        let conn = self.lock();
        conn.execute(
            "INSERT INTO browser_investigations (id, signal_id, url, prompt, status, method, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'pending', ?6, ?5, ?5) \
             ON CONFLICT(signal_id) DO NOTHING",
            params![id, signal_id, url, prompt, now, method],
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
    /// Pending investigations as `(id, method)`. The method decides which workflow the
    /// scheduler submits — the two tiers have different scopes, because one Chrome means
    /// concurrency 1 and an HTTP read does not.
    pub fn list_browser_investigations_pending(&self) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, COALESCE(method, 'browser') FROM browser_investigations \
             WHERE status IN ('pending', 'running') ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        collect(rows)
    }

    /// Hand this investigation to the browser tier, recording why the API tier could not
    /// answer. The `why` lands in `error` so the reason survives on the record even when
    /// the browser then succeeds — "Grafana had no series for that window" is worth
    /// knowing about a rule, not just about one firing.
    pub fn escalate_investigation_to_browser(
        &self,
        id: &str,
        url: &str,
        prompt: &str,
        why: &str,
    ) -> Result<BrowserInvestigation> {
        let conn = self.lock();
        conn.execute(
            "UPDATE browser_investigations SET method='browser', status='pending', url=?2, \
             prompt=?3, error=?4, attempts=0, updated_at=?5 WHERE id=?1",
            params![id, url, prompt, why, Utc::now().to_rfc3339()],
        )?;
        conn.query_row(
            &format!("{BROWSER_SELECT} LEFT JOIN signals s ON s.id=b.signal_id WHERE b.id=?1"),
            [id],
            row_to_browser_investigation,
        )
        .context("investigation not found")
    }

    pub fn complete_browser_investigation(
        &self,
        id: &str,
        findings: &str,
    ) -> Result<BrowserInvestigation> {
        self.finish_browser_investigation(id, "completed", Some(findings.trim()), None)
    }

    /// Complete a Grafana read: the conclusion, plus the series it was drawn from.
    pub fn complete_investigation_with_evidence(
        &self,
        id: &str,
        findings: &str,
        evidence: &str,
    ) -> Result<BrowserInvestigation> {
        {
            let conn = self.lock();
            conn.execute(
                "UPDATE browser_investigations SET evidence=?2 WHERE id=?1",
                params![id, evidence],
            )?;
        }
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

    // ---- Restate Cloud entities (see `src/cloud.rs`) ----

    /// Record which platform ids a signal's text mentioned. Idempotent: a watcher re-emits
    /// the same signal on every poll, and the mentions in it do not change.
    pub fn link_signal_cloud_ids(&self, signal_id: &str, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for id in ids {
            tx.execute(
                "INSERT INTO signal_cloud_ids (signal_id, entity_id) VALUES (?1, ?2) \
                 ON CONFLICT DO NOTHING",
                params![signal_id, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn put_cloud_entity(&self, e: &crate::cloud::Entity) -> Result<()> {
        self.lock().execute(
            "INSERT INTO cloud_entities (id, kind, name, status, fields, related, error, fetched_at, members) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, name=excluded.name, \
             status=excluded.status, fields=excluded.fields, related=excluded.related, \
             error=excluded.error, fetched_at=excluded.fetched_at, members=excluded.members",
            params![
                e.id,
                e.kind,
                e.name,
                e.status,
                serde_json::to_string(&e.fields)?,
                serde_json::to_string(&e.related)?,
                e.error,
                e.fetched_at.to_rfc3339(),
                serde_json::to_string(&e.members)?,
            ],
        )?;
        Ok(())
    }

    pub fn get_cloud_entity(&self, id: &str) -> Result<Option<crate::cloud::Entity>> {
        self.lock()
            .query_row(
                &format!("{CLOUD_SELECT} WHERE id=?1"),
                [id],
                row_to_cloud_entity,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Mentioned ids that have never been read, or whose cached copy has expired.
    ///
    /// `ttl` applies to successful reads and `error_ttl` to failed ones — a lookup that
    /// failed because the operator's token expired should come back minutes after they run
    /// `rcc login`, not six hours later.
    pub fn stale_cloud_ids(
        &self,
        ttl: chrono::Duration,
        error_ttl: chrono::Duration,
        limit: i64,
    ) -> Result<Vec<String>> {
        let now = Utc::now();
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT m.entity_id FROM signal_cloud_ids m \
             LEFT JOIN cloud_entities c ON c.id = m.entity_id \
             WHERE c.id IS NULL \
                OR (c.error IS NULL     AND c.fetched_at < ?1) \
                OR (c.error IS NOT NULL AND c.fetched_at < ?2) \
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                (now - ttl).to_rfc3339(),
                (now - error_ttl).to_rfc3339(),
                limit
            ],
            |r| r.get::<_, String>(0),
        )?;
        collect(rows)
    }

    /// Everything worth putting in a subject's context box: the ids its signals mentioned,
    /// plus every ancestor those reach through `related`.
    ///
    /// The ancestors are walked here rather than stored against the signal so the link table
    /// keeps meaning "this signal said this id". An account that moves to a new organization
    /// is then correct on the next read of every subject that ever mentioned it, instead of
    /// leaving each one holding its own stale copy of the hierarchy.
    ///
    /// Mentions come first and ancestors follow, breadth-first, which is the order the panel
    /// wants: the thing somebody asked about, then what it belongs to.
    pub fn cloud_entities_for_subject(
        &self,
        subject_key: &str,
    ) -> Result<Vec<crate::cloud::Entity>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT m.entity_id FROM signal_cloud_ids m \
             JOIN signals s ON s.id = m.signal_id \
             WHERE s.subject = ?1 ORDER BY m.entity_id",
        )?;
        let seeds: Vec<String> =
            collect(stmt.query_map([subject_key], |r| r.get::<_, String>(0))?)?;

        let mut queue = seeds;
        let mut seen: std::collections::HashSet<String> = queue.iter().cloned().collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < queue.len() {
            let id = queue[i].clone();
            i += 1;
            let found = conn
                .query_row(
                    &format!("{CLOUD_SELECT} WHERE id=?1"),
                    [&id],
                    row_to_cloud_entity,
                )
                .optional()?;
            // Not yet resolved — the scheduler has it queued. Skipped rather than rendered
            // as an empty box: a box with nothing in it is worse than no box.
            let Some(entity) = found else { continue };
            for r in &entity.related {
                if seen.insert(r.clone()) {
                    queue.push(r.clone());
                }
            }
            out.push(entity);
        }
        Ok(out)
    }

    /// Free-text search over the cached entities, optionally narrowed to one kind.
    ///
    /// Matches id, name, status **and the curated field values** — which is the point: the
    /// question an operator actually arrives with is rarely "what is `env_201k…`". It is
    /// "which environments are on the `replit.3-node.xlarge-vqueues` profile", or "what is
    /// in `us-east1`", and those live in the fields.
    ///
    /// An exact id match sorts first so pasting an id lands on it rather than on whatever
    /// else happens to mention it. An empty query browses — see [`Self::browse_cloud_entities`],
    /// which is a different query for a genuinely different question.
    pub fn search_cloud_entities(
        &self,
        query: &str,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<crate::cloud::Entity>> {
        let q = query.trim();
        if q.is_empty() && kind.is_none() {
            return self.browse_cloud_entities(limit);
        }
        let like = format!("%{}%", q.to_lowercase());
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{CLOUD_SELECT} WHERE (?1 = '' OR lower(id) LIKE ?2 OR lower(COALESCE(name,'')) LIKE ?2 \
                OR lower(COALESCE(status,'')) LIKE ?2 OR lower(fields) LIKE ?2) \
               AND (?3 IS NULL OR kind = ?3) \
             ORDER BY (lower(id) = ?1) DESC, kind, COALESCE(name, id) \
             LIMIT ?4"
        ))?;
        let rows = stmt.query_map(
            params![q.to_lowercase(), like, kind, limit],
            row_to_cloud_entity,
        )?;
        collect(rows)
    }

    /// The unfiltered list: a fair share of every kind, rather than the first page of one.
    ///
    /// Browsing and searching are different questions and were sharing an `ORDER BY`. With no
    /// query the list was `ORDER BY kind, name LIMIT 200` over the whole estate — and `account`
    /// sorts first alphabetically, so on a warmed store the entire first page was 200 of the
    /// 1013 accounts. Every user, environment, region, group and organization was reachable
    /// only by clicking a chip or typing something, and nothing on screen said so: a list that
    /// is all one kind looks like a store that only holds that kind. The complaint it produced
    /// was "why are there no users in the directory", about 1326 cached users.
    ///
    /// So the browse takes an equal slice of each kind and lets the chips carry the real
    /// counts. `ROW_NUMBER() OVER (PARTITION BY kind …)` rather than a query per kind: one
    /// statement, and the share adjusts on its own when the platform grows a kind.
    fn browse_cloud_entities(&self, limit: i64) -> Result<Vec<crate::cloud::Entity>> {
        let kinds = self.cloud_entity_kinds()?.len().max(1) as i64;
        // Rounded up, so the slice never collapses to nothing on a large kind count, and the
        // outer LIMIT trims whatever the rounding over-delivers.
        let share = ((limit + kinds - 1) / kinds).max(1);
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, name, status, fields, related, error, fetched_at, members FROM ( \
                 SELECT *, ROW_NUMBER() OVER ( \
                     PARTITION BY kind ORDER BY COALESCE(name, id) \
                 ) AS rn FROM cloud_entities \
             ) WHERE rn <= ?1 ORDER BY kind, COALESCE(name, id) LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![share, limit], row_to_cloud_entity)?;
        collect(rows)
    }

    /// The entities that name `id` as a parent — an account's environments, an organization's
    /// accounts, a region's environments.
    ///
    /// The reverse of `related`, and the half that makes the graph explorable: `related` gets
    /// you upward from a mention, and this gets you back down. Scanned rather than indexed
    /// because the whole table is a couple of thousand rows; the quoting makes it an exact
    /// token match inside the JSON array rather than a prefix collision.
    pub fn cloud_entity_children(
        &self,
        id: &str,
        limit: i64,
    ) -> Result<(Vec<crate::cloud::Entity>, i64)> {
        let needle = format!("%\"{id}\"%");
        let conn = self.lock();
        let total: i64 = conn.query_row(
            "SELECT count(*) FROM cloud_entities WHERE related LIKE ?1",
            [&needle],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(&format!(
            "{CLOUD_SELECT} WHERE related LIKE ?1 ORDER BY kind, COALESCE(name, id) LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![needle, limit], row_to_cloud_entity)?;
        Ok((collect(rows)?, total))
    }

    /// The entities `id` names in its `members` that do **not** already name it back.
    ///
    /// The other half of "what belongs to this", and the half the reverse edge cannot find. A
    /// user who owns an account names it, so [`Self::cloud_entity_children`] already returns
    /// them; a user who is merely a member of an organization names *nothing* — their document
    /// lists `accountMemberships`, and an org member has none. On the real estate that is 884
    /// of 1326 people: searchable by email, and completely unreachable by walking, which is
    /// most of what the Cloud page is for.
    ///
    /// The `related` exclusion is what keeps an account's owners from appearing twice, and it
    /// is done in SQL rather than against the capped child list so it stays exact however many
    /// children there are.
    pub fn cloud_entity_members(
        &self,
        id: &str,
        member_ids: &[String],
        limit: i64,
    ) -> Result<(Vec<crate::cloud::Entity>, i64)> {
        if member_ids.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let needle = format!("%\"{id}\"%");
        // A bound parameter per id: these come from a cached document rather than from a
        // caller, but building an IN list by interpolation is a habit worth not having.
        let holes = std::iter::repeat_n("?", member_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = member_ids
            .iter()
            .map(|m| Box::new(m.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        args.push(Box::new(needle));
        let conn = self.lock();

        let total: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM cloud_entities WHERE id IN ({holes}) AND related NOT LIKE ?"
            ),
            rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
            |r| r.get(0),
        )?;

        args.push(Box::new(limit));
        let mut stmt = conn.prepare(&format!(
            "{CLOUD_SELECT} WHERE id IN ({holes}) AND related NOT LIKE ? \
             ORDER BY kind, COALESCE(name, id) LIMIT ?"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
            row_to_cloud_entity,
        )?;
        Ok((collect(rows)?, total))
    }

    /// Every stored signal's id and text, for re-scanning mentions.
    ///
    /// Exists because the scanner can be *wrong*, and was: it matched only `env_201k…` while
    /// every alert label carries `env-201k…`, so 713 real mentions were never recorded. A
    /// watcher re-emitting a signal would fix its own row, but nothing re-emits an alert from
    /// last month — and those are exactly the ones worth having a context box on.
    ///
    /// Streamed as `(id, text)` rather than as `Signal`s: the scan needs the title and body
    /// and nothing else, and this runs over the whole table.
    pub fn signals_for_rescan(&self) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, title || char(10) || COALESCE(body, '') FROM signals")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        collect(rows)
    }

    /// Find an entity ignoring case, returning its **canonical** id.
    ///
    /// Recovers the casing the hyphenated form throws away. Kubernetes names an environment
    /// `env-201k…`, and that is what alert labels carry; environment ids are lowercase so the
    /// round trip is exact, but account and organization ids are mixed-case base62, and
    /// `acc-10vk05bjnjt1nlxvid4zh14` is not a thing `rcc` will ever find. The warmed estate
    /// knows every real id, so it can put the casing back.
    ///
    /// Returns `None` when nothing matches, which is the honest answer: without a warmed
    /// estate there is nothing to recover it from.
    pub fn find_cloud_entity_ci(&self, id: &str) -> Result<Option<String>> {
        self.lock()
            .query_row(
                "SELECT id FROM cloud_entities WHERE lower(id) = lower(?1) LIMIT 1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Kinds present in the cache with their counts — the filter chips, and an honest
    /// statement of what a search is searching.
    pub fn cloud_entity_kinds(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT kind, count(*) FROM cloud_entities GROUP BY kind ORDER BY count(*) DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        collect(rows)
    }

    /// Cache the diff a branch's reviewers will read.
    pub fn set_branch_diff(
        &self,
        branch_key: &str,
        head_sha: &str,
        report: &crate::prdiff::DiffReport,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO branch_diffs (branch_key, head_sha, report, fetched_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(branch_key, head_sha) DO UPDATE SET report=excluded.report, \
             fetched_at=excluded.fetched_at",
            params![
                branch_key,
                head_sha,
                serde_json::to_string(report)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// The cached diff for one commit of one branch.
    pub fn get_branch_diff(
        &self,
        branch_key: &str,
        head_sha: &str,
    ) -> Result<Option<crate::prdiff::DiffReport>> {
        let conn = self.lock();
        let raw: Option<String> = match conn.query_row(
            "SELECT report FROM branch_diffs WHERE branch_key=?1 AND head_sha=?2",
            params![branch_key, head_sha],
            |r| r.get(0),
        ) {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
    }

    /// Queue one reviewer's pass, or hand back the one that already exists.
    ///
    /// The unique key is `(branch, sha, reviewer, provider, model)`: asking the same reviewer
    /// on the same model about the same commit is the same question, and it should cost
    /// nothing the second time. Re-asking is what `re_open` is for.
    pub fn queue_branch_pass(&self, p: &crate::ghreview::Pass) -> Result<crate::ghreview::Pass> {
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO branch_review_passes \
                 (id, branch_key, repo, head, base, head_sha, reviewer, provider, model, \
                  status, created_at, updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending',?10,?10) \
                 ON CONFLICT(branch_key, head_sha, reviewer, provider, model) DO NOTHING",
                params![
                    p.id,
                    p.branch_key,
                    p.repo,
                    p.head,
                    p.base,
                    p.head_sha,
                    p.by.as_key(),
                    p.provider,
                    p.model,
                    Utc::now().to_rfc3339()
                ],
            )?;
        }
        self.branch_pass_for(
            &p.branch_key,
            &p.head_sha,
            &p.by.as_key(),
            &p.provider,
            &p.model,
        )?
        .context("branch review pass was not persisted")
    }

    fn branch_pass_for(
        &self,
        branch_key: &str,
        head_sha: &str,
        reviewer: &str,
        provider: &str,
        model: &str,
    ) -> Result<Option<crate::ghreview::Pass>> {
        let conn = self.lock();
        match conn.query_row(
            &format!(
                "{BRANCH_PASS_SELECT} WHERE branch_key=?1 AND head_sha=?2 AND reviewer=?3 \
                 AND provider=?4 AND model=?5"
            ),
            params![branch_key, head_sha, reviewer, provider, model],
            row_to_branch_pass,
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_branch_pass(&self, id: &str) -> Result<Option<crate::ghreview::Pass>> {
        let conn = self.lock();
        match conn.query_row(
            &format!("{BRANCH_PASS_SELECT} WHERE id=?1"),
            [id],
            row_to_branch_pass,
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Every pass on one branch, newest first. Dismissed ones included — the view shows them
    /// struck through rather than pretending they were never asked for.
    pub fn branch_passes(&self, branch_key: &str) -> Result<Vec<crate::ghreview::Pass>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{BRANCH_PASS_SELECT} WHERE branch_key=?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([branch_key], row_to_branch_pass)?;
        collect(rows)
    }

    /// Branches that have been reviewed, newest activity first.
    pub fn branch_review_keys(&self, limit: i64) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT branch_key, repo, head, MAX(updated_at) FROM branch_review_passes \
             GROUP BY branch_key ORDER BY MAX(updated_at) DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit.max(1)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        collect(rows)
    }

    pub fn list_branch_passes_pending(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id FROM branch_review_passes WHERE status IN ('pending','running') \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        collect(rows)
    }

    pub fn set_branch_pass_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE branch_review_passes SET status=?2, updated_at=?3 WHERE id=?1",
            params![id, status, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Record a finished pass.
    pub fn finish_branch_pass(
        &self,
        id: &str,
        recommendation: Option<&str>,
        summary: Option<&str>,
        comments: &[crate::ghreview::Placed],
    ) -> Result<crate::ghreview::Pass> {
        {
            let conn = self.lock();
            conn.execute(
                "UPDATE branch_review_passes SET status='done', recommendation=?2, summary=?3, \
                 comments=?4, error=NULL, updated_at=?5 WHERE id=?1",
                params![
                    id,
                    recommendation,
                    summary,
                    serde_json::to_string(comments)?,
                    Utc::now().to_rfc3339()
                ],
            )?;
        }
        self.get_branch_pass(id)?.context("pass not found")
    }

    pub fn fail_branch_pass(&self, id: &str, error: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE branch_review_passes SET status='failed', error=?2, updated_at=?3 \
             WHERE id=?1",
            params![id, error, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Dismiss or restore a pass. A flag, never a delete — see [`crate::ghreview`].
    pub fn set_branch_pass_dismissed(&self, id: &str, dismissed: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE branch_review_passes SET dismissed=?2, updated_at=?3 WHERE id=?1",
            params![id, dismissed as i64, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Put a pass back to `pending` so it runs again — the deliberate re-ask.
    pub fn reopen_branch_pass(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE branch_review_passes SET status='pending', dismissed=0, error=NULL, \
             comments=NULL, recommendation=NULL, summary=NULL, updated_at=?2 WHERE id=?1",
            params![id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Write (or replace) a source snooze. One row per source: re-snoozing is the later
    /// snooze winning, not two snoozes stacking.
    pub fn set_source_snooze(&self, s: &crate::quiet::Snooze) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO source_snoozes (source, until, floor, set_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(source) DO UPDATE SET until=excluded.until, floor=excluded.floor, \
             set_at=excluded.set_at",
            params![
                s.source.as_str(),
                s.until.to_rfc3339(),
                floor_str(s.floor),
                s.set_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Every stored snooze, expired ones included — [`crate::quiet::Quiet`] filters by time,
    /// so the store never has an opinion about "now".
    pub fn list_source_snoozes(&self) -> Result<Vec<crate::quiet::Snooze>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT source, until, floor, set_at FROM source_snoozes")?;
        let rows = stmt.query_map([], |r| {
            let source: String = r.get(0)?;
            let until: String = r.get(1)?;
            let floor: String = r.get(2)?;
            let set_at: String = r.get(3)?;
            Ok((source, until, floor, set_at))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (source, until, floor, set_at) = row?;
            // A row naming a source this build does not know is skipped rather than fatal:
            // it can only come from a newer build, and refusing to read the table would take
            // the whole rail down over one unknown string.
            let Some(source) = crate::signal::Source::parse(&source) else {
                continue;
            };
            let Ok(until) = DateTime::parse_from_rfc3339(&until) else {
                continue;
            };
            let set_at = DateTime::parse_from_rfc3339(&set_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            out.push(crate::quiet::Snooze {
                source,
                until: until.with_timezone(&Utc),
                floor: parse_floor(&floor),
                set_at,
            });
        }
        Ok(out)
    }

    /// End a snooze. `false` when there was nothing to end.
    pub fn clear_source_snooze(&self, source: crate::signal::Source) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM source_snoozes WHERE source=?1",
            params![source.as_str()],
        )?;
        Ok(n > 0)
    }

    /// Snooze every open subject whose signals come **only** from this source.
    ///
    /// `only` is the whole point of the query. A pull request with Slack chatter on it is not
    /// a Slack notification, and snoozing Slack must not take it off the board — so the
    /// `NOT EXISTS` clause excludes any subject carrying a signal from anywhere else.
    ///
    /// Subjects whose highest severity reaches the floor are left alone: if a thread contains
    /// something critical, the source snooze is not what should hide it.
    pub fn snooze_subjects_only_from(
        &self,
        source: crate::signal::Source,
        until: DateTime<Utc>,
        floor: Option<crate::signal::Severity>,
    ) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE subjects SET handled='snoozed', snoozed_until=?2, updated_at=?3 \
             WHERE handled IN ('open','seen') \
               AND EXISTS (SELECT 1 FROM signals g WHERE g.subject = subjects.key \
                           AND g.source = ?1) \
               AND NOT EXISTS (SELECT 1 FROM signals g WHERE g.subject = subjects.key \
                               AND g.source <> ?1) \
               AND NOT EXISTS (SELECT 1 FROM signals g WHERE g.subject = subjects.key \
                               AND g.severity IN (SELECT value FROM json_each(?4)))",
            params![
                source.as_str(),
                until.to_rfc3339(),
                Utc::now().to_rfc3339(),
                serde_json::to_string(&floor.map(at_or_above).unwrap_or_default())?
            ],
        )?;
        Ok(n)
    }

    /// Record a tidy decision. Idempotent on the thread id: GitHub re-lists an unread
    /// thread on every poll, and a failed clear will be reconsidered next time.
    pub fn record_tidy(&self, row: &TidiedNotification) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tidied_notifications \
             (thread_id, reason, rule, repo, title, url, cleared, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(thread_id) DO UPDATE SET \
               cleared = tidied_notifications.cleared OR excluded.cleared, \
               at = excluded.at",
            params![
                row.thread_id,
                row.reason,
                row.rule,
                row.repo,
                row.title,
                row.url,
                row.cleared as i64,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Walk back a `cleared` claim after the API call failed. The audit trail must not say
    /// something left the inbox when it is still sitting there.
    pub fn unrecord_tidy_cleared(&self, thread_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tidied_notifications SET cleared=0 WHERE thread_id=?1",
            params![thread_id],
        )?;
        Ok(())
    }

    /// What the tidier has taken, or would take — newest first.
    pub fn list_tidied(&self, limit: i64) -> Result<Vec<TidiedNotification>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT thread_id, reason, rule, repo, title, url, cleared, at \
             FROM tidied_notifications ORDER BY at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit.max(1)], |r| {
            Ok(TidiedNotification {
                thread_id: r.get(0)?,
                reason: r.get(1)?,
                rule: r.get(2)?,
                repo: r.get(3)?,
                title: r.get(4)?,
                url: r.get(5)?,
                cleared: r.get::<_, i64>(6)? != 0,
                at: r.get(7)?,
            })
        })?;
        collect(rows)
    }

    /// Queue (or re-queue) an analysis of one Slack thread.
    ///
    /// Re-pasting the same link updates `reply_count` and puts the row back to `pending`
    /// rather than inserting a second one — because "analyse this again, there are new
    /// replies" is the same question about the same thread, and the workflow key carries the
    /// count so the re-run is not refused as a duplicate.
    pub fn queue_thread_analysis(
        &self,
        channel: &str,
        thread_ts: &str,
        url: &str,
        reply_count: i64,
    ) -> Result<ThreadAnalysis> {
        let now = Utc::now().to_rfc3339();
        let id = format!("thr/{}", new_id());
        let conn = self.lock();
        conn.execute(
            "INSERT INTO thread_analyses \
             (id, channel, thread_ts, url, reply_count, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6) \
             ON CONFLICT(channel, thread_ts) DO UPDATE SET \
               reply_count=excluded.reply_count, url=excluded.url, status='pending', \
               error=NULL, updated_at=excluded.updated_at",
            params![id, channel, thread_ts, url, reply_count, now],
        )?;
        conn.query_row(
            &format!("{THREAD_SELECT} WHERE channel=?1 AND thread_ts=?2"),
            params![channel, thread_ts],
            row_to_thread_analysis,
        )
        .context("thread analysis was not persisted")
    }

    pub fn get_thread_analysis(&self, id: &str) -> Result<Option<ThreadAnalysis>> {
        let conn = self.lock();
        match conn.query_row(
            &format!("{THREAD_SELECT} WHERE id=?1"),
            [id],
            row_to_thread_analysis,
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Newest first — the list the Threads view shows.
    pub fn list_thread_analyses(&self, limit: i64) -> Result<Vec<ThreadAnalysis>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{THREAD_SELECT} ORDER BY updated_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit.max(1)], row_to_thread_analysis)?;
        collect(rows)
    }

    pub fn list_thread_analyses_pending(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id FROM thread_analyses WHERE status IN ('pending','running') \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        collect(rows)
    }

    pub fn finish_thread_analysis(
        &self,
        id: &str,
        status: &str,
        verdict: Option<&str>,
        error: Option<&str>,
    ) -> Result<ThreadAnalysis> {
        {
            let conn = self.lock();
            conn.execute(
                "UPDATE thread_analyses SET status=?2, verdict=COALESCE(?3, verdict), \
                 error=?4, updated_at=?5 WHERE id=?1",
                params![id, status, verdict, error, Utc::now().to_rfc3339()],
            )?;
        }
        self.get_thread_analysis(id)?
            .context("thread analysis not found")
    }

    /// The investigation for one signal, if any. The unique `signal_id` makes this at most
    /// one row, which is what lets ingest ask "has this alert already been looked at?".
    pub fn get_browser_investigation_for_signal(
        &self,
        signal_id: &str,
    ) -> Result<Option<BrowserInvestigation>> {
        let conn = self.lock();
        Self::browser_investigation_for_signal_locked(&conn, signal_id)
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
              verdict, confidence, implementation, critique, conversation, review_state, \
              also_fixes, analyzed_by, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17) \
             ON CONFLICT(issue_key, pr_repo, pr_number) DO UPDATE SET \
               pr_title=excluded.pr_title, pr_url=excluded.pr_url, pr_author=excluded.pr_author, \
               pr_state=excluded.pr_state, files=excluded.files, verdict=excluded.verdict, \
               confidence=excluded.confidence, implementation=excluded.implementation, \
               critique=excluded.critique, conversation=excluded.conversation, \
               review_state=excluded.review_state, also_fixes=excluded.also_fixes, \
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
                f.review_state,
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

    /// The newest commit cached for one repo — where a forward fetch resumes from.
    ///
    /// The counterpart to [`Self::oldest_commit_at`], and the two are needed for opposite
    /// reasons: the backward walk terminates at the root, while the forward fetch never
    /// does, because a repo keeps receiving commits.
    pub fn newest_commit_at(&self, full_name: &str) -> Result<Option<DateTime<Utc>>> {
        let conn = self.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT MAX(committed_at) FROM repo_commits WHERE full_name = ?1",
                [full_name],
                |r| r.get(0),
            )
            .optional()
            .map(Option::flatten)?;
        Ok(raw
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc)))
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

    // ---- personas -----------------------------------------------------------
    //
    // Note on locking throughout this section: [`Self::lock`] takes a non-reentrant
    // `std::sync::Mutex`, so a method that holds the guard must never call another method
    // that takes it. Where both are needed the guard is dropped explicitly first.

    /// The newest attributed signal id for a subject — the freshness token every
    /// per-subject workflow key is versioned on.
    ///
    /// Lives here rather than on one caller because two of them now need it and they must
    /// agree: `RootCause`, `Explain` and `PersonaPredict` all key on "nothing new has
    /// arrived", and two implementations of that phrase would eventually disagree about
    /// whether a subject had moved.
    pub fn subject_watermark(&self, subject_key: &str) -> String {
        self.signals_for_subject(subject_key)
            .ok()
            .and_then(|sigs| sigs.last().map(|s| s.id.clone()))
            .unwrap_or_else(|| "empty".into())
    }

    /// Signals authored by one handle on one source, newest first.
    ///
    /// The persona harvester's free half: Slack evidence is a re-reading of the signal log
    /// through one person's handle rather than new API traffic. Compared case-insensitively,
    /// because the actor field is whatever the watcher captured.
    pub fn signals_by_actor(
        &self,
        source: Source,
        handle: &str,
        limit: usize,
    ) -> Result<Vec<Signal>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{SIGNAL_SELECT} WHERE source = ?1 AND actor IS NOT NULL \
             AND LOWER(actor) = LOWER(?2) ORDER BY occurred_at DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(
            params![source.as_str(), handle, limit as i64],
            row_to_signal,
        )?;
        collect(rows)
    }

    /// Create or update a persona, including its identities.
    ///
    /// `created_at` is preserved across updates. Identities go through
    /// [`Self::link_persona_identity`], so an attempt to claim a handle another persona
    /// already owns fails loudly here rather than silently re-pointing it.
    pub fn put_persona(&self, p: &Persona) -> Result<()> {
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO personas \
                 (slug, display_name, role, notes, created_at, updated_at, harvested_at, \
                  profiled_at, evidence_watermark) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
                 ON CONFLICT(slug) DO UPDATE SET \
                    display_name=excluded.display_name, role=excluded.role, \
                    notes=excluded.notes, updated_at=excluded.updated_at",
                params![
                    p.slug,
                    p.display_name,
                    p.role,
                    p.notes,
                    p.created_at.to_rfc3339(),
                    p.updated_at.to_rfc3339(),
                    p.harvested_at.map(|t| t.to_rfc3339()),
                    p.profiled_at.map(|t| t.to_rfc3339()),
                    p.evidence_watermark,
                ],
            )?;
        }
        for identity in &p.identities {
            self.link_persona_identity(&p.slug, identity)?;
        }
        Ok(())
    }

    pub fn get_persona(&self, slug: &str) -> Result<Option<Persona>> {
        let identities = self.persona_identities(slug)?;
        let conn = self.lock();
        conn.query_row(
            "SELECT slug, display_name, role, notes, created_at, updated_at, harvested_at, \
             profiled_at, evidence_watermark FROM personas WHERE slug = ?1",
            [slug],
            |row| row_to_persona(row, identities.clone()),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every persona, most recently active first.
    pub fn list_personas(&self) -> Result<Vec<Persona>> {
        let slugs: Vec<String> = {
            let conn = self.lock();
            let mut stmt = conn.prepare("SELECT slug FROM personas ORDER BY updated_at DESC")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            collect(rows)?
        };
        let mut out = Vec::with_capacity(slugs.len());
        for slug in slugs {
            if let Some(p) = self.get_persona(&slug)? {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Remove a persona and everything derived from it.
    ///
    /// Deliberately complete: evidence, traits, refusals, predictions and identities all go.
    /// A "deleted" persona that left its harvested excerpts behind would be a profile of
    /// somebody the operator asked to stop modelling, which is the one outcome this must not
    /// have. Returns whether the persona existed.
    pub fn delete_persona(&self, slug: &str) -> Result<bool> {
        let conn = self.lock();
        for table in [
            "persona_identities",
            "persona_evidence",
            "persona_traits",
            "persona_removed",
            "persona_predictions",
            "persona_context",
        ] {
            conn.execute(&format!("DELETE FROM {table} WHERE persona = ?1"), [slug])?;
        }
        Ok(conn.execute("DELETE FROM personas WHERE slug = ?1", [slug])? > 0)
    }

    pub fn persona_identities(&self, slug: &str) -> Result<Vec<Identity>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT source, display, provenance, rationale FROM persona_identities \
             WHERE persona = ?1 ORDER BY source, handle",
        )?;
        let rows = stmt.query_map([slug], |row| {
            let source: String = row.get(0)?;
            let provenance: String = row.get(2)?;
            Ok(Identity {
                source: Source::parse(&source).unwrap_or(Source::GitHub),
                handle: row.get(1)?,
                provenance: IdentityProvenance::parse(&provenance)
                    // An unknown provenance is treated as a guess, which is the safe
                    // direction: it contributes no evidence until the operator confirms it.
                    .unwrap_or(IdentityProvenance::Proposed),
                rationale: row.get(3)?,
            })
        })?;
        collect(rows)
    }

    /// Attach a handle to a persona.
    ///
    /// Fails when another persona already owns the handle, naming it. Silently re-pointing
    /// would build two profiles from one person's writing and hand the operator two
    /// confident, differing answers about the same human — with nothing on screen to say
    /// which one is reading their words.
    pub fn link_persona_identity(&self, slug: &str, identity: &Identity) -> Result<()> {
        let handle = identity.handle.trim().to_ascii_lowercase();
        if handle.is_empty() {
            anyhow::bail!("an identity needs a handle");
        }
        let conn = self.lock();
        let owner: Option<String> = conn
            .query_row(
                "SELECT persona FROM persona_identities WHERE source = ?1 AND handle = ?2",
                params![identity.source.as_str(), handle],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(owner) = owner.filter(|o| o != slug) {
            anyhow::bail!(
                "{} '{}' already belongs to the persona '{owner}'. Unlink it there first — \
                 two personas sharing a handle would each build a profile from the same \
                 person's writing.",
                identity.source.as_str(),
                identity.handle
            );
        }
        conn.execute(
            "INSERT INTO persona_identities \
             (source, handle, persona, display, provenance, rationale, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7) \
             ON CONFLICT(source, handle) DO UPDATE SET \
                persona=excluded.persona, display=excluded.display, \
                provenance=excluded.provenance, rationale=excluded.rationale",
            params![
                identity.source.as_str(),
                handle,
                slug,
                identity.handle.trim(),
                identity.provenance.as_str(),
                identity.rationale,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Detach a handle. Returns whether it was attached.
    pub fn unlink_persona_identity(&self, source: Source, handle: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.execute(
            "DELETE FROM persona_identities WHERE source = ?1 AND handle = ?2",
            params![source.as_str(), handle.trim().to_ascii_lowercase()],
        )? > 0)
    }

    /// Which persona a handle belongs to, if any.
    pub fn persona_for_actor(&self, source: Source, handle: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT persona FROM persona_identities WHERE source = ?1 AND handle = ?2",
            params![source.as_str(), handle.trim().to_ascii_lowercase()],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Upsert harvested excerpts. Returns how many rows were written.
    ///
    /// Upsert rather than insert, because evidence ids are derived from the upstream identity
    /// of the excerpt: re-harvesting the same comment must refresh one row rather than append
    /// a second. An edited comment is the case that makes this matter — the excerpt changes
    /// and the id does not.
    pub fn put_persona_evidence(&self, evidence: &[Evidence]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut written = 0;
        for e in evidence {
            written += tx.execute(
                "INSERT INTO persona_evidence \
                 (id, persona, source, kind, subject_key, url, excerpt, context, state, \
                  occurred_at, ingested_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) \
                 ON CONFLICT(id) DO UPDATE SET \
                    excerpt=excluded.excerpt, context=excluded.context, state=excluded.state, \
                    subject_key=excluded.subject_key, url=excluded.url, \
                    ingested_at=excluded.ingested_at",
                params![
                    e.id,
                    e.persona,
                    e.source.as_str(),
                    e.kind.as_str(),
                    e.subject_key,
                    e.url,
                    e.excerpt,
                    e.context,
                    e.state,
                    e.occurred_at.to_rfc3339(),
                    e.ingested_at.to_rfc3339(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(written)
    }

    /// Harvested excerpts, oldest first.
    ///
    /// `limit` keeps the **most recent** N and still returns them oldest-first, which is the
    /// order a model should read them in. Taking the oldest N instead would hand the
    /// distiller a profile of who somebody was two years ago.
    pub fn persona_evidence(&self, slug: &str, limit: Option<usize>) -> Result<Vec<Evidence>> {
        let conn = self.lock();
        let mut out = match limit {
            Some(n) => {
                let mut stmt = conn.prepare(
                    "SELECT id, persona, source, kind, subject_key, url, excerpt, context, \
                     state, occurred_at, ingested_at FROM persona_evidence \
                     WHERE persona = ?1 ORDER BY occurred_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![slug, n as i64], row_to_persona_evidence)?;
                let mut v = collect(rows)?;
                v.reverse();
                v
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, persona, source, kind, subject_key, url, excerpt, context, \
                     state, occurred_at, ingested_at FROM persona_evidence \
                     WHERE persona = ?1 ORDER BY occurred_at ASC",
                )?;
                let rows = stmt.query_map([slug], row_to_persona_evidence)?;
                collect(rows)?
            }
        };
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    /// A token that changes whenever the evidence set does.
    ///
    /// `{count}@{newest ingested_at}`, and the count is the part that earns its place. The
    /// obvious token — the newest excerpt's id — does not change when the **backward** walk
    /// adds older material, and the backward walk is the main way a profile accumulates. So a
    /// `PersonaProfile` keyed on it would be refused as a duplicate for the entire backfill,
    /// and the profile would stay frozen at whatever the first pass happened to see. Nothing
    /// about that failure is visible: the workflow reports "already run at this key", which
    /// is normally the good answer.
    pub fn persona_evidence_watermark(&self, slug: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let (count, newest): (i64, Option<String>) = conn.query_row(
            "SELECT COUNT(*), MAX(ingested_at) FROM persona_evidence WHERE persona = ?1",
            [slug],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(newest.map(|n| format!("{count}@{n}")))
    }

    /// Replace a persona's profile: the traits that survived, and what was refused.
    ///
    /// One transaction, because the two halves are one fact. A partial write would leave the
    /// UI showing last pass's traits beside this pass's refusals, which reads as the filter
    /// having removed claims that are still on screen.
    pub fn replace_persona_traits(
        &self,
        slug: &str,
        traits: &[Trait],
        removed: &[Removed],
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM persona_traits WHERE persona = ?1", [slug])?;
        tx.execute("DELETE FROM persona_removed WHERE persona = ?1", [slug])?;
        for t in traits {
            tx.execute(
                "INSERT INTO persona_traits \
                 (id, persona, facet, claim, confidence, evidence, counter_evidence, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
                 ON CONFLICT(id) DO UPDATE SET \
                    claim=excluded.claim, confidence=excluded.confidence, \
                    evidence=excluded.evidence, counter_evidence=excluded.counter_evidence",
                params![
                    t.id,
                    slug,
                    t.facet.as_str(),
                    t.claim,
                    t.confidence,
                    json(&t.evidence)?,
                    json(&t.counter_evidence)?,
                    t.created_at.to_rfc3339(),
                ],
            )?;
        }
        for r in removed {
            tx.execute(
                "INSERT INTO persona_removed (persona, facet, claim, why, created_at) \
                 VALUES (?1,?2,?3,?4,?5)",
                params![slug, r.facet, r.claim, r.why, Utc::now().to_rfc3339()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn persona_traits(&self, slug: &str) -> Result<Vec<Trait>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, persona, facet, claim, confidence, evidence, counter_evidence, \
             created_at FROM persona_traits WHERE persona = ?1 ORDER BY confidence DESC",
        )?;
        let rows = stmt.query_map([slug], row_to_persona_trait)?;
        collect(rows)
    }

    pub fn persona_removed(&self, slug: &str) -> Result<Vec<Removed>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT facet, claim, why FROM persona_removed WHERE persona = ?1 \
             ORDER BY facet, created_at",
        )?;
        let rows = stmt.query_map([slug], |row| {
            Ok(Removed {
                facet: row.get(0)?,
                claim: row.get(1)?,
                why: row.get(2)?,
            })
        })?;
        collect(rows)
    }

    /// The whole profile, assembled: the persona, its verified traits, what was refused, and
    /// the counted facts. `None` when there is no such persona.
    pub fn persona_profile(&self, slug: &str) -> Result<Option<Profile>> {
        let Some(persona) = self.get_persona(slug)? else {
            return Ok(None);
        };
        let evidence = self.persona_evidence(slug, None)?;
        let traits = self.persona_traits(slug)?;
        // Computed on read rather than stored: it is a pure function of evidence plus traits,
        // both of which are already loaded here, and a stored copy would be a third thing to
        // keep in step with them. Same reasoning as `Stats`.
        let sme = crate::persona::sme::with_depth(crate::persona::sme::areas(&evidence), &traits);
        Ok(Some(Profile {
            persona,
            traits,
            removed: self.persona_removed(slug)?,
            stats: Stats::compute(&evidence),
            sme,
            context: self.persona_context(slug)?,
        }))
    }

    pub fn touch_persona_harvested(&self, slug: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE personas SET harvested_at = ?2, updated_at = ?2 WHERE slug = ?1",
            params![slug, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn touch_persona_profiled(&self, slug: &str, watermark: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE personas SET profiled_at = ?2, evidence_watermark = ?3, updated_at = ?2 \
             WHERE slug = ?1",
            params![slug, Utc::now().to_rfc3339(), watermark],
        )?;
        Ok(())
    }

    /// Attach a fact about a person. Returns the new entry's id.
    pub fn add_persona_context(
        &self,
        persona: &str,
        kind: &str,
        content: &str,
        summary: Option<&str>,
    ) -> Result<String> {
        let id = new_id();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO persona_context (id, persona, kind, content, summary, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, persona, kind, content, summary, Utc::now().to_rfc3339()],
        )?;
        Ok(id)
    }

    /// Everything the operator has attached to this person, oldest first.
    pub fn persona_context(&self, persona: &str) -> Result<Vec<crate::persona::Context>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, persona, kind, content, summary, created_at FROM persona_context \
             WHERE persona = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([persona], |row| {
            Ok(crate::persona::Context {
                id: row.get(0)?,
                persona: row.get(1)?,
                kind: row.get(2)?,
                content: row.get(3)?,
                summary: row.get(4)?,
                created_at: parse_time(row, 5)?,
            })
        })?;
        collect(rows)
    }

    pub fn remove_persona_context(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn.execute("DELETE FROM persona_context WHERE id = ?1", [id])? > 0)
    }

    // ---- the Slack directory ------------------------------------------------

    /// Replace the cached workspace directory.
    ///
    /// Replace rather than merge, so somebody who left the workspace leaves the cache — a
    /// directory that only ever grows would keep proposing people who are gone.
    pub fn put_slack_users(&self, users: &[crate::watchers::slack::SlackUser]) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM slack_users", [])?;
        for u in users {
            tx.execute(
                "INSERT INTO slack_users \
                 (id, name, real_name, display_name, email, is_bot, deleted, aliases, updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    u.id,
                    u.name,
                    u.real_name,
                    u.display_name,
                    u.email,
                    u.is_bot as i64,
                    u.deleted as i64,
                    json(&u.aliases())?,
                    now,
                ],
            )?;
        }
        tx.commit()?;
        Ok(users.len())
    }

    /// When the directory was last read, and how many entries it holds.
    pub fn slack_directory_age(&self) -> Result<(Option<DateTime<Utc>>, usize)> {
        let conn = self.lock();
        let (count, newest): (i64, Option<String>) = conn.query_row(
            "SELECT COUNT(*), MAX(updated_at) FROM slack_users",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((
            newest
                .and_then(|n| DateTime::parse_from_rfc3339(&n).ok())
                .map(|d| d.with_timezone(&Utc)),
            count as usize,
        ))
    }

    /// One Slack user by id.
    pub fn slack_user(&self, id: &str) -> Result<Option<crate::watchers::slack::SlackUser>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, real_name, display_name, email, is_bot, deleted \
             FROM slack_users WHERE id = ?1 COLLATE NOCASE",
            [id],
            row_to_slack_user,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Find a Slack user by anything somebody might type: the id, the `@handle`, their real
    /// name, or the local part of their email.
    ///
    /// This is what lets the operator link "pcholakov" or "Pavel Cholakov" without going to
    /// find an opaque id first — and what makes an `Exact` cross-source join possible at all,
    /// since a GitHub login matching an alias is a deterministic match rather than a guess.
    ///
    /// Exact alias match only. Fuzzy matching here would produce a *confident wrong join*,
    /// which is the one failure the identity model exists to prevent.
    pub fn find_slack_user(
        &self,
        needle: &str,
    ) -> Result<Option<crate::watchers::slack::SlackUser>> {
        let needle = needle.trim().trim_start_matches('@').to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(None);
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, real_name, display_name, email, is_bot, deleted, aliases \
             FROM slack_users WHERE deleted = 0",
        )?;
        let rows = stmt.query_map([], |row| {
            let aliases: Vec<String> = from_json_or_default(row, 7)?;
            Ok((row_to_slack_user(row)?, aliases))
        })?;
        for row in rows {
            let (user, aliases) = row?;
            if aliases.iter().any(|a| a == &needle) {
                return Ok(Some(user));
            }
        }
        Ok(None)
    }

    /// Workspace members whose handle or name looks like `needle`.
    ///
    /// For the "did you mean" on a failed link. Substring rather than exact, which is the
    /// opposite of [`Self::find_slack_user`] and deliberately so: this is a *suggestion* the
    /// operator reads, where that is a *join* nothing checks afterwards, and a loose match is
    /// helpful in one and dangerous in the other.
    pub fn slack_users_like(
        &self,
        needle: &str,
        limit: usize,
    ) -> Result<Vec<crate::watchers::slack::SlackUser>> {
        let needle = needle.trim().trim_start_matches('@').to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        // Longest common prefix is enough to make `lukebond` suggest `luke`: match on any
        // member whose name is a prefix of the needle, or vice versa.
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, real_name, display_name, email, is_bot, deleted \
             FROM slack_users WHERE deleted = 0 AND is_bot = 0 AND ( \
                 INSTR(LOWER(?1), LOWER(name)) > 0 OR INSTR(LOWER(name), LOWER(?1)) > 0 \
                 OR INSTR(LOWER(COALESCE(real_name, '')), LOWER(?1)) > 0 \
             ) ORDER BY LENGTH(name) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![needle, limit as i64], row_to_slack_user)?;
        collect(rows)
    }

    /// Record why the last harvest pass came back thin, or clear it on a clean pass.
    ///
    /// Clearing on success matters as much as setting it: a note left behind after the budget
    /// recovered would report a problem that has gone, which is the same class of lie as a
    /// stale "AI done" flag.
    pub fn set_persona_harvest_note(&self, slug: &str, note: Option<&str>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE personas SET harvest_note = ?2 WHERE slug = ?1",
            params![slug, note],
        )?;
        Ok(())
    }

    pub fn persona_harvest_note(&self, slug: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT harvest_note FROM personas WHERE slug = ?1",
            [slug],
            |r| r.get(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// How far back the GitHub walk has reached, and whether it is finished.
    pub fn persona_harvest_cursor(&self, slug: &str) -> Result<(Option<String>, bool)> {
        let conn = self.lock();
        let row: Option<(Option<String>, i64)> = conn
            .query_row(
                "SELECT harvest_cursor, harvest_complete FROM personas WHERE slug = ?1",
                [slug],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(c, done)| (c, done != 0)).unwrap_or((None, false)))
    }

    pub fn set_persona_harvest_cursor(
        &self,
        slug: &str,
        cursor: Option<&str>,
        complete: bool,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE personas SET harvest_cursor = ?2, harvest_complete = ?3 WHERE slug = ?1",
            params![slug, cursor, complete as i64],
        )?;
        Ok(())
    }

    /// Store a prediction, replacing any previous one for the same
    /// `(persona, subject, kind)`.
    pub fn put_persona_prediction(&self, p: &Prediction) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO persona_predictions \
             (persona, subject_key, kind, watermark, would_engage, confidence, \
              recommendation, summary, points, caveats, produced_by, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) \
             ON CONFLICT(persona, subject_key, kind) DO UPDATE SET \
                watermark=excluded.watermark, would_engage=excluded.would_engage, \
                confidence=excluded.confidence, recommendation=excluded.recommendation, \
                summary=excluded.summary, points=excluded.points, caveats=excluded.caveats, \
                produced_by=excluded.produced_by, created_at=excluded.created_at",
            params![
                p.persona,
                p.subject_key,
                p.kind.as_str(),
                p.watermark,
                p.would_engage as i64,
                p.confidence,
                p.recommendation,
                p.summary,
                json(&p.points)?,
                json(&p.caveats)?,
                p.produced_by,
                p.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_persona_prediction(
        &self,
        persona: &str,
        subject_key: &str,
        kind: PredictionKind,
    ) -> Result<Option<Prediction>> {
        let conn = self.lock();
        conn.query_row(
            &format!("{PREDICTION_SELECT} WHERE persona = ?1 AND subject_key = ?2 AND kind = ?3"),
            params![persona, subject_key, kind.as_str()],
            row_to_persona_prediction,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every prediction made about one subject, newest first — what the detail pane renders.
    pub fn predictions_for_subject(&self, subject_key: &str) -> Result<Vec<Prediction>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{PREDICTION_SELECT} WHERE subject_key = ?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([subject_key], row_to_persona_prediction)?;
        collect(rows)
    }

    /// Every prediction made *by* one persona, newest first — what the persona page renders.
    pub fn predictions_for_persona(&self, persona: &str, limit: usize) -> Result<Vec<Prediction>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{PREDICTION_SELECT} WHERE persona = ?1 ORDER BY created_at DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![persona, limit as i64], row_to_persona_prediction)?;
        collect(rows)
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
        method: row.get(11)?,
        evidence: row.get(12)?,
    })
}

fn row_to_cloud_entity(row: &Row) -> rusqlite::Result<crate::cloud::Entity> {
    let fields: String = row.get(4)?;
    let related: String = row.get(5)?;
    let members: String = row.get(8)?;
    Ok(crate::cloud::Entity {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        status: row.get(3)?,
        // A row this process wrote with `serde_json::to_string`. A parse failure means the
        // column was corrupted rather than that the entity has no fields, but an empty box
        // is a better outcome for the panel than a poisoned query.
        fields: serde_json::from_str(&fields).unwrap_or_default(),
        related: serde_json::from_str(&related).unwrap_or_default(),
        members: serde_json::from_str(&members).unwrap_or_default(),
        error: row.get(6)?,
        fetched_at: parse_ts(row, 7)?,
    })
}

/// A snooze floor as `source_snoozes.floor` spells it. `None` — silence everything — is
/// stored as the word `everything` rather than SQL NULL, so a row is never ambiguous about
/// whether a floor was set or simply lost.
fn floor_str(f: Option<crate::signal::Severity>) -> &'static str {
    match f {
        Some(s) => severity_str(s),
        None => "everything",
    }
}

/// Read a stored floor back. Anything unrecognised is read as `critical`, the safe direction:
/// a corrupted floor should let outages through, not swallow them.
fn parse_floor(raw: &str) -> Option<crate::signal::Severity> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "everything" | "all" | "none" => None,
        other => Some(crate::config::severity_from_str(other)),
    }
}

/// A severity as this module's own tables spell it — plain, matching
/// [`crate::config::severity_from_str`].
fn severity_str(s: crate::signal::Severity) -> &'static str {
    use crate::signal::Severity as S;
    match s {
        S::Info => "info",
        S::Notice => "notice",
        S::Warning => "warning",
        S::Critical => "critical",
    }
}

/// Severities at or above `floor`, in the **JSON-quoted** form the `signals.severity` column
/// actually holds.
///
/// That column is written with `json(&severity)`, so it stores `"warning"` *including the
/// quotes* — nine characters, not seven. Comparing against a bare `warning` matches nothing
/// and would silently make the "leave critical threads alone" guard a no-op, which is the
/// one thing in this feature that must not fail quietly. Serialising through serde is what
/// keeps the two forms identical by construction.
fn at_or_above(floor: crate::signal::Severity) -> Vec<String> {
    use crate::signal::Severity as S;
    [S::Info, S::Notice, S::Warning, S::Critical]
        .into_iter()
        .filter(|s| *s >= floor)
        .filter_map(|s| serde_json::to_string(&s).ok())
        .collect()
}

fn row_to_branch_pass(row: &Row) -> rusqlite::Result<crate::ghreview::Pass> {
    let comments: Option<String> = row.get(12)?;
    Ok(crate::ghreview::Pass {
        id: row.get(0)?,
        branch_key: row.get(1)?,
        repo: row.get(2)?,
        head: row.get(3)?,
        base: row.get(4)?,
        head_sha: row.get(5)?,
        by: crate::ghreview::Reviewer::parse(&row.get::<_, String>(6)?),
        provider: row.get(7)?,
        model: row.get(8)?,
        status: crate::ghreview::PassStatus::parse(&row.get::<_, String>(9)?),
        recommendation: row.get(10)?,
        summary: row.get(11)?,
        // A comments blob that will not parse degrades to no comments rather than failing the
        // row: the pass's own verdict is still worth showing, and one bad blob must not empty
        // the view for the other reviewers.
        comments: comments
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default(),
        dismissed: row.get::<_, i64>(13)? != 0,
        error: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn row_to_thread_analysis(row: &Row) -> rusqlite::Result<ThreadAnalysis> {
    Ok(ThreadAnalysis {
        id: row.get(0)?,
        channel: row.get(1)?,
        thread_ts: row.get(2)?,
        url: row.get(3)?,
        reply_count: row.get(4)?,
        status: row.get(5)?,
        verdict: row.get(6)?,
        error: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
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
        review_state: row.get(13)?,
        also_fixes: from_json::<Vec<String>>(row, 14)?,
        analyzed_by: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
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

// ---- persona row mappers -----------------------------------------------------
//
// Every enum-valued column is decoded with a fallback rather than a failure. A row written by
// a newer build — a facet or evidence kind this one has never heard of — must not fail the
// whole query and blank the profile; the same reasoning as the tolerant wire types in
// `crate::incident`. Where a fallback would be *unsafe* rather than merely lossy, it errs
// toward the harmless value: an unknown identity provenance becomes `Proposed`, which
// contributes no evidence (see `Store::persona_identities`).

fn row_to_persona(row: &Row, identities: Vec<Identity>) -> rusqlite::Result<Persona> {
    Ok(Persona {
        slug: row.get(0)?,
        display_name: row.get(1)?,
        role: row.get(2)?,
        notes: row.get(3)?,
        created_at: parse_time(row, 4)?,
        updated_at: parse_time(row, 5)?,
        harvested_at: parse_time_opt(row, 6)?,
        profiled_at: parse_time_opt(row, 7)?,
        evidence_watermark: row.get(8)?,
        identities,
    })
}

fn row_to_persona_evidence(row: &Row) -> rusqlite::Result<Evidence> {
    let source: String = row.get(2)?;
    let kind: String = row.get(3)?;
    Ok(Evidence {
        id: row.get(0)?,
        persona: row.get(1)?,
        source: Source::parse(&source).unwrap_or(Source::GitHub),
        kind: EvidenceKind::parse(&kind).unwrap_or(EvidenceKind::IssueComment),
        subject_key: row.get(4)?,
        url: row.get(5)?,
        excerpt: row.get(6)?,
        context: row.get(7)?,
        state: row.get(8)?,
        occurred_at: parse_time(row, 9)?,
        ingested_at: parse_time(row, 10)?,
    })
}

fn row_to_persona_trait(row: &Row) -> rusqlite::Result<Trait> {
    let facet: String = row.get(2)?;
    Ok(Trait {
        id: row.get(0)?,
        persona: row.get(1)?,
        facet: Facet::parse(&facet).unwrap_or(Facet::Style),
        claim: row.get(3)?,
        confidence: row.get(4)?,
        evidence: from_json_or_default(row, 5)?,
        counter_evidence: from_json_or_default(row, 6)?,
        created_at: parse_time(row, 7)?,
    })
}

fn row_to_persona_prediction(row: &Row) -> rusqlite::Result<Prediction> {
    let kind: String = row.get(2)?;
    let points: Vec<PredictedPoint> = from_json_or_default(row, 8)?;
    Ok(Prediction {
        persona: row.get(0)?,
        subject_key: row.get(1)?,
        kind: PredictionKind::parse(&kind).unwrap_or(PredictionKind::IssueResponse),
        watermark: row.get(3)?,
        would_engage: row.get::<_, i64>(4)? != 0,
        confidence: row.get(5)?,
        recommendation: row.get(6)?,
        summary: row.get(7)?,
        points,
        caveats: from_json_or_default(row, 9)?,
        produced_by: row.get(10)?,
        created_at: parse_time(row, 11)?,
    })
}

fn row_to_slack_user(row: &Row) -> rusqlite::Result<crate::watchers::slack::SlackUser> {
    Ok(crate::watchers::slack::SlackUser {
        id: row.get(0)?,
        name: row.get(1)?,
        real_name: row.get(2)?,
        display_name: row.get(3)?,
        email: row.get(4)?,
        is_bot: row.get::<_, i64>(5)? != 0,
        deleted: row.get::<_, i64>(6)? != 0,
    })
}

/// An RFC3339 column, decoded.
fn parse_time(row: &Row, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let raw: String = row.get(idx)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

fn parse_time_opt(row: &Row, idx: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = row.get(idx)?;
    Ok(raw
        .and_then(|r| DateTime::parse_from_rfc3339(&r).ok())
        .map(|d| d.with_timezone(&Utc)))
}

/// A JSON-encoded column, decoded to `T`, defaulting rather than failing.
///
/// Distinct from [`from_json`], which propagates a decode failure. These columns are all
/// lists of ids and strings, and an empty list degrades to "no citations" — which
/// verification already handles — where a failed row would blank the whole profile.
fn from_json_or_default<T: serde::de::DeserializeOwned + Default>(
    row: &Row,
    idx: usize,
) -> rusqlite::Result<T> {
    let raw: String = row.get(idx)?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
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
        ("issue_pr_fixes", "review_state", "TEXT"),
        // A nullable column on a table that already exists in any database created between
        // the personas feature landing and this fix — exactly the case this mechanism is for.
        ("personas", "harvest_note", "TEXT"),
        // The Grafana tier shares this table with the browser tier: one row per signal is
        // the invariant worth keeping (it is what stops a re-emitted signal re-queuing),
        // and "which tier is on it" is a state transition on that row, not a second row.
        ("browser_investigations", "method", "TEXT"),
        ("browser_investigations", "evidence", "TEXT"),
        // Downward edges, added when users joined the estate. A default rather than a
        // nullable column: every read treats it as a list, and an existing cache has no
        // members recorded for anything — which is the correct answer until the next warm.
        ("cloud_entities", "members", "TEXT NOT NULL DEFAULT '[]'"),
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

    fn cloud_entity(id: &str, related: &[&str], age: chrono::Duration) -> crate::cloud::Entity {
        crate::cloud::Entity {
            id: id.into(),
            kind: crate::cloud::kind_of(id).unwrap_or("unknown").into(),
            name: Some(id.into()),
            status: None,
            fields: vec![],
            related: related.iter().map(|s| s.to_string()).collect(),
            members: vec![],
            error: None,
            fetched_at: Utc::now() - age,
        }
    }

    /// The half of "what belongs to this" that the reverse edge cannot see.
    ///
    /// An organization's members name nothing — their document lists the accounts they own, and
    /// an org member owns none — so `cloud_entity_children` returns them not at all. On the real
    /// estate that was 884 of 1326 people: cached, searchable by email, and unreachable by
    /// walking. An account's owner *does* name it back, and must not then appear twice.
    #[test]
    fn an_organizations_members_are_found_even_though_they_name_nothing() {
        let store = Store::open_in_memory().unwrap();
        let zero = chrono::Duration::zero();
        let org = "org_10vj37k7NcqTaIavuJRsOcv";
        let owner = "usr_10uXl8CPozSuQiKolKUfqVI";
        let member = "usr_10vifGx1fTzG4tqpt2pt0Rp";
        let account = "acc_10vk05bJNjt1nLXvId4zh14";

        let mut org_entity = cloud_entity(org, &[], zero);
        org_entity.members = vec![owner.into(), member.into()];
        store.put_cloud_entity(&org_entity).unwrap();
        // The owner reaches the organization the ordinary way, through an account.
        store
            .put_cloud_entity(&cloud_entity(owner, &[org], zero))
            .unwrap();
        // The member names nothing at all — the case the reverse lookup cannot serve.
        store
            .put_cloud_entity(&cloud_entity(member, &[], zero))
            .unwrap();
        store
            .put_cloud_entity(&cloud_entity(account, &[org], zero))
            .unwrap();

        let (children, child_total) = store.cloud_entity_children(org, 200).unwrap();
        assert_eq!(child_total, 2, "only the two that name the organization");

        let (extra, extra_total) = store
            .cloud_entity_members(org, &org_entity.members, 200)
            .unwrap();
        assert_eq!(extra_total, 1);
        assert_eq!(extra[0].id, member, "the one nothing else would have found");
        // The owner is in `members` *and* in the reverse lookup, and appears once.
        assert!(!extra.iter().any(|e| e.id == owner));
        assert_eq!(children.len() + extra.len(), 3);

        // Nothing named, nothing found — and no query built from an empty IN list.
        assert_eq!(store.cloud_entity_members(org, &[], 200).unwrap().1, 0);
    }

    /// The panel wants the hierarchy, and only the *mention* is recorded against the signal.
    /// Walking `related` on read is what turns one pasted environment into the account,
    /// organization and Nuon install behind it.
    #[test]
    fn a_subjects_cloud_context_is_the_mention_plus_everything_it_reaches() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = sample("s1");
        sig.subject = Some("o/r#1".into());
        store.insert_signal(&sig).unwrap();
        store
            .link_signal_cloud_ids(&sig.id, &["env_201kpvffchrm1sq5p0qph42dqev".into()])
            .unwrap();

        let zero = chrono::Duration::zero();
        for e in [
            cloud_entity(
                "env_201kpvffchrm1sq5p0qph42dqev",
                &["acc_10vk05bJNjt1nLXvId4zh14", "rgn_26p9031nve"],
                zero,
            ),
            cloud_entity(
                "acc_10vk05bJNjt1nLXvId4zh14",
                &["org_10vj37k7NcqTaIavuJRsOcv"],
                zero,
            ),
            cloud_entity("rgn_26p9031nve", &["inltao53ge1b4pab3zo23si077"], zero),
            cloud_entity("org_10vj37k7NcqTaIavuJRsOcv", &[], zero),
            cloud_entity("inltao53ge1b4pab3zo23si077", &[], zero),
        ] {
            store.put_cloud_entity(&e).unwrap();
        }

        let got: Vec<String> = store
            .cloud_entities_for_subject("o/r#1")
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        // Breadth-first from the mention: the environment, then what it names, then what
        // those name. That order is what the panel renders.
        assert_eq!(
            got,
            vec![
                "env_201kpvffchrm1sq5p0qph42dqev",
                "acc_10vk05bJNjt1nLXvId4zh14",
                "rgn_26p9031nve",
                "org_10vj37k7NcqTaIavuJRsOcv",
                "inltao53ge1b4pab3zo23si077",
            ]
        );
    }

    /// An id the sweep has not reached yet is absent, not an empty box — and its absence
    /// must not stop the ids that *have* resolved from rendering.
    #[test]
    fn an_unresolved_mention_is_skipped_rather_than_rendered_blank() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = sample("s1");
        sig.subject = Some("o/r#1".into());
        store.insert_signal(&sig).unwrap();
        store
            .link_signal_cloud_ids(
                &sig.id,
                &[
                    "env_201kpvffchrm1sq5p0qph42dqev".into(),
                    "acc_10vk05bJNjt1nLXvId4zh14".into(),
                ],
            )
            .unwrap();
        store
            .put_cloud_entity(&cloud_entity(
                "acc_10vk05bJNjt1nLXvId4zh14",
                &[],
                chrono::Duration::zero(),
            ))
            .unwrap();

        let got = store.cloud_entities_for_subject("o/r#1").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "acc_10vk05bJNjt1nLXvId4zh14");
    }

    /// The sweep's work list: never-read ids, plus expired ones. A failed read expires on a
    /// much shorter clock, so `rcc login` takes effect on the next tick rather than tomorrow.
    #[test]
    fn the_sweep_picks_up_unread_and_expired_ids_on_separate_clocks() {
        let store = Store::open_in_memory().unwrap();
        let sig = sample("s1");
        store.insert_signal(&sig).unwrap();
        let ids: Vec<String> = [
            "env_201kpvffchrm1sq5p0qph42dqev",
            "acc_10vk05bJNjt1nLXvId4zh14",
            "rgn_26p9031nve",
            "org_10vj37k7NcqTaIavuJRsOcv",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        store.link_signal_cloud_ids(&sig.id, &ids).unwrap();

        // Fresh success; stale success; fresh-ish failure that is nonetheless past its own
        // shorter clock; and `env_…`, never read at all.
        store
            .put_cloud_entity(&cloud_entity(
                "acc_10vk05bJNjt1nLXvId4zh14",
                &[],
                chrono::Duration::minutes(5),
            ))
            .unwrap();
        store
            .put_cloud_entity(&cloud_entity(
                "rgn_26p9031nve",
                &[],
                chrono::Duration::hours(9),
            ))
            .unwrap();
        let mut failed = cloud_entity(
            "org_10vj37k7NcqTaIavuJRsOcv",
            &[],
            chrono::Duration::minutes(30),
        );
        failed.error = Some("token expired".into());
        store.put_cloud_entity(&failed).unwrap();

        let mut stale = store
            .stale_cloud_ids(
                chrono::Duration::hours(6),
                chrono::Duration::minutes(10),
                50,
            )
            .unwrap();
        stale.sort();
        assert_eq!(
            stale,
            vec![
                "env_201kpvffchrm1sq5p0qph42dqev",
                "org_10vj37k7NcqTaIavuJRsOcv",
                "rgn_26p9031nve",
            ],
            "the five-minute-old success is the only one left alone"
        );
    }

    /// The question an operator actually arrives with is rarely "what is `env_201k…`" — it is
    /// "which environments are on this profile" or "what is in us-east1". Both live in the
    /// curated fields, so the search has to reach them.
    #[test]
    fn searching_the_estate_reaches_field_values_not_just_ids() {
        let store = Store::open_in_memory().unwrap();
        let mut env = cloud_entity(
            "env_201kpvffchrm1sq5p0qph42dqev",
            &[],
            chrono::Duration::zero(),
        );
        env.name = Some("janeway".into());
        env.status = Some("PROVISIONING_READY".into());
        env.fields = vec![
            crate::cloud::Field {
                label: "profile".into(),
                value: "replit.3-node.xlarge-vqueues".into(),
            },
            crate::cloud::Field {
                label: "tier".into(),
                value: "CUSTOM".into(),
            },
        ];
        store.put_cloud_entity(&env).unwrap();
        let mut other = cloud_entity(
            "env_201kpvff44fbhw06x8wza0cptfw",
            &[],
            chrono::Duration::zero(),
        );
        other.name = Some("worf".into());
        other.fields = vec![crate::cloud::Field {
            label: "profile".into(),
            value: "standard.3-node.large".into(),
        }];
        store.put_cloud_entity(&other).unwrap();

        let hits = |q: &str| -> Vec<String> {
            store
                .search_cloud_entities(q, None, 50)
                .unwrap()
                .into_iter()
                .map(|e| e.id)
                .collect()
        };
        // By name, by field value, and case-insensitively by status.
        assert_eq!(hits("janeway"), vec!["env_201kpvffchrm1sq5p0qph42dqev"]);
        assert_eq!(
            hits("xlarge-vqueues"),
            vec!["env_201kpvffchrm1sq5p0qph42dqev"]
        );
        assert_eq!(
            hits("provisioning_ready"),
            vec!["env_201kpvffchrm1sq5p0qph42dqev"]
        );
        // An empty query browses rather than matching nothing.
        assert_eq!(hits("").len(), 2);
        // A kind with no rows narrows to nothing rather than ignoring the filter.
        assert!(store
            .search_cloud_entities("", Some("organization"), 50)
            .unwrap()
            .is_empty());
    }

    /// The unfiltered list must show the estate, not the alphabetically-first kind of it.
    ///
    /// Found by an operator asking why there were no users in the directory, about 1326 cached
    /// users. Browsing was `ORDER BY kind, name LIMIT 200` over everything, `account` sorts
    /// first, and there were 1013 accounts — so the whole first page was accounts and every
    /// other kind was past the end of it. A list that is all one kind reads as a store that
    /// holds only that kind, which is the sort of wrong that nothing on screen contradicts.
    #[test]
    fn browsing_shows_every_kind_rather_than_the_first_one_alphabetically() {
        let store = Store::open_in_memory().unwrap();
        let zero = chrono::Duration::zero();
        // Accounts outnumber everything, exactly as they do on the real estate.
        for i in 0..40 {
            store
                .put_cloud_entity(&cloud_entity(
                    &format!("acc_10vk05bJNjt1nLXvId4z{i:03}"),
                    &[],
                    zero,
                ))
                .unwrap();
        }
        for i in 0..5 {
            store
                .put_cloud_entity(&cloud_entity(
                    &format!("usr_10uXl8CPozSuQiKolKUf{i:03}"),
                    &[],
                    zero,
                ))
                .unwrap();
            store
                .put_cloud_entity(&cloud_entity(&format!("rgn_26p90311{i:02}"), &[], zero))
                .unwrap();
        }

        fn kinds(rows: &[crate::cloud::Entity]) -> Vec<&str> {
            let mut k: Vec<&str> = rows.iter().map(|e| e.kind.as_str()).collect();
            k.sort_unstable();
            k.dedup();
            k
        }

        // A limit far below the account count — the exact case that used to return accounts
        // and nothing else.
        let browsed = store.search_cloud_entities("", None, 12).unwrap();
        assert_eq!(kinds(&browsed), vec!["account", "region", "user"]);
        assert!(browsed.len() <= 12);

        // Picking a kind is still a hard filter, not a sample of it.
        let users = store.search_cloud_entities("", Some("user"), 50).unwrap();
        assert_eq!(users.len(), 5);
        assert_eq!(kinds(&users), vec!["user"]);

        // And a real query still ranks matches rather than sampling kinds.
        let one = store
            .search_cloud_entities("usr_10uXl8CPozSuQiKolKUf003", None, 50)
            .unwrap();
        assert_eq!(one.len(), 1);
    }

    /// Pasting an id must land on that id, not on whatever else happens to mention it.
    #[test]
    fn an_exact_id_sorts_above_the_things_that_reference_it() {
        let store = Store::open_in_memory().unwrap();
        let acc = "acc_10vk05bJNjt1nLXvId4zh14";
        // Three environments naming the account, plus the account itself.
        for ext in [
            "env_201kpvffchrm1sq5p0qph42dqev",
            "env_201kpvff44fbhw06x8wza0cptfw",
        ] {
            let mut e = cloud_entity(ext, &[acc], chrono::Duration::zero());
            e.name = Some(acc.into()); // worst case: the name matches too
            store.put_cloud_entity(&e).unwrap();
        }
        store
            .put_cloud_entity(&cloud_entity(acc, &[], chrono::Duration::zero()))
            .unwrap();

        let first = store.search_cloud_entities(acc, None, 50).unwrap();
        assert_eq!(first[0].id, acc, "the account itself is the first hit");
    }

    /// The reverse edge is what makes the graph walkable: `related` goes up from a mention,
    /// this comes back down. The total is reported separately because the list is capped.
    #[test]
    fn children_are_the_things_that_name_this_one_as_a_parent() {
        let store = Store::open_in_memory().unwrap();
        let acc = "acc_10vk05bJNjt1nLXvId4zh14";
        let rgn = "rgn_26p9031nve";
        for ext in [
            "env_201kpvffchrm1sq5p0qph42dqev",
            "env_201kpvff44fbhw06x8wza0cptfw",
            "env_201kpe1278xqa4ne187w8fbp640",
        ] {
            store
                .put_cloud_entity(&cloud_entity(ext, &[acc, rgn], chrono::Duration::zero()))
                .unwrap();
        }
        store
            .put_cloud_entity(&cloud_entity(acc, &[], chrono::Duration::zero()))
            .unwrap();

        let (children, total) = store.cloud_entity_children(acc, 200).unwrap();
        assert_eq!(total, 3);
        assert_eq!(children.len(), 3);
        assert!(children.iter().all(|c| c.kind == "environment"));

        // The cap is honest: the list shortens, the count does not.
        let (capped, total) = store.cloud_entity_children(acc, 2).unwrap();
        assert_eq!((capped.len(), total), (2, 3));

        // An entity nobody points at has none — not "everything".
        assert_eq!(
            store
                .cloud_entity_children("org_10vj37k7NcqTaIavuJRsOcv", 200)
                .unwrap()
                .1,
            0
        );
    }

    /// Quoting makes the reverse edge an exact token match. Without it, an id that is a
    /// prefix of another would drag in the other one's children.
    #[test]
    fn a_child_lookup_does_not_match_an_id_that_merely_starts_the_same() {
        let store = Store::open_in_memory().unwrap();
        store
            .put_cloud_entity(&cloud_entity(
                "env_201kpvffchrm1sq5p0qph42dqev",
                &["acc_10vk05bJNjt1nLXvId4zh14extra"],
                chrono::Duration::zero(),
            ))
            .unwrap();
        assert_eq!(
            store
                .cloud_entity_children("acc_10vk05bJNjt1nLXvId4zh14", 200)
                .unwrap()
                .1,
            0
        );
    }

    /// The search and the reverse edge against the operator's real, warmed estate — ~2000
    /// entities of genuine shape rather than four rows this test wrote itself.
    ///
    ///   cargo test store::tests::the_real_estate_is_searchable -- --ignored --nocapture
    #[test]
    #[ignore]
    fn the_real_estate_is_searchable() {
        let db = std::env::var("MUGGLEBOT_DB").unwrap_or_else(|_| "data/mugglebot.sqlite".into());
        let store = Store::open(std::path::Path::new(&db)).expect("open the store");
        for (kind, n) in store.cloud_entity_kinds().unwrap() {
            println!("{n:>5} {kind}");
        }
        let envs = store
            .search_cloud_entities("", Some("environment"), 5)
            .unwrap();
        assert!(
            !envs.is_empty(),
            "warm the estate first: mugglebot cloud-warm"
        );
        let sample = &envs[0];
        println!(
            "\nsample: {} {:?} {:?}",
            sample.id, sample.name, sample.status
        );
        // Walk up, then back down from whatever it belongs to.
        for parent in &sample.related {
            let p = store.get_cloud_entity(parent).unwrap();
            let (kids, total) = store.cloud_entity_children(parent, 3).unwrap();
            println!(
                "  ↑ {parent} {:?} — contains {total}, e.g. {:?}",
                p.and_then(|p| p.name),
                kids.iter().map(|k| k.id.clone()).collect::<Vec<_>>()
            );
            assert!(total > 0, "{parent} is named by {}", sample.id);
        }
    }

    /// A board reset drops signals, so the mentions are orphans and go. The entity cache is
    /// keyed by a platform id rather than by a signal, and re-reading an account from `rcc`
    /// to learn what it already knows is a subprocess for no new information.
    #[test]
    fn resetting_the_board_clears_mentions_but_keeps_the_entity_cache() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = sample("s1");
        sig.subject = Some("o/r#1".into());
        store.insert_signal(&sig).unwrap();
        store
            .link_signal_cloud_ids(&sig.id, &["acc_10vk05bJNjt1nLXvId4zh14".into()])
            .unwrap();
        store
            .put_cloud_entity(&cloud_entity(
                "acc_10vk05bJNjt1nLXvId4zh14",
                &[],
                chrono::Duration::zero(),
            ))
            .unwrap();

        store.clear_board_events().unwrap();

        assert!(store
            .cloud_entities_for_subject("o/r#1")
            .unwrap()
            .is_empty());
        assert!(store
            .get_cloud_entity("acc_10vk05bJNjt1nLXvId4zh14")
            .unwrap()
            .is_some());
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
    fn branch_pass(reviewer: crate::ghreview::Reviewer, model: &str) -> crate::ghreview::Pass {
        crate::ghreview::Pass {
            id: format!("bp/{}", new_id()),
            branch_key: "o/r@bh/otel1".into(),
            repo: "o/r".into(),
            head: "bh/otel1".into(),
            base: "main".into(),
            head_sha: "abc1234".into(),
            by: reviewer,
            provider: "claude".into(),
            model: model.into(),
            status: crate::ghreview::PassStatus::Pending,
            recommendation: None,
            summary: None,
            comments: vec![],
            dismissed: false,
            error: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// Asking the same reviewer on the same model about the same commit is the same question,
    /// and must hand back the existing row rather than queueing a second pass.
    #[test]
    fn a_branch_pass_is_unique_per_reviewer_model_and_commit() {
        let store = Store::open_in_memory().unwrap();
        use crate::ghreview::Reviewer;

        let first = store
            .queue_branch_pass(&branch_pass(Reviewer::Model, "claude-opus-5"))
            .unwrap();
        let again = store
            .queue_branch_pass(&branch_pass(Reviewer::Model, "claude-opus-5"))
            .unwrap();
        assert_eq!(first.id, again.id, "the same question, once");

        // A different model is a different question…
        let other_model = store
            .queue_branch_pass(&branch_pass(Reviewer::Model, "claude-sonnet-5"))
            .unwrap();
        assert_ne!(first.id, other_model.id);
        // …and so is a different reviewer.
        let persona = store
            .queue_branch_pass(&branch_pass(
                Reviewer::Persona("pavel".into()),
                "claude-opus-5",
            ))
            .unwrap();
        assert_ne!(first.id, persona.id);
        // …and so is a different commit.
        let mut moved = branch_pass(Reviewer::Model, "claude-opus-5");
        moved.head_sha = "def5678".into();
        let after_push = store.queue_branch_pass(&moved).unwrap();
        assert_ne!(first.id, after_push.id);

        assert_eq!(store.branch_passes("o/r@bh/otel1").unwrap().len(), 4);
    }

    /// The full lifecycle, including the part that matters: dismissal hides a pass from the
    /// diff and keeps the row, so it can be restored and so the record survives.
    #[test]
    fn a_branch_pass_finishes_dismisses_restores_and_reruns() {
        let store = Store::open_in_memory().unwrap();
        use crate::ghreview::{PassStatus, Placed, Reviewer};

        let queued = store
            .queue_branch_pass(&branch_pass(
                Reviewer::Persona("pavel".into()),
                "claude-opus-5",
            ))
            .unwrap();
        assert_eq!(queued.status, PassStatus::Pending);
        assert_eq!(
            store.list_branch_passes_pending().unwrap(),
            vec![queued.id.clone()]
        );

        let comments = vec![Placed {
            path: "src/main.rs".into(),
            severity: crate::prdiff::Severity::Concern,
            note: "He'd ask why this can't fail loudly.".into(),
            anchor: Some("+    unwrap_or_default();".into()),
            patch_index: Some(4),
            by: Reviewer::Persona("pavel".into()),
            because: vec!["pavel/style/1".into()],
        }];
        let done = store
            .finish_branch_pass(&queued.id, Some("request_changes"), Some("note"), &comments)
            .unwrap();
        assert_eq!(done.status, PassStatus::Done);
        assert_eq!(done.comments.len(), 1);
        assert_eq!(done.comments[0].patch_index, Some(4));
        assert_eq!(done.comments[0].by, Reviewer::Persona("pavel".into()));
        assert_eq!(done.recommendation.as_deref(), Some("request_changes"));
        assert!(store.list_branch_passes_pending().unwrap().is_empty());

        // Dismiss: gone from the diff, still on the record, comments intact.
        store.set_branch_pass_dismissed(&queued.id, true).unwrap();
        let hidden = store.get_branch_pass(&queued.id).unwrap().unwrap();
        assert!(hidden.dismissed);
        assert_eq!(hidden.comments.len(), 1, "dismissal is not a delete");
        assert!(crate::ghreview::visible(std::slice::from_ref(&hidden)).is_empty());

        // Restore.
        store.set_branch_pass_dismissed(&queued.id, false).unwrap();
        assert!(
            !store
                .get_branch_pass(&queued.id)
                .unwrap()
                .unwrap()
                .dismissed
        );

        // Re-ask: back to pending, and the old answer cleared so a half-finished rerun cannot
        // read as the new one.
        store.reopen_branch_pass(&queued.id).unwrap();
        let reopened = store.get_branch_pass(&queued.id).unwrap().unwrap();
        assert_eq!(reopened.status, PassStatus::Pending);
        assert!(reopened.comments.is_empty());
        assert_eq!(reopened.recommendation, None);
        assert!(!reopened.dismissed);
    }

    #[test]
    fn a_failed_branch_pass_carries_its_reason() {
        let store = Store::open_in_memory().unwrap();
        let queued = store
            .queue_branch_pass(&branch_pass(crate::ghreview::Reviewer::Model, "m"))
            .unwrap();
        store
            .fail_branch_pass(
                &queued.id,
                "bh/otel1 moved to def5678 while this was queued",
            )
            .unwrap();
        let failed = store.get_branch_pass(&queued.id).unwrap().unwrap();
        assert_eq!(failed.status, crate::ghreview::PassStatus::Failed);
        assert!(failed.error.unwrap().contains("moved to"));
    }

    /// A comments blob that will not parse must not take the row down: the pass's verdict is
    /// still worth showing, and one bad blob must not empty the view for other reviewers.
    #[test]
    fn an_unparseable_comments_blob_degrades_to_no_comments() {
        let store = Store::open_in_memory().unwrap();
        let queued = store
            .queue_branch_pass(&branch_pass(crate::ghreview::Reviewer::Model, "m"))
            .unwrap();
        {
            let conn = store.lock();
            conn.execute(
                "UPDATE branch_review_passes SET status='done', summary='s', comments='{{{' \
                 WHERE id=?1",
                params![queued.id],
            )
            .unwrap();
        }
        let read = store.get_branch_pass(&queued.id).unwrap().unwrap();
        assert!(read.comments.is_empty());
        assert_eq!(read.summary.as_deref(), Some("s"));
    }

    /// The bulk half of a source snooze, and the query where it could most easily do the
    /// wrong thing quietly.
    ///
    /// Four subjects, one of each shape that matters:
    ///   - purely Slack → snoozed
    ///   - Slack *and* GitHub → left alone (a PR with chatter on it is not a Slack
    ///     notification)
    ///   - purely GitHub → left alone
    ///   - purely Slack but carrying a Critical → left alone, because a source snooze is not
    ///     what should hide an outage
    #[test]
    fn a_source_snooze_takes_only_the_subjects_that_are_purely_that_source() {
        source_snooze_case();
    }

    /// The board's bulk snooze, and the sweep that gives its duration meaning.
    ///
    /// `snoozed_until` was written by both snooze paths from the beginning and read by
    /// neither — `Projection::views` drops anything whose `handled` is `Snoozed` and looks no
    /// further — so every "snooze for two hours" was in practice indefinite. These two halves
    /// are what make the duration a term rather than a decoration.
    #[test]
    fn a_bulk_snooze_expires_and_an_open_ended_one_does_not() {
        let store = Store::open_in_memory().unwrap();
        let mk = |n: u64| {
            let key = SubjectKey::issue("o/r", n);
            let sig = sample(&format!("s{n}"));
            store.insert_signal(&sig).unwrap();
            store
                .upsert_subject(&Subject::new(key.clone(), &sig, Utc::now()))
                .unwrap();
            key.as_str().to_string()
        };
        let (lapsed, live, forever) = (mk(1), mk(2), mk(3));
        let handled = |k: &str| store.get_subject(k).unwrap().unwrap().handled;

        // Two snoozed together with an expiry, and one snoozed open-endedly — the `s` key's
        // "off the board until it moves", which is a different thing to have asked for.
        let changed = store
            .set_handled_bulk(
                &[lapsed.clone(), live.clone()],
                Handled::Snoozed,
                Some(Utc::now() - chrono::Duration::minutes(1)),
            )
            .unwrap();
        assert_eq!(changed, 2);
        store.set_handled(&forever, Handled::Snoozed, None).unwrap();
        // Push one of the pair back into the future, so the sweep has to discriminate rather
        // than simply waking everything it finds.
        store
            .set_handled(
                &live,
                Handled::Snoozed,
                Some(Utc::now() + chrono::Duration::hours(2)),
            )
            .unwrap();

        let woken = store.wake_lapsed_snoozes().unwrap();
        assert_eq!(woken, vec![lapsed.clone()]);
        // Back to `open`, not `seen`: it left the board unread, and returning it as already
        // seen would hide it in the same breath as un-hiding it.
        assert_eq!(handled(&lapsed), Handled::Open);
        assert!(store
            .get_subject(&lapsed)
            .unwrap()
            .unwrap()
            .snoozed_until
            .is_none());
        assert_eq!(handled(&live), Handled::Snoozed, "not due yet");
        assert_eq!(handled(&forever), Handled::Snoozed, "no expiry to lapse");

        // Idempotent, so a tick that fires twice — or fires again after a restart — costs
        // nothing and wakes nobody a second time.
        assert!(store.wake_lapsed_snoozes().unwrap().is_empty());

        // A key that has since gone is not an error; the count is what actually changed.
        let changed = store
            .set_handled_bulk(
                &[live.clone(), "o/r#404".into()],
                Handled::Snoozed,
                Some(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();
        assert_eq!(changed, 1);
        assert_eq!(
            store.set_handled_bulk(&[], Handled::Snoozed, None).unwrap(),
            0
        );
    }

    /// The body of the test above, kept out of line only so the bulk-snooze test could sit
    /// beside it without displacing its documentation.
    fn source_snooze_case() {
        let store = Store::open_in_memory().unwrap();

        let mk = |key: SubjectKey, sigs: Vec<(Source, Severity, &str)>| {
            let mut first: Option<Signal> = None;
            for (i, (source, severity, ext)) in sigs.into_iter().enumerate() {
                let mut sig = sample(&format!("{}-{i}", ext));
                sig.source = source;
                sig.id = Signal::make_id(source, &format!("{}-{i}", ext), None);
                sig.severity = severity;
                sig.subject = Some(key.as_str().to_string());
                store.insert_signal(&sig).unwrap();
                if first.is_none() {
                    first = Some(sig);
                }
            }
            store
                .upsert_subject(&Subject::new(key, &first.unwrap(), Utc::now()))
                .unwrap();
        };

        let slack_only = SubjectKey::issue("o/r", 1);
        let mixed = SubjectKey::issue("o/r", 2);
        let github_only = SubjectKey::issue("o/r", 3);
        let slack_critical = SubjectKey::issue("o/r", 4);
        mk(
            slack_only.clone(),
            vec![(Source::Slack, Severity::Warning, "a")],
        );
        mk(
            mixed.clone(),
            vec![
                (Source::Slack, Severity::Notice, "b"),
                (Source::GitHub, Severity::Notice, "c"),
            ],
        );
        mk(
            github_only.clone(),
            vec![(Source::GitHub, Severity::Warning, "d")],
        );
        mk(
            slack_critical.clone(),
            vec![
                (Source::Slack, Severity::Notice, "e"),
                (Source::Slack, Severity::Critical, "f"),
            ],
        );

        let until = Utc::now() + chrono::Duration::hours(2);
        let n = store
            .snooze_subjects_only_from(Source::Slack, until, Some(Severity::Critical))
            .unwrap();
        assert_eq!(n, 1, "only the purely-Slack, non-critical subject");

        let handled = |k: &SubjectKey| store.get_subject(k.as_str()).unwrap().unwrap().handled;
        assert_eq!(handled(&slack_only), Handled::Snoozed);
        assert_eq!(
            handled(&mixed),
            Handled::Open,
            "a PR with chatter is not Slack"
        );
        assert_eq!(handled(&github_only), Handled::Open);
        assert_eq!(
            handled(&slack_critical),
            Handled::Open,
            "a snooze must not be what hides something critical"
        );

        // And it comes back on its own, at the snooze's own expiry.
        let snoozed = store.get_subject(slack_only.as_str()).unwrap().unwrap();
        assert_eq!(
            snoozed.snoozed_until.map(|d| d.timestamp()),
            Some(until.timestamp())
        );
    }

    /// Total silence — the literal "snooze all slack notifications" — takes the critical
    /// subject too. The same board, the same query, one argument different.
    #[test]
    fn total_silence_takes_the_critical_subject_as_well() {
        let store = Store::open_in_memory().unwrap();
        let key = SubjectKey::issue("o/r", 5);
        for (i, sev) in [Severity::Notice, Severity::Critical]
            .into_iter()
            .enumerate()
        {
            let mut sig = sample(&format!("crit-{i}"));
            sig.source = Source::Slack;
            sig.id = Signal::make_id(Source::Slack, &format!("crit-{i}"), None);
            sig.severity = sev;
            sig.subject = Some(key.as_str().to_string());
            store.insert_signal(&sig).unwrap();
            if i == 0 {
                store
                    .upsert_subject(&Subject::new(key.clone(), &sig, Utc::now()))
                    .unwrap();
            }
        }
        let until = Utc::now() + chrono::Duration::hours(1);

        // With the safe floor it is left alone…
        assert_eq!(
            store
                .snooze_subjects_only_from(Source::Slack, until, Some(Severity::Critical))
                .unwrap(),
            0
        );
        // …and with no floor it goes.
        assert_eq!(
            store
                .snooze_subjects_only_from(Source::Slack, until, None)
                .unwrap(),
            1
        );
        assert_eq!(
            store.get_subject(key.as_str()).unwrap().unwrap().handled,
            Handled::Snoozed
        );
    }

    /// A floor stored as `everything` reads back as no floor. Stored as a word rather than
    /// SQL NULL so a row is never ambiguous about whether a floor was set or lost.
    #[test]
    fn a_total_silence_floor_round_trips_through_the_store() {
        let store = Store::open_in_memory().unwrap();
        let s = crate::quiet::Snooze {
            source: Source::Slack,
            until: Utc::now() + chrono::Duration::hours(1),
            floor: None,
            set_at: Utc::now(),
        };
        store.set_source_snooze(&s).unwrap();
        let back = store.list_source_snoozes().unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].floor, None);
        assert!(back[0].total());
        // And an unrecognised floor fails safe: outages get through, not swallowed.
        assert_eq!(parse_floor("wat"), Some(Severity::Notice));
        assert_eq!(parse_floor("critical"), Some(Severity::Critical));
        assert_eq!(parse_floor("everything"), None);
    }

    /// Already-handled subjects are not re-snoozed: an acknowledged thread has been dealt
    /// with, and rewriting its state would move it from "I've seen this" to "hidden".
    #[test]
    fn a_source_snooze_leaves_already_handled_subjects_alone() {
        let store = Store::open_in_memory().unwrap();
        let key = SubjectKey::issue("o/r", 9);
        let mut sig = sample("ack");
        sig.source = Source::Slack;
        sig.id = Signal::make_id(Source::Slack, "ack", None);
        sig.subject = Some(key.as_str().to_string());
        store.insert_signal(&sig).unwrap();
        store
            .upsert_subject(&Subject::new(key.clone(), &sig, Utc::now()))
            .unwrap();
        store
            .set_handled(key.as_str(), Handled::Acknowledged, None)
            .unwrap();

        let n = store
            .snooze_subjects_only_from(
                Source::Slack,
                Utc::now() + chrono::Duration::hours(1),
                Some(Severity::Critical),
            )
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            store.get_subject(key.as_str()).unwrap().unwrap().handled,
            Handled::Acknowledged
        );
    }

    /// The encoding trap. `signals.severity` is stored JSON-quoted (`"warning"`, nine
    /// characters), so a comparison against a bare `warning` matches nothing and would make
    /// the critical guard above a silent no-op.
    #[test]
    fn the_severity_comparison_uses_the_form_the_column_actually_holds() {
        assert_eq!(
            at_or_above(Severity::Warning),
            vec!["\"warning\"".to_string(), "\"critical\"".to_string()]
        );
        assert_eq!(
            at_or_above(Severity::Critical),
            vec!["\"critical\"".to_string()]
        );
        assert_eq!(at_or_above(Severity::Info).len(), 4);
    }

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
        let dir =
            std::env::temp_dir().join(format!("mugglebot-add-column-test-{}", std::process::id()));
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

    /// The two tiers share one row, and the pending list is what routes it: the scheduler
    /// submits `GrafanaRead` or `BrowserRead` off this method.
    #[test]
    fn the_pending_queue_reports_which_tier_each_read_is_on() {
        let store = Store::open_in_memory().unwrap();
        for (ext, method) in [("a", "grafana"), ("b", "browser")] {
            let mut sig = sample(ext);
            sig.subject = Some("o/r#1".into());
            store.insert_signal(&sig).unwrap();
            store
                .queue_browser_investigation(&sig.id, "https://g/d/1", "{}", method)
                .unwrap();
        }
        let pending = store.list_browser_investigations_pending().unwrap();
        let methods: Vec<&str> = pending.iter().map(|(_, m)| m.as_str()).collect();
        assert!(
            methods.contains(&"grafana") && methods.contains(&"browser"),
            "{methods:?}"
        );
    }

    /// Escalation is a transition on the one row, not a second row — which is what keeps
    /// "one question per alert" true while still letting the browser answer it. The reason
    /// Grafana came up short survives on the record.
    #[test]
    fn escalating_to_the_browser_keeps_one_row_and_the_reason() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = sample("grafana-escalate");
        sig.subject = Some("o/r#9".into());
        store.insert_signal(&sig).unwrap();
        let queued = store
            .queue_browser_investigation(&sig.id, "https://g/d/1", "{}", "grafana")
            .unwrap();

        let handed = store
            .escalate_investigation_to_browser(
                &queued.id,
                "https://g/d/abc/tenant-overview",
                "Investigate one dashboard link…",
                "the queries returned no points for this window",
            )
            .unwrap();
        assert_eq!(
            handed.id, queued.id,
            "the same investigation, on another tier"
        );
        assert_eq!(handed.method, "browser");
        assert_eq!(handed.status, "pending");
        assert!(handed.url.contains("/d/abc/"));
        assert!(handed.error.as_deref().unwrap().contains("no points"));
        assert_eq!(
            store
                .browser_investigations_for_subject("o/r#9")
                .unwrap()
                .len(),
            1
        );
    }

    /// The conclusion and the series it came from are stored together, because a figure
    /// nobody can go back and check is the thing this tier exists to avoid.
    #[test]
    fn a_grafana_read_stores_its_evidence_beside_its_conclusion() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = sample("grafana-evidence");
        sig.subject = Some("o/r#11".into());
        store.insert_signal(&sig).unwrap();
        let queued = store
            .queue_browser_investigation(&sig.id, "https://g/d/1", "{}", "grafana")
            .unwrap();

        let done = store
            .complete_investigation_with_evidence(
                &queued.id,
                "Storage reached 0.94 against a 0.9 threshold.",
                r#"{"series":[{"ref_id":"A","max":0.94}]}"#,
            )
            .unwrap();
        assert_eq!(done.status, "completed");
        assert_eq!(done.method, "grafana");
        assert!(done.findings.as_deref().unwrap().contains("0.94"));
        assert!(done.evidence.as_deref().unwrap().contains("\"max\":0.94"));
    }

    /// Rows written before the Grafana tier existed are browser reads, which is what they
    /// were — the `COALESCE` in the select, exercised.
    #[test]
    fn a_row_with_no_method_reads_as_a_browser_investigation() {
        let store = Store::open_in_memory().unwrap();
        let mut sig = sample("legacy");
        sig.subject = Some("o/r#12".into());
        store.insert_signal(&sig).unwrap();
        let queued = store
            .queue_browser_investigation(&sig.id, "https://g/d/1", "prompt", "browser")
            .unwrap();
        {
            let conn = store.lock();
            conn.execute(
                "UPDATE browser_investigations SET method=NULL WHERE id=?1",
                params![queued.id],
            )
            .unwrap();
        }
        let read = store
            .get_browser_investigation(&queued.id)
            .unwrap()
            .unwrap();
        assert_eq!(read.method, "browser");
        let pending = store.list_browser_investigations_pending().unwrap();
        assert_eq!(pending[0].1, "browser");
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
                "browser",
            )
            .unwrap();
        assert_eq!(queued.status, "pending");
        // Queueing the same signal is idempotent.
        let duplicate = store
            .queue_browser_investigation(&signal.id, "https://ignored", "ignored", "browser")
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
        let dir = std::env::temp_dir().join(format!(
            "mugglebot-schema-compat-test-{}",
            std::process::id()
        ));
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
        let dir = std::env::temp_dir().join(format!(
            "mugglebot-schema-reopen-test-{}",
            std::process::id()
        ));
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
