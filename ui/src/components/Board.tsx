import {
  createMemo,
  createResource,
  createSignal,
  For,
  type JSX,
  onCleanup,
  Show,
} from "solid-js";
import { api } from "../api";
import {
  isBusy,
  patchHandled,
  reviewAs,
  setReviewAs,
  subjects,
} from "../state";
import type { PersonaPrediction, Signal, SubjectView } from "../types";

/// The three lanes, in the order an operator works them.
///
/// This replaces sorting by a numeric attention score. The score was the right idea
/// with the wrong output: on a real board every subject scores identically — same
/// severity, same `needed`, same reason — so the resulting order is one the operator
/// cannot account for. A lane is a claim you can check instead: *something is asking
/// me now*, *this is mine to schedule*, *nothing here wants me*.
type Lane = "decide" | "mine" | "clear";

const LANES: { id: Lane; label: string; empty: string }[] = [
  { id: "decide", label: "Decide", empty: "Nothing is waiting on you." },
  { id: "mine", label: "On your plate", empty: "Nothing assigned to you." },
  { id: "clear", label: "Nothing to do", empty: "" },
];

/// Signals still standing. `upstream_gone` is the reconciler reporting that the
/// notification cleared upstream, so a review that has since been answered must not
/// hold a subject in Decide for ever.
function standing(t: SubjectView): Signal[] {
  return t.signals.filter((s) => !s.upstream_gone);
}

/// Which lane a subject belongs in, ranked by who is blocked.
///
/// A flag or a failure is asking right now; a review request or a mention is asking
/// *you* specifically; an assignment is yours to schedule. Acknowledging something
/// is a statement that it no longer needs deciding, so it leaves Decide — that is
/// what the operator meant by pressing the button.
function lane(t: SubjectView): Lane {
  if (t.handled === "acknowledged" || !t.attention.needed) return "clear";
  if (t.severity === "critical" || t.severity === "warning") return "decide";
  const kinds = new Set(standing(t).map((s) => s.kind));
  if (
    kinds.has("review_requested") ||
    kinds.has("mention") ||
    kinds.has("ci_failure")
  ) {
    return "decide";
  }
  if (kinds.has("assigned")) return "mine";
  return "clear";
}

/// The kind column, source first.
///
/// `Issue` and `PR` alone said what a row *was* but not where it lived, which is the half
/// that decides where you go to act on it — and on a board that also carries Slack threads,
/// an unqualified "Issue" reads as though the source is the one thing not worth stating.
/// Slack was already source-qualified; these two now match it.
export const KIND_LABEL: Record<SubjectView["rank"], string> = {
  issue: "GitHub Issue",
  pull_request: "GitHub PR",
  slack_thread: "Slack",
  // Present for completeness, not because this board renders one: the main board's read
  // excludes incidents server-side (`board_views`). It is here so the map stays exhaustive
  // — the compiler asked for it, which is the point of typing it as a `Record`.
  incident: "Incident",
};

/// Types in the order a lane presents them, and what the group is called.
///
/// Pull requests lead: a review request is someone else blocked on you, where an
/// issue is usually yours to schedule. Mixing the two in one recency-ordered list
/// meant the reader had to re-establish "what kind of thing is this" on every row.
const TYPES: { rank: SubjectView["rank"]; label: string }[] = [
  { rank: "pull_request", label: "Pull requests" },
  { rank: "issue", label: "Issues" },
  { rank: "slack_thread", label: "Slack threads" },
];

/// The title with what the row already says stripped out of it.
///
/// GitHub notification titles arrive as `PR: Restate-cloud image bump to PR1200
/// (restatedev/nuon-byoc#140)` — a kind the kind column now shows, and a reference
/// the ref chip now shows. Printing all three was three copies of the same fact.
export function displayTitle(t: SubjectView): string {
  let title = t.title.replace(/^(pull request|issue|pr)\s*:\s*/i, "");
  const num = t.key.match(/[#!~](\d+)$/)?.[1];
  if (num) {
    // Only a parenthetical naming *this* subject; one naming a different issue is
    // information the operator wants.
    title = title.replace(
      new RegExp(`\\s*\\([^()]*[#!~]${num}\\)\\s*$`, "i"),
      "",
    );
  }
  return title.trim() || t.title;
}

/// The compact upstream reference: `nuon-byoc#140`. The key's `!` and `~` encode
/// rank for the router's benefit; a human reads every GitHub artifact as `#`.
export function ref(t: SubjectView): string {
  const m = t.key.match(/^(?:[^/]+)\/([^/#!~]+)[#!~](\d+)$/);
  if (m) return `${m[1]}#${m[2]}`;
  // A Slack key is `channel_id/thread_ts` — `C0744EUMHFF/1785793056.122949`, which is 29
  // characters of nothing. The signals carry the channel *name* as a resolution key and the
  // thread_ts is a unix time, so both halves can be said in a way a person can act on.
  const slack = t.key.match(/^(C|D|G)[A-Z0-9]+\/(\d+)\.\d+$/);
  if (slack) {
    const channel = t.signals
      .flatMap((s) => s.keys)
      .find((k) => k.kind === "channel")?.value;
    const when = new Date(Number(slack[2]) * 1000);
    const stamp = Number.isNaN(when.getTime())
      ? ""
      : when.toLocaleDateString(undefined, { day: "numeric", month: "short" });
    const where = channel ? `#${channel.replace(/^#/, "")}` : "slack";
    return stamp ? `${where} · ${stamp}` : where;
  }
  return t.key;
}

/// Who last moved this, and how much there is of it.
function provenance(t: SubjectView): string {
  const actor = [...t.signals]
    .sort((a, b) => b.occurred_at.localeCompare(a.occurred_at))
    .find((s) => s.actor)?.actor;
  const n = t.signals.length;
  const events = `${n} event${n === 1 ? "" : "s"}`;
  return actor ? `${actor} · ${events}` : events;
}

/// Coarse relative time. On a row the question is "is this today's problem", not
/// which second it landed — the exact timestamp is one hover away.
function ago(iso: string): string {
  const mins = Math.round((Date.now() - new Date(iso).getTime()) / 60000);
  if (mins < 1) return "now";
  if (mins < 60) return `${mins}m`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.round(hours / 24);
  return days < 7 ? `${days}d` : `${Math.round(days / 7)}w`;
}

/// One subject as one row: what it is, what it wants, who moved it, when.
///
/// Everything this used to also render — the summary, every member event, the tags,
/// the attempts, both explanations, the AI facet strip — is in the click-in view
/// already. Printing it here as well cost 533px a subject and put one card on the
/// screen at a time, which is not a board.
/// The short type marker. The full words were a 102px fixed column stating a fact the ref
/// sigil and the (former) group heading also stated — three copies, one fact.
const TYPE_MARK: Record<SubjectView["rank"], string> = {
  pull_request: "PR",
  issue: "ISSUE",
  slack_thread: "SLACK",
  incident: "INC",
};

/// What the AI has established, as filled or hollow pills.
///
/// Documented in AGENTS.md as part of what the board leads with, and absent from it until now
/// even though every field was already on the wire. A row of hollow pills is the "nobody has
/// looked at this yet" signal, which was otherwise only discoverable by opening the subject and
/// finding empty panels.
///
/// `⌂` counts passes run on this machine and `☁` ones that left it. Under local-by-default the
/// second should be zero unless somebody pressed 2ND OPINION — which makes a non-zero `☁` worth
/// seeing, and on a real board it was 1–2 on nearly every card with nothing saying so.
/// One sentence naming what has and has not been done, for the strip's own tooltip — so the
/// whole row is readable at once rather than dot by dot.
function facetSummary(facets: { key: string; on: boolean; tip: string }[]): string {
  const done = facets.filter((f) => f.on).map((f) => f.tip);
  const not = facets.filter((f) => !f.on).map((f) => f.tip);
  const parts = [];
  if (done.length) parts.push(`done: ${done.join(", ")}`);
  if (not.length) parts.push(`not yet: ${not.join(", ")}`);
  return parts.join(" · ") || "nothing looked at yet";
}

function AiStrip(props: { t: SubjectView }) {
  const d = () => props.t.attention.decorated;
  const facets = () => [
    { key: "sum", on: d().summary, tip: "summarised" },
    { key: "tags", on: d().tags, tip: "tagged" },
    { key: "dash", on: d().dashboard, tip: "dashboard read" },
    { key: "rc", on: d().root_cause === "complete", tip: `root cause: ${d().root_cause ?? "not run"}` },
    { key: "tri", on: d().triage === "complete", tip: `triage: ${d().triage ?? "not run"}` },
    { key: "prs", on: d().prs_judged > 0, tip: `${d().prs_judged} pull request(s) judged` },
  ];
  return (
    <span class="card-ai">
      {/* Dots, not labels. Six 9px abbreviations — SUM TAGS DASH RC TRI PRS — was a row of
          things to decode on every card, which is the clutter this rework exists to remove. The
          filled/hollow signal is what carries the meaning ("has anything looked at this?"), and
          a dot carries it without asking anyone to read it. The name is on hover. */}
      <span class="facet-dots" data-tip={facetSummary(facets())}>
        <For each={facets()}>
          {(f) => <i class="facet-dot" classList={{ on: f.on }} data-tip={f.tip} />}
        </For>
      </span>
      <Show when={d().local_passes}>
        <span class="pass-count" data-tip="passes run on this machine">
          ⌂{d().local_passes}
        </span>
      </Show>
      <Show when={d().cloud_passes}>
        <span
          class="pass-count cloud"
          data-tip="passes that left this machine — should be zero unless you asked for a second opinion"
        >
          ☁{d().cloud_passes}
        </span>
      </Show>
    </span>
  );
}

/// One subject as a card.
///
/// Each block answers exactly one question, in the order they get asked:
///
/// | Block | Question |
/// |---|---|
/// | head | what kind of thing is this, and how stale |
/// | title | what is it |
/// | why | why is it on my board |
/// | headline | what is already known |
/// | foot | what has the AI done, and what can I do |
///
/// The two things a card buys over the row it replaces: the **title can wrap** rather than
/// ellipsing at a fixed column, and the **reason can be shown at all** — `attention.reason` was
/// on the wire and had nowhere to go, so a lane said *Decide* without saying what was asking.
function Card(props: {
  t: SubjectView;
  onOpen: (key: string) => void;
  /// Triage the focused card from the keyboard. Working a lane shouldn't mean travelling to a
  /// button on every card.
  onKey?: (t: SubjectView, key: string) => void;
  actions: (t: SubjectView) => JSX.Element;
  verdict?: PersonaPrediction | null;
  onReview?: (t: SubjectView) => void;
  reviewing?: boolean;
}) {
  const t = () => props.t;
  const open = () => props.onOpen(t().key);
  return (
    <article
      class={`card rank-${t().rank}`}
      classList={{
        acked: t().handled === "acknowledged",
        dim: t().handled === "resolved" || t().handled === "snoozed",
        live: t().live,
      }}
      role="button"
      tabindex="0"
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          open();
          return;
        }
        props.onKey?.(t(), e.key);
      }}
    >
      <div class="card-head">
        <span class="card-type">{TYPE_MARK[t().rank]}</span>
        <span class="card-ref">{ref(t())}</span>
        <span class="card-spacer" />
        <Show when={t().gates_passed}>
          <span class="badge badge-cleared" data-tip="Approved, and nothing failing">
            nothing to do
          </span>
        </Show>
        <Show when={t().review_state === "approved" && !t().gates_passed}>
          <span class="badge badge-approved" data-tip="Approved, but something is still failing">
            approved
          </span>
        </Show>
        <Show when={t().review_state === "changes_requested"}>
          <span class="badge badge-blocked">changes requested</span>
        </Show>
        <Show when={t().live}>
          <span class="badge badge-live">live</span>
        </Show>
        <Show when={isBusy(t().key)}>
          <span class="badge badge-ai" data-tip="an AI pass is running or queued">
            <span class="thinking-dots">
              <i />
              <i />
              <i />
            </span>
          </span>
        </Show>
        <time class="card-when" data-tip={new Date(t().updated_at).toLocaleString()}>
          {ago(t().updated_at)}
        </time>
      </div>

      <h3 class="card-title">{displayTitle(t())}</h3>

      {/* Why this is on the board. The single most valuable thing that was on the wire and
          nowhere on screen: every card that needs you carries a reason — "you're in this one",
          "critical" — and the lane heading alone could not say which. */}
      <Show when={t().attention.needed}>
        <p class="card-why">
          <span class="why-label">needs you</span>
          <Show when={t().attention.reason}>
            <span class="why-reason">{t().attention.reason}</span>
          </Show>
        </p>
      </Show>

      {/* What is already known — and **only when it says something new.** The backend now
          refuses a headline that answers a person, reports that a thing exists, or repeats the
          title (`subject::headline_is_noise`), so a card with no line here is a card with
          nothing to add rather than one with a placeholder. Four of nine cards on a real board
          were carrying such a line. */}
      <Show when={t().headline}>
        <p class="card-headline">{t().headline}</p>
      </Show>

      {/* The predicted reaction, when reviewing the board as somebody. */}
      <Show when={props.verdict}>
        <p class="card-verdict">
          <PersonaVerdict p={props.verdict!} />
        </p>
      </Show>

      <div class="card-foot" onClick={(e) => e.stopPropagation()}>
        <AiStrip t={t()} />
        <Show when={t().tags.length}>
          <span class="card-tags">
            <For each={t().tags.slice(0, 3)}>{(g) => <i class="chip tag">{g}</i>}</For>
            <Show when={t().tags.length > 3}>
              <i class="muted" data-tip={t().tags.join(", ")}>
                +{t().tags.length - 3}
              </i>
            </Show>
          </span>
        </Show>
        <span class="card-spacer" />
        {/* Always visible, not hover-only. The row this replaces hid its actions behind
            `visibility: hidden`, which also hid `review as` — an affordance nobody could
            discover by looking. */}
        <span class="card-actions">
          <Show when={props.onReview && !props.verdict}>
            <button
              disabled={props.reviewing}
              data-tip="Predict what they would do about this, from their profile"
              onClick={() => props.onReview!(t())}
            >
              {props.reviewing ? "…" : "review as"}
            </button>
          </Show>
          {props.actions(t())}
        </span>
      </div>
    </article>
  );
}

/// One persona's predicted verdict, compressed to a chip.
///
/// Three states, and keeping them distinct is the whole value on a board row:
///
/// - **A verdict** (`request changes` / `comment` / `approve`) on a pull request — the thing you
///   scanned the lane to find.
/// - **Would not engage** — dimmed, because it is a real answer and not a missing one. On a
///   sweep this is most rows, and it must not read as "not predicted yet".
/// - **Engaged with something to say** on an issue or thread, where there is no verdict
///   vocabulary — the count of points stands in for it.
///
/// `data-tip` carries the note they would write, so hovering answers "what would they say"
/// without leaving the board. Anything more belongs in the detail view, where the citations and
/// caveats are.
function PersonaVerdict(props: { p: PersonaPrediction }) {
  const p = () => props.p;
  const label = () => {
    if (!p().would_engage) return "wouldn't engage";
    if (p().recommendation) return p().recommendation!.replace(/_/g, " ");
    return p().points.length ? `${p().points.length} point(s)` : "would comment";
  };
  return (
    <span
      class="badge persona-verdict"
      classList={{
        quiet: !p().would_engage,
        [`rec-${p().recommendation}`]: !!p().recommendation,
      }}
      data-tip={`${p().summary || "no note"}${
        p().caveats.length ? ` — ${p().caveats[0]}` : ""
      }`}
    >
      {label()}
    </span>
  );
}

/// Triage is one call on the subject: patch the store the board renders so it feels
/// instant, then tell the backend, which rebroadcasts the authoritative board.
async function triage(t: SubjectView, handled: "acknowledged" | "snoozed") {
  patchHandled(t.key, handled);
  await api.setHandled(t.key, handled).catch(() => {});
}

/// Bring handled work back onto the board, fully open.
///
/// Un-handling means un-handled. This used to set `acknowledged`, which is *still
/// handled*, so un-snoozing left the subject muted with no way back to open at all.
async function reopen(t: SubjectView) {
  patchHandled(t.key, "open");
  await api.setHandled(t.key, "open").catch(() => {});
}

export default function Board(props: {
  onOpen: (id: string) => void;
  // When set, show only threads carrying a signal from this source.
  sourceFilter?: string | null;
}) {
  // Resolved and snoozed subjects are handled — off the main board (the backend
  // already excludes them; doing it here too makes triage feel instant). A subject
  // merged away forwards its activity to the canonical one, so it isn't a row either.
  const active = createMemo(() =>
    Object.values(subjects)
      .filter(
        (t) =>
          t.handled !== "resolved" && t.handled !== "snoozed" && !t.same_as,
      )
      .filter(
        (t) =>
          !props.sourceFilter ||
          t.signals.some((s) => s.source === props.sourceFilter),
      ),
  );

  // A lane, split by type and newest-first within each.
  //
  // Type is the outer key because reviewing a pull request and scheduling an issue are
  // different kinds of work: batching them means one pass over the PRs and one over the
  // issues, rather than switching mode on every row. The lane still decides *whether*
  // the work is urgent; type decides what doing it involves.
  // A lane, sorted by type and then newest-first — **flat**, with no per-type heading.
  //
  // AGENTS.md argued for the heading on the grounds that reviewing a pull request and scheduling
  // an issue are different work, so batching them means one pass over each. That reasoning holds
  // and the *sort* is what delivers it; the heading only labelled the boundary. Measured on a
  // real board it cost one heading per 1.8 cards — `DECIDE 3` over `Pull requests 1` over a
  // single card — and each card already names its own type. So the batch survives and the
  // chrome does not.
  const inLane = (id: Lane) => {
    const order = new Map(TYPES.map((ty, i) => [ty.rank, i]));
    return active()
      .filter((t) => lane(t) === id)
      .sort(
        (a, b) =>
          (order.get(a.rank) ?? 9) - (order.get(b.rank) ?? 9) ||
          b.updated_at.localeCompare(a.updated_at),
      );
  };

  const laneCount = (id: Lane) =>
    active().filter((t) => lane(t) === id).length;

  const decideCount = createMemo(
    () => active().filter((t) => lane(t) === "decide").length,
  );

  // Handled subjects live outside the reconciled live store (the WS `board` event
  // would otherwise wipe them). Fetched on demand when the user reveals them.
  const [showHandled, setShowHandled] = createSignal(false);
  const [handled, setHandled] = createSignal<SubjectView[]>([]);

  async function loadHandled() {
    const all = await api.listSubjects(false).catch(() => [] as SubjectView[]);
    setHandled(
      all
        .filter((t) => t.handled === "resolved" || t.handled === "snoozed")
        .sort((a, b) => b.updated_at.localeCompare(a.updated_at)),
    );
  }

  async function toggleHandled() {
    const next = !showHandled();
    setShowHandled(next);
    if (next) await loadHandled();
    else setHandled([]);
  }

  // Reset the board: delete persisted events and their derived analysis. The
  // authoritative `board` WS event reconciles the store on its own.
  const [resetting, setResetting] = createSignal(false);
  async function resetBoard() {
    if (
      !confirm(
        "Delete every persisted board event?\n\nThis removes signals and their thread analysis from MuggleBot's database. It does not change GitHub, Slack, or other sources; still-active upstream notifications can return on a later sync.",
      )
    )
      return;
    setResetting(true);
    try {
      await api.resetBoard();
      if (showHandled()) await loadHandled();
    } catch {
      // Non-fatal: the board simply stays as-is if the reset request fails.
    } finally {
      setResetting(false);
    }
  }

  const rowKey = (t: SubjectView, key: string) => {
    if (key === "e") triage(t, "acknowledged");
    if (key === "s") triage(t, "snoozed");
  };

  // ---- reviewing the board as somebody -------------------------------------

  // Only personas with a profile. One without traits predicts nothing — the backend refuses,
  // so offering it here would be a button that returns a refusal.
  const [personas] = createResource(() => api.listPersonas());
  const profiled = () => personas()?.personas.filter((p) => p.traits > 0) ?? [];
  const personaName = (slug: string) =>
    profiled().find((p) => p.slug === slug)?.display_name ?? slug;

  // Every prediction this persona has made, in one call, keyed by subject. One request for the
  // whole board rather than one per row — a hundred rows would otherwise be a hundred round
  // trips to render a chip.
  const [verdicts, { refetch: refetchVerdicts }] = createResource(
    () => reviewAs(),
    async (slug) => {
      const rows = await api.predictionsBy(slug).catch(() => [] as PersonaPrediction[]);
      const map: Record<string, PersonaPrediction> = {};
      for (const p of rows) {
        // Newest wins when a subject has both a code-review and an issue-response prediction.
        const prev = map[p.subject_key];
        if (!prev || p.created_at > prev.created_at) map[p.subject_key] = p;
      }
      return map;
    },
  );

  const [reviewing, setReviewing] = createSignal<string[]>([]);
  let poll: number | undefined;
  onCleanup(() => window.clearInterval(poll));

  /// Predict one row as the selected persona.
  ///
  /// Submitted as a workflow, so this returns before the answer exists — hence the poll. The
  /// key is `{persona}@{kind}@{model}@{subject}@{watermark}`, so pressing it twice on an
  /// unchanged subject is a refused duplicate rather than a second pass.
  const review = async (t: SubjectView) => {
    const slug = reviewAs();
    if (!slug || reviewing().includes(t.key)) return;
    setReviewing((r) => [...r, t.key]);
    try {
      await api.predictPersonas(t.key, [slug]);
    } catch {
      // Non-fatal: the row simply keeps its "review as" button.
    }
    // Poll until it lands, bounded — a pass that dies terminally must not poll forever.
    window.clearInterval(poll);
    let ticks = 0;
    poll = window.setInterval(async () => {
      ticks += 1;
      await refetchVerdicts();
      const done = !!verdicts()?.[t.key];
      if (done || ticks > 30) {
        window.clearInterval(poll);
        setReviewing((r) => r.filter((k) => k !== t.key));
      }
    }, 4000);
  };

  /// Whether a row is worth offering a prediction for.
  ///
  /// Issues and pull requests — "review as" is a question about a change or a problem. A Slack
  /// thread gets an engagement prediction in the detail view, but offering it on every alert row
  /// would put a button on the noisiest half of the board for the least useful answer.
  const predictable = (t: SubjectView) =>
    t.rank === "issue" || t.rank === "pull_request";

  const rowProps = (t: SubjectView) =>
    reviewAs() && predictable(t)
      ? {
          verdict: verdicts()?.[t.key] ?? null,
          onReview: review,
          reviewing: reviewing().includes(t.key),
        }
      : {};

  return (
    <div class="board">
      <div class="board-head">
        <h1 class="board-title">Board</h1>
        <span class="board-counts">
          {active().length} open
          <Show when={decideCount()}>
            {" · "}
            <b>{decideCount()} to decide</b>
          </Show>
          <Show when={props.sourceFilter}>
            {" · "}
            {props.sourceFilter}
          </Show>
        </span>
        <span class="board-spacer" />
        {/* Pick the person once, then sweep. Reviewing as somebody is a mode you work in for a
            few minutes — per-row pickers would mean choosing them again on every card, and the
            question ("where would Pavel push back?") is about the lane, not the row. */}
        <Show when={profiled().length}>
          <select
            class="review-as"
            classList={{ on: !!reviewAs() }}
            value={reviewAs() ?? ""}
            data-tip="Predict how one person would respond to each issue and pull request"
            onChange={(e) => setReviewAs(e.currentTarget.value || null)}
          >
            <option value="">review as…</option>
            <For each={profiled()}>
              {(p) => <option value={p.slug}>as {p.display_name}</option>}
            </For>
          </select>
        </Show>
        <button classList={{ on: showHandled() }} onClick={toggleHandled}>
          {showHandled() ? "Hide handled" : "Show handled"}
        </button>
        <Show when={showHandled()}>
          <button onClick={loadHandled}>Refresh</button>
        </Show>
        <button class="danger" disabled={resetting()} onClick={resetBoard}>
          {resetting() ? "Resetting…" : "Reset board"}
        </button>
      </div>

      <Show
        when={active().length}
        fallback={
          <div class="empty">
            {props.sourceFilter
              ? `Nothing from ${props.sourceFilter}.`
              : "Awaiting signals…"}
          </div>
        }
      >
        <For each={LANES}>
          {(l) => {
            const cards = createMemo(() => inLane(l.id));
            const count = createMemo(() => laneCount(l.id));
            return (
              // A lane with nothing in it is worth one line — "nothing is waiting on
              // you" is the answer to the question the board was opened to ask.
              <Show when={count() || l.empty}>
                <section class={`lane lane-${l.id}`}>
                  <h2 class="lane-head">
                    {l.label}
                    <Show when={count()}>
                      <span class="lane-count">{count()}</span>
                    </Show>
                  </h2>
                  <Show when={count()} fallback={<p class="lane-empty">{l.empty}</p>}>
                    {/* One grid, sorted by type then recency. The per-type headings are gone;
                        the sort keeps the batch and each card names its own type. */}
                    <div class="card-grid">
                      <For each={cards()}>
                        {(t) => (
                          <Card
                            t={t}
                            onOpen={props.onOpen}
                            {...rowProps(t)}
                            onKey={rowKey}
                            actions={(t) => (
                              <>
                                <Show
                                  when={t.handled === "acknowledged"}
                                  fallback={
                                    <button
                                      data-tip="Acknowledge — keeps it on the board, out of Decide (e)"
                                      onClick={() => triage(t, "acknowledged")}
                                    >
                                      Ack
                                    </button>
                                  }
                                >
                                  <button onClick={() => reopen(t)}>Un-ack</button>
                                </Show>
                                <button
                                  data-tip="Snooze — off the board until it moves (s)"
                                  onClick={() => triage(t, "snoozed")}
                                >
                                  Snooze
                                </button>
                              </>
                            )}
                          />
                        )}
                      </For>
                    </div>
                  </Show>
                </section>
              </Show>
            );
          }}
        </For>
      </Show>

      <Show when={showHandled()}>
        <section class="lane">
          <h2 class="lane-head">
            Handled
            <Show when={handled().length}>
              <span class="lane-count">{handled().length}</span>
            </Show>
          </h2>
          <Show
            when={handled().length}
            fallback={<p class="lane-empty">Nothing handled yet.</p>}
          >
            <div class="card-grid">
            <For each={handled()}>
              {(t: SubjectView) => (
                <Card
                  t={t}
                  onOpen={props.onOpen}
                  {...rowProps(t)}
                  actions={(t) => (
                    // Named for what it undoes, so the operator can see which state
                    // they are leaving rather than reading "Reopen" against three.
                    <button
                      onClick={async () => {
                        await reopen(t);
                        setHandled((prev) =>
                          prev.filter((x) => x.key !== t.key),
                        );
                      }}
                    >
                      {t.handled === "snoozed" ? "Un-snooze" : "Un-resolve"}
                    </button>
                  )}
                />
              )}
            </For>
            </div>
          </Show>
        </section>
      </Show>
    </div>
  );
}
