# MuggleBot

> An ambient, single-pane-of-glass ops-awareness agent for one engineer.
> It watches the places where work and incidents show up, correlates them, and
> makes sure you never miss the thing that mattered — without pretending to be
> on-call for you.

MuggleBot is a local-first daemon that watches your notification surfaces
(GitHub, Slack — including designated alert channels — and Granola), normalizes
everything into a common
signal model, correlates related activity across sources, and surfaces it
through native macOS notifications and a Star-Trek **LCARS**-inspired web UI. It
also exposes an **MCP endpoint** so you can pull its correlated context straight
into a Claude or ChatGPT session for deeper investigation.

The name is the joke: it makes the "magic" of ops legible to mere muggles.

---

## Goals

- **One pane of glass.** Every signal that would otherwise be scattered across
  four apps and a dozen tabs shows up in one place, ranked by what deserves
  attention now.
- **Never miss the important thing.** Deduplicated, de-noised, and escalated by
  rules you control — a review request can whisper while a paging incident
  shouts.
- **Help you understand, fast.** When something is on fire, MuggleBot has already
  gathered the related signals, built a timeline, and can suggest safe,
  reversible first moves.

## Non-goals (at least for v1)

- **Not an autopilot.** MuggleBot informs and proposes. It never mutates a
  production system on its own. (See _Design principles → Copilot, not
  autopilot_.)
- **Not a paging system.** It's a lens over your existing tools, not a system of
  record or a paging authority — it won't page you or hold incident state.
- **Not multi-tenant.** It runs as a single-user, local-first tool on your Mac.
  No shared server, no central store of your signals.

---

## How it works (at a glance)

```
   ┌──────────────┐  ┌────────┐  ┌─────────┐
   │  GitHub API  │  │ Slack  │  │ Granola │   sources (Slack incl. alert channels)
   └──────┬───────┘  └───┬────┘  └────┬────┘
          │  watchers (one per source, normalize → Signal)
          └──────┬───────┴────────────┬────────┘
                 ▼                     ▼
          ┌───────────────────────────────┐
          │  Ingest / normalization        │  → common Signal type
          └───────────────┬───────────────┘
                          ▼
          ┌───────────────────────────────┐    ┌────────────────────┐
          │  Event store (SQLite)          │◄──►│ Correlation engine │
          └───────────────┬───────────────┘    │ (rules + LLM)      │
                          │                     └─────────┬──────────┘
        ┌─────────────────┼───────────────────┐          │ "local connection"
        ▼                 ▼                   ▼          ▼
  ┌───────────┐   ┌──────────────┐   ┌──────────────┐  ┌──────────────┐
  │ macOS      │   │ Web UI       │   │ MCP endpoint │  │ Claude /     │
  │ notifs     │   │ (LCARS, TS)  │   │ (stdio+HTTP) │  │ ChatGPT      │
  └───────────┘   └──────────────┘   └──────┬───────┘  └──────▲───────┘
                                            └─────────────────┘
```

- **Rust backend** (async/`tokio`): watchers, ingest, event store, correlation,
  the MCP server, the notifier, and the HTTP/WebSocket server for the UI.
- **TypeScript frontend**: the LCARS single-pane UI, fed live over a WebSocket.
- **macOS notifications**: native, actionable, rule-driven.
- **Configuration**: a single TOML file holds credentials and behavior.

---

## Signal sources (watchers)

Each source has a dedicated watcher that authenticates, subscribes or polls,
and emits normalized `Signal`s. Watchers are independent and fault-isolated: one
source being down or rate-limited must not stall the others.

| Source | What we watch | Transport |
|---|---|---|
| **GitHub** | Notifications feed: review requests, mentions, assigned issues/PRs, CI/check failures, thread replies | REST notifications API + conditional polling; GraphQL for enrichment |
| **Slack** | DMs, @-mentions, keyword hits, watched channels, and **alert channels** — designated channels whose posts are treated as alerts (higher base severity) | Socket Mode / Events API |
| **Granola** | Meeting notes & transcripts → extracted action items, decisions, owners | Granola API (poll) |

Watcher contract:

- Normalize into the common `Signal` (below) — no source-specific types leak
  past ingest.
- Be idempotent: re-ingesting the same upstream event must not create
  duplicates (dedup on `(source, external_id)`).
- Track a durable cursor/ETag so a restart resumes without gaps or replays.
- Degrade gracefully and report health (see MCP `source_health`).

### The normalized `Signal`

The whole system speaks one type. Sketch:

```rust
struct Signal {
    id: SignalId,                 // internal, stable
    source: Source,               // GitHub | Slack | Granola
    external_id: String,          // upstream id, for dedup
    kind: SignalKind,             // ReviewRequested | Mention | Alert | ...
    title: String,
    body: Option<String>,
    url: Option<Url>,             // deep-link back to the source
    actor: Option<Actor>,         // who caused it
    entities: Vec<Entity>,        // repo, PR#, channel, service, people
    severity: Severity,           // Info | Notice | Warning | Critical
    state: State,                 // Unseen | Seen | Acknowledged | Resolved | Snoozed
    occurred_at: DateTime<Utc>,
    ingested_at: DateTime<Utc>,
    thread: Option<ThreadId>,     // correlation grouping
    raw: serde_json::Value,       // original payload, for audit/enrichment
    tags: Vec<String>,            // routing tags (Slack messages classified per-signal)
}
```

`entities` are the correlation currency — a PR number, a service name, a
channel, a person. Signals that share entities within a time window are
candidates for the same `thread`. Threads in turn carry the relation graph —
`same` / `related` / `distinct` edges to other threads, each tagged with
provenance (`llm` verdict or a user `pin`) — plus any ad-hoc context you've
attached (see _Correlation & intelligence_).

---

## Correlation & intelligence

Correlation happens in two tiers so that the cheap, deterministic work never
waits on a model, and the model is only asked to reason when it adds value.

1. **Deterministic grouping (always on, in-process).** Group signals into
   threads by shared entities and time proximity: the alert in `#alerts` firing
   on `service-foo`, the Slack thread discussing it, and the GitHub PR that
   touches `service-foo` collapse into one topic. Fast, explainable,
   no LLM required.

2. **Semantic reasoning (on demand / via LLM).** For "what is actually going on
   here and what should I look at first," MuggleBot delegates to an LLM through a
   single internal `Reasoner` trait, backed by any of three provider kinds. This
   is where the original "uses the local connections on Claude and ChatGPT" idea
   lives — plus a local Ollama.

   | Provider kind | How MuggleBot reaches it | Inference runs | Auth / cost |
   |---|---|---|---|
   | **On-device** (Ollama) | HTTP at `localhost:11434` (`/api/chat` or OpenAI-compatible `/v1/chat/completions`) | **On the Mac** | none; offline-capable |
   | **Subscription CLI bridge** (Claude / ChatGPT) | shell out to `claude -p` (Claude Code headless) or `codex exec` (Codex CLI) | cloud | rides your existing Max/Pro / ChatGPT login — no API key, no metering |
   | **Direct cloud API** (Anthropic / OpenAI) | raw HTTP (reqwest); no official Anthropic Rust SDK | cloud | API key, metered |

   All three speak an OpenAI-compatible `/v1/chat/completions` shape (Ollama and
   OpenAI natively, Anthropic via its compat endpoint), so one HTTP client shape
   covers them by swapping base URL + model + auth; the CLI bridges are a
   separate `Reasoner` impl that spawns a subprocess.

   Two things the "local" label hides:
   - **Only Ollama is truly on-device.** The Claude/ChatGPT desktop apps are MCP
     _clients_, not callable inference servers — the genuine subscription-riding
     "local connection" is the CLI bridge (a local process using your existing
     login), but your signal data still leaves the machine to be reasoned over.
   - **Route by tier.** Ambient, high-frequency reasoning (the one-line "why did
     this thread light up?" on a notification) → **Claude Sonnet**
     (`claude-sonnet-5`), the default. Heavy / on-demand reasoning → Claude Opus
     (`claude-opus-4-8`). Sources pinned in `local_only_sources` → on-device
     Ollama, which never leaves the machine. Interactive deep-dive → the
     MCP-server path below, where your Claude/ChatGPT client connects to MuggleBot.

### Relatedness, de-duplication & the relation graph

Correlation doesn't just cluster — it classifies the link between any two
threads:

- **same** — duplicates of one underlying issue; collapsed into a single thread
  with a canonical entry, the rest marked as duplicates.
- **related** — distinct but connected (a deploy PR and the incident it caused);
  linked and cross-referenced, but kept separate.
- **distinct** — explicitly unrelated; a negative edge that stops future
  regrouping.

Deterministic grouping proposes candidate pairs (shared entities, tight time
window); the **LLM judges each candidate** — same / related / distinct —
returning a verdict, a confidence, a one-line rationale, and the signals it
weighed. The result is a **relation graph** over threads, persisted in SQLite
as an edges table.
Whether a high-confidence `same` verdict auto-merges or is merely _proposed_ for
your confirmation is a config switch (`auto_merge`).

### Human overrides (pins) and re-analysis

You are the authority. From any thread you can **associate** (mark related),
**merge** (mark same / duplicate), or **split** (dissociate signals the model
wrongly grouped). Each override is stored as a **pinned edge** — provenance
`user`, not `llm` — and pins always win.

Changing a pin **re-runs the LLM analysis** for the affected threads, with the
pins supplied as hard constraints ("the user says A and B are the same and C is
unrelated — reconcile everything else around that"). The model completes the
graph without contradicting your pins, so a correction propagates rather than
being silently re-overwritten on the next pass.

### Per-thread context

Beyond the global context library, you can attach **ad-hoc context to a single
thread** — free text ("third time this quarter") or a URL. Text is used as-is; a
URL runs through the same fetch → summarize → embed pipeline as the context
library. Attaching or editing thread context is another trigger that re-runs the
analysis, and the attached context is fed into that thread's reasoning prompt and
cited like any other evidence.

Every correlation and every suggestion **cites the signals it's built from**.
A thread summary that can't point at its evidence is a bug, not a feature.

---

## Memory & context library

Two curated, SQLite-backed stores give the reasoner background beyond the live
signal stream. Both are summarized on ingest, embedded for semantic recall, and
exposed over MCP — so an interactive Claude/ChatGPT session reasons over the
same grounding MuggleBot uses ambiently.

**Memory** — what MuggleBot has learned or been told: lessons from past
incidents, corrections, confirmed approaches ("a spike in X usually means Y").
Written by MuggleBot (postmortem-assist) and by you, and **fully editable** —
browse / add / edit / delete through the WebUI memory editor and the MCP memory
tools. One entry = one fact with a one-line summary; entries link back to the
signals or threads they came from.

**Context library** — external reference material you curate so MuggleBot starts
with the background a new teammate would read: runbooks, architecture docs, the
on-call/observability guide, service catalogs, status pages. Two source kinds:

- **URL** — fetched (reqwest), summarized, stored, and **refreshed on a
  schedule**. Refresh honors `ETag` / `Last-Modified` to skip unchanged pages;
  on a real change it re-summarizes, re-embeds, and (optionally) emits a
  low-severity "context changed" signal so you notice when a runbook or status
  page moves under you. **Authenticated URLs** (internal runbooks, dashboards)
  pull a credential from the Keychain and send it as a header at fetch time —
  the same credential store and config page as source tokens.
- **File** — a local path (Markdown, text, PDF); same pipeline, re-ingested when
  its mtime changes.
- **Managed directory** — files under `<data_dir>/contexts/<tag>/…` are ingested
  automatically: each immediate sub-directory names an automatic (pinned) tag, so
  dropping a runbook into `contexts/database/` files it under `database` with no
  LLM pass. A scheduler polls the tree, reloads files on change (mtime), and drops
  entries whose backing file disappears — effectively watching the directory.

**Shared ingest pipeline:** fetch/read → normalize to text → summarize via the
reasoner (Claude Sonnet by default; on-device Ollama for `local_only_sources`)
→ embed → store
in SQLite as `{raw, summary, tags, source, fetched_at, etag|mtime, refresh_interval}`
with the embedding indexed by `sqlite-vec`. A background scheduler drives URL
refresh; files are watched by mtime.

### Tags — categorical routing

Both stores (and threads and signals) carry **tags** drawn from one shared
**vocabulary** — a `{name, summary}` registry where the summary is the
description the classifier reads. Tags are the categorical complement to vector
similarity:

- **Assigned** on ingest by a two-tier auto-tagger (a cheap pass proposes tags,
  a heavy pass refines), or pinned by hand (folder tags, `tag_context` /
  `tag_memory` / `set_thread_tags`). Human-pinned tags are never overwritten.
- **Summaries** for automatically-created tags are backfilled once by an LLM
  pass over the content filed under them; thereafter they're edited by hand via
  `edit_tag`. Vocabulary hygiene — `merge_tags` (also renames) and `delete_tag`
  (strips the label from all content) — keeps near-duplicates in check.
- **Classification.** When an issue lands, the thread is classified into the
  vocabulary (LLM with a deterministic substring fallback); every **Slack**
  message is additionally classified per-signal at ingest. Classification is
  skipped while the vocabulary is empty.

**How grounding is used:** when a thread lights up, MuggleBot folds in the most
relevant memory + context entries — **tag-matched entries first** (the
categorical routing), then a vector-similarity fill for the rest of the budget —
with citations back to the source URL/file. Tag-matched entries contribute a
bounded excerpt of the actual body (a runbook's steps, a memory's full fact), not
just the summary, so precision doesn't cost fidelity. The same retrieval backs
the MCP `search_memory` / `search_context` tools, so the grounding is identical
whether reasoning happens ambiently or in your interactive session.

---

## Live assist

Threads you're actively in get closer attention. This needs MuggleBot to know
your own Slack identity (`user_id`) so it can tell your messages apart from
everyone else's.

**Trigger & debounce.** Any interaction in a watched or alert thread marks it
_live_ and schedules re-analysis. Dispatch to the LLM is **debounced — 1 minute
after the last activity, with a 5-minute hard cap** so a fast-moving thread still
gets analyzed. When the newest activity is one of your own messages, the
correctness/risk check is prioritized within that window.

**What a pass produces**, grounded in memory + the context library (runbooks,
docs) and cited:

- **hints** — the runbook that applies, a relevant past incident, a related
  thread you may not have connected.
- **suggestions** — a sensible next step, or a generic mitigation to consider.
- **flags on your own messages** — `factual_error` or `risky_action`, each with a
  rationale, a citation, and a confidence.

**Red-alert.** A high-confidence flag — you've said something the grounding
contradicts, or proposed a risky/irreversible action — flips the LCARS UI into
**red-alert mode** and fires a **Critical macOS notification**. It is strictly
advisory: it warns and cites, it never edits or sends anything. (MuggleBot sees
Slack messages only _after_ they post — it can't intercept the compose box — so
this is "you just said X, but runbook Y says otherwise," in time to correct
yourself.) Dismiss or mark false-positive from the notification or the thread;
false-positives feed back to memory so the same thing isn't re-flagged.

Tuning lives in a `[live]` block: debounce window, red-alert on/off, and the
minimum confidence to escalate.

## Agent chat

An interactive, **multimodal** chat panel in the WebUI where you talk to
MuggleBot directly and **drop screenshots, images, logs, or files** for it to
work from. The agent reasons over everything MuggleBot already holds — the live
board, signals, threads, memory, and the context library — through the same tool
surface as the MCP server, so "what's going on with `service-foo`?" and "here's
a screenshot of this dashboard — does it match the alert in `#alerts`?" both
work.

Chat routes to the heavy reasoner (Claude), which handles vision for dropped
images. It's the built-in counterpart to the MCP-server path — same grounding
and tools, but you don't need an external Claude/ChatGPT client. Anything useful
that surfaces in chat can be saved to memory or attached to a thread as context
in one action.

---

## MCP surface

MuggleBot exposes an MCP server over both stdio (for local clients) and
HTTP/SSE (for networked clients on `localhost`). Tools are typed and carry risk
metadata; read tools are free, any future write/act tools are gated (see design
principles).

**Tools (read):**

- `list_signals(source?, since?, severity?, state?)` — the current board.
- `get_signal(id)` — full detail incl. deep-link and raw payload.
- `list_threads(active_only?)` — correlated topics.
- `get_thread(id)` — signals + deterministic summary + timeline.
- `timeline(thread_id)` — reconstructed, ordered event timeline.
- `search(query)` — semantic/keyword search across ingested signals.
- `list_alerts(state?)` — signals from Slack alert channels, current state.
- `suggest_mitigations(thread_id)` — matches against the generic mitigations
  catalog; suggestions only, never executed.
- `source_health()` — per-watcher status, last cursor, error state.

**Tools (correlation — read/write, writes gated):**

- `relate(thread_a, thread_b, kind)` — pin a `same` / `related` / `distinct`
  edge (associate, mark duplicate, or dissociate); triggers re-analysis.
- `split_thread(thread_id, signal_ids)` — pull wrongly-grouped signals into their
  own thread.
- `attach_thread_context(thread_id, text | url)` — add ad-hoc grounding to one
  thread; triggers re-analysis.
- `reanalyze(thread_id)` — force the LLM correlation pass to re-run.

**Tools (grounding — read/write, writes gated):**

- `search_memory(query)` / `search_context(query)` — semantic retrieval over the
  two grounding stores.
- `list_memories()` / `get_memory(id)` — browse memory.
- `put_memory(text, links?, tags?)` / `edit_memory(id, text)` /
  `tag_memory(id, tags)` / `delete_memory(id)` — memory CRUD; the editable-memory
  surface. `tags` pin routing labels; omitted, they're auto-suggested on write.
- `list_context()` / `get_context(id)` — browse the context library.
- `add_context(url | path, tags?)` / `tag_context(id, tags)` /
  `refresh_context(id)` / `remove_context(id)` — manage context sources;
  `refresh_context` forces an immediate re-fetch.
- `list_tags()` / `edit_tag(name, summary)` / `merge_tags(from, into)` /
  `delete_tag(name)` — the tag vocabulary (see _Tags_). `edit_tag` sets the
  classifier-facing summary; `merge_tags` also renames; `delete_tag` strips the
  label from all content.
- `set_thread_tags(thread_id, tags)` — pin an issue's tags on the board and
  re-run its analysis (mirrors relation pins).

**Tools (live assist — read/write, writes gated):**

- `list_hints(thread_id?)` — current hints, suggestions, and flags.
- `dismiss_hint(id, false_positive?)` — dismiss a hint/flag; `false_positive`
  feeds it back to memory so it isn't re-raised.

**Resources:**

- `board://current` — live board snapshot.
- `config://redacted` — effective config with secrets stripped.
- `memory://` / `context://` — browsable grounding stores.
- `live://hints` — active live-assist hints and flags.

---

## Web UI — LCARS

A single-pane dashboard in the **LCARS** idiom (the swooping Okudagram panels
from Star Trek: TNG). The aesthetic isn't just fun — LCARS is genuinely good at
dense, color-coded, panelized status display, which is exactly the job.

- **Board view.** Threads and loose signals as panels, ranked by attention.
  Color = severity (LCARS palette maps cleanly onto Info→Critical). Snooze,
  acknowledge, deep-link out.
- **Thread view.** Timeline + correlated signals + summary with citations, plus
  the thread's relation graph. Inline controls to **associate**, **merge (mark
  duplicate)**, or **split**, and to **attach context** (text or URL) — any of
  which re-runs the analysis. LLM verdicts show confidence + rationale; user pins
  are visually distinct and authoritative.
- **Config page.** Manage per-source settings and, crucially, **credentials** —
  tokens are written to and read from the macOS Keychain here, never persisted
  in the TOML. Also toggles sources, notification rules, and reasoner routing.
- **Memory editor.** Browse, add, edit, and delete memory entries — the human
  side of institutional memory.
- **Context library.** Add/remove URL and file sources, see each one's last
  refresh and current summary, and force a refresh on demand.
- **Live assist.** In threads you're active in, inline hints and suggestions,
  plus flags on your own messages (factual error / risky action) with citations.
- **Agent chat.** A multimodal chat panel — drop screenshots, images, logs, or
  files and converse with MuggleBot over the live board, memory, and context.
- **Live.** Fed over a WebSocket; new signals animate in, resolved ones fade.
- **Red-alert mode.** A high-confidence live-assist flag shifts the interface to
  LCARS red-alert (color + optional audio cue, off by default) and fires a
  Critical macOS notification; clears on acknowledge/dismiss.
- **Read-mostly (except config).** The board reflects state and lets you triage
  (ack/snooze); it is not a console for mutating production. The config page is
  the one place that writes.

**SolidJS** (TypeScript). Its fine-grained reactivity updates only the panels
whose underlying signals changed — a better fit for a high-frequency live board
than a virtual-DOM diff cycle — with a tiny bundle and no runtime GC churn.

---

## Configuration

A single TOML file (path via `--config` or `$MUGGLEBOT_CONFIG`) holds
non-secret behavior. Credentials are **not** in the file — they live in the
macOS Keychain and are set through the WebUI config page. Sketch:

```toml
[general]
data_dir = "~/.mugglebot"      # SQLite DB (event store + memory + context) lives here
quiet_hours = "22:00-08:00"    # suppress non-Critical notifications

# Tokens are NOT stored here — they live in the macOS Keychain (service
# "dev.mugglebot", account = source name) and are set via the WebUI config page.
# These blocks hold only non-secret behavior.

[sources.github]
enabled = true
watch = ["review_requested", "mention", "ci_failure", "assigned"]

[sources.slack]
enabled = true
user_id = "U0123ABC"                             # your Slack id — flags your own messages
channels = ["#eng"]                              # watched for mentions / keywords
alert_channels = ["#alerts", "#prod-incidents"]  # posts here are alert signals
keywords = ["mugglebot", "prod down"]

[sources.granola]
enabled = true
poll_interval = "2m"

[notifications]
min_severity = "notice"        # below this, board-only, no macOS notif
critical_sound = true

[correlation]
window = "30m"                 # time window for entity-based grouping
dedup_threshold = 0.8          # min LLM confidence for a "same" verdict
auto_merge = false             # false → "same" verdicts are proposed, not applied

[live]
debounce = "1m"                # wait after last thread activity before re-analysis
debounce_max = "5m"            # hard cap so busy threads still get analyzed
red_alert = true               # red-alert + Critical notification on high-confidence flags
red_alert_min_confidence = 0.75

[reasoner]
# Default provider for ambient/unattended reasoning: Claude Sonnet.
# "claude" uses the Claude Code subscription bridge when available, else an API key.
ambient = "claude"            # claude | ollama | chatgpt
ambient_model = "claude-sonnet-5"

# Heavier on-demand reasoning (deep correlation, mitigations).
heavy = "claude"
heavy_model = "claude-opus-4-8"

# On-device fallback for anything pinned local-only (never leaves the machine).
ollama_url = "http://127.0.0.1:11434"
ollama_model = "llama3.1"

# Pin privacy-sensitive sources here to force on-device Ollama instead of Claude.
local_only_sources = []

[context]
refresh_default = "6h"         # per-source override allowed; managed live in the UI
urls = ["https://status.example.com"]        # public
files = ["~/notes/architecture.md"]

# Authenticated URL sources reference a Keychain credential (set via the config
# page); the token is injected as a header at fetch time.
[[context.authed_urls]]
url = "https://runbooks.internal/oncall"
credential = "runbooks"        # Keychain account under service "dev.mugglebot"
header = "Authorization"

[mcp]
stdio = true
http_listen = "127.0.0.1:8787"

[ui]
listen = "127.0.0.1:8080"
```

---

## Notifications (macOS)

- Native notifications (e.g. `mac-notification-sys` / `objc` bindings), not a
  polling banner hack.
- **Rule-driven, not firehose.** `min_severity`, quiet hours, and per-source
  filters decide what actually interrupts you.
- **Deduplicated** against the board — you get notified once per thread state
  change, not once per underlying signal.
- **Actionable.** Click → open the relevant thread in the LCARS UI (or deep-link
  straight to the source).
- **Red-alert.** A high-confidence live-assist flag maps to Critical, so it
  notifies even during quiet hours — "you're about to be wrong" is the one case
  worth interrupting for.

---

## Design principles

Drawn directly from the cited inspiration (see _References_). These are the
non-negotiables that keep an "AI ops helper" trustworthy.

1. **Copilot, not autopilot.** MuggleBot surfaces, correlates, and _proposes_.
   Any action that mutates a real system stays human-authorized. This mirrors
   Google SRE's multi-layer safety: deterministic typed tools (not free-form
   shell), risk metadata per action, and a human confirmation gate.

2. **Correlate before you conclude.** Isolate the blast radius first — is one
   service failing or all of them? Grouping and timeline come before any
   hypothesis, precisely to avoid the red-herring trap that costs MTTM.

3. **Mitigate generically, understand later.** The catalog of _generic
   mitigations_ — rollback, data-rollback, drain/redirect, quarantine, upsize,
   degrade, block-list — is surfaced as first-move suggestions during an
   incident. Good generic mitigations are **fast, reversible, low-risk, and
   broadly applicable** without needing root cause. "The most expensive stretch
   of an outage is the time when users can see it."

4. **Explainability by construction.** Every summary, correlation, and
   suggestion cites the signals it came from. No black-box "trust me."

5. **Optimize for time-to-awareness / time-to-mitigation**, not time-to-fix.
   The win condition is that you knew, and knew fast — not that MuggleBot solved
   it for you.

6. **Audit everything.** What was surfaced, what was suggested, what you did.
   Local, append-only, inspectable.

7. **Institutional memory, curated.** Past incidents and their resolutions are
   retained and made searchable so tomorrow's correlation is smarter than
   today's. Memory is editable, and you can feed in reference material (runbooks,
   docs, status pages) as a refreshed, summarized context library — grounding you
   own and can inspect, not a black box.

8. **Local-first storage; reasoning via Claude.** Signals live on your machine
   (SQLite). Reasoning defaults to Claude — Sonnet ambiently, Opus for deep work
   — while sources you pin in `local_only_sources` are reasoned over on-device
   with Ollama and never leave the machine. There is no MuggleBot-operated
   backend; only your machine and the LLM provider you route to.

---

## Roadmap

**Status: Phases 0–4 are implemented.** All items below are landed; the notes
remain as the design record. Reasoning degrades gracefully when no LLM provider
is reachable (deterministic grouping + summaries stand). Semantic recall is
implemented with embeddings stored as `f32` BLOBs and ranked in-process by cosine
similarity (exact and trivially fast at a curated store's scale) rather than a
native vector extension — same behavior, one fewer moving part; a default local
hashing embedder means recall works with no model, and Ollama embeddings are used
when configured.

**Phase 0 — Skeleton.** ✅
Rust daemon, TOML config loading, the SQLite event store, the `Signal` model,
one watcher end-to-end (GitHub), macOS Keychain credential storage, and native
macOS notifications. Proves the spine.

**Phase 1 — All sources + board.** ✅
Slack (including alert channels) and Granola watchers. Deterministic correlation
(entity + time grouping). LCARS UI board + thread views over a live WebSocket.
Read-mostly triage (ack / snooze).

**Phase 2 — MCP + LLM correlation.** ✅
MCP server (stdio + HTTP). LLM relatedness / de-duplication over the
deterministic candidates, the relation graph, and human overrides (associate /
merge / split) that pin constraints and re-run analysis. Per-thread context
attach. Interactive Claude/ChatGPT correlation via the MCP client path.
Citations everywhere.

**Phase 3 — Grounding: memory, context & mitigations.** ✅
Generic mitigations catalog + `suggest_mitigations`. SQLite-backed memory with
a WebUI/MCP editor, plus the curated context library (URL + file ingest,
scheduled refresh, summarize + embed, Keychain-authed fetches). Semantic recall
across both stores (embeddings in SQLite, in-process cosine KNN).

**Phase 4 — Live assist & agent chat.** ✅
Live-thread detection via your Slack id, debounced re-analysis (1 min / 5 min
cap), grounded hints + suggestions, and correctness/risk flags that drive LCARS
red-alert + Critical notifications. Multimodal agent chat (screenshots, files)
over the same grounding and tools.

---

## Decisions

Resolved as the plan has firmed up:

- **Event store, memory & context → SQLite** (via `rusqlite`, statically
  linked; `sqlite-vec` **compiled in** — statically registered at connection
  open, no runtime extension load — for vectors, FTS5 for keyword search). One embedded store
  covers all four access patterns — the append-mostly signal log (relational
  filters on source/severity/time/state), the thread **relation graph** (an
  edges table + joins / recursive CTEs), the memory + context stores, and
  **semantic recall** (`sqlite-vec` similarity search). RocksDB would serve only
  the log and leave us hand-rolling every index — most painfully the vector
  index; SQLite folds all of it into one single-file, single-binary, zero-ops
  store that any `sqlite3` tool can inspect. The signal write rate is trivial, so
  SQLite's single-writer model is a non-issue. (Fallback if native vectors are
  preferred: libSQL.)
- **UI framework → SolidJS.** Fine-grained reactivity updates only the panels
  whose signals changed, which suits a high-frequency live board far better than
  a virtual-DOM re-render cycle; TS-first, tiny bundle, no runtime GC churn.
- **Secrets → macOS Keychain**, managed through a WebUI config page (see
  _Web UI_). The TOML holds only non-secret behavior.
- **Alerts come from Slack, not Incident.io.** Incident.io is dropped; instead
  you designate Slack `alert_channels` whose posts are treated as alert signals.
- **Ambient reasoning → Claude Sonnet** (`claude-sonnet-5`); deep on-demand
  reasoning → Claude Opus (`claude-opus-4-8`). On-device Ollama is the fallback
  for anything pinned in `local_only_sources`.
- **Poll cadences → per-source interval + adaptive backoff.** Each watcher has a
  configurable `poll_interval` and backs off on rate-limit signals (GitHub
  `X-RateLimit-*`, `Retry-After`) to stay under limits without going stale.
- **Authenticated context sources → macOS Keychain.** URL context sources (and
  per-thread URL context) behind auth pull their credential from the Keychain —
  the same store as source tokens, managed on the config page — and inject it as
  a header at fetch time.
- **Re-analysis debounce → 1 min after last activity, 5 min hard cap.** Thread
  interactions (including your own Slack messages) coalesce before a dispatch to
  the LLM.

## Open questions

- **Local-only model.** If you pin any `local_only_sources`, which Ollama model
  to run for them — quality vs. local latency. (Ambient reasoning defaults to
  Claude Sonnet, so this matters only for pinned sources.)
- **Grounding budget.** How much retrieved memory + context to fold into a
  summary before it crowds out the actual signal (a top-k + max-tokens cap).
- **Re-analysis scope.** Whether re-analysis is confined to the touched thread
  or ripples to its neighbors in the relation graph. (Timing is decided — 1 min
  after last activity, 5 min hard cap.)
- **Red-alert calibration.** Keeping live-assist flags from crying wolf — the
  confidence threshold, and whether a flag should need corroboration from more
  than one grounding source before it escalates to red-alert.
- **Chat vision routing.** Screenshots need a vision-capable model — default to
  Claude for chat; decide whether a local vision model is acceptable when the
  dropped content is sensitive.

---

## References / inspiration

- [How Google SREs use Gemini CLI to solve real-world outages](https://cloud.google.com/blog/topics/developers-practitioners/how-google-sres-use-gemini-cli-to-solve-real-world-outages)
  — the investigate → correlate → mitigate → postmortem loop; deterministic
  typed tools; copilot-not-autopilot; MTTM focus.
- [How Google SRE is using agentic AI to improve operations](https://cloud.google.com/blog/products/devops-sre/how-google-sre-is-using-agentic-ai-to-improve-operations)
  — explainability, graduated autonomy, context-enriched decisions, human-in-the-loop
  for risk, institutional memory via embeddings.
- [Generic Mitigations](https://www.oreilly.com/content/generic-mitigations/)
  — mitigate before diagnosing; the catalog of reversible, low-risk, broadly-applicable
  first moves.

---

## Working in this repo (for humans and agents)

Conventions:

- **Rust** for the backend/daemon (`tokio` async), **TypeScript** for the UI.
  No third language without a reason.
- Keep watchers isolated and normalizing — nothing source-specific leaks past
  ingest. Each watcher separates its HTTP `poll` from a pure `normalize_*`
  function that's unit-tested.
- One implementation of each capability lives in `src/tools.rs`; the web API,
  MCP server, and agent chat all dispatch through it, so they never drift.
- Comment sparingly; make the code self-documenting. Explain _why_, never _what_.

Quality gates (all green):

```sh
cargo fmt
cargo clippy --all-targets     # warning-free
cargo test                     # backend unit/integration tests
cd ui && npx tsc --noEmit && npm run build   # UI typecheck + build
```

Module map: `signal` (the normalized type) · `store` (SQLite: signals, threads,
relation graph, memory, context, tags, hints, health) · `watchers/{github,slack,granola}`
· `correlation/{engine,llm}` (deterministic grouping + the LLM `Analyst`) ·
`reasoner/{ollama,cli,api}` (+ `MockReasoner`) · `embed` (hash/Ollama embedders,
cosine KNN) · `tags` (vocabulary, classify/suggest, normalization) · `memory` /
`context` / `mitigations` (grounding) · `tools` (shared surface) · `mcp` (stdio +
HTTP) · `live_engine` + `live` (live assist) · `chat` (agent) · `server` (HTTP/WS)
· `event` (WS bus) · `notify` / `keychain`.
