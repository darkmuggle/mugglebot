# MuggleBot

A single-pane-of-glass ops-awareness agent. Watches your notification surfaces
(GitHub, Slack — including alert channels — and Granola), correlates related
activity, and surfaces it through native macOS notifications and a Star-Trek
LCARS-inspired web UI. Rust backend, SolidJS frontend, macOS-native.

See [AGENTS.md](AGENTS.md) for the full design and roadmap.

## Status

**Phases 0–4 are implemented.** The full spine plus:

- **All sources** — GitHub, Slack (watched + alert channels, own-message
  tagging), and Granola watchers, each normalizing into the common `Signal`.
- **Correlation** — deterministic entity + time grouping into threads, plus an
  LLM tier that judges `same`/`related`/`distinct`, builds a persisted relation
  graph, and honors human override pins (associate / merge / split) that re-run
  analysis. Every summary cites its signals.
- **Grounding** — editable institutional **memory**, a curated **context
  library** (URL + file ingest, ETag/mtime-aware refresh, Keychain-authed
  fetches), semantic recall over both, and a generic-**mitigations** catalog.
- **MCP** — the full read/correlation/grounding/live-assist tool surface plus
  resources, over stdio **and** HTTP JSON-RPC.
- **Live assist** — debounced re-analysis of threads you're active in, grounded
  hints/suggestions and correctness/risk **flags** on your own messages, driving
  LCARS red-alert + a Critical notification.
- **Agent chat** — a multimodal chat panel over the same tools and grounding.
- **Browser investigation** — a Slack alert that links to a dashboard gets that
  page *read*: MuggleBot drives your already-signed-in Chrome over the DevTools
  Protocol (read-only) and files what it saw back onto the thread as evidence.
- **Root-cause investigation** — the org's repos are indexed by *reading their
  code* into a symptom→repo routing table, then a symptom is searched across issues,
  PRs, and the commit log, producing ranked candidate causes with citations (and
  falling back to code search when nothing has been filed yet).
- **Assigned-issue triage** — every issue assigned to you gets a board card even
  if it never produced a notification. MuggleBot checks the repo out, has the
  local coder model read the actual source, characterizes the issue, proposes
  three distinct patch approaches with files/risk/effort, checks whether an open PR
  (often somebody else's) already fixes it, and renders the whole thing in plain
  English.
- **Comment judgment** — every comment on an issue or PR is scored for whether it
  carries decision-relevant information, and selection is by merit rather than
  position, so a decisive comment buried mid-thread survives and "+1" doesn't.
  Blocking reviews are pinned at maximum merit and can't be demoted.
- **Attention + AI-decoration indicators** — the board leads with *does this need
  you* and *has the AI been over it* (per-facet, filled or hollow), with work
  attributed `⌂` on-device vs `☁` metered. The unseen/ack state machine is still
  there for filtering, but it's no longer the headline.
- **LCARS UI** — board, thread detail (timeline, relation graph, mitigations,
  inline associate/merge/split/attach-context), memory editor, context library,
  live-assist, agent chat, config/credentials, and red-alert — all fed live over
  a WebSocket.

## Where each model runs

The default model is the **local** one. Before running a task, the local model
grades how much reasoning that task actually needs, and the grade picks the tier:

| Grade | Who answers |
|---|---|
| `easy`, `medium` | local (`deepseek-coder:33b`) alone |
| `hard` | local drafts, **Sonnet cleans it up** — or Sonnet does it outright if local fails |
| `extra_hard` | **Opus** directly; local doesn't attempt it |

There's a fourth model that isn't a reasoning tier at all: **Haiku** (`brief`)
only rewrites an analysis another model already did into plain English. It's
never asked to conclude anything, which is why a small fast model is correct
there rather than merely cheap.

**Answers are cached.** MuggleBot re-reasons constantly — a thread is re-analyzed
on every new signal, the same call sites get graded, a restart replays work
already done — and most of those requests are byte-identical to one already
answered. Identical request in, stored answer out, no model involved. The cache is
in SQLite rather than memory because a restart is exactly when you most want the
answers back. Deliberate redos bypass it: "reconsider on model X", "re-triage this
issue", and chat all force a fresh call, so those actions never look like they did
nothing. Empty responses aren't cached either — a model returning nothing is a
transient failure, not an answer. Tune with `[reasoner.cache]`.

Grading is itself a local call, deliberately tiny (~10 output tokens, ~0.5s warm),
and cached per call-site shape — so a task type grades roughly once per process,
not once per invocation. Tune it in `[reasoner.routing]`: `cleanup = false` keeps
hard tasks fully on-device, `cloud_fallback = false` means nothing ever leaves the
machine, and `enabled = false` runs everything locally ungraded.

For `hard`, the local draft is passed *to* Sonnet as material rather than thrown
away — a draft that's 80% right anchors the cloud call, and the cleanup prompt
insists the output format is preserved (most callers here parse strict JSON).

Some work is pinned on-device **regardless of grade**, because it must never reach
a cloud model at all: tag classification, repo-index crawling, and reopen-matching
against handled threads. Those bypass routing entirely.

Two further rules:

- **Handled threads never reach a cloud model.** A snoozed, acknowledged, or
  resolved thread is settled work. New activity on one is matched *locally* to
  decide whether the issue genuinely recurred; if it did, the thread reopens and
  earns normal treatment. Asking to "reconsider" a handled thread is an error,
  not a silent no-op — reopen it first.
- **Investigation escalates, it doesn't start cloud-side.** Crawling repos and
  filtering dozens of issues and commits happens on-device; only
  `[investigation].shortlist_size` already-plausible candidates reach the routed
  tier. So an investigation is a handful of local passes and at most one cloud
  call — and only if that final verdict grades hard enough to need one.

With no reachable reasoner at all, correlation, live-assist, and investigation
degrade to deterministic behavior and the daemon keeps working. Pull the local
model with `ollama pull deepseek-coder:33b`.

## Assigned issues

Every open issue assigned to you on GitHub gets a board card, **whether or not it
ever produced a notification** — assignment is a standing state, not an event, so
the issue assigned three weeks ago with no activity since is both invisible to the
notification feed and the one most likely to have slipped. It's polled separately
(`[assigned]`) and reconciled against its own listing, so a card disappears when
the issue is closed or reassigned.

Each one is then triaged against the real source, because the cold start is the
expensive part of picking an issue back up:

1. **Pull the code** — shallow, read-only checkout under `<data_dir>/repos`. The
   cache is bounded by `[assigned].max_cache_mb` (5GB default) with LRU eviction;
   the code-derived repo index clones across the whole org, so the total matters
   more than the per-repo limit.
2. **Find the relevant files** — deterministically, by matching identifiers from
   the issue text against paths and contents. Works with no model at all.
3. **Characterize** — the local coder model reads the issue *and the source*.
4. **Propose three approaches** — deliberately distinct strategies (a minimal fix,
   a fuller refactor, a mitigation), each with its files, risk, and effort.
5. **Check whether somebody's already on it** — scan the repo's open PRs. For each
   plausible one: what the **diff** actually implements, a skeptical critique of
   whether it really fixes the issue, and which other open issues it would also
   resolve. A PR saying "closes #412" is a claim; the critique is the check. Local
   model first, escalating only if it can't answer.
6. **Plain English** — Haiku re-renders it for the board.

Steps 2–5 never leave the machine. Patch options are **proposals, never applied** —
nothing here commits, pushes, opens a PR, or comments on somebody else's, and paths
the model wasn't actually shown are dropped so a confident-looking citation can't send you hunting for a
file that doesn't exist. Every triage records the commit it read, so you can tell
when the analysis has gone stale. Needs `git` on `PATH`.

## Investigation & browser control

Root-cause investigation needs the `github` credential. At startup MuggleBot lists
the configured org's repositories and distills each README into a *purpose +
symptom* card; that index is what routes "environment stuck provisioning" to
`restate-cloud` rather than searching everything. Then it searches issues/PRs,
scans the commit log over the incident window, ranks the candidates, and — when
nothing explains the symptom — searches code. Every candidate is a **hypothesis
with a citation and a confidence**, never a conclusion.

Browser control is off by default because it needs Chrome listening on a debug
port. Start Chrome once with:

```sh
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --remote-debugging-port=9222
```

then set `[browser].enabled = true`. MuggleBot spawns `claude -p` with
[`chrome-devtools-mcp`](https://www.npmjs.com/package/chrome-devtools-mcp)
attached to that Chrome, so the dashboard is read through *your* SSO session.

This is deliberately **not** the Claude-in-Chrome or ChatGPT-Atlas extension:
those attach a model to a tab from inside the browser UI and expose no way for a
background daemon to hand them a URL and collect an answer. The CLI-plus-CDP path
reaches the same authenticated page and is scriptable.

It is **read-only by construction**: the tool allowlist grants navigate, snapshot,
and screenshot and never click, fill, or evaluate; `--strict-mcp-config` keeps your
own MCP servers (and their write tools) out of the session. An investigation cannot
acknowledge or silence an alert — including if the page tries to talk it into it.

## Run

```sh
cp config.example.toml config.toml   # secrets go in the Keychain, not the file
cargo run -- --config config.toml
```

Then open <http://127.0.0.1:8080> for the LCARS UI. On the **config page** you can
edit `config.toml` (enable sources, tune the reasoner) **and store credentials** —
tokens are written to the macOS Keychain. Enable a source and add its token there,
then restart.

**Store tokens via the config page** (not `security add-generic-password`): when
MuggleBot writes the Keychain item itself, it owns it and reads never prompt. An
item created by the `security` CLI belongs to a different app, so macOS gates each
read behind an access prompt — under a background launcher (Tilt) that prompt may
never be answered, which is why watchers appear "not to start". (MuggleBot now
reads keys off the async runtime with a timeout, so a stuck prompt no longer hangs
startup — it just skips that watcher with a warning.)

A GitHub token needs the `notifications` scope (classic PAT) or `Notifications:
read` (fine-grained). Slack needs `channels:history` + `channels:read`. Keychain
accounts are the source names: `github`, `slack`, `granola`, and optional `ollama`
(Ollama Cloud key). **Reasoning uses the Claude/Codex CLI** (`claude -p` /
`codex exec`) riding your existing subscription — no LLM API keys.

Logging is via `RUST_LOG` (default `info,mugglebot=debug`) and goes to **stderr**
(stdout is reserved for the MCP stdio transport).

## MCP endpoint

MuggleBot serves MCP over stdio and over HTTP JSON-RPC (default
`127.0.0.1:8787`). Point a Claude/ChatGPT client at it to reason over the same
board, threads, memory, and context:

```sh
# HTTP transport
curl -s localhost:8787 -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Read tools are free; write tools (`relate`, `split_thread`, `put_memory`, …)
carry `readOnlyHint: false` annotations. Resources: `board://current`,
`config://redacted`, `memory://`, `context://`, `live://hints`.

## Develop with Tilt (recommended)

[`tilt up`](https://tilt.dev) runs the backend (rebuild-on-change) and the Vite
UI (hot reload) together, with on-demand **test / clippy / fmt** buttons in the
Tilt UI. It reads the backend port from `config.toml` and passes it to both
processes, so they always agree.

```sh
cp config.example.toml config.toml   # once
tilt up                              # Tilt UI at http://localhost:10350
tilt down                            # stop
```

## Frontend

The LCARS web UI lives in [`ui/`](ui/) (SolidJS + Vite). **Build it once and the
backend serves it same-origin** — then just open `http://127.0.0.1:8080`:

```sh
cd ui && npm install && npm run build   # backend serves ui/dist automatically
```

For hot-reload development, run the Vite dev server instead (on `:5173`, allowed
cross-origin by the backend):

```sh
cd ui && npm run dev
```

The UI connects to the backend's `/ws` for the live board. It targets the current
origin in production and `localhost:8080` under `vite dev`; override with
`VITE_BACKEND`. Point the server at a different built UI with `$MUGGLEBOT_UI_DIR`.

## Quality gates

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```
