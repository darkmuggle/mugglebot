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

// What the AI has produced for a thread, and where the work ran. `local_passes` is
// work done on this machine; `cloud_passes` cost a metered call.
export interface Decorations {
  summary: boolean;
  tags: boolean;
  mitigations: boolean;
  dashboard: boolean;
  root_cause: string | null;
  triage: string | null;
  prs_judged: number;
  local_passes: number;
  cloud_passes: number;
}

// The two questions the board answers: does this need you, and has the AI been
// over it. Replaces reading the unseen/ack state machine at a glance.
export interface Attention {
  needed: boolean;
  reason: string | null;
  decorated: Decorations;
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
  attention: Attention;
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

export interface BrowserInvestigation {
  id: string;
  signal_id: string;
  thread_id: string | null;
  url: string;
  prompt: string;
  status: "pending" | "running" | "completed" | "failed";
  findings: string | null;
  error: string | null;
  attempts: number;
  created_at: string;
  updated_at: string;
}

// How a candidate relates to the incident. `cause` is still a hypothesis with a
// confidence — never render it as the confirmed cause.
export type CauseRelation = "cause" | "fix" | "duplicate" | "context";

export interface RootCauseCandidate {
  kind: "issue" | "pull_request" | "commit" | "code";
  /** `owner/repo#12`, `owner/repo@abc1234`, or `owner/repo:path`. */
  reference: string;
  repo: string | null;
  number: number | null;
  sha: string | null;
  title: string;
  url: string | null;
  state: string | null;
  author: string | null;
  when: string | null;
  labels: string[];
  files: string[];
  fragments?: string[];
  relation: CauseRelation;
  confidence: number;
  rationale: string;
}

export interface RootCauseReport {
  thread_id: string;
  status: "running" | "complete" | "failed";
  symptoms: string[];
  repos: string[];
  candidates: RootCauseCandidate[];
  verdict: string | null;
  error: string | null;
  created_at: string;
  updated_at: string;
}

// A proposed approach, never an applied change.
export interface PatchOption {
  id: string;
  title: string;
  approach: string;
  /** The platform extension point this uses (admission webhook, CRD schema, …). */
  mechanism: string;
  /** A tool/library not already in the repo's manifests, if the approach needs one. */
  new_dependency: string | null;
  files: string[];
  sketch: string;
  risk: string;
  effort: "small" | "medium" | "large";
  confidence: number;
}

export interface IssueTriage {
  /** `owner/repo#number`. */
  issue_key: string;
  repo: string;
  number: number;
  title: string;
  url: string | null;
  signal_id: string | null;
  status: "pending" | "running" | "complete" | "failed";
  /** The commit the analysis actually read. */
  head_sha: string | null;
  checkout: string | null;
  files: string[];
  characterization: string | null;
  patches: PatchOption[];
  plain_summary: string | null;
  error: string | null;
  created_at: string;
  updated_at: string;
}

// An open PR that may already fix an issue — possibly somebody else's.
export type PrVerdict = "fixes" | "partial" | "related" | "unrelated";

export interface PrFix {
  issue_key: string;
  pr_repo: string;
  pr_number: number;
  pr_title: string;
  pr_url: string | null;
  pr_author: string | null;
  pr_state: string | null;
  files: string[];
  verdict: PrVerdict;
  confidence: number;
  /** What the patch actually changes, read from the diff. */
  implementation: string | null;
  /** Whether it genuinely fixes the issue, and what it misses. */
  critique: string | null;
  /** Other issues this patch would also resolve, each as `ref — why`. */
  also_fixes: string[];
  /** Which tier judged it: local, or an escalation. */
  analyzed_by: string | null;
  created_at: string;
  updated_at: string;
}

export interface RepoEntry {
  full_name: string;
  description: string | null;
  topics: string[];
  language: string | null;
  archived: boolean;
  pushed_at: string | null;
  /** The purpose/symptom card derived from the repo's code. */
  summary: string | null;
  /** The commit the card was built from. */
  indexed_sha: string | null;
  fetched_at: string;
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
