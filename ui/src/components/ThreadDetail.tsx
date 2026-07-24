import { createEffect, createMemo, createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import { entityHref } from "../entities";
import { renderMarkdown } from "../markdown";
import { hints, patchThreadSignalState, removeHint, setChatSeed, threads } from "../state";
import type { Edge, Memory, Mitigation, Signal } from "../types";
import { SignalModal, signalHref } from "./SignalModal";

// UI-facing provider labels; the id is what the backend maps to a reasoner.
const PROVIDERS = [
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "ollama_local", label: "Ollama (Local)" },
  { id: "ollama", label: "Ollama Cloud" },
] as const;

function successfulCiOnly(signals: Signal[]) {
  if (!signals.length || !signals.every((signal) => {
    const raw = signal.raw && typeof signal.raw === "object"
      ? signal.raw as Record<string, unknown>
      : {};
    return signal.source === "github"
      && raw.subject_type === "CheckSuite";
  })) return false;
  const latest = [...signals].sort((a, b) => b.occurred_at.localeCompare(a.occurred_at))[0];
  const raw = latest.raw && typeof latest.raw === "object"
    ? latest.raw as Record<string, unknown>
    : {};
  return raw.ci_outcome === "success" || latest.title.toLowerCase().includes("succeeded");
}

type TimelineOutcome = "success" | "failure" | "degraded" | "recovered";

function timelineOutcome(signal: Signal): { kind: TimelineOutcome; label: string } | null {
  const raw = signal.raw && typeof signal.raw === "object"
    ? signal.raw as Record<string, unknown>
    : {};
  const ciOutcome = raw.ci_outcome;
  if (ciOutcome === "success") return { kind: "success", label: "passed" };
  if (ciOutcome === "failure") return { kind: "failure", label: "failed" };

  const text = `${signal.title}\n${signal.body ?? ""}`.toLowerCase();
  if (/\b(succeeded|passed|completed successfully)\b/.test(text)) {
    return { kind: "success", label: "passed" };
  }
  if (/\b(failed|failure|timed out|cancelled)\b/.test(text)) {
    return { kind: "failure", label: "failed" };
  }
  if (signal.source === "slack" && signal.state === "resolved") {
    return { kind: "recovered", label: "recovered" };
  }
  if (signal.source === "slack" && (signal.severity === "warning" || signal.severity === "critical")) {
    return { kind: "degraded", label: "active" };
  }
  return null;
}

function failureSuggestion(signal: Signal): string | null {
  if (timelineOutcome(signal)?.kind !== "failure") return null;
  const body = signal.body ?? "";
  const missingModule = body.match(/cannot find module\s+['"`]([^'"`]+)/i)?.[1];
  const prefix = signal.state === "resolved"
    ? "This run was later cleared by a passing check. If it recurs, "
    : "Suggested next step: ";
  if (missingModule) {
    return `${prefix}verify the import and generated source for “${missingModule}”, then rerun this workflow.`;
  }
  const typeScript = body.match(/\bTS\d{4}\b/)?.[0];
  if (typeScript) {
    return `${prefix}fix the ${typeScript} error shown in the CI log, then rerun the required check.`;
  }
  return `${prefix}open the failing job log, address the first error, and rerun the check.`;
}

function renderSummary(src: string): string {
  // Citations are evidence metadata, not prose. Collapse adjacent citations so
  // one well-supported sentence does not turn into a row of equal-weight pills.
  return renderMarkdown(src).replace(/(?:\[(sig|ctx|mem):([^\]\s]+)\])+/g, (group) => {
    const entries = [...group.matchAll(/\[(sig|ctx|mem):([^\]\s]+)\]/g)];
    const detail = entries
      .map(([, kind, id]) => `${kind === "sig" ? "signal" : kind}: ${id}`)
      .join(" · ")
      .replace(/[&<>"]/g, (ch) => ({
        "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;",
      })[ch]!);
    const label = entries.length === 1 ? "evidence" : `evidence ×${entries.length}`;
    return `<span class="citation" title="${detail}">${label}</span>`;
  });
}

export default function ThreadDetail(props: {
  id: string;
  onBack: () => void;
  onOpen: (id: string) => void;
  onOpenChat: () => void;
}) {
  const thread = createMemo(() => threads[props.id]);
  const threadHints = createMemo(() => hints().filter((h) => h.thread_id === props.id));
  const otherThreads = createMemo(() =>
    Object.values(threads).filter((t) => t.id !== props.id),
  );

  const [mitigations, { refetch: refetchMit }] = createResource(
    () => props.id,
    (id) => api.tool<Mitigation[]>("suggest_mitigations", { thread_id: id }),
  );

  const [ctxText, setCtxText] = createSignal("");
  const [ctxUrl, setCtxUrl] = createSignal("");
  const [relateId, setRelateId] = createSignal("");
  const [relateKind, setRelateKind] = createSignal("related");
  const [selected, setSelected] = createSignal<Set<string>>(new Set<string>());
  // The signal whose full detail is popped out in a modal (null = closed).
  const [detail, setDetail] = createSignal<Signal | null>(null);
  // A signal has "details" worth a pop-out when its body carries more than the
  // title already shown in the row.
  const hasDetails = (s: Signal) => !!s.body?.trim() && s.body.trim() !== s.title.trim();

  const [busy, setBusy] = createSignal("");
  const [actionError, setActionError] = createSignal("");
  const [postmortem, setPostmortem] = createSignal<string | null>(null);
  // True while an action that triggers a backend LLM re-analysis is in flight,
  // so the UI can show a "reconsidering" indicator the moment the user acts.
  const [reconsidering, setReconsidering] = createSignal(false);

  // Provider/model to reconsider the thread on. Model list is dynamic per
  // provider (Ollama Cloud lists hosted models when a key is set; Ollama Local
  // lists what's pulled on-device); the selection defaults to the first available
  // and is passed to `reanalyze` as an override.
  const [provider, setProvider] = createSignal<string>(PROVIDERS[0].id);
  const [model, setModel] = createSignal<string>("claude-sonnet-5");
  const [models, { refetch: refetchModels }] = createResource(provider, (p) => api.models(p));
  createEffect(() => {
    const list = models();
    if (list && list.length && !list.includes(model())) setModel(list[0]);
  });

  // The sentence distilled into memory by "save as memory", shown as confirmation.
  const [savedMemory, setSavedMemory] = createSignal<string | null>(null);
  // Postmortem: whether the drafted copy was persisted, and a transient
  // "copied" state for the clipboard button.
  const [pmSaved, setPmSaved] = createSignal(false);
  const [pmCopied, setPmCopied] = createSignal(false);

  const copyPostmortem = async () => {
    const text = postmortem();
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      setPmCopied(true);
      setTimeout(() => setPmCopied(false), 1500);
    } catch {
      /* clipboard blocked — nothing to do */
    }
  };

  // Run an action. `reanalyzes` marks actions whose backend re-runs the thread's
  // LLM analysis — those raise the thinking indicator until the pass returns.
  // Every action re-fetches mitigations, since the thread's signals may have moved.
  const run = async (label: string, fn: () => Promise<unknown>, reanalyzes = false) => {
    setBusy(label);
    setActionError("");
    if (reanalyzes) setReconsidering(true);
    try {
      await fn();
    } catch (e) {
      const detail = String(e).replace(/^Error:\s*/, "");
      setActionError(`${label} failed: ${detail}`);
    } finally {
      setBusy("");
      if (reanalyzes) setReconsidering(false);
      refetchMit();
    }
  };

  const toggle = (id: string) =>
    setSelected((prev) => {
      const next = new Set<string>(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const setState = async (s: Signal, state: Signal["state"]) => {
    patchThreadSignalState(props.id, s.id, state);
    await api.setSignalState(s.id, state).catch(() => {});
  };

  const other = (e: Edge) => (e.thread_a === props.id ? e.thread_b : e.thread_a);

  return (
    <div class="detail">
      <div class="detail-head">
        <button class="back" onClick={props.onBack}>
          ‹ BOARD
        </button>
        <Show when={thread()} fallback={<span class="muted">thread not found (merged?)</span>}>
          <h2 class={`sev-text-${thread()!.severity}`}>{thread()!.title}</h2>
          <span class={`state state-${thread()!.state}`}>{thread()!.state.toUpperCase()}</span>
        </Show>
      </div>

      <Show when={thread()}>
        {(t) => (
          <div class="detail-grid">
            <section class="panel">
              <h3>SUMMARY</h3>
              <Show when={reconsidering()}>
                <div class="reconsidering">
                  <span class="thinking-dots">
                    <i />
                    <i />
                    <i />
                  </span>
                  MuggleBot is reconsidering this thread…
                </div>
              </Show>
              <Show when={actionError()}>
                <div class="flag-strip action-error">
                  <span>{actionError()}</span>
                  <button class="linkish" onClick={() => setActionError("")}>dismiss</button>
                </div>
              </Show>
              <Show
                when={t().summary}
                fallback={<p class="summary muted">No summary yet.</p>}
              >
                <div class="summary md" innerHTML={renderSummary(t().summary!)} />
              </Show>
              <div class="chips">
                <For each={t().entities}>
                  {(e) => {
                    const href = entityHref(e);
                    return (
                      <Show
                        when={href}
                        fallback={
                          <span class="chip">
                            {e.kind}:{e.value}
                          </span>
                        }
                      >
                        <a class="chip chip-link" href={href} target="_blank" rel="noreferrer">
                          {e.kind}:{e.value}
                        </a>
                      </Show>
                    );
                  }}
                </For>
              </div>
              <div class="tags">
                <For each={t().tags} fallback={<span class="muted">no tags</span>}>
                  {(tag) => <span class="chip tag">{tag}</span>}
                </For>
                <button
                  class="linkish"
                  disabled={busy() !== ""}
                  onClick={() => {
                    const next = prompt("Tags (comma-separated):", t().tags.join(", "));
                    if (next === null) return;
                    const tags = next
                      .split(",")
                      .map((s) => s.trim())
                      .filter(Boolean);
                    run(
                      "tags",
                      () => api.tool("set_thread_tags", { thread_id: props.id, tags }),
                      true,
                    );
                  }}
                >
                  {busy() === "tags" ? "…" : t().tags_pinned ? "edit tags" : "tags (auto)"}
                </button>
              </div>
              <div class="model-bar">
                <select value={provider()} onChange={(e) => setProvider(e.currentTarget.value)}>
                  <For each={PROVIDERS}>{(p) => <option value={p.id}>{p.label}</option>}</For>
                </select>
                <select
                  value={model()}
                  // Only truly disabled when we have no list at all. A refetch
                  // (see onFocus) sets `models.loading` while keeping the prior
                  // value, so we stay enabled and don't collapse the open dropdown.
                  disabled={!models()?.length}
                  onFocus={() => refetchModels()}
                  onChange={(e) => setModel(e.currentTarget.value)}
                >
                  <Show
                    when={models()?.length}
                    fallback={
                      <option>
                        {models.loading ? "loading…" : models.error ? "unavailable" : "no models"}
                      </option>
                    }
                  >
                    <For each={models()}>{(m) => <option value={m}>{m}</option>}</For>
                  </Show>
                </select>
              </div>
              <div class="thread-actions">
                <button
                  disabled={busy() !== ""}
                  title="Re-run the LLM analysis on the selected model"
                  onClick={() =>
                    run(
                      "reanalyze",
                      () =>
                        api.tool("reanalyze", {
                          thread_id: props.id,
                          provider: provider(),
                          model: model() || undefined,
                        }),
                      true,
                    )
                  }
                >
                  {busy() === "reanalyze" ? "RECONSIDERING…" : "RECONSIDER"}
                </button>
                <button
                  disabled={busy() !== ""}
                  onClick={() =>
                    run("postmortem", async () => {
                      // Save on generate: a drafted postmortem is persisted to
                      // memory (linked to the thread) as soon as it's produced.
                      const r = await api.tool<{ draft: string; saved_memory: unknown }>(
                        "draft_postmortem",
                        { thread_id: props.id, save: true },
                      );
                      setPostmortem(r.draft);
                      setPmSaved(!!r.saved_memory);
                    })
                  }
                >
                  {busy() === "postmortem" ? "DRAFTING…" : "DRAFT POSTMORTEM"}
                </button>
                <button
                  disabled={busy() !== ""}
                  title="Distill this thread into a one-sentence memory"
                  onClick={() =>
                    run("distill", async () => {
                      const m = await api.tool<Memory>("distill_memory", { thread_id: props.id });
                      setSavedMemory(m.summary);
                    })
                  }
                >
                  {busy() === "distill" ? "SAVING…" : "SAVE AS MEMORY"}
                </button>
                <button
                  disabled={busy() !== ""}
                  title="Start a chat seeded with this thread"
                  onClick={() => {
                    setChatSeed({
                      prompt: `Let's dig into the thread "${t().title}" (thread id: ${props.id}). Summarize what's happening and suggest next steps.`,
                      tags: t().tags,
                    });
                    props.onOpenChat();
                  }}
                >
                  OPEN IN CHAT
                </button>
              </div>
              <Show when={savedMemory()}>
                <div class="saved-memory">
                  <span class="chip tag">memory</span> {savedMemory()}
                  <button class="linkish" onClick={() => setSavedMemory(null)}>dismiss</button>
                </div>
              </Show>
              <Show when={postmortem()}>
                <div class="postmortem">
                  <div class="postmortem-head">
                    <span class="muted">
                      {pmSaved() ? "✓ saved to memory" : "draft (not saved)"}
                    </span>
                    <span class="pm-actions">
                      <button
                        class="icon-btn"
                        title="Copy raw Markdown"
                        onClick={copyPostmortem}
                      >
                        {pmCopied() ? "✓ copied" : "📋 copy"}
                      </button>
                      <button class="linkish" onClick={() => setPostmortem(null)}>
                        dismiss
                      </button>
                    </span>
                  </div>
                  <div class="md" innerHTML={renderMarkdown(postmortem()!)} />
                </div>
              </Show>
            </section>

            <section class="panel timeline-panel">
              <h3>TIMELINE</h3>
              <ol class="timeline">
                <For each={t().signals}>
                  {(s) => (
                    <li>
                      <div class="tl-row">
                        <label class="tl-check">
                          <input
                            type="checkbox"
                            checked={selected().has(s.id)}
                            onChange={() => toggle(s.id)}
                          />
                        </label>
                        <span class={`src src-${s.source}`}>{s.source.toUpperCase()}</span>
                        <Show when={timelineOutcome(s)}>
                          {(outcome) => (
                            <span
                              class={`tl-outcome outcome-${outcome().kind}`}
                              title={`System outcome: ${outcome().label}`}
                            >
                              <span aria-hidden="true">
                                {outcome().kind === "success" || outcome().kind === "recovered" ? "✓" : "!"}
                              </span>
                              {outcome().label}
                            </span>
                          )}
                        </Show>
                        <time>{new Date(s.occurred_at).toLocaleString()}</time>
                        <span class={`state state-${s.state}`}>{s.state}</span>
                      </div>
                      <div class="tl-content">
                        {/* Titles originate upstream and can contain Markdown. Render
                            them with the same safe Markdown pipeline as summaries,
                            rather than showing their syntax as plain text. */}
                        <div class="tl-title md" innerHTML={renderMarkdown(s.title)} />
                        <Show when={failureSuggestion(s)}>
                          {(advice) => (
                            <div class="tl-advice">
                              <span>MUGGLEBOT SUGGESTION</span>
                              <p>{advice()}</p>
                            </div>
                          )}
                        </Show>
                        <div class="tl-tags">
                          <For each={s.tags}>{(tag) => <span class="chip tag">{tag}</span>}</For>
                        </div>
                        <div class="tl-actions">
                          <Show when={signalHref(s)}>
                            <a class="tl-source" href={signalHref(s)} target="_blank" rel="noreferrer">
                              open source ↗
                            </a>
                          </Show>
                          {/* Full alert content (Value/Labels/annotations) lives
                              behind a pop-out so the timeline stays scannable. */}
                          <Show when={hasDetails(s)}>
                            <button class="tl-details" onClick={() => setDetail(s)}>
                              details
                            </button>
                          </Show>
                          <button onClick={() => setState(s, "acknowledged")}>ack</button>
                          <button onClick={() => setState(s, "snoozed")}>snooze</button>
                          <button onClick={() => setState(s, "resolved")}>resolve</button>
                        </div>
                      </div>
                    </li>
                  )}
                </For>
              </ol>
              <div class="row">
                <button
                  disabled={selected().size === 0 || busy() !== ""}
                  onClick={() =>
                    run("split", async () => {
                      await api.tool("split_thread", {
                        thread_id: props.id,
                        signal_ids: [...selected()],
                      });
                      setSelected(new Set<string>());
                    }, true)
                  }
                >
                  SPLIT SELECTED ({selected().size})
                </button>
              </div>
            </section>

            <Show when={threadHints().length}>
              <section class="panel">
                <h3>LIVE ASSIST</h3>
                <For each={threadHints()}>
                  {(h) => (
                    <div class={`hint hint-${h.kind}`} classList={{ flag: h.kind === "flag" }}>
                      <div class="hint-head">
                        <span class="hint-kind">
                          {h.kind === "flag" ? (h.flag_type ?? "flag").replace("_", " ") : h.kind}
                        </span>
                        <span class="muted">{Math.round(h.confidence * 100)}%</span>
                      </div>
                      <div>{h.text}</div>
                      <Show when={h.rationale}>
                        <div class="muted">{h.rationale}</div>
                      </Show>
                      <Show when={h.citations.length}>
                        <div class="cites">cites: {h.citations.join(", ")}</div>
                      </Show>
                      <div class="row">
                        <button onClick={() => run("dismiss", async () => {
                          await api.tool("dismiss_hint", { id: h.id });
                          removeHint(h.id);
                        })}>DISMISS</button>
                        <button onClick={() => run("dismiss", async () => {
                          await api.tool("dismiss_hint", { id: h.id, false_positive: true });
                          removeHint(h.id);
                        })}>FALSE POSITIVE</button>
                      </div>
                    </div>
                  )}
                </For>
              </section>
            </Show>

            <section class="panel mitigations-panel">
              <h3>
                MITIGATIONS
                <Show when={mitigations.loading}>
                  <span class="mini-thinking"> reconsidering…</span>
                </Show>
              </h3>
              <Show
                when={mitigations()?.length}
                fallback={
                  <p class="muted">
                    {successfulCiOnly(t().signals)
                      ? "CI succeeded — no mitigation is needed."
                      : "No mitigations suggested."}
                  </p>
                }
              >
                <For each={mitigations()}>
                  {(m) => (
                    <div class="mitigation">
                      <strong>{m.name}</strong>
                      <p>{m.description}</p>
                      <div class="muted">reversible · cites {m.cited_signals.length} signal(s)</div>
                    </div>
                  )}
                </For>
              </Show>
            </section>

            <Show when={t().edges.some((e) => e.kind !== "distinct")}>
              <section class="panel">
                <h3>RELATION GRAPH</h3>
                {/* `distinct` edges say "these are NOT related" — noise to the
                    reader, so only same/related links are surfaced here. */}
                <For each={t().edges.filter((e) => e.kind !== "distinct")}>
                  {(e) => {
                    // The edge can point at a thread that's off the active board
                    // (resolved/snoozed) or merged away — only offer navigation
                    // when the target actually exists in the loaded board.
                    const target = () => threads[other(e)];
                    return (
                      <div class={`edge edge-${e.kind}`}>
                        <span class="edge-kind">{e.kind}</span>
                        <Show
                          when={target()}
                          fallback={
                            <span class="muted" title={other(e)}>
                              {other(e)} (off board — resolved, snoozed, or merged)
                            </span>
                          }
                        >
                          <button class="linkish" onClick={() => props.onOpen(other(e))}>
                            {target()!.title}
                          </button>
                        </Show>
                        <span class="muted">
                          {e.provenance} · {Math.round(e.confidence * 100)}%
                        </span>
                        <div class="muted">{e.rationale}</div>
                      </div>
                    );
                  }}
                </For>
              </section>
            </Show>

            <Show when={t().context.length}>
              <section class="panel">
                <h3>ATTACHED CONTEXT</h3>
                <For each={t().context}>
                  {(c) => (
                    <div class="ctx-item">
                      <span class="chip">{c.kind}</span> {c.summary ?? c.content}
                    </div>
                  )}
                </For>
              </section>
            </Show>

            <section class="panel">
              <h3>ACTIONS</h3>
              <div class="form">
                <label>Attach context</label>
                <textarea
                  placeholder="free text…"
                  value={ctxText()}
                  onInput={(e) => setCtxText(e.currentTarget.value)}
                />
                <button
                  disabled={!ctxText().trim() || busy() !== ""}
                  onClick={() => run("attach", async () => {
                    await api.tool("attach_thread_context", { thread_id: props.id, text: ctxText() });
                    setCtxText("");
                  }, true)}
                >
                  ATTACH TEXT
                </button>
                <input
                  placeholder="https://runbook…"
                  value={ctxUrl()}
                  onInput={(e) => setCtxUrl(e.currentTarget.value)}
                />
                <button
                  disabled={!ctxUrl().trim() || busy() !== ""}
                  onClick={() => run("attach", async () => {
                    await api.tool("attach_thread_context", { thread_id: props.id, url: ctxUrl() });
                    setCtxUrl("");
                  }, true)}
                >
                  ATTACH URL
                </button>
              </div>
              <div class="form">
                <label>Relate to thread</label>
                <select value={relateId()} onChange={(e) => setRelateId(e.currentTarget.value)}>
                  <option value="">— choose a thread —</option>
                  <For each={otherThreads()}>
                    {(t) => <option value={t.id}>{t.title}</option>}
                  </For>
                </select>
                <div class="row">
                  <select value={relateKind()} onChange={(e) => setRelateKind(e.currentTarget.value)}>
                    <option value="related">related</option>
                    <option value="same">same (merge)</option>
                    <option value="distinct">distinct</option>
                  </select>
                  <button
                    disabled={!relateId().trim() || busy() !== ""}
                    onClick={() => run("relate", async () => {
                      const merged = relateKind() === "same";
                      await api.tool("relate", {
                        thread_a: props.id,
                        thread_b: relateId(),
                        kind: relateKind(),
                      });
                      setRelateId("");
                      if (merged) props.onBack();
                    }, true)}
                  >
                    PIN EDGE
                  </button>
                </div>
              </div>
            </section>
          </div>
        )}
      </Show>

      <Show when={detail()}>
        {(s) => <SignalModal signal={s()} onClose={() => setDetail(null)} />}
      </Show>
    </div>
  );
}
