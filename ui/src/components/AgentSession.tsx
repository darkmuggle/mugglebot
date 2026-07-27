import { createMemo, For, Show } from "solid-js";
import { api } from "../api";
import { agentLog, clearAgentLog } from "../state";
import type { AgentChunk, ChunkKind } from "../types";

/// How each chunk kind reads. Thinking and tool calls are visually subordinate to text on
/// purpose: they are what you watch to judge whether the agent is on the right track, not what
/// you act on.
const KIND_CLASS: Record<ChunkKind, string> = {
  started: "ac-meta",
  text: "ac-text",
  thinking: "ac-thinking",
  tool: "ac-tool",
  result: "ac-result",
  error: "ac-error",
  exited: "ac-meta",
};

const KIND_LABEL: Record<ChunkKind, string> = {
  started: "SESSION",
  text: "",
  thinking: "THINKING",
  tool: "TOOL",
  result: "DONE",
  error: "ERROR",
  exited: "ENDED",
};

/// A live transcript of one agent session.
///
/// The whole point is watching it work: an agent reading the wrong file or heading down a blind
/// alley is obvious within seconds to a human and invisible in a final answer, so the thinking
/// and the tool calls are shown rather than hidden behind a spinner.
export default function AgentSession(props: {
  sessionId: string;
  repo: string;
  tool: string;
  onClose: () => void;
}) {
  const chunks = createMemo<AgentChunk[]>(() => agentLog[props.sessionId] ?? []);

  /// Cost, summed from whatever turns have reported one. Shown because a repo session is the one
  /// thing here that spends money by design — and a number nobody sees is a number nobody weighs.
  const cost = createMemo(() =>
    chunks().reduce((sum, c) => sum + (c.cost_usd ?? 0), 0),
  );

  const done = createMemo(() => chunks().some((c) => c.kind === "exited"));

  return (
    <div class="agent-session">
      <div class="agent-head">
        <span class="explain-label">AGENT · {props.tool.toUpperCase()}</span>
        <span class="agent-repo">{props.repo}</span>
        <Show when={cost() > 0}>
          <span class="chip model-chip" title="Reported by the CLI for this session">
            ${cost().toFixed(4)}
          </span>
        </Show>
        <Show
          when={done()}
          fallback={
            <button
              class="explain-btn"
              title="Kill this session"
              onClick={() => void api.stopAgentSession(props.sessionId)}
            >
              STOP
            </button>
          }
        >
          <span class="chip ph-done">ENDED</span>
        </Show>
        <button
          class="explain-btn"
          onClick={() => {
            clearAgentLog(props.sessionId);
            props.onClose();
          }}
        >
          CLOSE
        </button>
      </div>

      <div class="agent-stream">
        <Show
          when={chunks().length}
          fallback={
            <p class="muted">
              checking {props.repo} out and starting {props.tool}… the first output arrives once
              the agent has read something
            </p>
          }
        >
          <For each={chunks()}>
            {(c) => (
              <div class={`ac ${KIND_CLASS[c.kind]}`}>
                <Show when={KIND_LABEL[c.kind]}>
                  <span class="ac-label">{KIND_LABEL[c.kind]}</span>
                </Show>
                {/* A subagent's output is attributed to the tool call that spawned it —
                    otherwise it reads as the main agent talking to itself. */}
                <Show when={c.subagent_of}>
                  <span class="chip src-chip" title={`subagent of ${c.subagent_of}`}>
                    subagent
                  </span>
                </Show>
                <span class="ac-body">{c.text}</span>
              </div>
            )}
          </For>
        </Show>
      </div>
    </div>
  );
}
