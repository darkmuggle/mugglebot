import { createMemo, createSignal, For, type JSX, Show } from "solid-js";
import { api } from "../api";
import { entityHref } from "../entities";
import { AttentionBadge } from "./Attention";
import { renderMarkdown } from "../markdown";
import Attempt, { prKey } from "./Attempt";
import { dispatchesFor, isBusy, patchHandled, subjects } from "../state";
import {
  SEVERITY_RANK,
  type Explanation,
  type PrFix,
  type Signal,
  type SubjectView,
} from "../types";
import { SignalModal } from "./SignalModal";

function sources(t: SubjectView): string[] {
  return [...new Set(t.signals.map((s) => s.source))];
}

/// Member signals newest-first — the topic section lists them as event rows.
function events(t: SubjectView): Signal[] {
  return [...t.signals].sort((a, b) =>
    b.occurred_at.localeCompare(a.occurred_at),
  );
}

/// True when an event carries content beyond its title, so a pop-out is useful.
function hasDetails(s: Signal): boolean {
  const body = (s.body ?? "").trim();
  return !!body && body !== (s.title ?? "").trim();
}

/// Higher = more deserving of attention now: severity, lifted a little when live,
/// pushed down once triaged (acknowledged) or snoozed.
function attention(t: SubjectView): number {
  let score = SEVERITY_RANK[t.severity];
  if (t.live) score += 0.5;
  if (t.handled === "acknowledged") score -= 1.5;
  if (t.handled === "snoozed") score -= 3;
  return score;
}

/// Triage is one call on the subject now. Each of these used to loop over every
/// member signal, because handled-ness was per signal and a thread was only as
/// handled as its least-handled member.
async function triage(t: SubjectView, handled: "acknowledged" | "snoozed") {
  patchHandled(t.key, handled); // instant, in the store the board renders
  await api.setHandled(t.key, handled).catch(() => {});
}

/// Bring handled work back onto the board, fully open.
///
/// This used to set `acknowledged`, which is *still handled* — so un-snoozing something left it
/// muted and there was no way back to open at all. Un-handling means un-handled; if the operator
/// wants it sunk in the ranking again, ACK is right there.
///
/// The backend rebroadcasts the active board, so it reappears in the live store on its own.
async function reopen(t: SubjectView) {
  patchHandled(t.key, "open");
  await api.setHandled(t.key, "open").catch(() => {});
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
      classList={{ acked: s().upstream_gone }}
      onClick={props.onDetails}
    >
      <div class="event-meta">
        <span class="dot" />
        <span class={`src src-${s().source}`}>{s().source.toUpperCase()}</span>
        <Show when={s().actor}>
          <span class="event-actor">{s().actor}</span>
        </Show>
        <time>{new Date(s().occurred_at).toLocaleString()}</time>
        <Show when={s().upstream_gone}>
          <span class="state state-resolved">GONE</span>
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

/// Has activity landed since this explanation was written?
function staleExplanation(t: SubjectView, x: Explanation): boolean {
  if (!t.signals.length) return false;
  return t.signals[t.signals.length - 1].id !== x.watermark;
}

function Topic(props: {
  t: SubjectView;
  onOpen: (key: string) => void;
  onDetails: (s: Signal) => void;
  onExplain: (key: string) => void;
  actions: (t: SubjectView) => JSX.Element;
}) {
  const t = () => props.t;
  return (
    <section
      class={`topic sev-${t().severity}`}
      classList={{
        dim: t().handled === "resolved" || t().handled === "snoozed",
        acked: t().handled === "acknowledged",
        live: t().live,
      }}
    >
      <div class="topic-bar" />
      <div class="topic-main">
        <header class="topic-head" onClick={() => props.onOpen(t().key)}>
          <div class="topic-titleline">
            <For each={sources(t())}>
              {(src) => (
                <span class={`src src-${src}`}>{src.toUpperCase()}</span>
              )}
            </For>
            <span class="topic-title">{t().title}</span>
            <Show when={t().live}>
              <span class="live-badge">LIVE</span>
            </Show>
            {/* The board's half of the dispatch indicator: which cards have AI work in
                flight, and which had one fail. The detail view carries the full strip —
                here it is one badge, because a card is a list row and a failure the
                operator can't see is worse than a card that is one glyph busier. */}
            <Show when={isBusy(t().key)}>
              <span
                class="ai-badge ai-working"
                data-tip="an AI pass is running or queued"
              >
                <span class="thinking-dots">
                  <i />
                  <i />
                  <i />
                </span>
                AI
              </span>
            </Show>
            <Show when={!isBusy(t().key) && lastFailure(t().key)}>
              <span class="ai-badge ai-failed" data-tip={lastFailure(t().key)!}>
                AI FAILED
              </span>
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

        <Show when={t().tags.length || t().keys.length}>
          <div class="chips">
            <For each={t().tags}>
              {(tag) => <span class="chip tag">{tag}</span>}
            </For>
            <For each={t().keys.slice(0, 6)}>
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

        {/* Both explanations, each labelled by who wrote it. Local first — that one cost
            nothing and is what MuggleBot actually concluded; a second opinion is only here
            because someone asked for it, and reading it without knowing which is which
            would defeat the purpose of having asked. */}
        <For each={t().explanations}>
          {(x) => (
            <div
              class="explain-panel"
              classList={{ "explain-cloud": x.produced_by === "cloud" }}
              onClick={(e) => e.stopPropagation()}
            >
              <div class="explain-head">
                <span class="explain-label">
                  {x.produced_by === "cloud" ? "2ND OPINION" : "EXPLANATION"}
                </span>
                <span class="chip model-chip" data-tip="which model wrote this">
                  {x.produced_by === "cloud" ? "CLOUD" : "LOCAL"}
                </span>
                <For each={x.sources}>
                  {(src) => (
                    <span class="chip src-chip">{src.replace(/_/g, " ")}</span>
                  )}
                </For>
                {/* An explanation built from an older watermark still describes what it
                    described; saying so beats presenting it as current. */}
                <Show when={staleExplanation(t(), x)}>
                  <span
                    class="chip chip-stale"
                    data-tip="new activity has landed since this was written"
                  >
                    STALE
                  </span>
                </Show>
              </div>
              <div class="md" innerHTML={renderMarkdown(x.markdown)} />
              {/* What the dossier check took out. Shown rather than swallowed: an
                  explanation that had claims removed is one to read more carefully. */}
              <Show when={x.removed.length}>
                <div
                  class="explain-removed"
                  data-tip="removed because the dossier could not support it"
                >
                  <For each={x.removed}>{(r) => <div>— {r}</div>}</For>
                </div>
              </Show>
            </div>
          )}
        </For>

        {/* The attempts, nested under the problem. An issue whose PRs you have to click
            through to see reads as an issue nobody is working on. */}
        <Show when={t().pull_requests.length}>
          <div class="attempts" onClick={(e) => e.stopPropagation()}>
            <div class="attempts-label">
              {t().pull_requests.length} ATTEMPT
              {t().pull_requests.length === 1 ? "" : "S"}
            </div>
            <For each={t().pull_requests}>
              {(pr) => (
                <Attempt pr={pr} onExplain={() => props.onExplain(prKey(pr))} />
              )}
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
  // Resolved and snoozed subjects are handled — keep them off the main board (the
  // backend already excludes them; this makes triage feel instant). A subject merged
  // away forwards its activity to the canonical one, so it isn't a card either.
  const ranked = createMemo(() =>
    Object.values(subjects)
      .filter(
        (t) =>
          t.handled !== "resolved" && t.handled !== "snoozed" && !t.same_as,
      )
      .filter(
        (t) => !props.sourceFilter || sources(t).includes(props.sourceFilter),
      )
      .sort((a, b) => {
        const att = attention(b) - attention(a);
        if (att !== 0) return att;
        return b.updated_at.localeCompare(a.updated_at);
      }),
  );

  // Which subject is currently being explained, so the button can say so. The result
  // arrives over the WebSocket when the workflow writes it, so there is nothing to
  // await here beyond the submission.
  const [explaining, setExplaining] = createSignal<string | null>(null);
  const [explainNote, setExplainNote] = createSignal("");

  const explain = async (key: string, secondOpinion = false) => {
    setExplaining(key);
    setExplainNote("");
    try {
      const r = await api.explain(key, secondOpinion);
      const what = secondOpinion
        ? "asking the cloud model about"
        : "explaining";
      // `submitted: false` means the key collided — nothing has changed since the last
      // explanation, so the one already on the card *is* the answer. That is a success,
      // and saying "already current" beats a spinner that never resolves.
      setExplainNote(r.submitted ? `${what} ${key}…` : `${key}: ${r.note}`);
    } catch (e) {
      setExplainNote(`${key}: ${e}`);
    } finally {
      setExplaining(null);
    }
  };

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
        <button
          class="danger reset"
          disabled={resetting()}
          onClick={resetBoard}
        >
          {resetting() ? "RESETTING…" : "RESET BOARD"}
        </button>
        <button classList={{ on: showHandled() }} onClick={toggleHandled}>
          {showHandled() ? "HIDE HANDLED" : "SHOW HANDLED"}
        </button>
        <Show when={explainNote()}>
          <span class="muted board-note">{explainNote()}</span>
        </Show>
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
              onExplain={explain}
              actions={(t) => (
                <>
                  <button onClick={() => props.onOpen(t.key)}>OPEN</button>
                  {/* On an issue this explains the whole situation — its events, every
                      PR attempting it with the critiques and review conversations, the
                      proposed causes, the triage. On a PR, just that change. */}
                  <button
                    disabled={explaining() === t.key}
                    data-tip="Distil this and everything under it, on the local model"
                    onClick={() => explain(t.key)}
                  >
                    {explaining() === t.key ? "EXPLAINING…" : "EXPLAIN"}
                  </button>
                  {/* An acked card is still on the board, so it needs the way back — before
                      this, ACK on an already-acked card did nothing and there was no un-ack. */}
                  <Show
                    when={t.handled === "acknowledged"}
                    fallback={
                      <button onClick={() => triage(t, "acknowledged")}>
                        ACK
                      </button>
                    }
                  >
                    <button class="cloud-btn" onClick={() => reopen(t)}>
                      UN-ACK
                    </button>
                  </Show>
                  <button onClick={() => triage(t, "snoozed")}>SNOOZE</button>
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
                onExplain={explain}
                actions={(t) => (
                  <>
                    <button onClick={() => props.onOpen(t.key)}>OPEN</button>
                    {/* Named for what it undoes, so the operator can see which state they are
                        leaving rather than reading "REOPEN" against three different ones. */}
                    <button
                      class="cloud-btn"
                      onClick={async () => {
                        await reopen(t);
                        setHandled((prev) =>
                          prev.filter((x) => x.key !== t.key),
                        );
                      }}
                    >
                      {t.handled === "snoozed"
                        ? "UN-SNOOZE"
                        : t.handled === "resolved"
                          ? "UN-RESOLVE"
                          : "REOPEN"}
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

/// The most recent failed dispatch's message for a subject, if the last thing that
/// happened was a failure.
///
/// Only the *latest* row counts: a failure that has since been retried successfully is
/// history, and a card that keeps flagging it would train the operator to ignore the
/// badge.
function lastFailure(key: string): string | null {
  const latest = dispatchesFor(key)[0];
  if (!latest || latest.state !== "failed") return null;
  return latest.detail ?? "an AI pass failed";
}
