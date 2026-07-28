// Mirrors the Rust wire types (serde snake_case). Keep in sync with the backend
// (src/signal.rs, src/subject.rs, src/correlation.rs, src/live.rs, src/event.rs).

export type Source = "github" | "slack" | "granola";
export type Severity = "info" | "notice" | "warning" | "critical";

/// Operator triage, per *subject*. Acknowledging half a PR's CI failures was never
/// a coherent thing to express, which is why this is no longer per signal.
export type Handled = "open" | "seen" | "acknowledged" | "snoozed" | "resolved";

/// The three kinds of durable work, ranked: issue > pull request > Slack thread.
export type SubjectRank = "issue" | "pull_request" | "slack_thread";

/// Something a signal names. Only issue/PR/Slack-thread keys can *own* a signal;
/// the rest are how the owner was found, plus context.
export interface ResolutionKey {
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
  keys: ResolutionKey[];
  severity: Severity;
  /// Upstream version of a mutable event (GitHub `updated_at`, Slack edit ts).
  version: string | null;
  /// Gone upstream — no longer unread, issue closed. A fact about the source, not
  /// a triage decision.
  upstream_gone: boolean;
  occurred_at: string;
  ingested_at: string;
  /// The subject key that owns this signal; null is the unattributed lane.
  subject: string | null;
  raw: unknown;
  tags: string[];
}

export type RelationKind = "same" | "related" | "distinct";
export type Provenance = "llm" | "user";

export interface Edge {
  subject_a: string;
  subject_b: string;
  kind: RelationKind;
  provenance: Provenance;
  confidence: number;
  rationale: string;
  signals: string[];
  created_at: string;
}

export interface SubjectContext {
  id: string;
  subject_key: string;
  kind: "text" | "url";
  content: string;
  summary: string | null;
  created_at: string;
}

// What the AI has produced for a subject, and where the work ran. `local_passes` is
// work done on this machine; `cloud_passes` cost a metered call.
export interface Decorations {
  summary: boolean;
  tags: boolean;
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

/// A durable piece of work plus everything MuggleBot knows about it. `key` is its
/// upstream identity — `owner/repo#412`, `owner/repo!987`, `channel/thread_ts` —
/// and is how every API addresses it.
export interface SubjectView {
  key: string;
  rank: SubjectRank;
  title: string;
  summary: string | null;
  /// The one line a board row shows: `summary` reduced to a single plain-text
  /// sentence by the backend, or null when there is no usable summary yet. Derived
  /// on read, so it can never disagree with `summary`.
  headline: string | null;
  created_at: string;
  updated_at: string;
  last_reasoned_at: string | null;
  live: boolean;
  tags: string[];
  tags_pinned: boolean;
  handled: Handled;
  snoozed_until: string | null;
  /// Merged away into this canonical subject; activity forwards there.
  same_as: string | null;
  /// Parent issue, for a PR filed under the issue it closes.
  parent: string | null;
  signals: Signal[];
  keys: ResolutionKey[];
  severity: Severity;
  edges: Edge[];
  context: SubjectContext[];
  /// PRs filed under this issue.
  children: string[];
  /// The attempts at this issue, each with what it implements, MuggleBot's critique of
  /// the diff, and what reviewers said. Nested on the view because the nesting *is* the
  /// answer to "what's the state of this?".
  pull_requests: PrFix[];
  /// Distilled explanations of this subject and everything under it: the local one
  /// MuggleBot writes on its own, and the cloud one if a second opinion was asked for.
  /// Both, so they can be shown side by side and labelled — the point of a second opinion
  /// is comparing it to the first.
  explanations: Explanation[];
  attention: Attention;
}

export interface Explanation {
  subject_key: string;
  /** The newest attributed signal when it was written; compare to spot a stale one. */
  watermark: string;
  markdown: string;
  /** `"local"` (written unprompted) or `"cloud"` (asked for by name). */
  produced_by: "local" | "cloud";
  sources: string[];
  /**
   * Claims the dossier check removed before storing — an invented link, a reviewer
   * quote for a PR nobody reviewed, a section with nothing behind it. Empty is the
   * good case; anything here is worth showing, because an explanation that needed
   * correcting is one to read more carefully.
   */
  removed: string[];
  created_at: string;
}

export type HintKind = "hint" | "suggestion" | "flag";
export type FlagType = "factual_error" | "risky_action";
export type HintState = "active" | "dismissed" | "false_positive";

export interface Hint {
  id: string;
  subject_key: string;
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

export interface BrowserInvestigation {
  id: string;
  signal_id: string;
  subject_key: string | null;
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
  subject_key: string;
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
//
// **Nothing here is ever posted to GitHub.** The critique is a note in MuggleBot's own
// store, rendered in this console and nowhere else: it never becomes a PR comment, a
// review, or an approval. See AGENTS.md → "Copilot, not autopilot".
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
  /** Whether it genuinely fixes the issue, and what it misses. MuggleBot's reading. */
  critique: string | null;
  /**
   * What reviewers said, distilled from the merit-scored discussion. A human who read
   * the change and pushed back is better evidence than a model's reading of the same
   * diff, so this is shown alongside the critique rather than folded into it.
   */
  conversation: string | null;
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
  subject_key: string;
  hint_id: string;
  message: string;
}

export interface Snapshot {
  signals: Signal[];
  subjects: SubjectView[];
  hints: Hint[];
  health: SourceHealth[];
  /** In-flight and recent AI dispatches, so a mid-pass reload doesn't look idle. */
  dispatches: Dispatch[];
}

/**
 * Where a dispatched AI pass has got to.
 *
 * `duplicate` is not a failure: Restate refused the key because this exact work already
 * ran, which is what makes pressing a button twice free. It is shown because "nothing
 * happened" and "nothing needed to happen" are different answers.
 */
export type DispatchState =
  "queued" | "running" | "done" | "duplicate" | "failed";

/** One AI dispatch — a workflow submission, from accepted through to its outcome. */
export interface Dispatch {
  /** `{workflow}/{key}`, stable across the row's state changes. */
  id: string;
  /** The subject it belongs to, or `""` for work that isn't subject-scoped. */
  subject: string;
  /** The workflow name as the backend knows it (`RootCause`, `SecondOpinion`). */
  kind: string;
  state: DispatchState;
  /** A failure message, or the note explaining a duplicate. */
  detail: string | null;
  started_at: string;
  finished_at: string | null;
}

// Adjacently-tagged event bus payload: { type, data }.
export type Event =
  | { type: "snapshot"; data: Snapshot }
  | { type: "signal"; data: Signal }
  | { type: "subject"; data: SubjectView }
  | { type: "board"; data: SubjectView[] }
  | { type: "hint"; data: Hint }
  | { type: "health"; data: SourceHealth[] }
  | { type: "red_alert"; data: RedAlert }
  | { type: "clear_alert"; data?: undefined }
  | { type: "index_progress"; data: IndexProgressEvent }
  | { type: "agent_chunk"; data: AgentChunk }
  | { type: "dispatch"; data: Dispatch };

export type ChunkKind =
  "started" | "text" | "thinking" | "tool" | "result" | "error" | "exited";

/** One streamed line from an agent session running inside a checkout. */
export interface AgentChunk {
  session_id: string;
  repo: string;
  /** `claude` or `codex`. */
  tool: string;
  kind: ChunkKind;
  text: string;
  /**
   * The tool call this came from, when a subagent produced it — set by
   * `--forward-subagent-text`. Without it a subagent's thinking reads as the main agent talking.
   */
  subagent_of: string | null;
  native_session_id: string | null;
  /** Reported at the end of a turn. Shown, because these sessions spend money by design. */
  cost_usd: number | null;
  /**
   * A continuation of the previous block rather than a new one.
   *
   * Streaming arrives token by token, so appending is what makes a paragraph instead of one word
   * per row.
   */
  delta: boolean;
}

/**
 * One repo's indexing progress, pushed after each batch.
 *
 * Absolute figures, not deltas: a row is patched by replacement, and a delta would need the
 * client to accumulate — a second account of a number the indexer already knows.
 */
export interface IndexProgressEvent {
  repo: string;
  components: number;
  commits_cached: number;
  commits_summarized: number;
  dep_edges: number;
  /** How far back history has been walked; `null` means the walk hasn't started. */
  history_back_to: string | null;
  /** The newest cached commit. */
  last_commit: string | null;
  complete: boolean;
}

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

/// What the API is willing to say about a stored credential. Deliberately no
/// `value` — the backend has no route that returns one.
export interface SecretStatus {
  name: string;
  set: boolean;
  updated_at: string | null;
}

// ---- Issue scoring over the code index ----------------------------------------
// Which repo, component and change an issue is likely about. Ranked hypotheses with
// their evidence — never a confirmed cause.

export interface ScoreEvidence {
  /** `semantic` | `lexical` | `dependency`. */
  pass: string;
  weight: number;
  /** What matched, in the operator's terms. */
  detail: string;
}

export interface ScoreCandidate {
  repo: string;
  /** The module root within it, when the evidence points at one. */
  component: string | null;
  /** The specific change, when a commit summary matched. */
  commit: string | null;
  /** Fused 0..1. A ranking, not a probability. */
  score: number;
  evidence: ScoreEvidence[];
}

export interface ScoreReport {
  origin_repo: string | null;
  terms: string[];
  candidates: ScoreCandidate[];
  /** Set while the index is still building, so a thin answer is explained. */
  index_note: string | null;
}

// ---- code index ------------------------------------------------------------------

/** How far the code index has got with one repo. */
/** What a repo is for. `null` means nobody has said and the name gave no clue. */
export type RepoKind = "code" | "example" | "docs";

export interface RepoIndexProgress {
  full_name: string;
  kind: RepoKind | null;
  /** A human set the kind, so the crawl's name-matching guess won't overwrite it. */
  kind_pinned: boolean;
  summary: string | null;
  language: string | null;
  archived: boolean;
  /** The commit the repo card was built from. */
  indexed_sha: string | null;
  components: number;
  /** Commits fetched locally — the denominator for summarizing. */
  commits_cached: number;
  commits_summarized: number;
  depends_on: number;
  depended_on_by: number;
  /**
   * How far back history has been walked. `null` means the walk hasn't started, which is a
   * different state from "nothing left to do" and looks identical without this field.
   */
  history_back_to: string | null;
  /**
   * The newest cached commit — when the repo last changed, as far as the index has seen.
   *
   * The *other* end of the walk from `history_back_to`, which is the oldest. Showing only one
   * of them unlabelled had people reading the oldest as the newest.
   */
  last_commit: string | null;
}

/** An indexing invocation Restate is running or has just run. */
export interface IndexInvocation {
  repo: string;
  handler: string;
  status: string;
  scope: string | null;
  failure: string | null;
  created_at: string | null;
  completed_at: string | null;
}

export interface IndexTotals {
  repos: number;
  repos_with_components: number;
  repos_untouched: number;
  components: number;
  commits_cached: number;
  commits_summarized: number;
  dep_edges: number;
}

export interface IndexStatus {
  totals: IndexTotals;
  repos: RepoIndexProgress[];
  /** What is being crunched right now. Empty when Restate is unreachable. */
  active: IndexInvocation[];
}

export interface ComponentCard {
  full_name: string;
  path: string;
  purpose: string | null;
  symptoms: string | null;
  digest: string | null;
  indexed_sha: string | null;
}

export interface RepoDep {
  from_repo: string;
  to_repo: string;
  dep_name: string;
  source: string;
}

export interface CommitSummaryRow {
  sha: string;
  summary: string;
  components: string[];
  model: string | null;
  /** First line of the commit message — the summary is behavioural and doesn't restate it. */
  subject: string | null;
  author: string | null;
  committed_at: string | null;
  url: string | null;
}

export interface RepoIndexDetail {
  repo: string;
  entry: {
    full_name: string;
    description: string | null;
    topics: string[];
    language: string | null;
    archived: boolean;
    summary: string | null;
    indexed_sha: string | null;
    fetched_at: string;
  } | null;
  components: ComponentCard[];
  depends_on: RepoDep[];
  depended_on_by: RepoDep[];
  commit_summaries: CommitSummaryRow[];
  history_back_to: string | null;
}

// ---- pull request diffs ----------------------------------------------------------

export interface DiffFile {
  path: string;
  additions: number;
  deletions: number;
  /** The unified hunk, truncated per file. Absent for binary files. */
  patch: string | null;
  /**
   * The patch existed but wasn't kept, to bound what one PR costs in object state.
   *
   * Distinct from a missing patch: "binary" and "we didn't store it" look the same on
   * screen unless they're told apart, and one of them is a fact about the change.
   */
  patch_omitted?: boolean;
}

/**
 * What a review advises doing with the pull request.
 *
 * The three actions a reviewer actually has, rather than a score — a number reads as a grade
 * on the author, and this is only ever about the code.
 */
export type Recommendation = "approve" | "comment" | "request_changes";

/** How much one inline note matters. `praise` is not padding: it is what makes an approval specific. */
export type ReviewSeverity = "blocker" | "concern" | "nit" | "praise";

/** One note against a place in the diff. */
export interface ReviewComment {
  path: string;
  severity: ReviewSeverity;
  note: string;
  /** The line the model quoted, verbatim from the patch. */
  anchor?: string;
  line?: number;
  /**
   * Index into that file's patch lines, resolved by the backend.
   *
   * Absent when neither the quoted line nor the line number matched anything — the note then
   * renders at file level rather than being pinned to a guess, because a confident comment on
   * the wrong line is worse than one with no line.
   */
  patch_index?: number;
}

/** A code review of one pull request. */
export interface Review {
  recommendation: Recommendation;
  /** The review you would write above the Approve button. */
  rationale: string;
  comments: ReviewComment[];
  produced_by: string;
}

export interface PrDiff {
  repo: string;
  number: number;
  files: DiffFile[];
  file_count: number;
  additions: number;
  deletions: number;
  /** What the change does behaviourally, from the local model reading the patches. */
  summary: string | null;
  /** More files than the pane fetched — the change is larger than what is shown. */
  truncated: boolean;
  /** Set instead of the rest when this one PR could not be read. */
  error?: string;
  /** Came from the pull request's object state rather than a fresh read. */
  stored?: boolean;
  /** The review of this change. Absent when the model hasn't produced one. */
  review?: Review | null;
  reviewed_at?: string | null;
  /** When the diff was read from GitHub. */
  fetched_at?: string;
}

export interface PrDiffReport {
  subject_key: string;
  diffs: PrDiff[];
  /**
   * How many pull requests could be shown, whether or not their diffs are stored.
   *
   * The pane needs this to tell "no diff has been read yet" from "there is no pull
   * request here" — which look identical from an empty `diffs`.
   */
  target_count: number;
}
