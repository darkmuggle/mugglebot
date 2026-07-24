// REST client for the MuggleBot backend. Most capabilities go through the
// generic `/api/tool/:name` dispatcher, which mirrors the MCP tool surface.

import type {
  ChatBubble,
  ChatResponse,
  ChatSummary,
  ChatTurn,
  State,
  StoredChat,
  ThreadView,
} from "./types";

// Resolve the backend address. Explicit VITE_BACKEND wins (Tilt injects it from
// config's [ui].listen). In `vite dev` the UI runs on :5173 and talks to the
// backend cross-origin on :8080. When the Rust server serves the built UI, it's
// same-origin, so use the current host (whatever port it bound to).
const BACKEND =
  import.meta.env.VITE_BACKEND ??
  (import.meta.env.DEV ? `${location.hostname}:8080` : location.host);
const WS_SCHEME = location.protocol === "https:" ? "wss" : "ws";
export const WS_URL = `${WS_SCHEME}://${BACKEND}/ws`;
const API = `${location.protocol === "https:" ? "https" : "http"}://${BACKEND}`;
const JSON_HEADERS = { "content-type": "application/json" };

async function unwrap(res: Response): Promise<any> {
  if (!res.ok) throw new Error((await res.text()) || res.statusText);
  if (res.status === 204) return null;
  const text = await res.text();
  return text ? JSON.parse(text) : null;
}

export const api = {
  /** Call any backend tool by name. */
  tool<T = any>(name: string, args: Record<string, unknown> = {}): Promise<T> {
    return fetch(`${API}/api/tool/${name}`, {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify(args),
    }).then(unwrap);
  },

  /**
   * All threads as board views. `activeOnly` defaults to true (mirrors the live
   * WS board); pass false to include handled — resolved/snoozed — threads.
   */
  listThreads(activeOnly = true): Promise<ThreadView[]> {
    return api.tool<ThreadView[]>("list_threads", { active_only: activeOnly });
  },

  setSignalState(id: string, state: State): Promise<void> {
    return fetch(`${API}/api/signals/${encodeURIComponent(id)}/state`, {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ state }),
    }).then(unwrap);
  },

  /**
   * Board reset: permanently delete persisted events and their board-only thread
   * analysis. It does not mutate GitHub/Slack/etc.; still-active upstream items
   * can be ingested again on a later poll. Returns the number of deleted events.
   */
  resetBoard(): Promise<{ cleared: number }> {
    return fetch(`${API}/api/board/reset`, {
      method: "POST",
      headers: JSON_HEADERS,
    }).then(unwrap);
  },

  chat(
    messages: ChatTurn[],
    provider?: string,
    model?: string,
    tags?: string[],
  ): Promise<ChatResponse> {
    return fetch(`${API}/api/chat`, {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ messages, provider, model, tags }),
    }).then(unwrap);
  },

  /** Persisted agent chats (metadata only), newest activity first. */
  listChats(): Promise<ChatSummary[]> {
    return fetch(`${API}/api/chats`).then(unwrap);
  },

  /** Fetch one chat's full transcript. */
  getChat(id: string): Promise<StoredChat> {
    return fetch(`${API}/api/chats/${encodeURIComponent(id)}`).then(unwrap);
  },

  /** Create or update a chat (client-supplied id, upsert). */
  saveChat(id: string, title: string, messages: ChatBubble[]): Promise<void> {
    return fetch(`${API}/api/chats/${encodeURIComponent(id)}`, {
      method: "PUT",
      headers: JSON_HEADERS,
      body: JSON.stringify({ title, messages }),
    }).then(unwrap);
  },

  /** Delete a chat. */
  deleteChat(id: string): Promise<void> {
    return fetch(`${API}/api/chats/${encodeURIComponent(id)}`, {
      method: "DELETE",
    }).then(unwrap);
  },

  /** Models selectable for a provider (`anthropic` | `openai` | `ollama` | `ollama_local`). */
  models(provider: string): Promise<string[]> {
    return fetch(`${API}/api/models/${encodeURIComponent(provider)}`)
      .then(unwrap)
      .then((r: { models: string[] }) => r.models);
  },

  config(): Promise<unknown> {
    return fetch(`${API}/api/config`).then(unwrap);
  },

  configRaw(): Promise<string> {
    return fetch(`${API}/api/config/raw`).then((r) => {
      if (!r.ok) throw new Error(r.statusText);
      return r.text();
    });
  },

  saveConfig(toml: string): Promise<{ ok: boolean; message: string }> {
    return fetch(`${API}/api/config/raw`, {
      method: "PUT",
      headers: JSON_HEADERS,
      body: JSON.stringify({ toml }),
    }).then(unwrap);
  },

  credentials(): Promise<Record<string, boolean>> {
    return fetch(`${API}/api/credentials`).then(unwrap);
  },

  setCredential(account: string, secret: string): Promise<void> {
    return fetch(`${API}/api/credentials`, {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ account, secret }),
    }).then(unwrap);
  },

  deleteCredential(account: string): Promise<void> {
    return fetch(`${API}/api/credentials/${encodeURIComponent(account)}`, {
      method: "DELETE",
    }).then(unwrap);
  },
};
