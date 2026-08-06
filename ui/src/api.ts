// REST client for the MuggleBot backend. Most capabilities go through the
// generic `/api/tool/:name` dispatcher, which mirrors the MCP tool surface.

import type {
  ChatBubble,
  ChatResponse,
  ChatSummary,
  ChatTurn,
  Handled,
  IndexStatus,
  Persona,
  PersonaCandidate,
  PersonaDetail,
  PersonaList,
  PersonaPrediction,
  PredictionKind,
  PrDiffReport,
  RepoIndexDetail,
  RepoKind,
  ScoreReport,
  SecretStatus,
  StoredChat,
  SubjectView,
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
   * All subjects as board views. `activeOnly` defaults to true (mirrors the live
   * WS board); pass false to include handled work and merged-away subjects.
   */
  listSubjects(activeOnly = true): Promise<SubjectView[]> {
    return api.tool<SubjectView[]>("list_subjects", {
      active_only: activeOnly,
    });
  },

  /**
   * Distil a subject and everything under it, on the **local** model.
   *
   * `secondOpinion` asks the cloud model for its own read of the same dossier instead.
   * That flag is the only way anything outside the chat pane reaches a metered model, so
   * it is only ever set by a button the operator pressed.
   *
   * Returns whether a new run started — `false` means nothing has changed since the last
   * explanation, which is a successful outcome, not a failure.
   */
  explain(
    key: string,
    secondOpinion = false,
  ): Promise<{
    submitted: boolean;
    workflow: string;
    produced_by: string;
    note: string;
  }> {
    return api.tool("explain", {
      subject_key: key,
      second_opinion: secondOpinion,
    });
  },

  /**
   * Tag what a repo is for. Omitting `kind` drops the tag and hands it back to the crawl's
   * name-matching guess.
   */
  setRepoKind(repo: string, kind: RepoKind | null): Promise<unknown> {
    return api.tool("set_repo_kind", kind ? { repo, kind } : { repo });
  },

  /**
   * Assemble a chat context block for a repo, or for one commit in it.
   *
   * Deterministic — everything the index already holds, rendered for a model to read. The chat
   * then shells out to whichever provider is picked in the pane.
   */
  chatContext(
    repo: string,
    sha?: string,
  ): Promise<{ prompt: string; opening: string }> {
    return api.tool("chat_context", sha ? { repo, sha } : { repo });
  },

  /**
   * Check a repo out and run a coding agent inside it, streaming to the board.
   *
   * `tool` is claude or codex — ollama has no agent mode. Unlike the chat context, the agent
   * reads the real files rather than the index's summaries of them.
   */
  startAgentSession(
    repo: string,
    tool: string,
    prompt?: string,
  ): Promise<{ session_id: string; repo: string; tool: string }> {
    return api.tool(
      "start_agent_session",
      prompt ? { repo, tool, prompt } : { repo, tool },
    );
  },

  stopAgentSession(sessionId: string): Promise<{ stopped: boolean }> {
    return api.tool("stop_agent_session", { session_id: sessionId });
  },

  /** Code-index progress across every repo, plus the indexing work in flight. */
  indexStatus(): Promise<IndexStatus> {
    return api.tool<IndexStatus>("index_status");
  },

  /** Everything the index holds about one repo, including its commit summaries. */
  repoIndexDetail(repo: string, commitLimit = 25): Promise<RepoIndexDetail> {
    return api.tool<RepoIndexDetail>("repo_index_detail", {
      repo,
      commit_limit: commitLimit,
    });
  },

  /** Re-walk the org's repo list and re-card anything whose code has moved. */
  refreshRepoIndex(): Promise<{ ok: boolean; summarized: number }> {
    return api.tool("refresh_repo_index", {});
  },

  /**
   * A PR's diff with a model summary. Pass a PR key for that PR, or an issue key for every PR
   * attempting it. Fetched on demand — a diff is an API call and only interesting while open.
   */
  /**
   * A subject's pull request diffs.
   *
   * `storedOnly` answers from the pull request's object state and fetches nothing, which
   * is what makes it safe to call on render; without it, a PR with no stored diff is read
   * from GitHub and summarized, which takes seconds.
   */
  prDiff(
    subjectKey: string,
    opts: { storedOnly?: boolean; refresh?: boolean } = {},
  ): Promise<PrDiffReport> {
    return api.tool<PrDiffReport>("pr_diff", {
      subject_key: subjectKey,
      stored_only: opts.storedOnly ?? false,
      refresh: opts.refresh ?? false,
    });
  },

  /** Rank which repo, component and change an issue is likely about. */
  scoreIssue(key: string): Promise<ScoreReport> {
    return api.tool<ScoreReport>("score_issue", { subject_key: key });
  },

  /** Triage a subject. `until` applies to `snoozed`. */
  setHandled(key: string, handled: Handled, until?: string): Promise<void> {
    return fetch(`${API}/api/subjects/${encodeURIComponent(key)}/handled`, {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ handled, until }),
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

  /**
   * Send a chat turn.
   *
   * `persona` talks to a simulated colleague instead of to MuggleBot: same transport, a
   * different framing and grounding server-side. The reply comes back with `persona` set
   * so the pane can label it as a prediction rather than as an answer.
   */
  chat(
    messages: ChatTurn[],
    provider?: string,
    model?: string,
    tags?: string[],
    persona?: string,
  ): Promise<ChatResponse> {
    return fetch(`${API}/api/chat`, {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ messages, provider, model, tags, persona }),
    }).then(unwrap);
  },

  // ---- personas --------------------------------------------------------------

  /** The modelled people, with how much evidence is behind each and how fresh it is. */
  listPersonas(): Promise<PersonaList> {
    return api.tool<PersonaList>("list_personas");
  },

  /** One persona in full: traits with citations, what was refused, evidence, predictions. */
  getPersona(slug: string, evidenceLimit = 60): Promise<PersonaDetail> {
    return api.tool<PersonaDetail>("get_persona", {
      slug,
      evidence_limit: evidenceLimit,
    });
  },

  /** People seen in the signal log who are not modelled yet, most-dealt-with first. */
  proposePersonas(limit?: number): Promise<{ candidates: PersonaCandidate[] }> {
    return api.tool("propose_personas", limit ? { limit } : {});
  },

  createPersona(body: {
    display_name: string;
    slug?: string;
    role?: string;
    notes?: string;
    identities?: { source: string; handle: string; provenance?: string; rationale?: string }[];
  }): Promise<{ persona: Persona; armed: boolean }> {
    return api.tool("create_persona", body);
  },

  updatePersona(body: {
    slug: string;
    display_name?: string;
    role?: string;
    notes?: string;
  }): Promise<Persona> {
    return api.tool("update_persona", body);
  },

  deletePersona(slug: string): Promise<{ deleted: boolean }> {
    return api.tool("delete_persona", { slug });
  },

  linkPersonaIdentity(
    slug: string,
    source: string,
    handle: string,
  ): Promise<{ harvesting: boolean }> {
    return api.tool("link_persona_identity", { slug, source, handle });
  },

  unlinkPersonaIdentity(source: string, handle: string): Promise<{ unlinked: boolean }> {
    return api.tool("unlink_persona_identity", { source, handle });
  },

  /** Harvest now rather than waiting for the loop's next tick. */
  harvestPersona(slug: string): Promise<{ harvesting: boolean }> {
    return api.tool("harvest_persona", { slug });
  },

  /**
   * Re-distil a persona's traits.
   *
   * `submitted: false` means the profile is already current for everything harvested — a
   * success, not a failure. `force` redoes it anyway.
   */
  refreshPersonaProfile(
    slug: string,
    force = false,
  ): Promise<{ submitted: boolean; workflow: string; note: string }> {
    return api.tool("refresh_persona_profile", { slug, force });
  },

  /**
   * Predict what personas would do about a subject.
   *
   * Takes a list, because the question is "how will this land" and the answer is the set of
   * reactions — a reviewer who will block and one who will not care are one answer together.
   * Nothing is posted anywhere.
   */
  predictPersonas(
    subjectKey: string,
    personas: string[],
    opts: { kind?: PredictionKind; provider?: string; model?: string } = {},
  ): Promise<{
    subject_key: string;
    kind: PredictionKind;
    predictions: { persona: string; submitted: boolean; workflow: string }[];
    stored: PersonaPrediction[];
  }> {
    return api.tool("predict_persona", {
      subject_key: subjectKey,
      personas,
      ...opts,
    });
  },

  /** Where a persona's review activity concentrates, and who to ask about an area. */
  whoKnows(area: string): Promise<{
    area: string;
    candidates: {
      persona: string;
      display_name: string;
      role: string | null;
      area: string;
      kind: string;
      excerpts: number;
      reviews: number;
      share: number;
      established_expertise: boolean;
      depth: string | null;
    }[];
    personas_considered: number;
    note: string;
  }> {
    return api.tool("who_knows", { area });
  },

  /**
   * Attach something you know about a person. Text is verbatim; a URL is fetched and
   * summarized. Re-profiles, so the fact takes effect immediately.
   */
  addPersonaContext(slug: string, content: string): Promise<{ id: string; reprofiling: boolean }> {
    return api.tool("add_persona_context", { slug, content });
  },

  removePersonaContext(id: string): Promise<{ removed: boolean }> {
    return api.tool("remove_persona_context", { id });
  },

  /** Stored predictions for one subject. */
  predictionsFor(subjectKey: string): Promise<PersonaPrediction[]> {
    return api.tool<PersonaPrediction[]>("list_predictions", {
      subject_key: subjectKey,
    });
  },

  /**
   * Every stored prediction *by* one persona.
   *
   * One call for the whole board rather than one per row: the board is a hundred rows and
   * asking per row would be a hundred round trips to render a chip.
   */
  predictionsBy(slug: string): Promise<PersonaPrediction[]> {
    return api.tool<PersonaPrediction[]>("list_predictions", { slug });
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

  /** The incidents board: open incidents with whatever each has been mapped to. */
  incidents(): Promise<{ open: number; incidents: SubjectView[] }> {
    return fetch(`${API}/api/incidents`).then((r) => {
      if (!r.ok) throw new Error(`incidents: ${r.status}`);
      return r.json();
    });
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

  // Write-only: the API reports whether a secret is set and when it changed, and
  // has no route that returns a value.
  secrets(): Promise<{ secrets: SecretStatus[] }> {
    return fetch(`${API}/api/secrets`).then(unwrap);
  },

  setSecret(name: string, value: string): Promise<void> {
    return fetch(`${API}/api/secrets`, {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ name, value }),
    }).then(unwrap);
  },

  deleteSecret(name: string): Promise<void> {
    return fetch(`${API}/api/secrets/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }).then(unwrap);
  },
};
