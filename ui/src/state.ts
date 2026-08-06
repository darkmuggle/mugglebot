// Live application state, fed by the WebSocket event bus. A single connection is
// established on first import; components read the exported accessors reactively.

import { createSignal } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { WS_URL } from "./api";
import type {
  AgentChunk,
  Dispatch,
  Event,
  Handled,
  Hint,
  IndexProgressEvent,
  RedAlert,
  Signal,
  SourceHealth,
  SubjectView,
} from "./types";

const [connected, setConnected] = createSignal(false);
const [signals, setSignals] = createStore<Record<string, Signal>>({});
// Keyed by subject key (`owner/repo#412`, `channel/ts`), which is also how every
// API addresses one — no lookup table between the board and the backend.
const [subjects, setSubjects] = createStore<Record<string, SubjectView>>({});
const [hints, setHints] = createSignal<Hint[]>([]);
const [health, setHealth] = createSignal<SourceHealth[]>([]);
const [redAlert, setRedAlert] = createSignal<RedAlert | null>(null);

// A pending "open this subject as a new chat" hand-off. The board sets it and
// switches to the chat view; the Chat component consumes it once (starting a fresh
// conversation seeded with the subject's prompt + tags), then clears it.
export interface ChatSeed {
  prompt: string;
  tags: string[];
  /**
   * Open the chat talking *as* this persona.
   *
   * Carried on the seed rather than as its own signal so the two hand-offs stay one
   * mechanism: "open this subject in chat" and "talk to this person" both set a seed and
   * switch views, and the Chat component consumes either shape once on mount.
   */
  persona?: string;
}
const [chatSeed, setChatSeed] = createSignal<ChatSeed | null>(null);

/**
 * The persona the board is currently reviewing *as*, or null for nobody.
 *
 * Shared state rather than a Board-local signal so it survives clicking into a subject and
 * back: "review the board as Pavel" is a mode you work in for a few minutes, and losing it on
 * every navigation would make it useless for the thing it is for — sweeping a lane to see where
 * one person would push back.
 */
const [reviewAs, setReviewAs] = createSignal<string | null>(null);

export {
  connected,
  signals,
  subjects,
  hints,
  health,
  redAlert,
  chatSeed,
  setChatSeed,
  reviewAs,
  setReviewAs,
};

function upsertHint(h: Hint) {
  setHints((prev) => {
    const rest = prev.filter((x) => x.id !== h.id);
    return h.state === "active" ? [h, ...rest] : rest;
  });
}

/** Locally mark a hint resolved (after dismiss) without waiting for a push. */
export function removeHint(id: string) {
  setHints((prev) => prev.filter((h) => h.id !== id));
  if (redAlert()?.hint_id === id) setRedAlert(null);
}

/**
 * Optimistically reflect a subject's triage change so it feels instant — the
 * authoritative WS `board` event reconciles moments later.
 *
 * One field, one write. This used to have to walk every member signal and
 * recompute an aggregate, because triage was per signal.
 */
export function patchHandled(key: string, handled: Handled) {
  if (subjects[key]) setSubjects(key, "handled", handled);
}

/**
 * Indexing progress pushed over the WebSocket, by repo.
 *
 * Kept beside the fetched baseline rather than merged into it: the fetch answers "what does the
 * whole org look like", the events answer "what just changed", and the panel overlays the second
 * on the first. Merging them here would mean the panel could not tell a repo it has never heard
 * of from one whose row simply hasn't moved.
 */
const [indexProgress, setIndexProgress] = createStore<
  Record<string, IndexProgressEvent>
>({});
export { indexProgress };

/**
 * Agent session transcripts, by session id, in arrival order.
 *
 * Appended rather than replaced: the value of watching an agent is the sequence — which file it
 * read, what it concluded, what it tried next — and a store keeping only the latest chunk would
 * show a cursor rather than a transcript.
 */
const [agentLog, setAgentLog] = createStore<Record<string, AgentChunk[]>>({});
export { agentLog };

/**
 * AI dispatches by id, pushed as each one is accepted, starts, and finishes.
 *
 * A flat store keyed by dispatch id rather than a per-subject list: the backend patches one
 * row at a time, and the strip for a subject is a filter over this. Keeping the whole set
 * also means the board can mark *which* cards have work in flight without asking.
 */
const [dispatches, setDispatches] = createStore<Record<string, Dispatch>>({});
export { dispatches };

/** One subject's dispatches, newest first. */
export function dispatchesFor(subject: string): Dispatch[] {
  return Object.values(dispatches)
    .filter((d) => d.subject === subject)
    .sort((a, b) => b.started_at.localeCompare(a.started_at));
}

/** Whether a subject has an AI pass accepted-but-unfinished right now. */
export function isBusy(subject: string): boolean {
  return Object.values(dispatches).some(
    (d) =>
      d.subject === subject && (d.state === "queued" || d.state === "running"),
  );
}

/** Forget one session's transcript. */
export function clearAgentLog(id: string) {
  setAgentLog(id, []);
}

function apply(ev: Event) {
  switch (ev.type) {
    case "snapshot": {
      const s: Record<string, Signal> = {};
      for (const sig of ev.data.signals) s[sig.id] = sig;
      setSignals(reconcile(s));
      const t: Record<string, SubjectView> = {};
      for (const sub of ev.data.subjects) t[sub.key] = sub;
      setSubjects(reconcile(t));
      setHints(ev.data.hints.filter((h) => h.state === "active"));
      setHealth(ev.data.health);
      // Reconciled, not merged: the daemon restarting is what clears in-memory dispatch
      // state, and a client holding rows the backend has forgotten would show work that
      // is no longer running as still in flight.
      const d: Record<string, Dispatch> = {};
      for (const disp of ev.data.dispatches ?? []) d[disp.id] = disp;
      setDispatches(reconcile(d));
      break;
    }
    case "signal":
      setSignals(ev.data.id, ev.data);
      break;
    case "subject":
      setSubjects(ev.data.key, ev.data);
      break;
    case "board": {
      // Authoritative active set — reconcile so merged-away or handled subjects
      // drop off the board rather than lingering.
      const t: Record<string, SubjectView> = {};
      for (const sub of ev.data) t[sub.key] = sub;
      setSubjects(reconcile(t));
      break;
    }
    case "hint":
      upsertHint(ev.data);
      break;
    case "health":
      setHealth(ev.data);
      break;
    case "red_alert":
      setRedAlert(ev.data);
      break;
    case "clear_alert":
      setRedAlert(null);
      break;
    case "agent_chunk": {
      const id = ev.data.session_id;
      setAgentLog(id, (prev) => {
        const log = prev ?? [];
        const last = log[log.length - 1];
        // Coalesce a delta into the block it continues. Without this a streamed answer renders
        // one word per row, which is unreadable and defeats the point of streaming it.
        if (
          ev.data.delta &&
          last &&
          last.kind === ev.data.kind &&
          last.subagent_of === ev.data.subagent_of
        ) {
          const merged = { ...last, text: last.text + ev.data.text };
          return [...log.slice(0, -1), merged];
        }
        return [...log, ev.data];
      });
      break;
    }
    case "dispatch":
      // Upsert by id: submit → running → done is one row moving, not three rows.
      setDispatches(ev.data.id, ev.data);
      break;
    case "index_progress":
      // One repo, replaced wholesale. Fine-grained reactivity means only the row that moved
      // repaints, which matters on a 147-repo list.
      setIndexProgress(ev.data.repo, ev.data);
      break;
  }
}

let ws: WebSocket | undefined;
let retry: number | undefined;
let started = false;

export function connect() {
  if (started) return;
  started = true;
  const open = () => {
    ws = new WebSocket(WS_URL);
    ws.onopen = () => setConnected(true);
    ws.onclose = () => {
      setConnected(false);
      retry = window.setTimeout(open, 2000);
    };
    ws.onerror = () => ws?.close();
    ws.onmessage = (e) => {
      try {
        apply(JSON.parse(e.data as string) as Event);
      } catch {
        /* ignore malformed frame */
      }
    };
  };
  open();
}

export function disconnect() {
  started = false;
  if (retry) clearTimeout(retry);
  ws?.close();
}
