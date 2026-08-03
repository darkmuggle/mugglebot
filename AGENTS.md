# MuggleBot

> An ambient, single-pane-of-glass ops-awareness agent for one engineer.
> It watches the places where work and incidents show up, attributes them to the
> thing they're actually about, and makes sure you never miss the item that
> mattered — without pretending to be on-call for you.

MuggleBot is a local-first daemon that watches your notification surfaces
(GitHub, Slack — including designated alert channels — and Granola), normalizes
everything into a common signal model, attributes each signal to the durable
piece of work it belongs to, and surfaces it through native macOS notifications
and a Star-Trek **LCARS**-inspired web UI. It also exposes an **MCP endpoint** so
you can pull its correlated context straight into a Claude or ChatGPT session for
deeper investigation.

Durable state and durable execution are split deliberately: **[Restate](https://restate.dev)
holds the work in flight — the virtual objects that model each piece of work and
the workflows that update them — and SQLite holds the record.** Everything runs on
your machine, Restate included.

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
- **Survive the restart.** Ingest, analysis, triage, and investigation are
  durable: a crash, a rate limit, or a `cargo build` mid-pipeline resumes where it
  stopped rather than starting over or silently dropping the work.

## Non-goals (at least for v1)

- **Not an autopilot.** MuggleBot informs and proposes. It never mutates a
  production system on its own. (See _Design principles → Copilot, not
  autopilot_.)
- **Not a paging system.** It's a lens over your existing tools, not a system of
  record or a paging authority — it won't page you or hold incident state.
- **Not multi-tenant.** It runs as a single-user, local-first tool on your Mac.
  No shared server, no central store of your signals — the Restate server is a
  local container, not a hosted cluster.

---

## How it works (at a glance)

```
   ┌──────────────┐  ┌────────┐  ┌─────────┐
   │  GitHub API  │  │ Slack  │  │ Granola │   sources (Slack incl. alert channels)
   └──────┬───────┘  └───┬────┘  └────┬────┘
          │  Watcher virtual objects (poll loops) + the Slack socket task
          └──────┬───────┴────────────┬────────┘
                 ▼                     ▼
      ┌────────────────────────────────────────────┐
      │  Restate ingress  (idempotency-key = source:external_id)
      └───────────────────┬────────────────────────┘
                          ▼
      ┌────────────────────────────────────────────┐
      │  Ingest service — normalize → resolve subject│
      └───────────────────┬────────────────────────┘
                          ▼   attribution climbs the hierarchy
   ┌──────────────────────────────────────────────────────┐
   │  Subjects (Restate virtual objects, one per key)      │
   │                                                       │
   │     Issue  ◄── parent of ──  PullRequest              │
   │        ▲                          ▲                   │
   │        └────── context ───────────┘                   │
   │            SlackThread · Granola meeting              │
   └───┬──────────────────────────────────────────┬───────┘
       │ workflows update subjects                │ subjects read/write the record
       ▼                                          ▼
  ┌──────────────────────────┐          ┌────────────────────────┐
  │ Workflows                │          │ SQLite — the record     │
  │ IssueTriage · RootCause  │◄────────►│ signals · comments      │
  │ BrowserRead · PrCritique │          │ memory · context · tags │
  │ RepoIndex · Merge        │          │ artifacts · secrets     │
  └──────────────────────────┘          └───────────┬────────────┘
       │ vqueues bound concurrency                  │
       ▼ (github · local-llm · cloud-llm · browser) ▼
  ┌───────────┐   ┌──────────────┐   ┌──────────────┐  ┌──────────────┐
  │ macOS     │   │ Web UI       │   │ MCP endpoint │  │ Claude /     │
  │ notifs    │   │ (LCARS, TS)  │   │ (stdio+HTTP) │  │ ChatGPT      │
  └───────────┘   └──────────────┘   └──────┬───────┘  └──────▲───────┘
                                            └─────────────────┘
```

- **Rust backend** (`tokio`): the watchers, the Restate service endpoint (virtual
  objects + workflows), the SQLite store, the MCP server, the notifier, and the
  HTTP/WebSocket server for the UI — one binary, one process.
- **Local Restate server**: a container started by Tilt. It holds invocation
  journals, virtual-object state, durable timers, and the vqueues.
- **TypeScript frontend**: the LCARS single-pane UI, fed live over a WebSocket.
- **macOS notifications**: native, actionable, rule-driven.
- **Configuration**: a single TOML file for behavior; secrets live in SQLite.

---

## The data model — three subjects and a hierarchy

The unit the board is built from is a **subject**: the durable piece of work a
signal is *about*. There are exactly three kinds, and they are ranked:

> **GitHub Issue** > **Pull Request** > **Slack thread**

Each is a **Restate virtual object**, keyed by its real upstream identity:

| Subject | Virtual object | Key |
|---|---|---|
| GitHub issue | `Issue` | `{owner}/{repo}#{number}` |
| Pull request | `PullRequest` | `{owner}/{repo}!{number}` |
| Slack conversation | `SlackThread` | `{team}/{channel}/{thread_ts}` |

**The rank is the whole point.** An issue is the durable statement of what the
work is; a PR is one attempt at it; a Slack thread is people talking about it.
So a signal is attributed as far *up* that chain as it can be resolved, and the
highest rank that resolves is the subject that owns it. Concretely:

- A CI run resolves through its head branch to the open PR it ran on; on a
  default branch — where no open PR exists — through its commit to the PR that
  merged it.
- A PR resolves to the issue it closes, via GitHub's closing keywords.
- A Slack message resolves to a PR or issue if the thread references one (a link,
  an `owner/repo#123`, a bot post carrying the URL) — otherwise the Slack thread
  is itself the subject.
- A Granola meeting is **never** a subject. Its extracted action items resolve to
  an issue or PR, or they stay in the meeting record.

Each rank the attribution climbs is retained as a secondary link, so a later
notification naming only the PR still lands on the same subject.

Without the hierarchy the same piece of work fragments: CI clusters by branch, the
PR sits on its own, the Slack thread is a third card, and the issue everyone is
actually working on becomes a fourth with none of the activity attached.

### What is not a subject

`repo`, `environment`, `service`, `channel`, `person`, `branch`, and `commit` are
**resolution keys and context, never subjects.** They're how a signal finds its
subject, and they're what the reasoner reads for background — but nothing is
keyed on them.

The reason is the same in every case: they're long-lived and shared. `main` is
shared by every CI run in a repository forever; a Restate Cloud environment is
shared by months of alerts; `#alerts` is shared by everything that ever fired.
Keying a subject on any of them collapses the repo's whole history into one card.

**`environment` is the one that keeps a job.** A Restate Cloud environment id names
one customer's environment, which is specific in the way `main`, `repo`, and
`#alerts` are not — it was the *top* rank in the pre-Restate model for exactly that
reason. It still isn't a durable piece of work, so it isn't a subject. But
demoting it to plain context would cost something real: alerts arrive through
Slack, so every one of them carries a `slack_thread` key, and two alerts about
`env-2abc` in two different threads would stop collapsing. So `environment` is a
**deterministic merge key within the Slack rank**: two `SlackThread` subjects
sharing an environment key inside the correlation window merge without asking the
LLM. It groups; it never owns.

Signals that resolve to no subject at all — a CI failure on a commit with no PR,
a meeting action item naming nothing — go to an **unattributed** lane in SQLite.
They are deliberately *not* given a subject of their own: minting a virtual object
per unresolvable event is exactly how you get a board full of near-identical
one-signal cards.

> **Terminology note.** This replaces the earlier synthetic `Thread` — a group
> invented by the correlation engine and keyed by an internal id. Subjects are
> keyed by the upstream identity instead, which is what makes them addressable
> from any watcher, any workflow, and the MCP surface without a lookup table. The
> code's `thread_id` becomes `subject` (see _Migration_).

### Why virtual objects

Three properties, each of which replaces something currently hand-rolled:

1. **Serialized writes per key, for free.** Restate runs at most one write-access
   handler per object key at a time. Two watchers ingesting activity about
   `restatedev/restate#412` in the same second cannot interleave, so the
   read-modify-write on a subject's links, counters, and debounce state needs no
   lock, no transaction retry, and no "who won" reconciliation. The correlation
   engine's races disappear because the concurrency model forbids them.
2. **State that is already keyed by the thing it describes.** `ctx.get_state` /
   `ctx.set_state` on the `Issue` object *is* the issue's record of what
   MuggleBot knows. There is no cache-coherency question between an in-memory map
   and a table.
3. **Durable timers on the entity.** The re-analysis debounce, the poll cadence,
   and the context-refresh schedule are `ctx.sleep` + delayed self-sends. They
   survive a restart, which in-process `tokio::time` does not.

The **shared** (read-only) handlers matter as much: `Issue::get`, `Issue::attention`
run concurrently with each other and with the exclusive writers, so the board
reading two hundred subjects never queues behind an in-progress analysis.

### Subject state — what a virtual object holds

Very little, and that is the point. The storage rule ("Restate holds the work in flight,
SQLite holds the record") applies to the objects themselves:

```rust
// Issue / PullRequest / SlackThread object state, in full:
signal_count: u32,                  // a ranking hint; the authoritative count is a query
debounce_deadline: Option<u64>,     // when the re-analysis pass should run
first_activity: Option<u64>,        // start of the current debounce window, for the cap
```

Everything else a subject knows — its title, summary, tags, triage state, merge pointer,
parent link, and every artifact — lives in SQLite and is read through the store. Three
reasons, in order of how much they'd hurt:

1. **The board is a cross-key query.** "Every subject needing attention, ranked, filtered
   by source" cannot be answered from object state at all, so the data has to be in SQL
   regardless. Keeping a second copy in the object would mean two things to reconcile.
2. **A Restate wipe must cost only in-flight work.** Enabling vqueues requires a fresh
   cluster. Anything held only in object state would be lost by an operation the
   deployment instructions actively recommend.
3. **Journals should carry ids, not bodies.** State is journalled; a summary or a
   critique in object state is replayed on every retry.

What the object *does* own is the part that only makes sense in flight: the debounce
window, and the counter that arms it.

Handlers on `Issue`:

| Handler | Access | Purpose |
|---|---|---|
| `record(signal_id)` | exclusive | attribute a signal, notify once per state change, arm the debounce |
| `analyze()` | exclusive | the debounced re-analysis pass, then the root-cause gate |
| `triage(handled)` | exclusive | ack / snooze / resolve |
| `set_tags(tags)` | exclusive | pin routing labels, then re-analyze |
| `mark_same_as(key)` | exclusive | merged away: re-point the signals, forward future activity |
| `get()` | **shared** | the read surface for UI, MCP, and the notifier |

`record` takes a signal **id**, not a signal: the body is already in SQLite, and a 200KB
raw notification passed as a handler argument is 200KB in the journal, replayed on every
retry.

The hierarchy links, the relation pins, and the ad-hoc context are **not** object
handlers — they're writes to the store made through the tool surface, because none of
them is a read-modify-write on a subject's in-flight state and routing them through a
handler would buy nothing but an extra hop. What has to be in an exclusive handler is
what races: attribution, the notification watermark, and the debounce.

### Context flows up, attention does not

Granola and Slack **add context to an `Issue` or `PullRequest`**; they do not
compete with it for attention.

When a Slack message resolves upward, its content is attached to the resolved
subject with `slack_thread` provenance and no `SlackThread` object is created —
the conversation is evidence about the issue, not a second thing to look at. A
`SlackThread` object exists only when it is the top rank present: an alert in
`#alerts` about a service with no filed issue, a DM, an incident channel thread.
That is the case where the conversation genuinely *is* the work.

**Late resolution is the interesting failure.** An alert thread often names the issue on
message twelve, not message one — by which time it is already a subject with its own
analysis, its own notifications, and possibly its own root-cause report. Discovering the
link then must not leave two cards. So the thread is demoted: it gets
`same_as = Some(issue_key)`, **its signals move with it**, and `record` addressed to the
demoted object forwards to the canonical one from then on. Demotion is idempotent and
re-runnable, because a second link discovery on message fourteen must be a no-op rather
than a second merge.

Two halves of that are easy to implement separately and wrong to separate. The board
hides a subject with `same_as` set, so a pointer written without moving the signals
doesn't collapse two cards into one — it hides one card *and every signal on it*, which
on an incident thread is eleven messages of history disappearing with no error anywhere.
And a forward that arms the debounce on the demoted object schedules an analysis of a
subject whose signals have all left, so it summarizes nothing while the canonical subject
is never analyzed at all. Both are one transaction and one code path
(`Store::merge_subject_into`), and both have a test.

---

## Deduplication — three layers, three mechanisms

"Don't show me the same thing twice" is three different problems. Conflating them
is why dedup logic rots.

### 1. Ingest dedup — exactly-once per upstream event

Every observed event is submitted through the Restate ingress with an
**idempotency key** derived from the event itself:

```
idempotency-key: github:notification:{thread_id}:{updated_at}
idempotency-key: slack:{team}/{channel}/{ts}:{edited_ts|-}
idempotency-key: granola:{meeting_id}:{updated_at}
```

Restate dedups the *invocation*: a re-poll that re-sees the same notification, a
watcher restart that replays its cursor, or a retry after a half-finished ingest
resolves to the original invocation instead of a second one — and an in-flight
duplicate attaches to the running one rather than racing it. That is a guarantee
the current unique-index-and-hope arrangement does not provide, because the index
catches the duplicate row *after* the side effects of ingest have already run
twice.

The version component (`updated_at`, `edited_ts`) is deliberate. A GitHub
notification thread is mutable — the same `thread_id` legitimately re-fires when a
new comment lands — so keying on the id alone would swallow real activity. Keying
on id-plus-version makes "the same event" and "the same thread, changed" distinct.

**The retention window is not a ledger.** Restate keeps idempotent results for a
bounded period (24h by default, per-service configurable). It is the fast,
cheap layer. The durable `(source, external_id, version)` unique index in SQLite
stays as the long-horizon backstop, and every ingest step is written
conditionally, per Restate's own guidance on pairing durable execution with a
database. Deleting `restate-data` must not resurrect last month's notifications.

### 2. Attribution dedup — the hierarchy does the clustering

Most of what used to require correlation is now deterministic. A CI failure, a
review request, a `@`-mention, and an assignment about one PR all resolve to
`{owner}/{repo}!{n}` and land on the same object. No time window, no entity
overlap scoring, no LLM, no candidate pairs.

This also retires a wart: the two GitHub watchers (notifications and assigned)
each reconcile against their own complete listing, and neither listing contains
the other's ids, which previously forced a synthetic `assigned/` id prefix to
keep one snapshot from resolving the other's cards wholesale. Keyed on the real
GitHub identity, both watchers converge on the same object by construction, and
the prefix hack goes away.

### 3. Semantic dedup — the LLM judges what identity cannot

What's left is genuinely hard and stays a model's job: two issues filed for one
bug, an alert thread and the issue about it, a PR that turns out to fix something
already fixed. Deterministic candidacy proposes pairs (shared resolution keys,
tight time proximity, tag overlap); the **LLM classifies each pair** — `same` /
`related` / `distinct` — returning a verdict, a confidence, a one-line rationale,
and the signals it weighed:

- **same** — duplicates of one underlying issue. Collapsed by the `Merge`
  workflow into a canonical subject; the other carries `same_as`.
- **related** — distinct but connected (a deploy PR and the incident it caused).
  Linked and cross-referenced, kept separate.
- **distinct** — explicitly unrelated. A negative edge that stops future
  regrouping.

The result is a **relation graph over subject keys**, persisted in SQLite as an
edges table. Whether a high-confidence `same` verdict auto-merges or is merely
_proposed_ for your confirmation is a config switch (`auto_merge`).

Time proximity is still measured **around the signal's own `occurred_at`, never
around wall-clock now.** Anchoring the window to `now` silently breaks every
catch-up ingest: on a first poll, a restart, or any backlog, signals arrive hours
after they happened, so a `now - 30m` cutoff excludes all of them, every signal
looks like it has no neighbours, and nothing is ever a candidate. The failure is
invisible — no error, just a board of unlinked cards. This bit MuggleBot once; the
hierarchy reduces how much rides on it, but the residual Slack-to-Slack candidacy
still depends on getting it right.

### 4. Notification dedup — once per subject state change

The macOS notifier fires on a *subject* transition, not per signal:
`last_notified: Option<(when, severity)>` lives in the object's state and is
updated in the same exclusive handler that raised the severity. Because that
handler is serialized per key, "notify once" needs no separate deduplication
table — the thing that decides to notify and the thing that records having
notified are the same critical section.

### Human overrides (pins) and re-analysis

You are the authority. From any subject you can **associate** (mark related),
**merge** (mark same / duplicate), or **split** (dissociate signals the model
wrongly attributed). Each override is stored as a **pinned edge** — provenance
`user`, not `llm` — and pins always win.

Changing a pin **re-runs the analysis** for the affected subjects, with the pins
supplied as hard constraints ("the user says A and B are the same and C is
unrelated — reconcile everything else around that"). The model completes the
graph without contradicting your pins, so a correction propagates rather than
being silently re-overwritten on the next pass.

A split is the one override that can contradict the hierarchy — it says "this CI
run does not belong to that PR". It is recorded as a per-signal attribution
override so re-ingest of the same signal does not undo your correction.

---

## Restate workflows — the multi-step work that updates subjects

Objects hold state and serialize writes. **Workflows** are for the expensive,
multi-step, resumable pipelines whose *results* land in a subject: exactly-once
per workflow id, journalled step by step, and interactive while they run.

| Workflow | Key | What it does |
|---|---|---|
| `IssueTriage` | `{owner}/{repo}#{n}@{sha}[#a{attempt}]` | check out the code, select files, characterize, propose N approaches, render plain English |
| `RootCause` | `{subject}@{watermark}` | symptoms → route → issues/PRs → commit log → rank → code search |
| `PrCritique` | `{owner}/{repo}!{n}@{sha}` | read the diff and reviews, judge whether it fixes the issue |
| `BrowserRead` | `{investigation-id}` | drive the authenticated Chrome, read the dashboard, file evidence |
| `RepoIndex` | `{org}@{bucket}` | fan out over the org's repos, distil an index card per repo |
| `Merge` | `{a}+{b}` | collapse two subjects: re-attribute signals, rewrite edges, carry artifacts |
| `ContextIngest` | `{context-id}@{etag\|mtime}` | fetch → normalize → summarize → embed → store |
| `Explain` | `{subject}@{watermark}+{critiques}` | distil a subject *and everything under it* into something readable |

The `RepoIndexer` object (one per repo) drives the code index — see _The code index_. It's an
object rather than a workflow because indexing is recurring, resumable, and needs a cursor;
a workflow instance per batch would be lifecycle for nothing.

Each ends by calling back into the subject: `Issue::apply_artifact(ArtifactRef)`.
The workflow writes the bulky output to SQLite and hands the object a pointer and
a freshness stamp — which is what keeps object state small and the board's "has
the AI looked at this?" strip honest.

### Why these are workflows and ingest is not

`Ingest` is a plain **Service**. It is high-frequency and single-purpose: one
event in, one subject resolution, one send to the object. Modelling it as a
workflow would mint an instance per event, each with its own retention and
lifecycle, for no gain — the exactly-once property it needs comes from the
ingress idempotency key, not from workflow identity.

The seven above are workflows because each has the three properties that make a
workflow worth its instance: **multiple expensive steps** (so resuming mid-way is
worth real money and minutes), **a natural once-per-subject identity**, and — for
some — **a need to be interacted with while running**.

### Keys chosen so that re-running is free

`IssueTriage` keyed on `{issue}@{sha}` is the sharpest example. Restate refuses a
second submission of the same workflow id and lets you attach to the first
result, so "re-triage an issue whose code hasn't moved" *is* a key collision —
you get the previous analysis back, instantly, without a model call. That is
precisely the logic the reasoner cache currently reproduces by comparing the
commit it's about to read against the commit the last analysis read. New commit,
new key, real work. The `#a{attempt}` suffix exists for the one case that must
bypass it: you explicitly asked for a redo on unchanged code.

`RootCause` keyed on `{subject}@{watermark}` (the latest attributed signal id)
gets the same property: nothing new has arrived, nothing to re-investigate.

### Durable steps and the money they save

Every step that talks to the outside — a GitHub request, a `git fetch`, an Ollama
completion, a `claude -p` subprocess — is wrapped in `ctx.run`, which is both the
determinism requirement and the point:

```rust
#[restate_sdk::workflow]
impl RootCause {
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>, req: RootCauseReq)
        -> Result<Report, HandlerError>
    {
        let symptoms = ctx.run(|| extract_symptoms(&req)).await?;
        let repos    = ctx.run(|| route(&symptoms)).await?;          // local model
        let existing = ctx.run(|| search_issues_and_prs(&repos)).await?;
        let commits  = ctx.run(|| commit_log_before(&req.earliest)).await?;
        let short    = ctx.run(|| shortlist(&existing, &commits)).await?; // local
        let ranked   = ctx.run(|| rank(&short)).await?;              // local, over the shortlist
        ctx.object_client::<IssueClient>(req.subject)
            .apply_artifact(store(&ranked)?)
            .send();
        Ok(ranked)
    }
}
```

A 403 from GitHub at step 3 today restarts the whole investigation, re-cloning and redoing
steps 1–2. Journalled, the retry resumes at step 3. Each of those steps is a local model pass
over a checkout, so the saving is minutes of GPU per flake rather than dollars — the argument
for journalling them didn't depend on the bill.

Error classification is explicit, because Restate retries by default and forever:
rate limits, timeouts, and connection resets are **transient** (retry with the
configured policy); 404, "repo not found", revoked-token 401, and "the model
returned unparseable JSON three times" are **`TerminalError`** — they fail the
invocation and surface on the subject rather than hammering a dead endpoint for a
week.

### Debounce as a durable timer, not a task

Live-assist re-analysis is not a workflow — it belongs to the subject, repeats
indefinitely, and is defined by state the object already holds. So it's the
canonical delayed-self-send pattern:

```rust
// exclusive handler on Issue
async fn record(&self, ctx: ObjectContext<'_>, sig: SignalRef) -> Result<(), HandlerError> {
    // ... update counters, severity, links ...
    let now = sig.occurred_at;
    let cap = ctx.get_state::<DateTime<Utc>>("first_activity").await?.unwrap_or(now)
        + Duration::minutes(5);                       // hard cap
    let fire = std::cmp::min(now + Duration::minutes(1), cap);
    ctx.set_state("debounce_deadline", &fire);
    ctx.object_send_client::<IssueClient>(ctx.key())
        .analyze()
        .send_after(fire - now);                      // durable timer
    Ok(())
}

async fn analyze(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
    let due: Option<DateTime<Utc>> = ctx.get_state("debounce_deadline").await?;
    // A later record() moved the deadline and armed its own timer — this one is stale.
    if due.is_some_and(|d| d > ctx.now()) { return Ok(()); }
    ctx.clear_state("debounce_deadline");
    // ... the pass ...
}
```

The current in-process timer loses every pending re-analysis on restart — which,
during a `tilt up` rebuild cycle, is most of them. The durable timer doesn't, and
the coalescing check is safe precisely because both handlers are exclusive on the
same key.

### Watchers as objects, sockets as tasks

Polling is a recurring task, so each poller becomes a **`Watcher` virtual
object** keyed by source name (`github-notifications`, `github-assigned`,
`slack-history`, `granola`), with `poll()` as an exclusive handler that ends by
scheduling itself:

- The cursor / ETag is object state, not a table row plus an in-memory copy.
- Exclusivity is the guarantee that two polls never overlap — which today is
  implicit in the shape of the loop and breaks the moment you add a "poll now"
  button to the UI. Here, "poll now" is just a call, and it queues behind the
  scheduled one instead of double-reading the cursor.
- The cadence is a durable timer, so a restart doesn't skip a beat, and adaptive
  backoff on `X-RateLimit-*` / `Retry-After` is a longer `send_after` rather than
  a sleeping task.
- One caveat from Restate's own cron guidance: a failing poll being retried while
  the next tick arrives can overlap. Exclusivity serializes them, and the stale
  check above (`if the cursor already moved past this tick, return`) makes the
  second one a no-op.

**Long-lived connections stay in the daemon.** Slack Socket Mode is a persistent
websocket and cannot be a poll handler; the UI's WebSocket server and the MCP
stdio transport are likewise the daemon's. They are *clients* of the ingress: the
socket task's only job is to submit each event with an idempotency key and get
back to reading the socket. The boundary is worth being explicit about — Restate
does the work, the daemon owns the sockets and the local surfaces.

---

## vqueues — bounding what a laptop can actually do

Restate 1.7's flow control gives concurrency limits scoped by a **scope** key,
with invocations held in a virtual queue until a slot frees. For an always-on
agent on one machine, this replaces four separate hand-rolled throttles:

| Scope | Limit | Why |
|---|---|---|
| `local-llm` | 1–2 | There is one Ollama and one GPU. A 33B model with four concurrent requests is slower *and* worse than a queue of one. This is the single strongest fit. |
| `cloud-llm` | 3 | Every concurrent invocation is real money. Limit keys per tier (`sonnet`, `opus`) bound the expensive one separately. |
| `github` | 4 | Keep burst concurrency under the API's tolerance; the repo-index fan-out over an org is otherwise a self-inflicted rate limit. |
| `browser` | 1 | One Chrome, one investigation at a time. Replaces the claim-a-row-and-hope worker loop. |
| `checkout` | 2, limit key per repo | Two clones at once is disk-bound; two clones *of the same repo* is a corrupt working tree. |
| `repo-index` | 1 | One org crawl at a time. Its own scope rather than `github`, because the hazard is two of *itself*: two crawls enumerate the same repos, pick the same uncarded ones in the same order, and clone them into the same directory. A shared API-burst scope doesn't express that. |

### The GitHub budget is a rate, and the vqueue is not

This was written as a caveat and then learned the hard way: a 5000-request-per-hour budget is
not protected by a concurrency limit. With 147 repo indexers ticking every 30 seconds and up to
twelve API calls each — `repo_checkout_info`, a history page, and one `commit_files` per commit
in the batch — the burn is roughly **200,000 requests an hour**, forty times the budget. A
`github` scope of 4 bounds how many of those run at once and nothing else. The result was a 403.

No cadence would have fixed it either: even one call per tick is 17,000 an hour. So the client
carries a **process-wide budget**, learned from `X-RateLimit-Remaining` / `-Reset` on every
response — global because the budget belongs to the *token*, and a per-client limiter would let
each of the three clients believe it had the whole thing, which is the arithmetic that produced
the 403 in the first place.

Two mechanisms on top of it:

**Callers are split by priority.** Watchers and operator actions are `Interactive`: never paced,
never refused. Indexing and org crawling are `Background`. If background work drains the budget
then notification polling stops, and an ops agent that has stopped noticing incidents is broken
in a way that a late index simply is not — so 1000 requests are held in reserve, and background
callers are refused once that is all that remains. The refusal says when it lifts, because a
message that reads as permanent sends you looking for a bug.

**Background pacing is self-correcting.** Spacing is `time until reset ÷ requests spendable`, so
headroom costs nothing and a nearly-spent budget slows to a crawl rather than hitting a wall.
Nothing needs to know how many indexers exist, which matters because that number is the org's
repo count.

One distinction worth keeping: a **403 is not automatically a rate limit**. It is also how a
missing scope and a SAML block arrive, and parking every background call for an hour over a
permissions problem would be a self-inflicted outage. The exhausted-budget header, or a
`Retry-After`, is what separates them.

> **`[restate.limits]` does nothing unless `[restate] vqueues = true`.** They are separate
> settings, and a tuned limits block with the flag off is a config that reads as configured
> and behaves as unconfigured — measured live, with every limit inert and four concurrent org
> crawls racing. The daemon now warns at boot when it finds a customized limits block and the
> flag off, naming the one line that fixes it.
>
> Because of that, a vqueue is never the *only* thing standing between the system and a
> destructive race. Where the failure would be corruption rather than slowness — the org
> crawl's shared checkout — the work is *also* sized so a single instance finishes inside the
> cadence that submits the next one. Belt as well as braces.

```rust
ctx.workflow_client::<RootCauseClient>(key)
    .run(req)
    .scope("cloud-llm")
    .limit_key("opus")
    .call()
    .await?;
```

```bash
restate rules set "*"          --concurrency 32 --description "global default"
restate rules set "local-llm"  --concurrency 1
restate rules set "cloud-llm"  --concurrency 3
restate rules set "github"     --concurrency 4
restate rules set "browser"    --concurrency 1
restate rules list --extra
```

Three caveats, all of which shape the local setup:

1. **Experimental in 1.7.** Requires `RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true`
   and `RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7=true`.
2. **Fresh cluster only.** vqueues cannot be enabled on a cluster with in-flight
   invocations, so turning them on means wiping `data/restate`. That is
   acceptable here *only because* the record lives in SQLite — see the storage
   rule below. It's also why the Tiltfile documents the wipe rather than hiding
   it.
3. **Concurrency is not rate.** GitHub's limit is requests per hour, which a
   concurrency cap doesn't express. The token-bucket virtual object pattern
   covers that: a `GithubBudget` object keyed by token, holding the remaining
   quota reported by `X-RateLimit-Remaining`, that callers `reserve()` against.
   vqueues smooth the burst; the bucket enforces the hour.

Observability comes free: `sys_vqueues`, `sys_vqueue_meta`, `sys_rules`,
`sys_scheduler`, and `sys_user_limits` are queryable over the admin SQL
interface, which is how "why is nothing being analyzed?" gets answered without
adding logging.

---

## What lives where — Restate state vs SQLite

One rule, and everything follows from it:

> **Restate holds the work in flight. SQLite holds the record.**

**In Restate:** invocation journals, virtual-object state (the sketch above),
durable timers, workflow promises and signals, vqueue occupancy. All of it small,
all of it hot, all of it reconstructible-or-expendable.

**In SQLite:** the signal log with raw payloads, comments, artifacts (triage
proposals, root-cause reports, browser readings, PR critiques), embeddings,
memory, the context library, the tag vocabulary, the reasoner completion cache,
the audit log, the relation-edge table, and **secrets**.

The division is not aesthetic. Three concrete reasons:

1. **The board is a cross-key query.** "Every subject needing attention, ranked,
   filtered by source and severity, matching this text" is a `SELECT`, and it is
   cheaper as one. Restate *does* expose a cross-key `state` table over the same
   Datafusion endpoint as `sys_invocation` — an earlier version of this document
   claimed otherwise, which was wrong and is corrected in
   [`restate/state.rs`](src/restate/state.rs) — but every read of it is HTTP plus a
   scan. Right for a panel that repaints every few seconds; wrong for the ranked,
   filtered, text-matched board. So the projection the UI reads is a SQLite table
   that subject handlers write to.
2. **Journals should carry ids, not bodies.** A 200KB raw notification payload
   passed between handlers is 200KB in the journal, replayed on every retry. So
   handler payloads are keys and `SignalRef`s; the body is read from SQLite
   inside `ctx.run`. Restate state is not a blob store, and treating it as one
   is how a laptop-scale deployment gets slow.
3. **A Restate wipe must be survivable.** Enabling vqueues requires a fresh
   cluster; debugging occasionally wants one. That has to cost at most the
   in-flight work — never a signal, never a memory, never a token. This is the
   property that makes the experimental feature usable at all.

The corollary is that the two must be reconciled rather than assumed consistent:
a subject's `signal_count` is a hint for ranking, and the authoritative count is
the query. On boot, subjects are lazily re-hydrated from SQLite on first access,
so a wiped cluster refills as the board is read rather than needing a migration
step.

### A diff on the object — where the rule bends, and why

A pull request's summarized diff lives in the `PullRequest` object's state, not in SQLite,
and it is the one place a handler payload deliberately carries a body rather than an id.
Both departures are on purpose.

It was on-demand and never kept, on the reasoning that a diff is one API call plus a model
pass and eagerly diffing 147 repositories would spend the budget the watchers depend on.
That was right about **what not to diff** and wrong about **what not to keep**. The set that
matters is the pull requests the operator is actually in — tens — and for those the diff is
read over and over: from the PR's card, from the issue it attempts, and again after clicking
in. Each read paid the same call and the same model pass for an answer that had not moved,
and clicking into an issue lost the attempt altogether.

- **Why object state and not a table.** The diff is a fact about one pull request, always
  read by that key, and it is *derived* — a Restate wipe costing it means one re-read, which
  is exactly the "in-flight or expendable" test. A shared handler answering from state is a
  single ingress round trip (~40ms measured), which is what makes it cheap enough to open the
  pane on render for every attempt on an issue.
- **Why the payload is the body.** `put_diff` ships the report itself, because the report is
  the fact being stored and there is nowhere else it lives. What keeps that honest is a
  bound: `prdiff::trim_for_state` keeps every file's path and counts — so the collapsed pane
  and the totals stay correct — and keeps patch text only up to a budget. A file whose patch
  was dropped says "not stored" rather than showing the same "—" as a binary file, because
  those are different facts and one of them is about the change.
- **Freshness without another call.** `PrDiff` is keyed `{pr}@{watermark}`, so a PR nothing
  has happened to is a refused submission rather than a second read. A PR that is *not* a
  subject (an attempt the PR-fix finder turned up) has no watermark, and would otherwise be
  read once and never again — for those the token is the judgment's own `updated_at`, which
  moves when the finder re-judges. Neither costs an API call, which a head sha would.
- **What the operator sees.** The pane opens itself from state (`stored_only`, which fetches
  nothing), falls back to a button for a PR nothing is stored for — the first read costs what
  it always cost, and it is the last time — and offers RE-READ with the age of what it has,
  because a force-push notifies nobody and so cannot move the watermark.
- **Folded on the board, unfolded in the click-in view.** A card is a row in a list of
  subjects, so there the diff is a file list at scanning density. Clicking in is the opposite
  request: the patches are the substance of the page, at a size a unified diff is legible at,
  and hiding them behind a disclosure triangle hides the answer the view exists to give. The
  one exception is a PR **closed without merging** — a dead end, worth listing and not worth
  the screen — which is why `github::PullRequest` carries `merged_at`: GitHub reports merged
  and abandoned alike as `state: "closed"`, and those are nearly opposite facts. An unknown
  state counts as outstanding, because the cost of showing a change is scrolling and the cost
  of hiding one is missing what happened.

### Reviewing the change, not explaining it

The diff summary says what a change does. That is useful and it is not a review: it leaves the
reader to decide whether the change is good, which is the judgment they wanted help with. So
each pull request also gets a **review** — a recommendation (`approve` / `comment` /
`request_changes`), the note you would write above the Approve button, and inline comments
anchored to lines of the patch. It is stored beside the diff on the `PullRequest` object, and
like every critique here it is **never posted to GitHub**.

It reviews the code and says nothing about who wrote it. Most pull requests on this board are
the operator's own, and a review that goes easy on those is worthless exactly where it is read
most.

**Findings first, verdict second.** Asked for both in one pass, the on-device model returned
`approve` with an empty comment list and a rationale that restated the title — "a good
improvement to the system" on an eighteen-file feature. That is the explainer problem wearing a
verdict, and it happens because one prompt asks a small model to read a large diff, decide, and
justify at once. Now each batch of files is reviewed for *findings only*, with no verdict to
reach for, and a final pass decides the recommendation **from the findings**. Two consequences
worth keeping:

- Batches are largest-file-first and capped (`MAX_REVIEW_BATCHES`), so a review that must
  truncate drops the one-line files rather than the seven-thousand-character one.
- The findings are the evidence, so they override the verdict, **both ways**
  (`prdiff::reconcile`). A blocker means the verdict is `request_changes` whatever the model
  labelled it; *no* blocker demotes `request_changes` to `comment`. The second direction is not
  symmetry for its own sake — a real pull request enabling Linkerd HA correctly came back
  `request_changes` on two `concern` findings whose substance was "not recommended in a
  production environment", which is an appeal to a norm rather than to anything in the diff. A
  verdict that blocks a merge has to rest on a finding that says something *is* wrong, or it is
  the generic-advice failure again wearing a stronger word. Nits and praise never block and
  never prevent an approval.

**Claims the diff cannot support are discarded.** The first real run produced five `blocker`
findings of the form "not used anywhere in the codebase", every one of them wrong, about symbols
the diff had just introduced. A review of a few hunks cannot see callers, exports, tests, or the
rest of the file. Hardening the prompt was not enough on its own — told not to say "not used
anywhere", the model said "not used in the file" and returned the same five — so
`prdiff::unverifiable` drops the whole claim shape deterministically, the same way
`explain::verify` strips claims the dossier cannot support. The cost is accepted: a genuinely
unused parameter, fully visible in the diff, is dropped along with the guesses. A false blocker
is the one finding a reader acts on.

**Where the diff shows up.** Any pull request in view carries its diff, and there are three ways
one gets there: the subject *is* a PR, the view's `pull_requests` lists it, or a triage pass
turned it up as an attempt on an issue. A PR subject is the case that was silently broken —
`pull_requests` is keyed by the issue a PR attempts, so a PR is never listed under itself, and a
PR subject's own diff had been read, stored, and then rendered nowhere. Its own diff now leads
the click-in view under THE CHANGE, unfolded; the attempt sources are unioned and deduped so
"any PR means show me the diff" holds whichever one has it.

**Anchoring.** A comment carries the line it is about, copied verbatim, and the backend resolves
that to an index into the patch. The quoted line is tried before any line *number* the model
offered, because a model copying a line it is looking at is usually exact and the same model
counting positions in a hunk is often off by a few. Unresolvable notes render at file level —
guessing a line would attach a confident comment to the wrong code, which is the only failure
mode worse than having no line.

**It runs in the background.** Several model passes over the patches measured at ~5 minutes on
an eighteen-file change, so the diff returns immediately and the review is submitted as a
workflow. The pane polls object state (~40ms) while a review is outstanding and stops the moment
it lands; the dispatch strip shows the pass in flight. A `refresh` must force a *new* workflow
key — the first version reused the spent one, so a re-read came back with a fresh diff and
yesterday's verdict.

**How this differs from the PR *critique*.** `PrFixFinder` answers "does this pull request fix
_that issue_" — a question about a relationship, used to tell you somebody is already on it. The
review answers "should this land", about the code alone. Neither replaces the other, and only
the review takes a position on merging.

### Secrets in SQLite

Credentials — GitHub token, Slack app + bot tokens, Granola key, Anthropic /
OpenAI keys, per-context-source authenticated-fetch tokens — live in a `secrets`
table in the same SQLite file as everything else, managed through the WebUI config
page. The TOML holds no secrets and never did.

**Why not the macOS Keychain**, which earlier drafts of this document specified
(and which the code never implemented — the `credentials` table has been the real
store all along). The Keychain is scoped to a logged-in user session and to the
identity of the process asking. That
was fine when exactly one signed binary in one login session needed the token. It
stops being fine now: the service endpoint may be served by a process started by
Tilt, by a test harness, or (later) from a container, and every one of those either
prompts, or fails with a code-signing error, or works only in the session where a
human clicked Allow. A credential store that a background daemon can't read
without a GUI prompt is not a credential store.

And the honest version of the security argument: **the database already holds
every signal body MuggleBot has ever ingested** — your Slack DMs, private issue
contents, alert payloads, meeting transcripts. Guarding the token in a stronger
vault than the data it fetched is theatre. The file is the sensitive artifact
either way; protect the file.

So:

- One file, mode `0600`, in `data_dir`, on a FileVault volume. Same threat model
  as `~/.aws/credentials`, `~/.netrc`, or `gh`'s `hosts.yml`.
- **Optional envelope encryption**, off by default: with
  `[secrets] encrypt = true`, values are sealed under a key derived from
  `$MUGGLEBOT_MASTER_KEY`. This is a real improvement against a stolen backup and
  no improvement at all against a process running as you, so it's a choice rather
  than a default that implies more than it delivers.
- **Write-only over every API.** The config page and MCP can set a secret and can
  read *whether* one is set and when it changed; nothing returns a value.
  `config://redacted` stays redacted, and the tracing layer scrubs known secret
  names.
- **Read at use time, not at boot,** so rotating a token takes effect on the next
  poll without a restart.
- **No secret crosses the Restate boundary — ever.** Not in a handler argument,
  not in object state, not in a `ctx.run` return value. Anything that enters a
  journal is persisted in `restate-data`, rendered in the Restate UI, and visible
  in `sys_invocations`. Handlers take a *credential name*; the fetch inside
  `ctx.run` resolves it from SQLite and the token never leaves the stack frame.
  This is the one rule in the document with no exceptions.

---

## Signal sources (watchers)

Each source has a dedicated watcher that authenticates, subscribes or polls, and
emits normalized `Signal`s into the ingress. Watchers are independent and
fault-isolated: one source being down or rate-limited must not stall the others —
which is now structural, since each is its own object with its own timer and its
own retry state.

| Source | What we watch | Transport |
|---|---|---|
| **GitHub** | Notifications feed: review requests, mentions, assigned issues/PRs, CI/check failures, thread replies | REST notifications API + conditional polling; GraphQL for enrichment |
| **Slack** | DMs, @-mentions, keyword hits, watched channels, and **alert channels** — designated channels whose posts are treated as alerts (higher base severity) | Socket Mode / Events API |
| **Granola** | Meeting notes & transcripts → extracted action items, decisions, owners | Granola API (poll) |

Watcher contract:

- Normalize into the common `Signal` — no source-specific types leak past ingest.
- Submit through the ingress with the idempotency key above. Idempotence is the
  ingress's job now, not a hand-rolled check.
- Keep the cursor/ETag in object state so a restart resumes without gaps or
  replays.
- Separate the HTTP `poll` from a pure `normalize_*` function that's unit-tested —
  the normalizer must be testable without Restate or a network.
- Degrade gracefully and report health (see MCP `source_health`).

### The normalized `Signal`

The whole system speaks one type:

```rust
struct Signal {
    id: SignalId,                 // internal, stable
    source: Source,               // GitHub | Slack | Granola
    external_id: String,          // upstream id
    version: Option<String>,      // updated_at / edited_ts — completes the dedup key
    kind: SignalKind,             // ReviewRequested | Mention | Alert | CiFailure | ...
    title: String,
    body: Option<String>,
    url: Option<Url>,             // deep-link back to the source
    actor: Option<Actor>,         // who caused it
    keys: Vec<ResolutionKey>,     // issue, pr, branch, commit, repo, env, service, channel, person
    subject: Option<SubjectKey>,  // where attribution landed; None → unattributed lane
    severity: Severity,           // Info | Notice | Warning | Critical
    occurred_at: DateTime<Utc>,
    ingested_at: DateTime<Utc>,
    raw: serde_json::Value,       // original payload, for audit/enrichment
    tags: Vec<String>,            // routing tags (Slack messages classified per-signal)
}
```

`keys` are the currency of attribution — the ranked climb reads them. `subject`
is the outcome. Note what's gone: the per-signal `state` machine. Handled-ness is
a property of the *subject* (`handled`), because acknowledging half of a PR's CI
failures was never a coherent thing to express, and "a subject is only as handled
as its least-handled member" was a rule invented to paper over that.

---

## Correlation & intelligence

Reasoning happens in two tiers so the cheap, deterministic work never waits on a
model, and the model is only asked to reason when it adds value.

1. **Deterministic attribution (always on, in-process).** The hierarchy climb
   above. Fast, explainable, no LLM.
2. **Semantic reasoning (on demand / via LLM).** For "what is actually going on
   here and what should I look at first," MuggleBot delegates to an LLM through a
   single internal `Reasoner` trait, backed by any of three provider kinds.

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
   - **Which is why the default is Ollama for everything but one pass.** Every automatic
     pass except assigned-issue triage is on-device, so no signal data leaves the machine
     unless the operator asks a cloud model a question themselves — the chat picker or the
     second-opinion button. The CLI bridge is how those calls happen when they do, and it
     is also what triage rides: unmetered, but off the machine. Triage is the one place
     the queueing argument beat the privacy one; see the triage section for why. Interactive deep-dive is the
     MCP-server path, where your Claude/ChatGPT client connects to MuggleBot; there the
     data leaves because you are the one asking.

Every reasoner call is a `ctx.run` step inside a handler, scoped to a vqueue. The
consequence worth naming: a model call that succeeded is journalled, so a retry
of the surrounding handler never re-pays for it. That is a *second* cache layer
below the completion cache — the journal stops a retry from re-paying, the SQLite
cache stops a *new* invocation from re-paying.

### What the board reports: attention, and whether the AI looked

Handled-ness (`Open` / `Seen` / `Acked` / `Snoozed` / `Resolved`) is bookkeeping.
It records what you *did*, which is not what you want to read at a glance. So the
board leads with the two questions that actually get asked:

1. **Does this need me?** One derived boolean plus a short reason — critical,
   warning, a live-assist flag on something you said, you're personally in the
   subject, or it's assigned to you and not yet triaged. Handled work never asks.
2. **Has the AI been over it, and at whose expense?** A strip of per-facet
   indicators — tags, summary, dashboard read, root cause, code
   triage, PRs judged — filled when the artifact exists and hollow when it
   doesn't. A row of hollow pills is the "nobody has looked at this yet" signal,
   which is otherwise only discoverable by opening the subject and finding empty
   panels.

Alongside them, the work is attributed by where it ran: `⌂n` for passes on this machine and
`☁n` for cloud calls. Under local-by-default the second number should be **zero** unless
somebody pressed 2ND OPINION, which makes the indicator worth more than it was when it merely
tracked a bill: a non-zero `☁` nobody expected means something is escalating that shouldn't
be.

Both are **derived, never stored**. A stored "needs attention" flag drifts the
moment a subject is acknowledged elsewhere, and a stored "AI done" flag lies
after a failed pass. Deriving them from the artifacts that actually exist means
the badge cannot disagree with the panel underneath it. `Issue::attention()` is a
*shared* handler computing the verdict from state plus the artifact refs — cheap,
concurrent, and impossible to leave stale.

### What the AI is doing right now — the dispatch strip

The decoration indicators above answer "has the AI been over it". They cannot answer
"is it going over it *now*", and the difference is a button press.

Every expensive pass is a workflow submitted with `send`, so the tool call returns as
soon as the ingress accepts it — long before the work runs. That is the right shape (a
root-cause investigation takes minutes; an HTTP request held open for one makes the UI
hostage to it), but it left three outcomes indistinguishable:

- **queued** behind a vqueue concurrency limit, about to run;
- **refused as a duplicate**, because this exact key already ran — which is what makes
  pressing a button twice free, and is a *success*;
- **failed**, at the ingress or inside the handler.

All three looked the same from the board: the button flashed and nothing changed. The
first live check of the strip found both of the invisible ones immediately — an
INVESTIGATE on a PR whose report already existed (refused key, answer already on
screen) and one on an acknowledged subject (`handled subjects are not investigated`, a
terminal error nothing had ever surfaced).

So [`dispatch`](src/dispatch.rs) is a **process-wide registry**, pushed over the same
WebSocket as everything else and rendered as a strip above the detail panels, plus one
badge per board card. Two properties it needs that per-caller state cannot give:

1. **The submitter and the runner are different call stacks.** `Ingress::submit_workflow`
   records `Queued` (or `Duplicate`, or a submit failure); the workflow handler, running
   minutes later under Restate, records `Running` then `Done`/`Failed` through
   `workflows::tracked`. They share a process — the SDK endpoint is served by this
   binary — and nothing else.
2. **A failure has to outlive the thing that failed.** A workflow that dies on a
   terminal error leaves no artifact anywhere, which is precisely why the error has to
   be held somewhere that isn't the artifact.

It is deliberately in memory and deliberately lossy: a liveness display, not an audit
trail. The invocation journal is the durable record, and `list_workflows` reads it back
from Restate, which is the authority. Two rules the display has to keep to stay honest:

- **A duplicate submission never downgrades live work.** A re-submit while the first
  invocation is still running answers `PreviouslyAccepted`, and showing that as "already
  done" tells the operator the opposite of the truth. Observed within a minute of turning
  this on: the boot sweep submits a triage, a catch-up tick re-submits it thirty seconds
  later.
- **In-flight rows are never evicted by retention.** Only finished ones are, so a strip
  under pressure loses history rather than claiming work stopped.

### Per-subject context

Beyond the global context library, you can attach **ad-hoc context to a single
subject** — free text ("third time this quarter") or a URL. Text is used as-is; a
URL runs through the same `ContextIngest` workflow as the context library.
Attaching or editing context is another trigger that re-arms the debounce, and
the attached context is fed into that subject's reasoning prompt and cited like
any other evidence.

Every correlation and every suggestion **cites the signals it's built from**. A
summary that can't point at its evidence is a bug, not a feature.

---

## Memory & context library

Two curated, SQLite-backed stores give the reasoner background beyond the live
signal stream. Both are summarized on ingest, embedded for semantic recall, and
exposed over MCP — so an interactive Claude/ChatGPT session reasons over the same
grounding MuggleBot uses ambiently.

**Memory** — what MuggleBot has learned or been told: lessons from past
incidents, corrections, confirmed approaches ("a spike in X usually means Y").
Written by MuggleBot (postmortem-assist) and by you, and **fully editable** —
browse / add / edit / delete through the WebUI memory editor and the MCP memory
tools. One entry = one fact with a one-line summary; entries link back to the
signals or subjects they came from.

**Context library** — external reference material you curate so MuggleBot starts
with the background a new teammate would read: runbooks, architecture docs, the
on-call/observability guide, service catalogs, status pages. Three source kinds:

- **URL** — fetched, summarized, stored, and **refreshed on a schedule**. Each
  source is a `ContextSource` virtual object whose refresh cadence is a durable
  timer, so a laptop asleep for a weekend resumes its schedule rather than losing
  it. Refresh honors `ETag` / `Last-Modified` to skip unchanged pages; on a real
  change `ContextIngest` re-summarizes, re-embeds, and (optionally) emits a
  low-severity "context changed" signal. **Authenticated URLs** name a credential
  in the `secrets` table, resolved inside the fetch step and sent as a header.
- **File** — a local path (Markdown, text, PDF); same pipeline, re-ingested when
  its mtime changes.
- **Managed directory** — files under `<data_dir>/contexts/<tag>/…` are ingested
  automatically: each immediate sub-directory names an automatic (pinned) tag, so
  dropping a runbook into `contexts/database/` files it under `database` with no
  LLM pass. Files reload on mtime change, and entries whose backing file
  disappears are dropped.

**Shared ingest pipeline** (`ContextIngest`, keyed on `{id}@{etag|mtime}` so an
unchanged source is a free no-op): fetch/read → normalize to text → summarize via
the reasoner → embed → store as
`{raw, summary, tags, source, fetched_at, etag|mtime, refresh_interval}`.

### Tags — categorical routing

Both stores (and subjects and signals) carry **tags** drawn from one shared
**vocabulary** — a `{name, summary}` registry where the summary is the
description the classifier reads. Tags are the categorical complement to vector
similarity:

- **Assigned** on ingest by a two-tier auto-tagger (a cheap pass proposes tags, a
  heavy pass refines), or pinned by hand (folder tags, `tag_context` /
  `tag_memory` / `set_subject_tags`). Human-pinned tags are never overwritten.
- **Summaries** for automatically-created tags are backfilled once by an LLM pass
  over the content filed under them; thereafter they're edited by hand via
  `edit_tag`. Vocabulary hygiene — `merge_tags` (also renames) and `delete_tag`
  (strips the label from all content) — keeps near-duplicates in check.
- **Classification.** When a subject lights up it is classified into the
  vocabulary (LLM with a deterministic substring fallback); every **Slack**
  message is additionally classified per-signal at ingest. Classification is
  skipped while the vocabulary is empty.

**How grounding is used:** when a subject lights up, MuggleBot folds in the most
relevant memory + context entries — **tag-matched entries first** (the
categorical routing), then a vector-similarity fill for the rest of the budget —
with citations back to the source URL/file. Tag-matched entries contribute a
bounded excerpt of the actual body (a runbook's steps, a memory's full fact), not
just the summary, so precision doesn't cost fidelity. The same retrieval backs
the MCP `search_memory` / `search_context` tools, so the grounding is identical
whether reasoning happens ambiently or in your interactive session.

---

## Live assist

Subjects you're actively in get closer attention. This needs MuggleBot to know
your own Slack identity (`user_id`) so it can tell your messages apart from
everyone else's.

**Trigger & debounce.** Any interaction in a watched or alert conversation marks
its subject _live_ and arms the durable debounce described above — **1 minute
after the last activity, with a 5-minute hard cap** so a fast-moving thread still
gets analyzed. When the newest activity is one of your own messages, the
correctness/risk check is prioritized within that window.

**What a pass produces**, grounded in memory + the context library and cited:

- **hints** — the runbook that applies, a relevant past incident, a related
  subject you may not have connected.
- **suggestions** — a sensible next step, grounded in the runbooks and past
  incidents attached to the subject. A pass with no such grounding says nothing
  rather than falling back on generic advice.
- **flags on your own messages** — `factual_error` or `risky_action`, each with a
  rationale, a citation, and a confidence.

**Red-alert.** A high-confidence flag — you've said something the grounding
contradicts, or proposed a risky/irreversible action — flips the LCARS UI into
**red-alert mode** and fires a **Critical macOS notification**. It is strictly
advisory: it warns and cites, it never edits or sends anything. (MuggleBot sees
Slack messages only _after_ they post — it can't intercept the compose box — so
this is "you just said X, but runbook Y says otherwise," in time to correct
yourself.) Dismiss or mark false-positive from the notification or the subject;
false-positives feed back to memory so the same thing isn't re-flagged.

Tuning lives in a `[live]` block: debounce window, red-alert on/off, and the
minimum confidence to escalate.

## Agent chat

An interactive, **multimodal** chat panel in the WebUI where you talk to
MuggleBot directly and **drop screenshots, images, logs, or files** for it to
work from. The agent reasons over everything MuggleBot already holds — the live
board, signals, subjects, memory, and the context library — through the same tool
surface as the MCP server, so "what's going on with `service-foo`?" and "here's a
screenshot of this dashboard — does it match the alert in `#alerts`?" both work.

Chat routes to the heavy reasoner (Claude), which handles vision for dropped
images. It's the built-in counterpart to the MCP-server path — same grounding and
tools, but you don't need an external Claude/ChatGPT client. Anything useful that
surfaces in chat can be saved to memory or attached to a subject as context in one
action.

---

## Browser investigation — reading the dashboard behind the link

A Slack alert that links to a Grafana panel carries almost none of its own
evidence. "CPU high on api" is the notification; the *numbers* — the error rate,
the saturation curve, the time range, the deploy marker — live on a page behind
SSO. Correlating alert text alone means reasoning about a symptom nobody has
actually looked at.

So MuggleBot looks. When an ingested signal links to a URL matching
`[browser].url_patterns`, a **`BrowserRead` workflow** is submitted in the
`browser` vqueue scope — concurrency 1, because there is one Chrome. It drives the
operator's **already signed-in Chrome** over the DevTools Protocol, then files
what it saw onto the subject as evidence, cited `[browser:ID]`.

The queue is the notable simplification: the current design hand-rolls a claim-a-
row worker loop with a status column to serialize access to the shared browser. A
vqueue of one expresses the same constraint declaratively, and a crashed
investigation releases its slot rather than leaving a row claimed forever.

**How the browser is actually driven — and what doesn't work.** MuggleBot spawns
its existing agent CLI bridge (`claude -p`) with a browser MCP server
(`chrome-devtools-mcp`) attached over stdio and pointed at the running Chrome
(`--browserUrl http://127.0.0.1:9222`). The session, cookies, and SSO state are
the ones already in that profile.

This is deliberately **not** the Claude-in-Chrome or ChatGPT-Atlas extension. The
original idea was "use the Claude/ChatGPT browser extension to control Chrome",
but those extensions attach a model to a tab *from inside the browser UI* and
expose no interface for a background daemon to hand them a URL and collect an
answer — they are not automatable. The CLI-plus-CDP path reaches the same
authenticated page, needs no new credentials, and is scriptable, which is what a
watcher loop requires. The cost is that Chrome must be started with
`--remote-debugging-port`, which is why `[browser].enabled` is off by default:
nothing should look enabled when nothing is listening.

**Read-only by construction**, in three layers:

1. **The tool allowlist** — `--allowedTools` names only navigate / snapshot /
   screenshot / console / network. `click`, `fill`, and `evaluate_script` are
   never granted, so there is no *mechanism* to acknowledge or silence an alert.
2. **`--strict-mcp-config`** — only the browser server MuggleBot passes in is
   loaded; the operator's own MCP servers (and their write tools) are not
   inherited.
3. **The prompt** — states the contract, and marks page content as untrusted.

Layer 1 is the real enforcement. Layers 2 and 3 matter because the page is
untrusted input: a dashboard annotation could contain text aimed at the agent
reading it, so the brief instructs it to *report* anything instruction-shaped
rather than act on it.

Failures are contained by classification rather than by a retry counter: no Chrome
on the port, no `npx`, or an unparseable response is **transient** and retried
under the workflow's policy; a 404 or an auth wall is a `TerminalError` recorded
on the subject. A permanently unreachable link stops consuming the slot instead of
spinning forever.

## Assigned issues — the work you own, triaged against the code

The notification feed is an *event* stream: it tells you what changed. Assignment
isn't an event, it's a standing state. An issue assigned to you three weeks ago
with no activity since emits nothing, so it never reaches the board — and that is
precisely the issue most likely to have fallen off your radar. So assigned issues
are polled separately (`GET /issues?filter=assigned`) and **always get a subject**,
independent of notifications.

Reconciliation is a fan-out from the `github-assigned` watcher: `Issue::record`
for everything in the listing, `Issue::unassigned` for subjects the listing no
longer contains. Because both GitHub watchers key on the real issue identity, one
watcher's snapshot can no longer resolve the other's cards — the failure that
previously required scoping the reconciler by an `assigned/` id prefix is
structurally absent.

**Triage against real source.** The expensive part of picking an assigned issue
back up isn't noticing it — it's the cold start: reloading what the issue is
about, finding the code, working out the options. That's twenty minutes every
time. So `IssueTriage` runs ahead of you, keyed `{issue}@{sha}`:

1. **Pull the code** — shallow (`--depth 1`, single branch), read-only, in the
   `checkout` scope with a per-repo limit key. The triage only reads the current
   tree; full history on a large repo costs minutes for nothing. The token
   reaches git through its *environment* config — never the remote URL (which
   would persist it to `.git/config`) and never `argv` (which would expose it to
   `ps`). Note also that git authenticates a GitHub token over **Basic** as
   `x-access-token:<token>`; Bearer is rejected with a bare "Authentication
   failed", which is a confusing way to discover this.

   The cache is bounded in total, not just per repo. The per-repo limit reads
   GitHub's reported size, which undercounts what lands on disk — measured on a
   real org, five repos came to 427MB, one docs site accounting for 335MB of
   assets. Since the code-derived repo index clones across the entire org, the sum
   is the number that matters, so `max_cache_mb` evicts least-recently-used
   checkouts (by mtime, which a fetch bumps) to stay under budget.
2. **Select files deterministically** — identifiers from the issue text
   (backticked spans, `snake_case`, `camelCase`, dotted paths) matched against
   paths and contents, with a path hit worth far more than a body hit. No model
   is involved, so selection works even with nothing reachable, and the model is
   never asked to guess at paths it hasn't seen.
3. **Characterize** with the source in hand.
4. **Propose N distinct approaches** — distinctness is the requirement, not a
   nicety: three variations on one idea is a single option wearing three hats and
   doesn't help you choose. Each carries files, a risk, an effort, and a
   confidence. Paths the model wasn't shown are dropped, because a patch citing a
   file that doesn't exist reads as authoritative and sends you hunting.
5. **Plain English**, told explicitly to add nothing. That constraint is what would
   make a small model right here: it's re-wording a conclusion, not reaching one.

Steps 3–5 run on the `triage` tier, which is **the one automatic pass not on-device** —
and the exception is about queueing, not capability. Reading source is still work you'd
rather keep local, and a coder model is still better at it than a generalist; what
overrode that is the single local permit. Triage makes several large calls per issue, so
on the local tier an issue assigned to *you* queued behind the code indexer's 147-repo
crawl — the pass whose whole point is to be ready before you look at the board was the
one guaranteed not to be. Its own tier decouples them. The default is the CLI bridge, so
it is unmetered but not private: the source excerpts leave the machine. Nothing in this
path writes to a repository: no commit, no push, no PR. The output is an artifact
in SQLite and an `ArtifactRef` on the subject, and the workflow key records the
commit it read — so "has this analysis gone stale?" is answered by comparing the
key to the current head, and re-triage on unchanged code is a free key collision.

## Root-cause investigation — from symptom to the change that caused it

Attribution answers "what lit up, and what else belongs to this work?". The next
question an on-call engineer asks is *why*, and answering it means leaving the
notification stream and going into the code.

**The repo index.** `RepoIndex` lists the watched org's repositories, checks each
one out, and distils a two-line card from its **code**: `PURPOSE:` (what this
runs) and `SYMPTOMS:` (the terms that should route an incident here), stored on a
`RepoCard` object per repo. That index is the routing table. It matters because
searching every repo for every alert is slow, noisy, and rate-limited; routing
"environment stuck provisioning" to `restate-cloud` and nothing else is what
makes the rest affordable. The fan-out runs in the `github` scope so indexing an
org doesn't spend the whole hour's rate budget in ninety seconds.

Reading the code rather than the README is deliberate. A README states intent, and
intent goes stale, turns aspirational, or is simply absent — plenty of real
services have a README that is one line and a badge. The directory layout, the
manifests, and the module names say what the thing actually *is*, and they cannot
drift from the code because they *are* the code. What the model sees is a
structural digest — layout, file-type counts, manifest headers, module paths —
rather than file contents: names are the highest-signal-per-token description a
codebase offers, and a digest stays bounded whether the repo is a small service or
a monorepo. A README, where one exists, is one input among several rather than the
basis.

Characterizations are cached against the commit they were built from
(`indexed_sha`), so a refresh only re-reads repos whose code has actually moved.
An unchanged org costs a shallow `git fetch` per repo and **no** model calls.

**Routing runs in two tiers**, mirroring attribution's own shape. Deterministic
keyword routes (`[investigation.routes]`) always apply and never wait on a model —
"cloud"/"environment" means `restate-cloud`, "invocation"/"partition processor"
means `restate`. Everything else is routed by the model reading the index cards.
Repo names the index doesn't know are dropped, so a hallucinated repo never
reaches the GitHub API.

**The pipeline** is the `RootCause` workflow sketched earlier, per subject:

1. **Symptoms** — search terms extracted from the subject's signals *and* the
   browser's dashboard reading (which is where the concrete numbers are).
2. **Route** — the two tiers above, capped at a handful of repos.
3. **Existing issues and PRs** — someone may have already filed it. That's the
   cheapest possible answer.
4. **The commit log** — over `commit_window` before the *earliest* signal, because
   a cause precedes its symptom.
5. **Rank** — into candidates, each with a `relation` (`cause` / `fix` /
   `duplicate` / `context`), a confidence, and a rationale.
6. **Code search** — only when nothing above explains it. "No issue, no PR, no
   commit" means the answer to "what's causing this?" becomes "here's the code
   that implements the failing thing."

Every candidate is a **hypothesis with a citation**, cited `[cause:REF]` and
rendered with its confidence beside it. A summary may say "likely caused by
#412"; it may never say "caused by #412". Nothing here closes an issue, reverts a
commit, or touches a repository.

Investigation is gated so it doesn't fire on everything: the subject has to look
broken (warning-or-worse, or an alert/CI-failure signal), and the workflow key
(`{subject}@{watermark}`) means a subject with a completed report isn't
re-investigated until new activity moves the watermark. A *failed* report is
retried by the workflow's own retry policy — the failure was often a rate limit,
and that is exactly what durable execution is for.

## The code index — scoring an issue onto a repo, a component, and a change

Root-cause investigation answers "why did this break?" by going and looking, per subject,
when something looks broken. The question underneath it is asked far more often and much
earlier: **given this issue, where in the codebase should I even start?**

Answering that by crawling doesn't work at org scale. Searching 147 repositories for every
issue is slow, rate-limited, and noisy — which is why routing exists at all. So the code
is indexed once, and the question becomes a retrieval.

### Three artifacts, built once

- **A summary per commit**, keyed by sha. A sha is immutable, so a summary is computed
  exactly once and is correct forever — which is what makes eager indexing a *one-time
  cost* rather than a running bill. The summary is behavioural ("stops returning the
  connection on the error path"), because that is what a symptom is matched against; a
  diff-shaped summary matches nothing an alert says.
- **A card per component.** A component is a module root — a directory carrying a
  manifest, or one directly under `crates/`, `packages/`, `services/`. This is the
  granularity an engineer acts on: "which repo" is useful once, and in a monorepo it
  barely narrows anything. Same `PURPOSE:` / `SYMPTOMS:` shape the repo index uses, so one
  vocabulary covers both. Re-derived only when the component's code moves.
- **Dependency edges between indexed repos**, from manifests actually present in the
  checkout (see [`ecosystem`](src/ecosystem.rs), which already refuses to infer them).
  Only edges to repos MuggleBot also indexes are stored: an edge to `serde` is true and
  useless, because the graph exists to point somewhere the index can look.

All of it runs on the **local** model, one call at a time. Reading code to describe it is
exactly the work that shouldn't leave the machine, and there is one Ollama — so a queue of
one is faster as well as cheaper.

### One request at a time, and where that gate lives

One Ollama is one GPU. Four concurrent requests to a 33B model are slower *and* worse than a
queue of one — they contend for the same weights, and Ollama serializes or thrashes depending
on how much memory the model needs. Since local-by-default routes *everything* here, that
contention got sharply worse: work that used to spread across cloud tiers now all lands on
one GPU.

So the gate is a **process-wide semaphore inside the Ollama reasoner** — at the resource, not
at any one caller. Three things about that placement are load-bearing:

**It has to be global, not a field.** `OllamaReasoner` instances are created in several
places and some at runtime: the configured tiers, the separate vision handle, and a fresh one
per request whenever the chat pane or a "reconsider on model X" override names a model. A
per-instance semaphore gates each of those independently and adds up to exactly the
concurrency it was meant to prevent.

**It replaced a per-consumer permit.** The code indexer briefly held its own, which fixed
five armed repos hammering the GPU and did nothing about indexing competing with triage and
correlation. Moving it to the reasoner covers every caller and deletes the private one.

**Ollama Cloud is exempt.** It is a fleet, not a GPU; queueing against it throws away the one
thing you pay it for. The check is on the *host* rather than a substring, because
`contains("ollama.com")` also matches `http://localhost/?upstream=ollama.com`. Everything
self-hosted is gated, loopback or a box down the hall — both are one process with one GPU.

**Embeddings are not gated**, for two reasons. It would deadlock the obvious caller: anything
that generates text and then embeds it would hold the permit while asking for the embedding.
And it would be the wrong trade anyway — an embedding is milliseconds against a small model
where a completion is tens of seconds against a 33B one, so queueing recall behind a
generation would make search feel broken to save contention that barely exists.

`[reasoner] local_concurrency` sizes it (default 1), first-call-wins: resizing a semaphore
other tasks are already queued on is how two callers end up holding a one-permit gate.

Not the `local-llm` vqueue, and that is worth recording because the vqueue looks like the
obvious answer. `send_after(..).scope(..)` exists, so the indexer's self-rescheduling tick
*could* carry a scope — but a scope queues the whole invocation (the clone, the GitHub
paging, the SQL) when the only contended resource is the GPU, and a scope contributes to the
partition key, which for a keyed object whose `start` comes from the ingress and whose `tick`
reschedules itself is a way to split one object key across two partitions. The vqueue still
bounds the invocations that *are* submitted through the ingress; it just can't be the thing
that protects the GPU.

Lockfiles, docs and assets are recorded as
"no code changes" without a model call, which is most of what keeps the one-time cost
affordable: a dependency bump is the commonest commit in many repos and the least useful
thing to have summarized.

### The org crawl: the list, then the cards

The crawl that feeds the index — "which repos exist?" — is two passes, and the split is
load-bearing:

1. **The list.** Every repo gets a row, from metadata, before any model runs or any repo is
   cloned. Two API pages. This is what the rest of the system needs: the code indexer can only
   arm repos that are in `repo_index`, so nothing downstream starts without it.
2. **The cards.** A bounded batch of repos are characterized from their code — a clone plus a
   local model call each — and the rest are deferred to the next crawl.

Measured live, the version that wrote each row *inline* with its characterization left the
index knowing about **2 repos out of 147**: the loop was blocked on repo 3's clone, so rows
4–147 didn't exist, and component carding, commit summaries, the dependency graph and scoring
were all starved at once. Split into passes, the full list lands in about 15 seconds.

Two supporting mechanisms:

**The cadence adapts.** `repo-index` ticks daily in steady state — a no-change crawl is a
shallow fetch per repo and no model calls. But the *first* crawl is a model call per repo, and
if it is interrupted the daily cadence means the next attempt is **tomorrow**. So the scheduler
ticks every 5 minutes until the index is complete, where complete means every listed repo has
a row *and* nothing is still owed a card. Both counts are recorded by the crawl itself
(`repo_index_enumerated`, `repo_index_pending`) rather than re-derived, because "does this repo
want a card?" is `worth_indexing`'s judgment — archived and long-stale repos deliberately never
get one, and a SQL predicate duplicating that rule would drift and hold the fast cadence on
forever.

**One crawl at a time**, via the `repo-index` vqueue. The first version of the catch-up cadence
had no such limit and produced four concurrent crawls, each cloning the same repos into the
same directories. Note the ordering trap: the workflow key is bucketed to de-duplicate ticks,
so the bucket has to be *shorter than* the catch-up cadence — otherwise every catch-up tick
lands in the failed crawl's bucket and is refused as a free redo, which is exactly the stuck
state the catch-up was added to escape.

### Where the history comes from

Nothing else in MuggleBot fetches a repo's history, so the indexer walks it itself:

- The **checkout is shallow** (`--depth 1`) and always will be — deepening 147 clones to
  index them is the cost the index exists to avoid.
- The **investigation path** caches commits, but only a 72-hour window around one incident.

So `RepoIndexer` pages *backwards* from the oldest commit it already has, one page per tick,
sharing the `repo_commit_windows` cursor with the investigation path (which only ever moves
it backwards, so a 72-hour window can't undo a completed index). The walk continues to the
repository's root commit. Fetching and summarization stay bounded per tick, but the overall
index deliberately has no age cutoff: every immutable commit is summarized exactly once.

Two things about that walk are easy to get wrong, and both make a hollow index *look
finished*:

- **An empty file list means unknown, not "changed nothing".** GitHub's commit-list
  endpoint omits changed files, so a freshly fetched commit arrives with none; treating that
  as a no-op commit fills the index with placeholder rows and then reports complete. The
  file list is fetched per sha (one call, cached on the row, immutable), and a commit whose
  files can't be determined is left unsummarized for the next tick.
- **`0 of 0` commits is not completeness.** A repo whose history has never been fetched has
  nothing done out of nothing to do. So "complete" — which is what drops the cadence from
  30s to hourly — requires the history walk to have reached the horizon *and* every
  component to be carded *and* every cached commit summarized. Likewise the scorer's report
  names the number of repos with no components at all before it reports a percentage.

### Why the index is in SQLite and not in object state

This is the sharpest test of the storage rule, so it is worth stating plainly. Keying
commit summaries into a `RepoIndexer` object's state — `<sha>` → summary — is the obvious
shape, and it breaks the thing it is for:

**First, a correction to what this section used to claim.** It said object state "is
addressable only by key, so ranking would mean loading every repo's every commit". That is
false. Restate exposes a `state` table through the same Datafusion SQL surface as
`sys_invocation`, with `service_name`, `service_key`, `key`, `value_utf8` and `value` columns —
and `GROUP BY`, predicates on values, and even `LIKE` *inside* a value all work server-side
(measured: 6–30ms over ~125 state entries). Cross-key querying of object state is a real
capability, and the argument below has to stand without that claim.

What actually rules it out for the index:

1. **No vector operations.** The semantic pass is cosine similarity over f32 embeddings. SQL
   over `state` cannot express that, so ranking means transferring *every* embedding to the
   client per query — as base64 inside JSON, over HTTP. The same bytes are a local read from
   SQLite. (The current implementation does load them all either way; this is a constant
   factor, not an algorithmic one, but it is a large constant on the hot path.)
2. **A Restate wipe must cost only in-flight work** — and enabling vqueues *requires* a fresh
   cluster. This is the load-bearing argument, and it is not hypothetical: enabling vqueues
   during development meant recreating the cluster twice in one afternoon. The index is
   thousands of local model calls and hours of GPU; losing it to a documented operational step
   would be brutal.
3. **`state` is an introspection surface**, documented for debugging and operations. Putting
   the product's read path on it couples the board to a shape Restate has not promised to keep.
   That is a judgment about risk rather than a limitation.

Note what is *not* on this list: cross-key queryability, and journal cost — state lives in the
partition store and `ctx.get(key)` reads one key, so holding many keys is not itself a replay
expense.

So the object owns the **indexing**, not the index: a cursor, a cadence, and exclusivity so
two indexers never clone the same repo. Bounded batches on a durable timer, because a first
index over an org is hours — in batches every tick leaves the index strictly more complete,
a restart resumes rather than restarting, and the scorer works off a partial index from the
first batch onwards. The cadence adapts: fast while there is a backlog, hourly once caught
up.

(Base64-encoding the summaries, specifically, buys nothing: it costs FTS, costs embedding
without a decode step, adds a third to the size, and SQLite stores text fine.)

### Watching it get built — the CODE INDEX panel

A one-time cost measured in hours of local model time is one you have to be able to *see*,
or the only two states an operator can distinguish are "scoring works" and "scoring returns
nothing", with no way to tell a half-built index from a broken one. The panel shows both
halves of that, and they come from different places on purpose:

- **How much is built** — per repo: components carded, commits fetched, commits summarized,
  dependency edges each way, how far back history has been walked. From SQLite, so it
  survives a Restate wipe, which is the whole storage rule restated as a UI property.
- **What is being crunched right now** — the in-flight `RepoIndexer`/`RepoIndex`
  invocations, from Restate's own introspection. Without this, "thin index" and "thin index
  and nothing is working on it" look identical.

Three deliberate choices in how it reports:

**No percentage for the repo as a whole.** The denominators arrive *as the walk proceeds* —
a repo whose history hasn't been fetched has 0 of 0 commits — so a bar would show an
untouched repo and a finished one identically. Phases (`NOT STARTED` → `CARDING` →
`SUMMARIZING` → `INDEXED`) say where a repo actually is, and the one bar that is drawn is
over commits genuinely fetched.

**`TARGET, UNINDEXED` is called out.** A repo with inbound dependency edges and no
components is the worst case for scoring: the structural pass raises it — "you depend on
this and nothing in the issue mentions it" — and there is nothing inside to look at. It is a
lead that cannot be followed, which is different from an absence, and the first version of
this panel filtered it out of the default view.

**Commit summaries are readable, not just counted.** "40 commits summarized" is
unfalsifiable from outside. Reading three tells you at once whether the local model is
describing behaviour or paraphrasing the commit message back at you — and on the first repo
indexed live, it was doing the latter. Recorded-with-nothing rows (lockfile bumps, docs) are
dimmed rather than hidden: they are why the summarized count climbs without the index
getting smarter, and hiding them would make that look like progress.

### Scoring: three passes, fused, every contribution cited

Three retrievals that are **independent on purpose** — each blind to what the others find,
because each fails differently:

| Pass | Finds | Fails when |
|---|---|---|
| **Semantic** | "the pool never returns connections" against a commit that says "release the guard on the error path" — no shared vocabulary at all | the issue is mostly identifiers, or embeddings are unavailable |
| **Lexical** | `max_connections` exactly, in a message, a path, or a component digest | the issue paraphrases, which is most incident prose |
| **Structural** | the symptom is in the consumer and the change is in the dependency — two codebases sharing no words | the graph has no edge, or the issue's repo is unknown |

Contributions **compound** rather than sum: `1 - Π(1 - wᵢ)`. Two independent passes agreeing
is much stronger evidence than one pass firing twelve times, and a plain sum lets a dozen
weak substring hits outrank a single strong cross-pass agreement — precisely the wrong
ranking.

A bare dependency edge scores around 0.22 at one hop — shown, but ranked under anything the
text itself supports, and never with a component. That is the honest reading: "you depend on
this and nothing in the issue mentions it" is a lead worth surfacing, since it is the one
thing the other two passes structurally cannot find, and the citation names the hop and the
manifest so the operator can see it is a lead and not a finding.

Every candidate carries **which passes found it and what each matched**, including how many
hops through the graph it came and via which manifest. A score with no explanation is worse
than no score, because the operator cannot tell a strong semantic match from a lucky
substring, so they end up trusting all of it or none of it.

And when the index is still being built, the report says so — a thin answer from a
half-built index is a different thing from a thin answer from a complete one, and only the
report can tell the operator which.

## Subjects and signals as object state

The `subjects` and `signals` tables are being replaced by the state of the object that owns the
work. The layout, in [`restate::subject_state`](src/restate/subject_state.rs):

| State key | Value |
|---|---|
| `subject` | the `Subject` record as JSON, including its parent and merge key |
| `signal:<id>` | one `Signal` as JSON |

**One state key per signal, not one key holding a list.** A list is a read-modify-write, and
while Restate serializes handlers for one key it does not across keys — a merge touches two
objects, so a list would lose an append. Keying by signal id also makes a re-delivery an
overwrite rather than a duplicate, which is the `UNIQUE(source, external_id, version)` guarantee
expressed as the shape of the state instead of as a constraint.

Two things the tables carried needed new homes rather than translations: unattributed signals
live on an `~unattributed` singleton object, since a signal that resolves to nothing has no
subject to hang off; and the dedup backstop beyond Restate's idempotency retention becomes a
per-tuple key rather than a unique index.

### Why there is still a read model

Cross-key reads go through Restate's `state` table — an HTTP call and a Datafusion scan. The
board reads subjects on every push, and `subject_view` is called from 37 places, every one of
them synchronous. Making them async cascades through the notifier, the event broadcast, and
every handler that renders a card: far more change than the migration itself, for a read that
has to be fast regardless.

So [`SubjectStore`](src/subject/store.rs) keeps an in-memory map and it is a **cache, not a
second source of truth** — built from object state at boot, refreshed on a 30s timer, discarded
on exit. Nothing reconciles it, because there is nothing to reconcile it *with*: if the model and
the objects disagree, the objects are right and the next refresh fixes it.

A write updates the model immediately, so the caller's next read is consistent, and sends the
durable write to the owning object through the ingress. The send is fire-and-forget because the
call sites are synchronous, which means a rejected send leaves the model briefly ahead of the
record. Those failures are logged loudly: a silently-dropped durable write is the one failure
this shape cannot detect for itself, since the model will keep serving the value.

### The cost that came with it

`record` used to take a signal **id**, with the body read from SQLite — deliberately, so a
200KB raw notification payload stayed out of the invocation journal and off every retry. With no
table to read back from, the body has to travel through the handler argument and through
`PollOutcome`, which is a journalled `ctx.run` result. That is inherent to the direction rather
than a defect in it, and it lands on the highest-frequency path in the system.

And the state lives in the Restate cluster, so a wipe takes the board with it — where enabling
vqueues *requires* a fresh cluster. That is a deliberate acceptance, not an oversight.

## Reasoning tiers — local by default, cloud only when asked

**The local model does the work.** Every pass MuggleBot runs on its own — correlation,
root cause, explanation, code indexing, tagging, live assist, chat — runs on
`deepseek-coder:33b` via Ollama and nowhere else.

**Assigned-issue triage is the one exception**, on `claude-sonnet-5` via the CLI bridge
(`[reasoner] triage`). It is a queueing decision: one Ollama is one GPU, triage makes
several large calls per issue, and it was reliably stuck behind the indexer. Unmetered,
but the source excerpts leave the machine; `triage = "ollama_local"` reverts it.

Otherwise a cloud model is used only when the operator asks for one, **by name**, in one
of two places:

| How you ask | What it does |
|---|---|
| The **chat pane's model picker** | Sends that turn, and only that turn, to the chosen model. |
| The **2ND OPINION** button on a subject | Re-explains that one subject on the cloud tier and shows the answer beside the local one, labelled. |

Nothing else can. That is a property of the wiring, not a policy someone has to remember:
[`Reasoners`](src/reasoner/mod.rs) exposes exactly one cloud-capable handle, `cloud`, and
`grep -rn 'reasoners\.cloud\|ops\.cloud' src/` returns those two callers. A third would be a
visible diff to a struct field documented as the chokepoint, rather than an innocuous-looking
call-site change nobody notices.

### Why this way round

The earlier design graded every task's difficulty and escalated `hard` work to Sonnet and
`extra_hard` to Opus. It was defensible — a small model quietly producing a worse answer is
the failure mode you never notice — and it was still wrong for an always-on daemon:

- **An always-on agent makes its own decisions to spend.** A watcher poll at 03:00 that
  grades `hard` bills you while you sleep, for work nobody asked for yet.
- **Grading is itself a model call.** Paying for a judgment whose only purpose is deciding
  whether to escalate is pure overhead once you aren't escalating.
- **The failure mode has a cheaper fix than a better model.** Where the local model was
  actually producing garbage, what fixed it was a stricter prompt and a deterministic check
  against the source data — see the explanation verifier below. That is a *guarantee*; a
  frontier model is a better guess.

So difficulty routing still exists, fully implemented, and ships **off**
(`[reasoner.routing] enabled = false`). Turning it on re-enables automatic escalation, which
is exactly what the default exists to prevent — the config says so in those words.

### What replaced the metered tier where it mattered

Explanations were the one job pinned to the top tier, because an explanation is what the
operator acts on and a fluent wrong one costs more than the call it saved. That was not
superstition: on its first live run the local model produced four fabrications in one page —
a markdown link to `link_to_pr`, "reviewers said this approach is effective" about a PR with
no reviews, a verdict's *confidence* read as "only fixes 90% of it", and an "Attempts"
section on a dossier with no attempts.

The prompt now forbids all four. [`explain::verify`](src/restate/workflows/explain.rs) is
what makes that a guarantee instead of a request, deterministically and with no second model
call:

| Fabrication | What the check does |
|---|---|
| A link the dossier never supplied | Strips the destination, keeps the text — the sentence is usually still true, and the fake URL is the part that wastes a click |
| A reviewer quote when nothing has been reviewed | Drops that sentence, leaving the surrounding paragraph about the diff intact |
| A section with nothing behind it | Drops the heading and its body |

It only ever **removes**. A verifier that rewrote prose would introduce the very thing it
exists to catch. Each removal is reported and shown under the explanation, because one that
needed correcting should be read more carefully than one that didn't.

The same verification runs on the cloud second opinion. Asking a more expensive model buys
no license to invent a link, and it means the two answers differ by model and nothing else —
which is the only reason a second opinion tells you anything about the first.

### The tiers that remain

| Handle | Model | Who holds it |
|---|---|---|
| `local` | `deepseek-coder:33b` | everything automatic |
| `routed` | `local`, unless routing is turned on | call sites that *would* escalate, so the set is visible |
| `vision` | a local vision model (`qwen2.5vl:7b`) | chat, and only for a turn carrying images |
| `cloud` | `claude-opus-4-8` via the CLI bridge | the chat picker, and `SecondOpinion` |

Vision is separate because it is a different *capability*, not a better tier: a coder model
has no image encoder, so pointing this at one makes MuggleBot answer confidently about a
screenshot it never saw. It is also only used when a turn actually carries an image — a 7B
vision model is a downgrade for ordinary text, and paying it on every message for the sake
of the few with screenshots is the wrong trade.

One consequence worth stating: **unrecognized provider names resolve to local.** The inputs
to `provider_label` come from a request body and a TOML file, so its catch-all decides what
a typo does, and under this policy a typo must not quietly start billing. Every cloud
provider has to be named exactly.

**Answers are cached at two levels.** The journal handles retries: a completed
`ctx.run` model call is never re-paid when the handler around it is retried or
replayed. The SQLite completion cache handles *new* invocations: a decorator in
front of every tier keys completions on the whole request (tier label, system
prompt, messages, images, sampling limits) and serves stored answers. It lives in
SQLite rather than a process map deliberately — a restart is precisely when the
accumulated answers are most valuable, and an in-memory cache discards them
exactly then. The tier label is part of the key, so switching models can't serve
answers from the model you switched away from, and the key uses two independent
hash lanes because a collision here would serve the *wrong* answer.

Three things are deliberately never cached:

- **Deliberate redos.** "Reconsider on model X", an explicit re-triage
  (`#a{attempt}`), and chat set `no_cache`. The user asked for the work to be
  *redone*; a cache hit would make the action look broken.
- **Empty responses.** A model returning nothing is a transient failure, not an
  answer. Caching it would make one bad minute stick for the whole TTL.
- **The `session` key**, which is excluded from the key entirely — it's CLI
  conversation bookkeeping, not part of the question, and including it would mint
  a fresh key per session and make the cache useless.

Two further policies, both enforced in code rather than left to convention:

**Handled subjects are not re-analyzed at all.** A snoozed, acknowledged, or resolved
subject is settled work; re-summarizing it and re-judging its relations re-litigates a
decision the operator already made. `Issue::analyze` refuses these
outright. An explicit "reconsider on model X" against a handled subject is an **error**, not
a silent skip — otherwise the UI would look like it worked.

(This predates local-by-default and was originally about metered calls. It still matters for
a reason that has nothing to do with cost: a settled subject that keeps changing its own
summary is a board you stop trusting.)

The one model allowed to look at handled work is the local classifier, doing
exactly one job: deciding whether new activity means the issue actually came
back. A snoozed subject is muted so recurring chatter stops interrupting — but
"the same failure, worse" is not chatter. `triage_handled` un-mutes the subject
when the local model is confident past `reopen_min_confidence`; below that, and
whenever Ollama is unreachable, it stays muted. That asymmetry is deliberate: a
false reopen re-raises a notification the operator deliberately silenced, so
uncertainty must fail closed.

**Investigation narrows before it reasons.** Steps 1–4 above and the shortlisting are
mechanical — reading dozens of issue titles and commit subjects to decide what is even worth
considering. Only `shortlist_size` already-plausible candidates reach the ranking pass.

That shape was designed to put one metered call at the end of several local ones. It now runs
entirely local, and the shortlist is *why* that works: the ranking pass reads a handful of
candidates with their evidence, not a repository, and a 33B model is adequate for exactly
that. Narrowing first turned out to be the thing that made the expensive model unnecessary,
rather than merely affordable.

---

## Human gates — the confirmation that isn't a promise

"Copilot, not autopilot" is currently guaranteed by omission: there are no write
tools, so nothing can act. That holds until the first one is wanted, and "we'll
add a confirmation dialog then" is not a design.

Durable promises make the gate real. A handler that reaches a gated step blocks on
a signal:

```rust
let approved: bool = ctx.signal::<bool>("approved").await?;   // durable, survives restart
```

The UI surfaces the pending gate, and approving resolves it. Three properties fall
out that a dialog box does not have: the pending decision survives a restart
rather than being silently abandoned; it has an id, so the audit log records *what
was authorized, by whom, and when*; and an un-answered gate is visible on the
board as blocked work rather than something that quietly didn't happen. Rejection
is `reject(TerminalError)`, which fails the invocation with a recorded reason.

No gated action ships in v1. The mechanism is specified now because retrofitting
authorization onto a pipeline that already acts is the wrong order.

---

## MCP surface

MuggleBot exposes an MCP server over both stdio (for local clients) and HTTP/SSE
(for networked clients on `localhost`). Tools are typed and carry risk metadata;
read tools are free, any future write/act tools are gated (see design principles).
All of them dispatch through the same `src/tools.rs` implementation the web API and
agent chat use, which in turn calls Restate handlers — so the three surfaces
cannot drift, and none of them reaches around the object model to write state
directly.

Subjects are addressed by their key (`restatedev/restate#412`,
`restatedev/restate!987`, `T01/C02/1721822400.001`) everywhere a `subject`
parameter appears.

**Tools (read):**

- `list_signals(source?, since?, severity?)` — the raw signal stream.
- `get_signal(id)` — full detail incl. deep-link and raw payload.
- `list_subjects(rank?, needs_attention?, handled?)` — the current board.
- `get_subject(subject)` — signals, links, summary, artifacts, timeline.
- `timeline(subject)` — reconstructed, ordered event timeline.
- `search(query)` — semantic/keyword search across ingested signals.
- `list_alerts(handled?)` — signals from Slack alert channels, current state.
- `list_unattributed()` — signals that resolved to no subject.
- `source_health()` — per-watcher status, last cursor, error state.
- `list_repos()` — the code-derived repo index used for symptom routing.
- `score_issue(subject | text, repo?)` — rank which repo, component and commit an issue is
  likely about, with each candidate's evidence. Hypotheses, never a confirmed cause.
- `list_components(repo)` — a repo's module roots and their routing cards.
- `repo_deps(repo)` — the dependency edges in and out, with the manifest each came from.
- `list_pr_fixes(subject)` — open PRs that may already fix an assigned issue,
  with the diff-derived implementation, the critique, and what else they resolve.
- `pr_diff(subject, stored_only?, refresh?)` — a PR's diff with a behavioural summary,
  read from the pull request's own object state (see _A diff on the object_). Pass an issue
  key to get every attempt's diff. `stored_only` answers from state alone, which is what
  lets the pane open itself.
- `get_root_cause(subject)` — ranked issue/PR/commit/code candidates with
  confidences and rationales. Hypotheses with citations, never conclusions.
- `list_browser_investigations(subject)` — dashboard readings and their status.
- `list_issue_triage()` / `get_issue_triage(subject)` — assigned issues read
  against their source, with candidate patch approaches.
- `explain(subject)` — distil this subject and everything under it (see _Explaining a
  subject_). Free when nothing has changed since the last one.
- `get_explanation(subject)` — the stored explanation, with the watermark it was built
  from so a stale one is visibly stale.
- `list_dispatches(subject?)` — what the AI is doing right now, from the daemon's own
  registry: each dispatched pass with its state (queued, running, done, refused as a
  duplicate, or failed with its message). The strip the UI renders, and the fastest way
  to answer "did that button do anything" from a terminal.
- `list_workflows(subject?, state?)` — in-flight and recent workflow invocations
  with their ids, current step, and failures. "Why is there no triage yet?" is a
  question about an invocation, and it should be answerable without opening the
  Restate UI.

**Tools (attribution & correlation — read/write, writes gated):**

- `relate(a, b, kind)` — pin a `same` / `related` / `distinct` edge (associate,
  mark duplicate, or dissociate); triggers re-analysis.
- `merge(a, b)` — submit the `Merge` workflow to collapse two subjects.
- `reattribute(signal_id, subject | none)` — override the hierarchy climb for one
  signal; recorded so re-ingest doesn't undo it.
- `attach_context(subject, text | url)` — add ad-hoc grounding to one subject;
  triggers re-analysis.
- `split_subject(subject, signal_ids)` — detach wrongly-attributed signals to the
  unattributed lane, and remember the correction.
- `reanalyze(subject)` — force the analysis pass to re-run. Errors on a handled
  subject rather than silently skipping it.
- `investigate_root_cause(subject)` — submit `RootCause`. Refuses handled
  subjects.
- `investigate_link(subject, url)` — submit `BrowserRead` for one dashboard link.
- `record_browser_investigation(id, findings)` — file findings by hand when the
  browser can't reach the page.
- `retriage_issue(subject, force?)` — submit `IssueTriage`; `force` bumps the
  attempt suffix so unchanged code is re-read.
- `refresh_repo_index()` — submit `RepoIndex`, skipping repos whose commit is
  already indexed.
- `resolve_gate(invocation_id, approve | reject, reason?)` — answer a pending
  human gate.

**Tools (grounding — read/write, writes gated):**

- `search_memory(query)` / `search_context(query)` — semantic retrieval over the
  two grounding stores.
- `list_memories()` / `get_memory(id)` — browse memory.
- `put_memory(text, links?, tags?)` / `edit_memory(id, text)` /
  `tag_memory(id, tags)` / `delete_memory(id)` — memory CRUD; the
  editable-memory surface. `tags` pin routing labels; omitted, they're
  auto-suggested on write.
- `list_context()` / `get_context(id)` — browse the context library.
- `add_context(url | path, tags?)` / `tag_context(id, tags)` /
  `refresh_context(id)` / `remove_context(id)` — manage context sources.
- `list_tags()` / `edit_tag(name, summary)` / `merge_tags(from, into)` /
  `delete_tag(name)` — the tag vocabulary.
- `set_subject_tags(subject, tags)` — pin a subject's tags and re-run its
  analysis (mirrors relation pins).

**Tools (secrets — write-only):**

- `list_secrets()` — names, whether set, and when last changed. **Never values.**
- `set_secret(name, value)` / `delete_secret(name)` — manage credentials.

**Tools (live assist — read/write, writes gated):**

- `list_hints(subject?)` — current hints, suggestions, and flags.
- `dismiss_hint(id, false_positive?)` — dismiss a hint/flag; `false_positive`
  feeds it back to memory so it isn't re-raised.

**Resources:**

- `board://current` — live board snapshot.
- `config://redacted` — effective config, secrets stripped.
- `memory://` / `context://` — browsable grounding stores.
- `live://hints` — active live-assist hints and flags.
- `subject://{key}` — one subject's full state.

---

## Explaining a subject — the hierarchy, distilled

The board answers "what is there?". The question after it is "so what is going on?",
and the honest answer spans the hierarchy: a bug is not explained without the pull
requests attempting it, and a pull request is not explained without the problem it is
attempting.

So **`Explain` is one operation at two levels.** Run it on a pull request and you get
that change: what it does mechanically, whether it fixes the thing, what reviewers said.
Run it on the issue above and you get the whole situation — what happened, the proposed
cause, where it stands, one block per attempt with each one's critique and review
conversation, and what to do next. Same workflow; it gathers from the subject's rank
downwards.

**The dossier is assembled deterministically, and the model only writes it up.** Every
field is a store read: the events, the PR critiques, the review conversations, the
root-cause candidates, the triage approaches, the dashboard readings, the attached
context. A model asked to *find* its context invents some, and an explanation citing a
PR that doesn't exist reads as authoritative and sends you hunting. The prompt says use
only this, cite `[sig:ID]` and `[cause:REF]` as given, and say the dossier is thin rather
than padding it. Sections the dossier can't support are omitted rather than emitted
empty — an empty heading is an invitation to fill it in.

**Explaining runs on the local model, and is verified against the dossier afterwards.**

It was briefly the one job pinned to the top tier, for a real reason. Measured on a live
dossier, the local 33B model invented a `[#991](link_to_pr)` placeholder link, reported
"reviewers said this approach is effective" about a pull request nobody had reviewed, read
"verdict fixes at 90%" as "fixes 90% of the problem", and produced an "Attempts" section on a
dossier with no attempts. This is the output the operator *acts on*, so a fluent wrong one is
worse than none.

What fixed it was not a bigger model. Three prompt rules came out of that measurement, each
because its absence produced a specific fabrication — and then
[`explain::verify`](src/restate/workflows/explain.rs) turned the prompt from a request into a
guarantee, deterministically and without a second model call: an unsupported link loses its
destination, a reviewer claim with nothing reviewed loses its sentence, a section with nothing
behind it loses its block. Each removal is reported and shown, because an explanation that
needed correcting should be read more carefully than one that didn't.

The verifier only ever **removes**. One that rewrote prose would introduce the very thing it
exists to catch. It runs on the cloud second opinion too: a more expensive model gets no
license to invent a link either, and it keeps the two answers differing by model and nothing
else — which is the only thing that makes a second opinion informative about the first.

The three prompt rules:

- **The section list is built from the dossier**, not fixed. Naming every possible
  section up front reliably produced all of them — including an "Attempts" section on a
  dossier with no attempts in it.
- **Absence is stated, never omitted.** "This PR has no review discussion; do not claim
  reviewers approved or objected" is in the dossier explicitly. Leaving the line out is
  what invited invented reviewer approval, which is the single worst thing this feature
  could fabricate.
- **A verdict's confidence is labelled as confidence in the judgment**, in words, because
  a bare percentage next to a verdict reads as a completeness score.

Two steps, journalled separately: gather, then write. A rate limit in the write-up
doesn't re-walk the hierarchy, and the retry writes up *the same* dossier the first
attempt gathered — an explanation that half-describes two different states of the world
is worse than a stale one.

The key is `{subject}@{watermark}+{critiques}`. The watermark alone would pin an issue's
first explanation forever while its attempts were still being judged, so the count of PR
critiques rides along: new evidence about the work is a new question. Everything else
about it is the usual free-redo property — ask twice about unchanged work and the second
ask costs a refused submission.

Explanations carry the watermark they were built from, so the board marks one **stale**
when activity has landed since. An explanation that has gone out of date still describes
what it described; presenting it as current is the only version of that which lies.

### Second opinion

`SecondOpinion` is the same workflow with a different model: same gather step, same dossier,
same prompt, same verification. It is the **only** workflow that reaches a cloud model, and it
runs only from a button press — so "did this cost money?" has one answer, which is "only if
someone clicked".

Both explanations are stored (the table is keyed `(subject_key, produced_by)`) and both are
rendered, labelled `LOCAL` and `CLOUD`. Replacing the local one would throw away the thing the
second opinion is being compared against.

## Nothing is ever written back to GitHub

A PR critique is a note in MuggleBot's own store. It is rendered in the LCARS console and
nowhere else: it never becomes a PR comment, a review, an approval, or a label.

This is worth stating as an invariant rather than leaving implicit, because a critique
*reads* like a review — "this papers over the leak, and a reviewer already objected" is
exactly what someone would want to post — and the value of the whole feature depends on
it not being posted. An ops agent that comments on your colleagues' pull requests is a
different, much worse product.

Mechanically: the GitHub client has no write methods. The only `POST` anywhere in the
codebase is Granola's read API, which requires one to fetch documents. Nothing calls
`PATCH`, `PUT`, or `DELETE` against any source. The triage path is the same — no commit,
no push, no PR — as is the browser path, where the tool allowlist grants navigate and
snapshot but never `click`, `fill`, or `evaluate_script`, so there is no *mechanism* to
acknowledge an alert even if something wanted to.

The UI says so where it matters: each critique block is footed with "never posted to
GitHub", because the person reading it is the person who might otherwise assume it was.

## Comments — read on merit, not on position

The discussion on an issue is usually where its real content is. A title says what
broke; the comments say what was tried, what was ruled out, what a maintainer
decided, and — on a pull request — what a reviewer is blocking on. Reasoning from
the body alone reliably misses an answer somebody already wrote down.

A long thread can't go into a bounded context window whole, and the obvious
shortcuts are both wrong. **Keeping the most recent N** throws away the framing:
the opening carries the reproduction and the initial diagnosis, so truncating to
the tail keeps a conclusion and discards what it was a conclusion about.
**Keeping the first N and last M** is better but still decides by position, and a
decisive comment in the middle of a fifty-comment thread — "this is a duplicate of
the connection-pool bug, see #204" — is exactly the one that changes what you do.

So **every comment is scored on its own merits** and selection is by that score:
does this carry decision-relevant information? One batched local call scores all
of them by index; underneath sits a deterministic heuristic (evidence markers,
pasted code or stack traces, cross-references, minus social patterns and bots) so
the pass still works with nothing reachable. The model can only *raise* a score,
never lower it — a blocking review is pinned at maximum merit and cannot be
demoted by a model that overlooked it, because "this doesn't handle the retry
case" is the single most decision-changing sentence a PR can contain.

Conversation order is restored before rendering: ranking says which comments
matter more, not that the discussion happened backwards. The prompt states how
many of how many were kept, so the model knows it's seeing a selection.

Comments feed the two deep-read paths: an issue's discussion goes into
`IssueTriage`'s characterization and patch proposals, and a PR's reviews go into
`PrCritique` — where the instruction is explicitly to defer to a human reviewer
who already objected, since that's better evidence than the model's own reading of
the diff.

Cross-references found in comments are also **resolution keys**: "fixes #204" in a
PR comment is how a subject discovers its parent when the PR body forgot to say
so.

## Web UI — LCARS

A single-pane dashboard in the **LCARS** idiom (the swooping Okudagram panels from
Star Trek: TNG). The aesthetic isn't just fun — LCARS is genuinely good at dense,
color-coded, panelized status display, which is exactly the job.

- **Board view.** Subjects as panels, ranked by attention, grouped by rank
  (issues, then unparented PRs, then Slack-only conversations) with the
  unattributed lane last. Color = severity. Snooze, acknowledge, deep-link out.
- **Subject view.** Timeline + attributed signals + summary with citations, the
  hierarchy (parent issue, child PRs, contributing Slack threads and meetings),
  and the relation graph. **Nested, not flat:** a bug renders with the pull requests
  attempting it indented beneath it, each showing its verdict, what it implements,
  MuggleBot's critique, and — on its own line — what reviewers said. An issue whose
  attempts you have to click through to see reads as an issue nobody is working on.
  A reviewer's objection gets the emphasis rather than being folded into the critique,
  because a human who read the change and pushed back is better evidence than a
  model's reading of the same diff.
- **Explain.** One button per subject, at every level. On a PR it distils that change;
  on the issue above it, the whole situation including every attempt. The result is a
  panel above the summary, with a strip naming what it drew on and a **stale** marker
  when activity has landed since it was written. Inline controls to **associate**, **merge**, **split**,
  **re-attribute**, and to **attach context** — any of which re-runs the
  analysis. LLM verdicts show confidence + rationale; user pins are visually
  distinct and authoritative.
- **Work in flight.** Per subject, the running and recent workflow invocations:
  which step, how long, what failed, retry countdown. This is the panel that
  makes durable execution legible instead of magic — a triage that is *queued
  behind the local-model vqueue* looks identical to one that is broken unless the
  UI says so.
- **Config page.** Manage per-source settings and, crucially, **credentials** —
  written to the SQLite `secrets` table here, write-only, never rendered back.
  Also toggles sources, notification rules, reasoner routing, and vqueue limits.
- **Memory editor.** Browse, add, edit, and delete memory entries — the human
  side of institutional memory.
- **Context library.** Add/remove URL and file sources, see each one's last
  refresh and current summary, and force a refresh on demand.
- **Live assist.** In subjects you're active in, inline hints and suggestions,
  plus flags on your own messages (factual error / risky action) with citations.
- **Agent chat.** A multimodal chat panel — drop screenshots, images, logs, or
  files and converse with MuggleBot over the live board, memory, and context.
- **Live.** Fed over a WebSocket; new signals animate in, resolved ones fade.
- **Red-alert mode.** A high-confidence live-assist flag shifts the interface to
  LCARS red-alert (color + optional audio cue, off by default) and fires a
  Critical macOS notification; clears on acknowledge/dismiss.
- **Read-mostly (except config).** The board reflects state and lets you triage
  (ack/snooze); it is not a console for mutating production. The config page and
  the gate approvals are the places that write.

**SolidJS** (TypeScript). Its fine-grained reactivity updates only the panels
whose underlying signals changed — a better fit for a high-frequency live board
than a virtual-DOM diff cycle — with a tiny bundle and no runtime GC churn.

---

## Configuration

A single TOML file (path via `--config` or `$MUGGLEBOT_CONFIG`) holds non-secret
behavior. Credentials are **not** in the file — they live in the SQLite `secrets`
table and are set through the WebUI config page or `set_secret`.

```toml
[general]
data_dir = "~/.mugglebot"      # SQLite DB (signals, artifacts, grounding, secrets)
quiet_hours = "22:00-08:00"    # suppress non-Critical notifications

[secrets]
# Credentials live in the `secrets` table in the SQLite DB (mode 0600), not here.
encrypt = false                # true → seal values under $MUGGLEBOT_MASTER_KEY

[restate]
ingress = "http://127.0.0.1:8080"     # where watchers submit signals
admin = "http://127.0.0.1:9070"       # deployment registration, SQL introspection
endpoint_listen = "127.0.0.1:9080"    # where this binary serves its handlers
register_on_boot = true               # self-register the deployment (--force)
vqueues = true                        # requires the server's experimental flags

[restate.limits]                      # vqueue concurrency per scope
local_llm = 1                  # one Ollama, one GPU
cloud_llm = 3                  # bound metered spend
github = 4
browser = 1                    # one Chrome
checkout = 2

[sources.github]
enabled = true
watch = ["review_requested", "mention", "ci_failure", "assigned"]
poll_interval = "1m"

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

[attribution]
window = "30m"                 # proximity window for residual Slack-to-Slack candidacy
dedup_threshold = 0.8          # min LLM confidence for a "same" verdict
auto_merge = false             # false → "same" verdicts are proposed, not applied
default_branch_ci = "merge_commit"   # how CI on main resolves: via the merging PR

[live]
debounce = "1m"                # wait after last activity before re-analysis
debounce_max = "5m"            # hard cap so busy subjects still get analyzed
red_alert = true
red_alert_min_confidence = 0.75

[reasoner]
# Does all the work. Every automatic pass runs here and nowhere else.
local = "ollama_local"
local_model = "deepseek-coder:33b"
vision_model = "qwen2.5vl:7b"  # images dropped into chat; a coder model has no encoder

# Used ONLY when asked for by name: the chat pane's picker, or 2ND OPINION on a subject.
cloud = "claude"
cloud_model = "claude-opus-5"

# Escalation tier for [reasoner.routing] below, which is off.
mid = "claude"
mid_model = "claude-sonnet-5"

ollama_url = "http://127.0.0.1:11434"
ollama_model = "deepseek-coder:33b"
local_only_sources = []        # pins a source local even if routing is later turned on

# Off: escalating on a grade is a cloud call nobody asked for, which is the one thing
# the default policy exists to prevent. Turn it on and MuggleBot escalates on its own.
[reasoner.routing]
enabled = false               # true → grade every task and escalate on the grade
cleanup = false               # true → a `hard` local draft goes to `mid` to be corrected
cloud_fallback = false        # true → a local outage escalates instead of erroring

[context]
refresh_default = "6h"
urls = ["https://status.example.com"]
files = ["~/notes/architecture.md"]

# Authenticated URL sources name a credential in the `secrets` table; the token is
# resolved inside the fetch step and injected as a header. It never enters a journal.
[[context.authed_urls]]
url = "https://runbooks.internal/oncall"
credential = "runbooks"
header = "Authorization"

[mcp]
stdio = true
http_listen = "127.0.0.1:8787"

[ui]
listen = "127.0.0.1:8080"
```

> **Port collision, on purpose visible:** Restate's ingress default is also 8080.
> Pick one — the example above has to be resolved before it runs, and the loader
> errors on the overlap rather than letting the UI shadow the ingress. (Suggested:
> UI on `8081`.)

---

## Local development (Tilt)

MuggleBot is not Kubernetes: it's a local-first macOS app, so `tilt up` runs local
processes plus one container.

| Resource | What it is |
|---|---|
| `restate` | `docker.restate.dev/restatedev/restate` — ingress `8080`, admin + UI `9070`, node `5122`, data in `./data/restate`, vqueue flags set. Readiness on `http://127.0.0.1:9070/health`. |
| `backend` | `cargo build` then the binary, which serves the Restate endpoint on `9080` and the UI API/WS. |
| `restate-register` | `restate deployments register --force http://host.docker.internal:9080`, after both are up. Manual until the endpoint exists. |
| `ui` | Vite dev server, hot reload. |
| `test` / `clippy` / `fmt` | On-demand buttons. |

Two things worth knowing:

- **Re-registration on handler change.** Restate discovers handlers at
  registration. Adding a handler or changing a signature means re-registering;
  `--force` makes that idempotent. `register_on_boot` does it automatically in
  normal running, and the Tilt button is for when you want to see the discovery
  output.
- **Enabling vqueues needs a fresh cluster.** `tilt down && rm -rf data/restate
  && tilt up`. This costs only in-flight invocations, never the record — which is
  the whole reason the storage rule exists.

Requires the Restate CLI: `brew install restatedev/tap/restate`.

---

## Notifications (macOS)

- Native notifications (e.g. `mac-notification-sys` / `objc` bindings), not a
  polling banner hack.
- **Rule-driven, not firehose.** `min_severity`, quiet hours, and per-source
  filters decide what actually interrupts you.
- **Deduplicated per subject** — one notification per subject state change, not
  one per underlying signal, enforced by `last_notified` in the same exclusive
  handler that changed the state.
- **Actionable.** Click → open the subject in the LCARS UI (or deep-link straight
  to the source).
- **Red-alert.** A high-confidence live-assist flag maps to Critical, so it
  notifies even during quiet hours — "you're about to be wrong" is the one case
  worth interrupting for.

---

## Design principles

Drawn directly from the cited inspiration (see _References_). These are the
non-negotiables that keep an "AI ops helper" trustworthy.

1. **Copilot, not autopilot.** MuggleBot surfaces, correlates, and _proposes_.
   Any action that mutates a real system stays human-authorized — mechanically,
   via a durable gate (see _Human gates_), not via a dialog box that a restart
   forgets. This mirrors Google SRE's multi-layer safety: deterministic typed
   tools (not free-form shell), risk metadata per action, and a human
   confirmation gate.

2. **Attribute before you conclude.** Resolve what a signal is *about* and what
   else belongs to it before hypothesizing why. Grouping and timeline come before
   any hypothesis, precisely to avoid the red-herring trap that costs MTTM.

3. **Removed: the generic-mitigations catalog.** There used to be a catalog of
   _generic mitigations_ here — rollback, data-rollback, drain/redirect,
   quarantine, upsize, degrade, block-list — keyword-matched against a subject
   and surfaced as first-move suggestions, on the reasoning that mitigating
   before diagnosing is what shortens the visible part of an outage.

   It is gone, and the reason is worth keeping. Because it was keyword-matched
   from a fixed list, **every subject got roughly the same three cards** — the
   words "fails", "error", and "slow" are everywhere in engineering prose. The
   advice was true, generic, and therefore told an on-call engineer nothing they
   did not already know, while occupying the space where a real finding would
   have gone. Successive gates (incident-only, GitHub work items excluded,
   nature-of-signal rather than vocabulary) narrowed *when* it fired without
   fixing *what* it said.

   The principle survives in a different form: the live-assist and root-cause
   passes are prompted to produce a next step **grounded in the runbooks and past
   incidents attached to this subject**, and to say nothing when they have no such
   grounding. A suggestion that cites something specific is worth reading; one
   assembled from a taxonomy is not. If a first-move catalog comes back, it has to
   be keyed on the actual failure signature, not on the presence of the word
   "error".

4. **Explainability by construction.** Every summary, correlation, and suggestion
   cites the signals it came from. No black-box "trust me."

5. **Optimize for time-to-awareness / time-to-mitigation**, not time-to-fix. The
   win condition is that you knew, and knew fast — not that MuggleBot solved it
   for you.

6. **Audit everything.** What was surfaced, what was suggested, what you did.
   Local, append-only, inspectable — and now also the invocation journal, which
   records every step that ran, in order, with its result.

7. **Institutional memory, curated.** Past incidents and their resolutions are
   retained and made searchable so tomorrow's attribution is smarter than
   today's. Memory is editable, and you can feed in reference material as a
   refreshed, summarized context library — grounding you own and can inspect, not
   a black box.

8. **Local-first storage, local-first execution, local-first reasoning.** Signals,
   grounding, and credentials live on your machine (SQLite); the orchestrator
   runs on your machine (a Restate container); the default model runs on your
   machine (Ollama). Cloud tiers are an escalation you can see, queue, and switch
   off — not the baseline. There is no MuggleBot-operated backend.

9. **The record outlives the runtime.** Restate state is process state. Anything
   you would miss if `data/restate` were deleted belongs in SQLite. This is what
   keeps an experimental server feature from being a data-loss risk.

10. **Idempotent everything.** Every ingest carries a dedup key, every workflow
    has a key that makes a redundant run free, every handler can be retried.
    Durable execution retries by default and forever; a non-idempotent step under
    that regime is a bug with a timer on it.

---

## Roadmap

**Status: Phases 0–5 are implemented on the pre-Restate architecture.** Phase 6
is the data-model change this document describes. Reasoning degrades gracefully
when no LLM provider is reachable (deterministic attribution + summaries stand).
Semantic recall uses embeddings stored as `f32` BLOBs ranked in-process by cosine
similarity — exact and trivially fast at a curated store's scale — rather than a
native vector extension; same behavior, one fewer moving part.

**Phase 0 — Skeleton.** ✅ Rust daemon, TOML config, SQLite store, the `Signal`
model, one watcher end-to-end (GitHub), credential storage, macOS notifications.

**Phase 1 — All sources + board.** ✅ Slack (including alert channels) and Granola
watchers. Deterministic grouping. LCARS board + detail views over a live
WebSocket. Read-mostly triage.

**Phase 2 — MCP + LLM correlation.** ✅ MCP server (stdio + HTTP). LLM
relatedness / de-duplication, the relation graph, human overrides. Per-subject
context attach. Citations everywhere.

**Phase 3 — Grounding: memory & context.** ✅ SQLite-backed memory with a WebUI/MCP editor, the curated context library,
semantic recall across both.

**Phase 4 — Live assist & agent chat.** ✅ Live detection via your Slack id,
debounced re-analysis, grounded hints + flags driving red-alert, multimodal chat.

**Phase 5 — Reaching outside the notification stream.** ✅ Difficulty-graded
local-first routing; browser investigation; the code-derived repo index and
root-cause search; assigned-issue triage.

**Phase 6 — Restate: the durable data model.** ✅ Landed. Five virtual objects, seven
workflows, durable timers throughout, and scope-based concurrency limits. The
file-level record is in [MIGRATION.md](MIGRATION.md):

1. **Secrets, hardened.** ✅ Already in SQLite; the `secrets` rename, `0600`
   enforcement, a write-only API with `updated_at`, log scrubbing, and optional
   envelope encryption under `$MUGGLEBOT_MASTER_KEY`.
2. **Endpoint + local server.** ✅ `restate-sdk` 0.11, an endpoint served on
   `9080`, self-registration, the container and its buttons in Tilt.
3. **Subjects.** ✅ `Issue` / `PullRequest` / `SlackThread` objects; the ranked
   resolver; the `Thread` → subject-key migration, backfilled from the existing
   signals table. The board reads a projection, so the UI changed shape once.
4. **Ingest through the ingress**, with idempotency keys. ✅ Signals are recorded on
   their subject's object, deduped by `{source}:{external_id}:{version}`; each
   watcher is a `Watcher` object with its cursor in state and its cadence on a
   durable timer. `poll_loop` is deleted. Slack's socket stays a daemon task.
5. **Workflows.** ✅ All seven, keyed so a redundant run is a free key collision:
   `IssueTriage` `{issue}@{sha}`, `RootCause` `{subject}@{watermark}`, `PrCritique`
   `{issue}@{sha}`, `RepoIndex` `{org}@{bucket}`, `ContextIngest` `{id}@{etag}`,
   `BrowserRead` `{id}`, `Merge` `{keep}+{drop}`. A `Scheduler` object ticks the
   recurring ones, replacing five `tokio` loops and two claim-a-row workers.
6. **vqueues.** ✅ The rule book is applied from `[restate.limits]` on boot, and
   scoped invocations carry their scope through the scoped ingress path. Off by
   default (`[restate].vqueues`) while the feature is experimental.
7. **Human gates.** ✅ The mechanism (`restate/gate.rs`): a gated step blocks on a
   durable promise the UI resolves, so a pending decision survives a restart, has an
   id for the audit log, and shows on the board as blocked work. No gated action
   ships — that was always the point of building it first.

---

## Decisions

Resolved as the plan has firmed up:

- **Subjects → Restate virtual objects, keyed by upstream identity.** Issue > PR
  > Slack thread; Granola and Slack contribute context upward. Per-key exclusive
  handlers give serialized writes without a lock, and the key *is* the address,
  so every surface refers to a subject the same way.
- **Multi-step pipelines → Restate workflows**, keyed so that a redundant run is
  a free key collision (`{issue}@{sha}`, `{subject}@{watermark}`). Ingest stays a
  plain service — its exactly-once property comes from the ingress idempotency
  key, and a workflow instance per event would be lifecycle for nothing.
- **Dedup is three mechanisms, not one.** Ingress idempotency key (exactly-once
  per event) + deterministic attribution (the hierarchy) + LLM same/related/
  distinct (genuine ambiguity). The SQLite unique index remains as the
  long-horizon backstop beyond the idempotency retention window.
- **Restate holds the work in flight; SQLite holds the record.** Journals carry
  ids, not bodies. The board is a SQL query over a projection, because Restate
  state has no cross-key query. Deleting `restate-data` must cost only in-flight
  work.
- **Secrets → the SQLite DB**, not the macOS Keychain. The Keychain is scoped to a
  login session and a signed process identity, which a Restate-served endpoint
  can't rely on; and the DB already holds every signal body, so a stronger vault
  for the token than for the data it fetched is theatre. Mode `0600`, write-only
  APIs, optional envelope encryption under `$MUGGLEBOT_MASTER_KEY`, and **no
  secret ever crosses the Restate boundary**.
- **Event store, memory & context → SQLite** (via `rusqlite`, statically linked;
  FTS5 for keyword search). One embedded store covers the append-mostly signal
  log, the relation graph, the grounding stores, and semantic recall; SQLite folds
  all of it into one single-file, zero-ops store that any `sqlite3` tool can
  inspect. The write rate is trivial, so the single-writer model is a non-issue.
- **vqueues for concurrency, a token-bucket object for rate.** Concurrency limits
  express "one Ollama, one Chrome, don't burst GitHub"; they don't express
  requests-per-hour, so `GithubBudget` stays a virtual object.
- **Debounce → a durable timer on the subject**, 1 min after last activity with a
  5-min hard cap. The in-process timer lost every pending re-analysis on restart,
  which during development was most of them.
- **UI framework → SolidJS.** Fine-grained reactivity suits a high-frequency live
  board better than a virtual-DOM re-render cycle; TS-first, tiny bundle.
- **Alerts come from Slack, not Incident.io.** You designate `alert_channels`
  whose posts are treated as alert signals.
- **All reasoning → on-device Ollama; a cloud model only when the operator names one.**
  Difficulty routing exists and ships off: an always-on daemon deciding for itself to
  escalate is a bill arriving for work nobody asked for. Where the local model was actually
  producing garbage, a deterministic check against the source data fixed it — a guarantee
  rather than a better guess. Exactly one cloud-capable handle exists, with two callers: the
  chat pane's picker and the second-opinion button.
- **Poll cadences → per-source durable timer + adaptive backoff** on GitHub
  `X-RateLimit-*` / `Retry-After`.

## Open questions

- **Object state ceiling.** A very long-lived issue accumulates links, meeting
  refs, and counters. At what point does `IssueState` need to spill to SQLite and
  keep only a pointer — and is the answer just "cap the vectors and page the rest"?
- **Retention for workflow instances.** `IssueTriage` keyed per commit mints an
  instance per sha per issue. The keying is what makes redundant runs free, so the
  retention window has to be long enough for that to pay off and short enough not
  to accumulate forever. Days? Weeks?
- **vqueue graduation.** vqueues are experimental in 1.7 and need a fresh
  cluster. Do we ship them on by default, or default off with the hand-rolled
  semaphores as the fallback until the feature is stable?
- **Where the projection is written.** Subject handlers writing the board
  projection to SQLite is a write on every `record`. Alternative: the projection
  is rebuilt by a reader from the signals table and the objects are read
  lazily. The first is simpler, the second can't drift.
- **Late demotion cost.** When a `SlackThread` with a completed root-cause report
  is demoted under an issue, is the report carried over as context, re-run against
  the merged subject, or discarded? Carrying it is cheap and might be wrong.
- **Local-only model.** If you pin any `local_only_sources`, which Ollama model to
  run for them — quality vs. local latency.
- **Grounding budget.** How much retrieved memory + context to fold into a summary
  before it crowds out the actual signal (a top-k + max-tokens cap).
- **Red-alert calibration.** Keeping live-assist flags from crying wolf — the
  confidence threshold, and whether a flag should need corroboration from more
  than one grounding source before it escalates.
- **Chat vision routing.** Screenshots need a vision-capable model — default to
  Claude for chat; decide whether a local vision model is acceptable when the
  dropped content is sensitive.

---

## References / inspiration

- [How Google SREs use Gemini CLI to solve real-world outages](https://cloud.google.com/blog/topics/developers-practitioners/how-google-sres-use-gemini-cli-to-solve-real-world-outages)
  — the investigate → correlate → mitigate → postmortem loop; deterministic typed
  tools; copilot-not-autopilot; MTTM focus.
- [How Google SRE is using agentic AI to improve operations](https://cloud.google.com/blog/products/devops-sre/how-google-sre-is-using-agentic-ai-to-improve-operations)
  — explainability, graduated autonomy, context-enriched decisions, institutional
  memory via embeddings.
- [Generic Mitigations](https://www.oreilly.com/content/generic-mitigations/)
  — mitigate before diagnosing; the catalog of reversible first moves. Read, tried,
  and removed — see principle 3 for why a keyword-matched version of this idea
  produces identical advice on every subject.
- [Restate: service types](https://docs.restate.dev/foundations/services) — the
  Service / Virtual Object / Workflow distinction and when each applies.
- [Restate: context actions](https://docs.restate.dev/foundations/actions) —
  `ctx.run`, determinism, terminal vs transient errors, durable timers, signals.
- [Restate: databases and Restate](https://docs.restate.dev/guides/databases) —
  when state belongs in K/V vs a database; deterministic idempotency tokens and
  conditional writes.
- [Restate: idempotency & invocation lifecycle](https://docs.restate.dev/services/invocation/http)
  — the `idempotency-key` header, retention, attach.
- [Restate: cron / recurring tasks](https://docs.restate.dev/guides/cron) — the
  delayed self-send loop and its overlap caveat.
- [Restate: rate limiting](https://docs.restate.dev/guides/rate-limiting) — the
  token-bucket virtual object, and how it differs from flow control.
- [Restate 1.7 release notes](https://github.com/restatedev/restate/blob/main/release-notes/v1.7.0.md)
  — flow control, scopes, `restate rules`, the `sys_vqueue*` tables, experimental
  flags.
- [Restate Rust SDK](https://docs.rs/restate-sdk/latest/restate_sdk/) — `#[object]`
  / `#[workflow]` macros, context types, endpoint serving.

---

## Working in this repo (for humans and agents)

Conventions:

- **Rust** for the backend/daemon (`tokio` async), **TypeScript** for the UI. No
  third language without a reason.
- Keep watchers isolated and normalizing — nothing source-specific leaks past
  ingest. Each watcher separates its HTTP `poll` from a pure `normalize_*`
  function that's unit-tested **without Restate and without a network**.
- One implementation of each capability lives in `src/tools.rs`; the web API, MCP
  server, and agent chat all dispatch through it, so they never drift. Tools call
  Restate handlers rather than writing subject state directly.
- **Restate rules, enforced in review:** every external call inside `ctx.run`;
  nothing non-deterministic outside it; errors classified transient vs
  `TerminalError` deliberately; handler payloads carry ids, not bodies; no secret
  in a handler argument, in object state, or in a `ctx.run` result.
- Comment sparingly; make the code self-documenting. Explain _why_, never _what_.

Quality gates (all green):

```sh
cargo fmt
cargo clippy --all-targets     # warning-free
cargo test                     # backend unit/integration tests
cd ui && npx tsc --noEmit && npm run build   # UI typecheck + build
```

Handler logic is tested by calling the inner functions directly — the `ctx.run`
bodies are ordinary async functions and should stay that way, so the tests need
no Restate server. Integration tests that do need one bring up the container and
address it through the ingress.

Module map: `signal` (the normalized type, `SubjectKey`, `ResolutionKey`) ·
`store` (SQLite: signals, artifacts, relation edges, memory, context, tags,
hints, health, **secrets**) · `restate/objects/{issue,pull_request,slack_thread,
watcher,repo_card,context_source}` · `restate/workflows/{issue_triage,root_cause,
pr_critique,browser_read,repo_index,merge,context_ingest}` ·
`restate/{ingest,endpoint,scopes}` · `attribution` (the ranked resolver) ·
`correlation/llm` (the `Analyst`: same/related/distinct) ·
`watchers/{github,slack,granola,assigned}` (poll + pure normalizers) ·
`reasoner/{ollama,cli,api,router,cache}` (+ `MockReasoner`) · `embed` (hash/Ollama
embedders, cosine KNN) · `tags` · `memory` / `context` (grounding) · `comments` (merit scoring) · `checkout` · `repos` · `tools` (shared
surface) · `mcp` (stdio + HTTP) · `live` (live assist) · `chat` (agent) · `server`
(HTTP/WS) · `event` (WS bus) · `notify` · `secrets`.
