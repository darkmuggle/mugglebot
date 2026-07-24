// Mirrors the Rust wire types (serde snake_case). Keep in sync with the backend
// (src/signal.rs, src/correlation.rs, src/live.rs, src/store.rs, src/event.rs).

export type Source = "github" | "slack" | "granola";
export type Severity = "info" | "notice" | "warning" | "critical";
export type State = "unseen" | "seen" | "acknowledged" | "resolved" | "snoozed";

export interface Entity {
  kind: string;
  value: string;
}

export interface Signal {
  id: string;
  source: Source;
  external_id: string;
  kind: string;
  title: string;
  body: string | null;
  url: string | null;
  actor: string | null;
  entities: Entity[];
  severity: Severity;
  state: State;
  occurred_at: string;
  ingested_at: string;
  thread: string | null;
  raw: unknown;
  tags: string[];
}

export type RelationKind = "same" | "related" | "distinct";
export type Provenance = "llm" | "user";

export interface Edge {
  thread_a: string;
  thread_b: string;
  kind: RelationKind;
  provenance: Provenance;
  confidence: number;
  rationale: string;
  signals: string[];
  created_at: string;
}

export interface ThreadContext {
  id: string;
  thread_id: string;
  kind: "text" | "url";
  content: string;
  summary: string | null;
  created_at: string;
}

export interface ThreadView {
  id: string;
  title: string;
  summary: string | null;
  created_at: string;
  updated_at: string;
  last_reasoned_at: string | null;
  live: boolean;
  tags: string[];
  tags_pinned: boolean;
  signals: Signal[];
  entities: Entity[];
  severity: Severity;
  state: State;
  edges: Edge[];
  context: ThreadContext[];
}

export type HintKind = "hint" | "suggestion" | "flag";
export type FlagType = "factual_error" | "risky_action";
export type HintState = "active" | "dismissed" | "false_positive";

export interface Hint {
  id: string;
  thread_id: string;
  kind: HintKind;
  flag_type: FlagType | null;
  text: string;
  rationale: string | null;
  citations: string[];
  confidence: number;
  state: HintState;
  created_at: string;
}

export interface SourceHealth {
  source: string;
  last_poll_at: string | null;
  last_ok_at: string | null;
  ok: boolean;
  detail: string | null;
  cursor: string | null;
}

export interface Memory {
  id: string;
  text: string;
  summary: string;
  links: string[];
  tags: string[];
  tags_pinned: boolean;
  created_at: string;
  updated_at: string;
}
export interface MemoryHit extends Memory {
  score: number;
}

export interface ContextSource {
  id: string;
  kind: "url" | "file";
  location: string;
  credential: string | null;
  header: string | null;
  tags: string[];
  tags_pinned: boolean;
  summary: string | null;
  raw: string | null;
  etag: string | null;
  last_modified: string | null;
  mtime: string | null;
  fetched_at: string | null;
  refresh_interval: string;
  created_at: string;
}
export interface ContextHit extends ContextSource {
  score: number;
}

export interface Tag {
  name: string;
  summary: string;
  created_at: string;
}

export interface Mitigation {
  id: string;
  name: string;
  description: string;
  reversible: boolean;
  score: number;
  cited_signals: string[];
}

export interface RedAlert {
  thread_id: string;
  hint_id: string;
  message: string;
}

export interface Snapshot {
  signals: Signal[];
  threads: ThreadView[];
  hints: Hint[];
  health: SourceHealth[];
}

// Adjacently-tagged event bus payload: { type, data }.
export type Event =
  | { type: "snapshot"; data: Snapshot }
  | { type: "signal"; data: Signal }
  | { type: "thread"; data: ThreadView }
  | { type: "board"; data: ThreadView[] }
  | { type: "hint"; data: Hint }
  | { type: "health"; data: SourceHealth[] }
  | { type: "red_alert"; data: RedAlert }
  | { type: "clear_alert"; data?: undefined };

export interface ToolCall {
  tool: string;
  arguments: unknown;
  result: unknown;
}
export interface ChatResponse {
  answer: string;
  tool_calls: ToolCall[];
}
export interface ChatImage {
  media_type: string;
  base64: string;
}
export interface ChatTurn {
  role: "user" | "assistant";
  content: string;
  images: ChatImage[];
}

// A rendered chat message (a user turn or an assistant reply, with its optional
// tool trace). This is the shape persisted to and restored from the backend.
export interface ChatBubble {
  role: "user" | "assistant";
  content: string;
  images: ChatImage[];
  tools?: ToolCall[];
}

// Metadata row for the chat list (no messages).
export interface ChatSummary {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
}

// A persisted chat with its full transcript.
export interface StoredChat extends ChatSummary {
  messages: ChatBubble[];
}

export const SEVERITY_RANK: Record<Severity, number> = {
  info: 0,
  notice: 1,
  warning: 2,
  critical: 3,
};
