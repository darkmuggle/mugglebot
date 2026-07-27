# Reworking the codebase onto the Restate data model

The design is in [AGENTS.md](AGENTS.md). This is the plan for getting ~25k lines
of working Rust from where it is to where that document says it should be, in
increments that each ship.

## Status: complete

| Phase | Verified by |
|---|---|
| 0 — Secrets hardened | migration + sealing + wrong-passphrase error, against the real 3.6MB DB |
| 1 — `Thread` → subject | 270 tests; 234/234 signals re-attributed on the real DB, legacy tables dropped |
| 2 — Endpoint + three objects | live: duplicate key → 1 invocation, id+new version → 2, hierarchy linked |
| 3 — Watchers as objects, `poll_loop` deleted | live: cursor in object state, loop reschedules, restart recognises the durable timer |
| 4 — All seven workflows | live: unchanged sha → `PreviouslyAccepted` (free), `force` → new key |
| 5 — Durable-timer debounce | live: 3 records in 3s → 1 analysis pass (22s), 2 early returns (6ms) |
| 6 — vqueues | live: rule book applied from config, `local-llm` scope on the invocation |
| 7 — Human gates | mechanism in `restate/gate.rs`; no gated action ships, by design |

Phases ran 0 → 1 → 2 → 5 → 4 → 6 → 3 → 7 rather than in numeric order: 5 is small and
finishes the objects, 4 and 6 are what the data-model change was *for*, and 3 is
refactoring behind an interface the earlier phases had already made work.

**What the runtime looks like now.** Twelve registered services: five virtual objects
(`Issue`, `PullRequest`, `SlackThread`, `Watcher`, `Scheduler`) and seven workflows
(`RootCause`, `IssueTriage`, `BrowserRead`, `PrCritique`, `RepoIndex`, `ContextIngest`,
`Merge`). `main.rs` went from 988 lines to ~510: no `poll_loop`, no browser worker, no
triage worker, no live-engine tick loop, no repo-index or context refresh loops. What
remains there is construction, the three long-lived listeners (UI WebSocket, MCP, the
Restate endpoint), the completion-cache pruner, and one boot task that arms the
watchers and schedulers.

**`[restate].enabled` is gone.** Restate is the substrate, not an option: with the poll
loops, the debounce, and the pipelines all being handlers, there is no second execution
model to fall back to. A daemon that can't reach the ingress ingests nothing — visible
immediately rather than silently degraded.

Two principles decide the ordering:

1. **Rename before re-architecting.** The `Thread` → subject change touches 1056
   occurrences across 29 Rust files and 9 TS files. Doing it inside a single
   process, with the existing tests, is a mechanical refactor you can verify.
   Doing it while also moving execution into Restate is two unverifiable changes
   at once.
2. **Nothing gets a second execution model for long.** A `[restate] enabled =
   false` legacy path is useful for exactly two phases and then it is the thing
   stopping the migration from finishing. It is deleted in Phase 4, by plan, not
   by hope.

---

## Ground truth: three places the code already disagrees with the doc

Checked before planning, because two of them change what work is left.

**Secrets are already in SQLite.** `store.rs:144` has
`credentials(account TEXT PRIMARY KEY, secret TEXT NOT NULL)`, and
`credential_get/set/delete` (`store.rs:2249-2277`) are what `main.rs:175`
(`token_for`), `context.rs:455` (authed context fetch), and `server.rs:428` (the
config page's "is it set?" probe) actually use. There is no `keychain` module and
no `security-framework` dependency — AGENTS.md described an intent that was never
implemented. **So requirement 1 is hardening, not migration**, which moves it to
Phase 0 and makes it a day's work rather than a week's.

**The identity hierarchy already exists**, in `correlation/engine.rs:361`
`identity_rank`: `environment 5 > issue 4 > pr/discussion 3 >
branch/commit/slack_thread/meeting 2 > everything else 1`, with
`controlling_keys` enforcing strict matching on the top rank present. The new
model keeps the mechanism and changes the table — see the decision below, because
demoting `environment` is a behaviour change with a real regression attached.

**`poll_loop` is the whole system.** `main.rs:734-988` is 250 lines that per poll
does: enrich Slack links → queue browser investigation → queue issue triage →
insert → correlate → notify → collect Slack messages to classify → spawn
reanalyze → spawn root-cause investigation → spawn handled-thread triage →
reconcile the snapshot → repair orphans → push the board, four times. Every phase
below is partly a story about deleting a piece of this function, and the
decomposition is the main source of risk in the whole plan.

---

## Target module layout

```
src/
  signal.rs            SubjectKey, SubjectRank, ResolutionKey, Signal (no `state`, no `thread`)
  subject/
    mod.rs             the domain: SubjectState, Attention, Decorations, Handled
    resolve.rs         the ranked resolver (was correlation/engine.rs's key logic)
    projection.rs      the SQLite board projection: write on record, read by UI/MCP
  restate/
    mod.rs             Endpoint construction, deployment self-registration
    ingest.rs          Service: normalize → resolve → dispatch to a subject
    scopes.rs          vqueue scope + limit-key constants, one place
    objects/
      issue.rs         Issue VO
      pull_request.rs  PullRequest VO
      slack_thread.rs  SlackThread VO
      watcher.rs       Watcher VO (cursor + poll cadence)
      repo_card.rs     RepoCard VO
      context_source.rs
      github_budget.rs token bucket for requests/hour
    workflows/
      issue_triage.rs  root_cause.rs  pr_critique.rs  browser_read.rs
      repo_index.rs    merge.rs       context_ingest.rs
  correlation/llm.rs   the Analyst: same/related/distinct over subject keys (stays)
  store.rs             unchanged in kind; renamed columns; claim-queues deleted
  ... (watchers, reasoner, embed, tags, memory, context, comments, tools, mcp, server ...)
```

`correlation/engine.rs` (896 lines) does not survive as a module: its ranking
logic becomes `subject/resolve.rs`, its `thread_view`/`thread_views` become
`subject/projection.rs`, and its grouping loop is replaced by the resolver plus
`Issue::record`. `correlation/llm.rs` survives nearly intact — it was always
about judging pairs, and pairs of subject keys read the same as pairs of thread
ids.

---

## Phase 0 — Secrets, hardened (½ day)

Small, independent, and it removes a lie from the docs.

- Rename `credentials` → `secrets` with a migration (`ALTER TABLE ... RENAME`,
  guarded by a `PRAGMA user_version` bump — the store has no migration
  mechanism yet, so this phase introduces the smallest one that works).
- Enforce `0600` on the DB file at open, and on `-wal`/`-shm`. Currently whatever
  umask gives.
- **Write-only API.** `server.rs:428` already returns only presence; make that the
  only shape by adding `secret_status(name) -> {set: bool, updated_at}` and
  removing any path that could return a value. Add `updated_at` to the table.
- Add `secrets.rs`: `Secrets::get(name)` reading at call time (already the
  behaviour via `token_for`, so this is a rename plus a home), plus optional
  envelope encryption under `$MUGGLEBOT_MASTER_KEY` behind `[secrets] encrypt`.
  New deps: `aes-gcm` + `argon2`, or `ring`. Off by default.
- Scrub known secret names in the `tracing` layer.
- Fix the docs that claim otherwise: `config.example.toml:2,27,40,79,98` and
  `.gitignore:6` all say Keychain.

**Done when:** `cargo test` green, the config page still sets a GitHub token, and
`grep -ri keychain` returns nothing.

---

## Phase 1 — `Thread` → subject, in-process (3–4 days, the biggest refactor)

No Restate. The goal is that the domain model is right *before* the execution
model moves, so that when it does, only one thing is changing.

**1a. Types** (`signal.rs`, new `subject/`)

```rust
pub enum SubjectRank { SlackThread = 1, PullRequest = 2, Issue = 3 }
pub struct SubjectKey(String);      // "owner/repo#412" | "owner/repo!987" | "T01/C02/1721822400.001"
impl SubjectKey { fn rank(&self) -> SubjectRank; fn parse(&str) -> Option<Self>; }
pub struct ResolutionKey { pub kind: ResolutionKind, pub value: String }
```

`SubjectKey` is a newtype over the string, not an enum of parts: it goes into
SQLite columns, URLs, MCP arguments, and Restate object keys, so one canonical
string form with a validating parser is worth more than structured access.
`Entity` becomes `ResolutionKey` — same shape, honest name.

Delete `signal::State` and `Signal::state`. Handled-ness moves to the subject
(`Handled { Open, Seen, Acked, Snoozed(until), Resolved }`), which retires
`engine.rs:444`'s "a thread is as handled as its least-handled member" min-fold.
This is the change that touches the most SQL.

**1b. The resolver** (`subject/resolve.rs`)

```rust
pub fn resolve(sig: &Signal) -> Option<SubjectKey>
```

Ported from `identity_rank` + `controlling_keys`, but it now returns *one* key
instead of a set to match against, because the key is the address. The climb:
`issue` key → `pr` key → (`branch`|`commit` → PR lookup) → `slack_thread` →
`None`. Secondary links are returned alongside for `link_pr` / `link_slack`.

The branch/commit → PR lookup is a network call (`github.rs`), so it is
memoised in SQLite (`branch_pr_map`, `commit_pr_map`) — today this resolution
happens in the watcher; keep it there and have the watcher put the resolved `pr:`
key in `keys` so the resolver stays pure and unit-testable. That constraint is
load-bearing for the whole plan: **the resolver must never do I/O**, or Phase 2
can't call it inside an ingest handler without a `ctx.run`.

**1c. Schema** (`store.rs`, one migration)

| From | To |
|---|---|
| `signals.thread` | `signals.subject` |
| `signals.state` | dropped |
| `signals.entities` | `signals.keys` |
| `threads(id, …)` | `subjects(key, rank, handled, snoozed_until, …)` |
| `thread_edges(thread_a, thread_b, …)` | `subject_edges(a, b, …)` |
| `thread_context`, `thread_mitigations`, `thread_root_cause`, `hints.thread_id` | `subject_*`, `hints.subject` |
| `issue_triage.issue_key` | `issue_triage.subject` (already effectively this) |

Backfill: for each existing thread, pick the highest-ranked resolution key among
its signals, mint the `SubjectKey`, and rewrite `signals.subject`. Threads whose
signals yield no key become unattributed (`subject IS NULL`) — expected and
correct, and the count is worth logging loudly during the migration so you can
see how much of the old board was ranking on `repo` alone.

Add `signals.version` and widen the unique index to
`UNIQUE(source, external_id, version)` now, so Phase 3's idempotency keys have a
durable backstop to match.

**1d. Everything downstream.** `correlation/llm.rs`, `live_engine.rs`,
`rootcause.rs`, `triage.rs`, `prfix.rs`, `browser.rs`, `mitigations.rs`,
`tools.rs` (16 tool names + `ToolDef` list), `mcp.rs`, `server.rs` (16 routes),
`event.rs`, and the UI (`types.ts`, `Board.tsx`, `ThreadDetail.tsx` →
`SubjectDetail.tsx`, `Attention.tsx`).

**Done when:** `cargo test`, `cargo clippy --all-targets`, `npx tsc --noEmit`
green; the board renders the same subjects it did before, minus whatever
correctly fell into the unattributed lane; and `grep -rn '\bthread' src` only
matches Slack threads and GitHub notification threads.

**Decision required before starting — the `environment` rank.** Today
`environment` outranks `issue` (rank 5), on the stated reasoning that "an env id
names a specific customer's environment, which is the most specific thing an
alert can be about." The reworked AGENTS.md demotes it to context-only, alongside
`repo` and `main`. That is a **regression for tenant alerts**: alerts arrive via
Slack, so they all carry a `slack_thread` key, and two alerts about `env-2abc` in
two different Slack threads would stop collapsing.

Recommendation: demote `environment` out of the *subject* hierarchy as the doc
says, but keep it as a **deterministic merge key within the Slack rank** — two
`SlackThread` subjects sharing an `environment` key inside the correlation window
merge without asking the LLM. An env id is specific in the way `main` and `#alerts`
are not, which is exactly why it was rank 5; it just isn't a *durable piece of
work*, which is why it shouldn't be a subject. This keeps the behaviour and the
model. It needs one paragraph in AGENTS.md if accepted.

---

## Phase 2 — The Restate endpoint and the three subject objects (3–4 days)

Now the execution model moves, and nothing else does.

- `Cargo.toml`: `restate-sdk = "0.11"`.
- `restate/mod.rs`: build the `Endpoint`, bind the objects, serve on
  `[restate] endpoint_listen` (9080) from a `tokio::spawn` alongside the existing
  axum server. Self-register against the admin API on boot when
  `register_on_boot`, tolerating "already registered".
- `restate/objects/{issue,pull_request,slack_thread}.rs`: `IssueState` as in
  AGENTS.md; handlers `record`, `link_*`, `attach_context`, `set_tags`,
  `pin_relation`, `analyze`, `apply_artifact`, `mark_same_as`,
  `ack`/`snooze`/`resolve`, and **shared** `get`/`attention`/`timeline`.
- **The handler bodies are thin.** Each handler validates, mutates object state,
  and calls an existing free function for the real work — `Analyst::reanalyze`,
  `store::insert_signal`, and so on, wrapped in `ctx.run`. This is what keeps the
  test suite: the free functions stay directly callable, and no test needs a
  server.
- `subject/projection.rs`: every exclusive handler that changes a subject writes
  the board row. The UI keeps reading SQL and does not learn what Restate is.

**Transitional dual path.** `[restate] enabled` chooses between
`Correlator::ingest` and a call to `Issue::record`. Both write the same tables,
so the board is identical either way and you can flip back mid-debug. This flag
exists for Phases 2–3 only.

**Done when:** with `enabled = true`, a GitHub notification appears on the board
having gone through a virtual object; `restate` UI at :9070 shows the invocation;
and `enabled = false` still works.

---

## Phase 3 — Ingest through the ingress, and the God loop dies (4–5 days)

The riskiest phase, so it is the one with the most explicit sequencing.

**3a. `restate/ingest.rs`** — a plain Service (not a workflow; the exactly-once
property comes from the ingress key). One handler:
`ingest(Signal) -> Option<SubjectKey>`. It does: enrich → persist → resolve →
`Issue::record` send → link secondaries. The enrichment currently inline in
`poll_loop` (`enrich_slack_links`, `queue_dashboard_investigation`,
`queue_issue_triage`) becomes explicit steps here, each a `ctx.run`.

**3b. Watchers submit instead of processing.** `poll_loop` shrinks to:

```rust
for sig in batch.signals {
    ingress.send("Ingest/ingest", &sig)
        .idempotency_key(&format!("{}:{}:{}", sig.source, sig.external_id,
                                  sig.version.as_deref().unwrap_or("-")))
        .await?;
}
```

Everything else it did moves: notification to `Issue::record`; Slack tag
classification to a step in `ingest`; `reanalyze`/`investigate` spawns to the
subject's debounce (Phase 5) and the `RootCause` workflow (Phase 4);
`triage_handled` to `Issue::record`'s handled branch; `repair_orphaned_threads`
**deleted** (orphans were a consequence of threads being synthetic — a subject
cannot be orphaned from its own key); the snapshot reconciler to a `Watcher`
handler.

**3c. `Watcher` objects.** One per source, keyed by name, cursor in object state,
`poll()` exclusive and self-scheduling via `send_after`. Stale-tick guard as in
AGENTS.md. The `Watcher` trait keeps `poll()`/`normalize_*` exactly as-is — this
phase changes who calls it and where the cursor lives, not the HTTP code.

**Slack's socket stays a `tokio` task** in `main.rs`, submitting to the ingress.
Same for the UI WebSocket and MCP stdio.

**3d. Delete the flag.** `[restate] enabled` and the `Correlator::ingest` path go
away, and `correlation/engine.rs` is reduced to nothing (its remaining useful
parts already moved in Phase 1).

**Done when:** `main.rs` is under ~300 lines, `poll_loop` is gone, and a
duplicate submission of the same notification is visibly deduplicated by the
ingress (`restate` UI shows one invocation, not two).

---

## Phase 4 — Workflows (5–7 days)

Order by payoff: the two expensive pipelines first, because journalling is worth
most where a mid-pipeline failure currently costs minutes and metered calls.

| Workflow | Replaces | Notes |
|---|---|---|
| `RootCause` | `rootcause.rs` orchestration | steps already separable; `investigate_if_worthwhile` (`main.rs:683`) becomes the gate on submission |
| `IssueTriage` | `triage.rs:600` worker + `claim_issue_triage` | key `{subject}@{sha}[#a{n}]`; **delete** the `status`/`attempts` columns — the invocation is the status |
| `BrowserRead` | `browser.rs:305` worker + `claim_browser_investigation` | ditto; `browser_investigations` keeps findings, loses queue state |
| `PrCritique` | `prfix.rs` | key `{pr}@{sha}` |
| `RepoIndex` | `main.rs:337` refresh loop | fan-out per repo, each writing a `RepoCard` |
| `ContextIngest` | `main.rs:427,444` refresh + dir-sync loops | key `{id}@{etag\|mtime}`; `ContextSource` object holds the schedule |
| `Merge` | `Analyst`'s auto-merge path | plus the late-demotion case, which has no current equivalent |

Two things to get right, both easy to get wrong:

- **Error classification.** Restate retries forever by default. Every existing
  `anyhow::Error` return needs triage into transient vs `TerminalError`. The
  rule: 4xx that won't change (404, 401 on a revoked token, "no such repo") and
  unparseable-after-N-attempts model output are terminal; rate limits, 5xx,
  timeouts, connection resets, and "Ollama isn't running" are transient. Getting
  this wrong in the terminal direction loses work silently; getting it wrong in
  the transient direction hammers an endpoint for a week. Audit it per call site,
  not per module.
- **The two cache layers coexist.** `reasoner/cache.rs` stays exactly as it is.
  The journal stops a *retry* re-paying; the cache stops a *new invocation*
  re-paying. Deleting the cache because "Restate caches now" would be wrong.

**Done when:** `claim_*` is gone from `store.rs`, the status columns are gone, and
killing the process mid-triage resumes at the step it stopped on.

---

## Phase 5 — Durable timers (1–2 days)

`live_engine.rs`'s in-memory `HashMap<thread_id, Pending>` plus tick loop
(`live_engine.rs:52-144`) becomes `debounce_deadline` in object state plus
`analyze().send_after(...)`, with the stale-deadline guard. `LiveEngine::run` is
deleted; `analyze_thread` becomes the body of `Issue::analyze`.

This is small and high-value: during development the process restarts constantly,
and every restart currently drops every pending re-analysis.

---

## Phase 6 — vqueues (1–2 days, gated on the experimental flags)

Last, deliberately: it needs a fresh cluster, and the limits are only tunable
once there is traffic in `sys_vqueues` to look at.

- `restate/scopes.rs`: the scope and limit-key constants in one place.
- `.scope(...)`/`.limit_key(...)` on the call sites: reasoner tier calls
  (`local-llm`, `cloud-llm`/`{sonnet,opus}`), GitHub calls (`github`),
  `BrowserRead` (`browser`), checkout (`checkout`, limit key per repo).
- `restate rules set` invocations from `[restate.limits]` on boot, so config is
  the source of truth rather than a shell history.
- **Delete the hand-rolled equivalents**: whatever serialization currently exists
  around Ollama and the shared browser. The `GithubBudget` token bucket is *not*
  deleted — concurrency is not rate.
- Add a UI panel reading `sys_vqueues` via the admin SQL endpoint, because "queued
  behind the local model" and "broken" otherwise look identical.

**Done when:** two subjects analyzed simultaneously visibly serialize on the local
model, and the UI says why.

---

## Phase 7 — Human gates (1 day)

`ctx.signal::<bool>("approved")` on any handler that would act, the pending-gate
list on the board, `resolve_gate` in `tools.rs`, and an audit row per resolution.
No gated action ships — the mechanism exists so that the first write tool doesn't
have to invent authorization.

---

## Cross-cutting

**Testing.** The rule that keeps the suite alive: handler bodies stay thin
wrappers over free functions, and the free functions are what the tests call. The
existing ~unit tests in `store.rs` (2978+), `correlation/engine.rs` (757+),
`triage.rs`, `checkout.rs`, `context.rs` survive Phase 1 as renames. Add one
integration test module, `#[ignore]` by default, that brings up the container and
drives the ingress — enough to cover idempotency-key dedup, the debounce
coalescing, and one workflow resume, which are exactly the three behaviours no
unit test can reach.

**Rollback.** Phases 0–1 are ordinary migrations with a `user_version` bump; keep
a copy of the DB before running them. Phases 2–3 have the `[restate] enabled`
flag. From Phase 4 there is no flag — the rollback is git, which is why 4 comes
after the model is settled and verified.

**What gets deleted, in total:** `correlation/engine.rs` (896), `live_engine.rs`'s
loop (~120 of 709), the browser and triage claim loops and their status columns
(~150 across three files), `poll_loop` (~250), `repair_orphaned_threads` and its
callers, three refresh loops in `main.rs` (~90). Net: the rework should *shrink*
the codebase by roughly 1200 lines while adding the `restate/` tree, because most
of what it adds is the substrate's job now.

**Rough total: 4–5 weeks** at this level of care, with Phase 1 and Phase 3 the two
that will overrun.

---

## Open decisions the plan needs answered

1. **The `environment` rank** (blocks Phase 1). Recommendation above: demote from
   the subject hierarchy, keep as a deterministic merge key within the Slack rank.
2. **Unattributed backfill.** Phase 1c will orphan some current threads. Log and
   accept, or hand-map the top N before cutting over?
3. **vqueues on by default?** They're experimental and need a fresh cluster.
   Recommendation: ship Phase 6 off by default until 1.8 makes them GA, with the
   config knob present.
4. **Workflow retention.** `IssueTriage` mints an instance per `{issue}@{sha}`;
   retention has to be long enough for the free-redo property to pay and short
   enough not to accumulate. Start at 30 days and measure.
