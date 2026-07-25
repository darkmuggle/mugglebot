import { createMemo, createSignal, For, type JSX, Show } from "solid-js";
import { api } from "../api";
import { entityHref } from "../entities";
import { AttentionBadge } from "./Attention";
import { renderMarkdown } from "../markdown";
import { patchThreadSignalState, threads } from "../state";
import { SEVERITY_RANK, type Signal, type ThreadView } from "../types";
import { SignalModal } from "./SignalModal";

function sources(t: ThreadView): string[] {
  return [...new Set(t.signals.map((s) => s.source))];
}

/// Member signals newest-first — the topic section lists them as event rows.
function events(t: ThreadView): Signal[] {
  return [...t.signals].sort((a, b) => b.occurred_at.localeCompare(a.occurred_at));
}

/// True when an event carries content beyond its title, so a pop-out is useful.
function hasDetails(s: Signal): boolean {
  const body = (s.body ?? "").trim();
  return !!body && body !== (s.title ?? "").trim();
}

/// Higher = more deserving of attention now: severity, lifted a little when live,
/// pushed down once triaged (acknowledged) or snoozed.
function attention(t: ThreadView): number {
  let score = SEVERITY_RANK[t.severity];
  if (t.live) score += 0.5;
  if (t.state === "acknowledged") score -= 1.5;
  if (t.state === "snoozed") score -= 3;
  return score;
}

async function ackAll(t: ThreadView) {
  // Snapshot the ids up front — the thread store mutates as we patch.
  const ids = t.signals.filter((s) => s.state === "unseen").map((s) => s.id);
  for (const id of ids) {
    patchThreadSignalState(t.id, id, "acknowledged"); // instant, in the store the board renders
    await api.setSignalState(id, "acknowledged").catch(() => {});
  }
}

/// Snooze the whole thread: mute every non-resolved signal so the thread drops
/// off the board and stays hidden as new activity lands (the backend keeps new
/// signals muted until the user re-engages).
async function snoozeAll(t: ThreadView) {
  const ids = t.signals
    .filter((s) => s.state !== "resolved" && s.state !== "snoozed")
    .map((s) => s.id);
  for (const id of ids) {
    patchThreadSignalState(t.id, id, "snoozed");
    await api.setSignalState(id, "snoozed").catch(() => {});
  }
}

/// Bring a handled (resolved/snoozed) thread back onto the board by moving every
/// member signal to `acknowledged` — visible but sunk in the ranking. The backend
/// rebroadcasts the active board on each state change, so the thread reappears in
/// the live store on its own; we just drop it from the local handled list.
async function reopen(t: ThreadView) {
  for (const s of t.signals) {
    await api.setSignalState(s.id, "acknowledged").catch(() => {});
  }
}

/// A single event within a topic: a compact meta line (source, actor, time,
/// state, link-out) and a one-line title. The full body stays behind a pop-out
/// so a busy card doesn't unfurl hundreds of lines of alert text. Clicking the
/// row opens that pop-out; clicking a link does not.
function EventRow(props: { s: Signal; onDetails: () => void }) {
  const s = () => props.s;
  return (
    <div
      class={`event sev-${s().severity}`}
      classList={{ acked: s().state === "acknowledged" || s().state === "seen" }}
      onClick={props.onDetails}
    >
      <div class="event-meta">
        <span class="dot" />
        <span class={`src src-${s().source}`}>{s().source.toUpperCase()}</span>
        <Show when={s().actor}>
          <span class="event-actor">{s().actor}</span>
        </Show>
        <time>{new Date(s().occurred_at).toLocaleString()}</time>
        <Show when={s().state !== "unseen"}>
          <span class={`state state-${s().state}`}>{s().state.toUpperCase()}</span>
        </Show>
        <Show when={s().url}>
          <a
            class="event-open"
            href={s().url!}
            target="_blank"
            rel="noreferrer"
            onClick={(ev) => ev.stopPropagation()}
          >
            open ↗
          </a>
        </Show>
      </div>
      <div class="event-line">
        <span class="event-title">{s().title}</span>
        <Show when={hasDetails(s())}>
          <span class="event-details">details →</span>
        </Show>
      </div>
    </div>
  );
}

/// A topic = one correlated thread, rendered as a section: a header with bulk
/// actions above its events listed as compact rows.
function Topic(props: {
  t: ThreadView;
  onOpen: (id: string) => void;
  onDetails: (s: Signal) => void;
  actions: (t: ThreadView) => JSX.Element;
}) {
  const t = () => props.t;
  return (
    <section
      class={`topic sev-${t().severity}`}
      classList={{
        dim: t().state === "resolved" || t().state === "snoozed",
        acked: t().state === "acknowledged",
        live: t().live,
      }}
    >
      <div class="topic-bar" />
      <div class="topic-main">
        <header class="topic-head" onClick={() => props.onOpen(t().id)}>
          <div class="topic-titleline">
            <For each={sources(t())}>
              {(src) => <span class={`src src-${src}`}>{src.toUpperCase()}</span>}
            </For>
            <span class="topic-title">{t().title}</span>
            <Show when={t().live}>
              <span class="live-badge">LIVE</span>
            </Show>
          </div>
          <div class="topic-meta">
            <span class="kind">
              {t().signals.length} EVENT{t().signals.length === 1 ? "" : "S"}
            </span>
            <time>{new Date(t().updated_at).toLocaleTimeString()}</time>
          </div>
          <AttentionBadge attention={t().attention} />
        </header>

        <Show when={t().summary}>
          <div class="topic-summary">
            <span class="topic-summary-label">SUMMARY</span>
            <div class="md" innerHTML={renderMarkdown(t().summary!)} />
          </div>
        </Show>

        <Show when={t().tags.length || t().entities.length}>
          <div class="chips">
            <For each={t().tags}>{(tag) => <span class="chip tag">{tag}</span>}</For>
            <For each={t().entities.slice(0, 6)}>
              {(e) => {
                const href = entityHref(e);
                return href ? (
                  <a
                    class="chip chip-link"
                    href={href}
                    target="_blank"
                    rel="noreferrer"
                    onClick={(ev) => ev.stopPropagation()}
                  >
                    {e.kind}:{e.value}
                  </a>
                ) : (
                  <span class="chip">
                    {e.kind}:{e.value}
                  </span>
                );
              }}
            </For>
          </div>
        </Show>

        <div class="events">
          <For each={events(t())}>
            {(s) => <EventRow s={s} onDetails={() => props.onDetails(s)} />}
          </For>
        </div>

        <div class="topic-actions" onClick={(e) => e.stopPropagation()}>
          {props.actions(t())}
        </div>
      </div>
    </section>
  );
}

export default function Board(props: {
  onOpen: (id: string) => void;
  // When set, show only threads carrying a signal from this source.
  sourceFilter?: string | null;
}) {
  // Resolved and snoozed threads are "handled" — keep them off the main board (the
  // backend already excludes them; this makes triage feel instant).
  const ranked = createMemo(() =>
    Object.values(threads)
      .filter((t) => t.state !== "resolved" && t.state !== "snoozed")
      .filter((t) => !props.sourceFilter || sources(t).includes(props.sourceFilter))
      .sort((a, b) => {
        const att = attention(b) - attention(a);
        if (att !== 0) return att;
        return b.updated_at.localeCompare(a.updated_at);
      }),
  );

  // Handled threads live outside the reconciled live store (the WS `board` event
  // would otherwise wipe them). Fetched on demand when the user reveals them.
  const [showHandled, setShowHandled] = createSignal(false);
  const [handled, setHandled] = createSignal<ThreadView[]>([]);

  async function loadHandled() {
    const all = await api.listThreads(false).catch(() => [] as ThreadView[]);
    setHandled(
      all
        .filter((t) => t.state === "resolved" || t.state === "snoozed")
        .sort((a, b) => b.updated_at.localeCompare(a.updated_at)),
    );
  }

  async function toggleHandled() {
    const next = !showHandled();
    setShowHandled(next);
    if (next) await loadHandled();
    else setHandled([]);
  }

  // The signal popped out in the detail modal (null = closed).
  const [detail, setDetail] = createSignal<Signal | null>(null);

  // Reset the board: delete persisted events and their derived board analysis.
  // The authoritative `board` WS event reconciles the store on its own.
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

  return (
    <div class="board">
      <div class="board-toolbar" onClick={(e) => e.stopPropagation()}>
        <button class="danger reset" disabled={resetting()} onClick={resetBoard}>
          {resetting() ? "RESETTING…" : "RESET BOARD"}
        </button>
        <button classList={{ on: showHandled() }} onClick={toggleHandled}>
          {showHandled() ? "HIDE HANDLED" : "SHOW HANDLED"}
        </button>
        <Show when={showHandled()}>
          <button onClick={loadHandled}>REFRESH</button>
        </Show>
      </div>

      <Show
        when={ranked().length}
        fallback={
          <div class="empty">
            {props.sourceFilter
              ? `NO ${props.sourceFilter.toUpperCase()} THREADS`
              : "AWAITING SIGNALS…"}
          </div>
        }
      >
        <For each={ranked()}>
          {(t) => (
            <Topic
              t={t}
              onOpen={props.onOpen}
              onDetails={setDetail}
              actions={(t) => (
                <>
                  <button onClick={() => props.onOpen(t.id)}>OPEN</button>
                  <button onClick={() => ackAll(t)}>ACK ALL</button>
                  <button onClick={() => snoozeAll(t)}>SNOOZE</button>
                </>
              )}
            />
          )}
        </For>
      </Show>

      <Show when={showHandled()}>
        <div class="board-section">HANDLED</div>
        <Show
          when={handled().length}
          fallback={<div class="empty">NO HANDLED THREADS</div>}
        >
          <For each={handled()}>
            {(t) => (
              <Topic
                t={t}
                onOpen={props.onOpen}
                onDetails={setDetail}
                actions={(t) => (
                  <>
                    <button onClick={() => props.onOpen(t.id)}>OPEN</button>
                    <button
                      onClick={async () => {
                        await reopen(t);
                        setHandled((prev) => prev.filter((x) => x.id !== t.id));
                      }}
                    >
                      REOPEN
                    </button>
                  </>
                )}
              />
            )}
          </For>
        </Show>
      </Show>

      <Show when={detail()}>
        {(s) => <SignalModal signal={s()} onClose={() => setDetail(null)} />}
      </Show>
    </div>
  );
}
