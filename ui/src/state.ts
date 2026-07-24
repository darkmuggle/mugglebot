// Live application state, fed by the WebSocket event bus. A single connection is
// established on first import; components read the exported accessors reactively.

import { createSignal } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { WS_URL } from "./api";
import type { Event, Hint, RedAlert, Signal, State, SourceHealth, ThreadView } from "./types";

// Least-triaged member state wins for a thread's aggregate (mirrors the backend).
const STATE_RANK: Record<State, number> = {
  unseen: 0,
  seen: 1,
  acknowledged: 2,
  snoozed: 3,
  resolved: 4,
};

function aggregateState(sigs: Signal[]): State {
  let best: State = "resolved";
  for (const s of sigs) if (STATE_RANK[s.state] < STATE_RANK[best]) best = s.state;
  return sigs.length ? best : "unseen";
}

const [connected, setConnected] = createSignal(false);
const [signals, setSignals] = createStore<Record<string, Signal>>({});
const [threads, setThreads] = createStore<Record<string, ThreadView>>({});
const [hints, setHints] = createSignal<Hint[]>([]);
const [health, setHealth] = createSignal<SourceHealth[]>([]);
const [redAlert, setRedAlert] = createSignal<RedAlert | null>(null);

// A pending "open this board thread as a new chat" hand-off. The board sets it
// and switches to the chat view; the Chat component consumes it once (starting a
// fresh conversation seeded with the thread's prompt + tags), then clears it.
export interface ChatSeed {
  prompt: string;
  tags: string[];
}
const [chatSeed, setChatSeed] = createSignal<ChatSeed | null>(null);

export { connected, signals, threads, hints, health, redAlert, chatSeed, setChatSeed };

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

/** Locally reflect a signal state change for snappy triage. */
export function patchSignalState(id: string, state: Signal["state"]) {
  if (signals[id]) setSignals(id, "state", state);
}

/**
 * Optimistically reflect a signal's state change inside its thread (what the
 * board actually renders) so triage feels instant — the authoritative WS `board`
 * event reconciles it moments later. Also recomputes the thread's aggregate state.
 */
export function patchThreadSignalState(threadId: string, signalId: string, state: State) {
  const t = threads[threadId];
  if (!t) return;
  const idx = t.signals.findIndex((s) => s.id === signalId);
  if (idx < 0) return;
  setThreads(threadId, "signals", idx, "state", state);
  setThreads(threadId, "state", aggregateState(threads[threadId].signals));
  patchSignalState(signalId, state);
}

function apply(ev: Event) {
  switch (ev.type) {
    case "snapshot": {
      const s: Record<string, Signal> = {};
      for (const sig of ev.data.signals) s[sig.id] = sig;
      setSignals(reconcile(s));
      const t: Record<string, ThreadView> = {};
      for (const th of ev.data.threads) t[th.id] = th;
      setThreads(reconcile(t));
      setHints(ev.data.hints.filter((h) => h.state === "active"));
      setHealth(ev.data.health);
      break;
    }
    case "signal":
      setSignals(ev.data.id, ev.data);
      break;
    case "thread":
      setThreads(ev.data.id, ev.data);
      break;
    case "board": {
      // Authoritative active-thread set — reconcile so merged/split/resolved
      // threads drop out of the board rather than lingering.
      const t: Record<string, ThreadView> = {};
      for (const th of ev.data) t[th.id] = th;
      setThreads(reconcile(t));
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
