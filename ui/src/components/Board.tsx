import { createMemo, createSignal, For, type JSX, Show } from "solid-js";
import { api } from "../api";
import { isBusy, patchHandled, subjects } from "../state";
import type { Signal, SubjectView } from "../types";

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
  return m ? `${m[1]}#${m[2]}` : t.key;
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
function Row(props: {
  t: SubjectView;
  onOpen: (key: string) => void;
  /// Triage the focused row from the keyboard. Working a lane shouldn't mean
  /// travelling to a button on every row.
  onKey?: (t: SubjectView, key: string) => void;
  actions: (t: SubjectView) => JSX.Element;
}) {
  const t = () => props.t;
  const open = () => props.onOpen(t().key);
  return (
    <div
      class={`lane-row rank-${t().rank}`}
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
      <span class="row-kind">{KIND_LABEL[t().rank]}</span>
      <span class="row-mid">
        <span class="row-titleline">
          <span class="row-title">{displayTitle(t())}</span>
          <span class="row-ref">{ref(t())}</span>
          {/* Signed off, or blocked. On the board this replaces asking for attention:
              once a human has said yes, the answer to "does this need me" has been
              given, and the row should say so rather than sit in Decide.

              Two states, not one. `gates_passed` is the stronger claim — approved *and*
              nothing still failing — and it is the one an operator can act on by not
              acting. A bare "approved" on a PR with red CI would read as finished when
              it isn't, so the badge only makes the stronger claim when it's true. */}
          <Show when={t().gates_passed}>
            <span
              class="badge badge-cleared"
              data-tip="Approved, and nothing failing — nothing to do"
            >
              nothing to do
            </span>
          </Show>
          <Show when={t().review_state === "approved" && !t().gates_passed}>
            <span
              class="badge badge-approved"
              data-tip="Approved, but something is still failing"
            >
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
        </span>
        {/* The one line the board is for. A subject with no usable summary says so
            rather than showing a truncated event body dressed as a conclusion. */}
        <Show
          when={t().headline}
          fallback={<span class="row-headline none">Not summarised yet</span>}
        >
          <span class="row-headline">{t().headline}</span>
        </Show>
      </span>
      <span class="row-who">{provenance(t())}</span>
      <time class="row-when" data-tip={new Date(t().updated_at).toLocaleString()}>
        {ago(t().updated_at)}
      </time>
      <span class="row-actions" onClick={(e) => e.stopPropagation()}>
        {props.actions(t())}
      </span>
    </div>
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
  const inLane = (id: Lane) => {
    const rows = active().filter((t) => lane(t) === id);
    return TYPES.map((ty) => ({
      ...ty,
      rows: rows
        .filter((t) => t.rank === ty.rank)
        .sort((a, b) => b.updated_at.localeCompare(a.updated_at)),
    })).filter((g) => g.rows.length);
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
            const groups = createMemo(() => inLane(l.id));
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
                  <Show
                    when={count()}
                    fallback={<p class="lane-empty">{l.empty}</p>}
                  >
                    <For each={groups()}>
                      {(g) => (
                        <>
                          {/* Only worth a heading when the lane actually holds more
                              than one type — a lone "Issues" label over every row in
                              an issues-only lane is a label with no alternative. */}
                          <Show when={groups().length > 1}>
                            <h3 class="type-head">
                              {g.label}
                              <span class="lane-count">{g.rows.length}</span>
                            </h3>
                          </Show>
                          <For each={g.rows}>
                            {(t) => (
                              <Row
                                t={t}
                                onOpen={props.onOpen}
                                onKey={rowKey}
                                actions={(t) => (
                                  <>
                                    <Show
                                      when={t.handled === "acknowledged"}
                                      fallback={
                                        <button
                                          data-tip="Acknowledge — keeps it on the board, out of Decide (e)"
                                          onClick={() =>
                                            triage(t, "acknowledged")
                                          }
                                        >
                                          Ack
                                        </button>
                                      }
                                    >
                                      <button onClick={() => reopen(t)}>
                                        Un-ack
                                      </button>
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
                        </>
                      )}
                    </For>
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
            <For each={handled()}>
              {(t) => (
                <Row
                  t={t}
                  onOpen={props.onOpen}
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
          </Show>
        </section>
      </Show>
    </div>
  );
}
