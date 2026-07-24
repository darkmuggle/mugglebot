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
- **LCARS UI** — board, thread detail (timeline, relation graph, mitigations,
  inline associate/merge/split/attach-context), memory editor, context library,
  live-assist, agent chat, config/credentials, and red-alert — all fed live over
  a WebSocket.

Reasoning defaults to Claude (Sonnet ambient, Opus heavy) via the subscription
CLI bridge or the API; with no reachable reasoner, correlation and live-assist
degrade to deterministic behavior and the daemon keeps working.

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
