import {
  createEffect,
  createMemo,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import { api } from "../api";
import { entityHref } from "../entities";
import { renderMarkdown } from "../markdown";
import {
  hints,
  patchHandled,
  removeHint,
  setChatSeed,
  subjects,
} from "../state";
import type {
  BrowserInvestigation,
  GrafanaEvidence,
  Edge,
  IssueTriage,
  Memory,
  PrFix,
  ScoreReport,
  RootCauseReport,
  Signal,
} from "../types";
import { AttentionBadge } from "./Attention";
import PersonaPredict from "./PersonaPredict";
import Attempt, { prKey } from "./Attempt";
import { displayTitle, KIND_LABEL, ref } from "./Board";
import DiffPane from "./DiffPane";
import DispatchStrip from "./DispatchStrip";
import { SignalModal, signalHref } from "./SignalModal";

// UI-facing provider labels; the id is what the backend maps to a reasoner.
// Local first, because it is the default. RECONSIDER used to default to Anthropic, which
// meant the button re-ran the analysis on a metered model without anyone asking for one.
const PROVIDERS = [
  { id: "ollama_local", label: "Ollama (Local)" },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "ollama", label: "Ollama Cloud" },
] as const;

// Reuse the signal-state pills so a browser investigation reads like the rest of
// the board rather than inventing a second status vocabulary.
const BROWSER_STATE: Record<BrowserInvestigation["status"], string> = {
  pending: "unseen",
  running: "seen",
  completed: "resolved",
  failed: "unseen",
};

/// The series behind a Grafana reading, folded.
///
/// Not a chart. A chart would put us back where the browser tier is — a picture you have
/// to interpret — and the point of this tier is that the figures in the conclusion above
/// were *checked* against these numbers. So this shows the numbers: what each series is,
/// where it ran, and its sparkline as text. The alert threshold is called out, because
/// "how far past it went" is the question, and any series that touched it is marked.
function GrafanaSeries(props: { raw: string | null }) {
  const parsed = createMemo<GrafanaEvidence | null>(() => {
    if (!props.raw) return null;
    try {
      return JSON.parse(props.raw) as GrafanaEvidence;
    } catch {
      // Stored evidence that won't parse is a bug worth seeing, not worth crashing the
      // page over — the conclusion above still stands on its own.
      return null;
    }
  });
  const threshold = () => parsed()?.rule?.thresholds?.[0] ?? null;
  // Three significant figures, matching `grafana::num` — a figure shown differently here
  // than in the conclusion would read as a discrepancy where there is none.
  const fmt = (v: number) => {
    if (!Number.isFinite(v)) return "n/a";
    if (v === 0) return "0";
    const mag = Math.abs(v);
    const dp = mag >= 100 ? 0 : mag >= 10 ? 1 : mag >= 1 ? 2 : Math.min(6, Math.max(2, 2 - Math.floor(Math.log10(mag))));
    return String(Number(v.toFixed(dp)));
  };
  const breached = (s: GrafanaEvidence["series"][number]) => {
    const t = threshold();
    return t !== null && s.max >= t;
  };

  return (
    <Show when={parsed()?.series?.length}>
      <details class="grafana-evidence">
        <summary>
          {parsed()!.series.length} series read from Grafana
          <Show when={threshold() !== null}>
            <span class="muted"> · threshold {fmt(threshold()!)}</span>
          </Show>
          <Show when={parsed()!.series_omitted > 0}>
            <span class="muted"> · {parsed()!.series_omitted} more not read</span>
          </Show>
        </summary>
        <For each={parsed()!.series}>
          {(s) => (
            <div class="series" classList={{ breached: breached(s) }}>
              <div class="series-head">
                <span class="series-ref">{s.ref_id}</span>
                <span class="series-labels">
                  {Object.entries(s.labels)
                    .map(([k, v]) => `${k}=${v}`)
                    .join(" ") || "(no labels)"}
                </span>
                <Show when={breached(s)}>
                  <span class="series-breach" data-tip="This series reached the alert threshold">
                    past threshold
                  </span>
                </Show>
              </div>
              <div class="series-stats">
                <span>min {fmt(s.min)}</span>
                <span>max {fmt(s.max)}</span>
                <span>mean {fmt(s.mean)}</span>
                <span>first {fmt(s.first)}</span>
                <span>last {fmt(s.last)}</span>
                <span class="muted">{s.points.length} pts</span>
              </div>
            </div>
          )}
        </For>
      </details>
    </Show>
  );
}

const TRIAGE_STATE: Record<IssueTriage["status"], string> = {
  pending: "unseen",
  running: "seen",
  complete: "resolved",
  failed: "unseen",
};

function successfulCiOnly(signals: Signal[]) {
  if (
    !signals.length ||
    !signals.every((signal) => {
      const raw =
        signal.raw && typeof signal.raw === "object"
          ? (signal.raw as Record<string, unknown>)
          : {};
      return signal.source === "github" && raw.subject_type === "CheckSuite";
    })
  )
    return false;
  const latest = [...signals].sort((a, b) =>
    b.occurred_at.localeCompare(a.occurred_at),
  )[0];
  const raw =
    latest.raw && typeof latest.raw === "object"
      ? (latest.raw as Record<string, unknown>)
      : {};
  return (
    raw.ci_outcome === "success" ||
    latest.title.toLowerCase().includes("succeeded")
  );
}

type TimelineOutcome = "success" | "failure" | "degraded" | "recovered";

function timelineOutcome(
  signal: Signal,
): { kind: TimelineOutcome; label: string } | null {
  const raw =
    signal.raw && typeof signal.raw === "object"
      ? (signal.raw as Record<string, unknown>)
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
  if (signal.source === "slack" && signal.upstream_gone) {
    return { kind: "recovered", label: "recovered" };
  }
  if (
    signal.source === "slack" &&
    (signal.severity === "warning" || signal.severity === "critical")
  ) {
    return { kind: "degraded", label: "active" };
  }
  return null;
}

function failureSuggestion(signal: Signal): string | null {
  if (timelineOutcome(signal)?.kind !== "failure") return null;
  const body = signal.body ?? "";
  const missingModule = body.match(/cannot find module\s+['"`]([^'"`]+)/i)?.[1];
  const prefix = signal.upstream_gone
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

// Citation kinds the summary may cite: signals, grounding, dashboard readings,
// and suspected causes. Keep in sync with the summary prompt in correlation/llm.rs.
const CITE_KINDS = "sig|ctx|mem|browser|cause";
const CITE_LABEL: Record<string, string> = {
  sig: "signal",
  browser: "dashboard",
};

/// Drop the `**Headline:**` section. It exists so the board row has one line to show;
/// on this page the title is at the top and the sections below say it at length, so
/// rendering it here is the same fact a third time.
function withoutHeadline(src: string): string {
  return src
    .replace(/^\s*(?:#+\s*)?\*{0,2}headline\*{0,2}\s*:.*$/im, "")
    .trimStart();
}

function renderSummary(src: string): string {
  // Citations are evidence metadata, not prose. Collapse adjacent citations so
  // one well-supported sentence does not turn into a row of equal-weight pills.
  return renderMarkdown(withoutHeadline(src)).replace(
    new RegExp(`(?:\\[(${CITE_KINDS}):([^\\]\\s]+)\\])+`, "g"),
    (group) => {
      const entries = [
        ...group.matchAll(
          new RegExp(`\\[(${CITE_KINDS}):([^\\]\\s]+)\\]`, "g"),
        ),
      ];
      const detail = entries
        .map(([, kind, id]) => `${CITE_LABEL[kind] ?? kind}: ${id}`)
        .join(" · ")
        .replace(
          /[&<>"]/g,
          (ch) =>
            ({
              "&": "&amp;",
              "<": "&lt;",
              ">": "&gt;",
              '"': "&quot;",
            })[ch]!,
        );
      const label =
        entries.length === 1 ? "evidence" : `evidence ×${entries.length}`;
      return `<span class="citation" data-tip="${detail}">${label}</span>`;
    },
  );
}

/// A sticky index of the panels on the page.
///
/// Read from the DOM after render rather than declared alongside the panels, and that is the
/// point: which panels exist depends on what has been analysed — a subject with no triage has no
/// approaches, one with no diff has no verdict — and a hand-maintained list would drift from the
/// render every time a panel gained a condition. Reading the headings back means the spine cannot
/// disagree with the page.
function SectionSpine() {
  const [items, setItems] = createSignal<{ id: string; label: string }[]>([]);

  /// Re-read on a beat rather than on every reactive change: panels appear as their resources
  /// resolve, over a second or two, and the spine only needs to be right once they have.
  const scan = () => {
    const found: { id: string; label: string }[] = [];
    document.querySelectorAll(".detail-grid > section").forEach((el, i) => {
      const h = el.querySelector("h3");
      if (!h) return;
      const id = `sect-${i}`;
      el.setAttribute("id", id);
      // The heading's first clause. "The change — this pull request's diff and review" is a
      // sentence; a spine needs a word.
      const label = (h.textContent || "")
        .split(/[—·(]/)[0]
        .trim()
        .slice(0, 22);
      if (label) found.push({ id, label });
    });
    setItems(found);
  };

  onMount(() => {
    scan();
    const t = window.setInterval(scan, 1500);
    onCleanup(() => window.clearInterval(t));
  });

  return (
    <Show when={items().length > 2}>
      <nav class="spine">
        <For each={items()}>
          {(s) => (
            <button
              class="spine-item"
              onClick={() =>
                document
                  .getElementById(s.id)
                  ?.scrollIntoView({ behavior: "smooth", block: "start" })
              }
            >
              {s.label}
            </button>
          )}
        </For>
      </nav>
    </Show>
  );
}

export default function SubjectDetail(props: {
  id: string;
  onBack: () => void;
  onOpen: (id: string) => void;
  onOpenChat: () => void;
}) {
  const thread = createMemo(() => subjects[props.id]);
  const threadHints = createMemo(() =>
    hints().filter((h) => h.subject_key === props.id),
  );
  const otherThreads = createMemo(() =>
    Object.values(subjects).filter((t) => t.key !== props.id),
  );

  const [browserInvestigations, { refetch: refetchBrowserInvestigations }] =
    createResource(
      () => props.id,
      (id) =>
        api.tool<BrowserInvestigation[]>("list_browser_investigations", {
          subject_key: id,
        }),
    );
  const [rootCause, { refetch: refetchRootCause }] = createResource(
    () => props.id,
    (id) =>
      api.tool<RootCauseReport | null>("get_root_cause", { subject_key: id }),
  );
  const [triage, { refetch: refetchTriage }] = createResource(
    () => props.id,
    (id) => api.tool<IssueTriage[]>("get_issue_triage", { subject_key: id }),
  );
  // PR-fix candidates per triaged issue, fetched together so the panel can show
  // "somebody is already on this" next to the issue it belongs to.
  const [prFixes, { refetch: refetchPrFixes }] = createResource(
    () =>
      triage()
        ?.map((t) => t.issue_key)
        .join(","),
    async (keys) => {
      const out: Record<string, PrFix[]> = {};
      for (const key of keys.split(",").filter(Boolean)) {
        out[key] = await api
          .tool<PrFix[]>("list_pr_fixes", { issue_key: key })
          .catch(() => []);
      }
      return out;
    },
  );
  /// Every pull request on this work, from both places one can be recorded, deduped by
  /// `repo!number`.
  ///
  /// The subject itself is excluded: when this subject is a PR, its diff already leads the page
  /// under THE CHANGE, and listing it again as an attempt at itself is a second copy of the same
  /// diff.
  const attempts = createMemo(() => {
    const seen = new Set<string>([props.id]);
    const out: PrFix[] = [];
    for (const pr of [
      ...(thread()?.pull_requests ?? []),
      ...Object.values(prFixes() ?? {}).flat(),
    ]) {
      const key = prKey(pr);
      if (seen.has(key)) continue;
      seen.add(key);
      out.push(pr);
    }
    return out;
  });

  // The browser worker and the investigator both run in the background, so poll
  // while either is in flight rather than leaving a stale "running" on screen.
  createEffect(() => {
    const inFlight = (s?: string) => s === "pending" || s === "running";
    const pending =
      browserInvestigations()?.some((i) => inFlight(i.status)) ||
      triage()?.some((t) => inFlight(t.status)) ||
      rootCause()?.status === "running";
    if (!pending) return;
    const timer = setInterval(() => {
      refetchBrowserInvestigations();
      refetchRootCause();
      refetchTriage();
      refetchPrFixes();
    }, 5000);
    onCleanup(() => clearInterval(timer));
  });

  const [ctxText, setCtxText] = createSignal("");
  const [ctxUrl, setCtxUrl] = createSignal("");
  const [relateId, setRelateId] = createSignal("");
  const [relateKind, setRelateKind] = createSignal("related");
  const [selected, setSelected] = createSignal<Set<string>>(new Set<string>());
  // The signal whose full detail is popped out in a modal (null = closed).
  const [detail, setDetail] = createSignal<Signal | null>(null);
  // A signal has "details" worth a pop-out when its body carries more than the
  // title already shown in the row.
  const hasDetails = (s: Signal) =>
    !!s.body?.trim() && s.body.trim() !== s.title.trim();

  const [busy, setBusy] = createSignal("");
  const [actionError, setActionError] = createSignal("");
  const [postmortem, setPostmortem] = createSignal<string | null>(null);
  // Set only when a submission was refused because nothing has changed — the panel
  // below is then already showing the current answer, and saying so beats a button that
  // appears to have done nothing.
  const [explainNote, setExplainNote] = createSignal("");
  // Ranked candidates from the code index. Fetched on demand rather than with the
  // subject: it's a KNN over the whole index, and most subjects are never scored.
  const [scores, setScores] = createSignal<ScoreReport | null>(null);
  const [browserFindings, setBrowserFindings] = createSignal<
    Record<string, string>
  >({});
  // True while an action that triggers a backend LLM re-analysis is in flight,
  // so the UI can show a "reconsidering" indicator the moment the user acts.
  const [reconsidering, setReconsidering] = createSignal(false);

  // Provider/model to reconsider the thread on. Model list is dynamic per
  // provider (Ollama Cloud lists hosted models when a key is set; Ollama Local
  // lists what's pulled on-device); the selection defaults to the first available
  // and is passed to `reanalyze` as an override.
  const [provider, setProvider] = createSignal<string>(PROVIDERS[0].id);
  // Filled in from the provider's model list by the effect below; empty rather than a
  // hardcoded cloud model, which would send the first RECONSIDER off-machine.
  const [model, setModel] = createSignal<string>("");
  const [models, { refetch: refetchModels }] = createResource(provider, (p) =>
    api.models(p),
  );
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
  const run = async (
    label: string,
    fn: () => Promise<unknown>,
    reanalyzes = false,
  ) => {
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
    }
  };

  const toggle = (id: string) =>
    setSelected((prev) => {
      const next = new Set<string>(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  // Triage applies to the whole subject: the operator is deciding about the work,
  // not about one of its notifications.
  const setHandled = async (
    handled: "acknowledged" | "snoozed" | "resolved" | "open",
  ) => {
    patchHandled(props.id, handled);
    await api.setHandled(props.id, handled).catch(() => {});
  };

  /// Set when a second opinion has been asked for and has not arrived yet.
  ///
  /// The workflow runs for tens of seconds and writes its answer over the WebSocket, so without
  /// this the button did nothing observable and the panel simply changed some time later. That is
  /// why it read as broken when it was working — the backend had already written a 1.7KB answer.
  const [awaitingSecond, setAwaitingSecond] = createSignal(false);

  /// The cloud explanation, once it exists.
  const cloudOpinion = () =>
    thread()?.explanations.find((x) => x.produced_by === "cloud") ?? null;

  /// Stop waiting the moment one lands — the arrival *is* the completion signal, so nothing has
  /// to poll or time out.
  createEffect(() => {
    if (cloudOpinion()) setAwaitingSecond(false);
  });

  /// Whether this subject is currently muted, so the buttons offer the way back rather than a
  /// second helping of what has already been done.
  const isHandled = () => {
    const h = thread()?.handled;
    return h === "acknowledged" || h === "snoozed" || h === "resolved";
  };

  const other = (e: Edge) =>
    e.subject_a === props.id ? e.subject_b : e.subject_a;

  const [allRelations, setAllRelations] = createSignal(false);

  /// A reasoning pass ran and stored something the backend then judged unusable.
  ///
  /// `decorated.summary` is the backend's own verdict on the stored text, so this reads
  /// its judgement rather than re-implementing it here — a second copy of that rule in
  /// TypeScript would drift from the Rust one.
  const summaryFailed = createMemo(() => {
    const t = thread();
    return !!t?.last_reasoned_at && !t.attention.decorated.summary;
  });

  /// Which related subjects are worth the reader's attention, and which are behind
  /// "N more".
  ///
  /// This panel used to render every non-distinct edge — ten of them on a normal
  /// subject, each an LLM guess at 80-95% confidence with a rationale along the lines
  /// of "both deal with Restate Cloud", several pointing at subjects that are off the
  /// board anyway. Ten equally-weighted maybes is not a graph, it's a haystack.
  ///
  /// Ranked by how much the link is worth acting on: a human pin beats a machine
  /// guess, `same` beats `related`, and a target you can actually open beats one that
  /// has been resolved or merged away.
  const relations = createMemo(() => {
    const t = thread();
    const edges = (t?.edges ?? [])
      .filter((e) => e.kind !== "distinct")
      .sort((a, b) => rank(b) - rank(a));
    return allRelations()
      ? { shown: edges, hidden: [] as Edge[] }
      : { shown: edges.slice(0, 3), hidden: edges.slice(3) };
  });

  function rank(e: Edge): number {
    let score = e.confidence;
    if (e.provenance === "user") score += 10; // a decision, not a guess
    if (e.kind === "same") score += 2;
    if (subjects[other(e)]) score += 1; // still on the board, so still actionable
    return score;
  }

  return (
    <div class="detail">
      <div class="detail-head">
        <button class="back" onClick={props.onBack}>
          ‹ Board
        </button>
        <Show
          when={thread()}
          fallback={<span class="muted">thread not found (merged?)</span>}
        >
          {/* Same treatment as a board row: the kind and the reference come out of the
              title string, where GitHub sends them as a prefix and a parenthetical. */}
          <h2 class={`sev-text-${thread()!.severity}`}>
            {displayTitle(thread()!)}
          </h2>
          <span class="detail-ref">{ref(thread()!)}</span>
          <AttentionBadge attention={thread()!.attention} />
        </Show>
      </div>

      {/* What the AI is doing for this subject, above everything it has produced. The
          dispatches are the answer to "did that button do anything", so they belong where
          the eye lands after pressing one — not in a panel further down. */}
      <DispatchStrip subjectKey={props.id} />

      {/* The map. Seven panels over several screens with nine same-weight headings gave no
          indication of what the page even contained — you landed on Summary with no way to know
          there was a verdict, three proposed approaches, PR-fix candidates and predictions below.
          Built from the panels actually present, so it never offers a section that is not there. */}
      <SectionSpine />

      <Show when={thread()}>
        {(t) => (
          <div class="detail-grid">
            <section class="panel">
              <h3>Summary</h3>
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
                  <button class="linkish" onClick={() => setActionError("")}>
                    dismiss
                  </button>
                </div>
              </Show>
              {/* The local explanation and, if asked for, the cloud second opinion —
                  both, each labelled. Comparing them is the whole reason to have asked. */}
              {/* A placeholder where the answer will land, so the wait is visible in the place the
                  operator is already looking. A button that greys out and a panel that appears
                  half a minute later, with nothing in between, is why this read as broken. */}
              <Show when={awaitingSecond()}>
                <div class="explain-panel explain-cloud">
                  <div class="explain-head">
                    <span class="explain-label">Second opinion</span>
                    <span class="chip model-chip">CLOUD</span>
                    <span class="muted">
                      reading the same dossier — this takes tens of seconds and
                      arrives here on its own
                    </span>
                  </div>
                </div>
              </Show>
              <For each={t().explanations}>
                {(x) => (
                  <div
                    class="explain-panel"
                    classList={{ "explain-cloud": x.produced_by === "cloud" }}
                  >
                    <div class="explain-head">
                      <span class="explain-label">
                        {x.produced_by === "cloud"
                          ? "Second opinion"
                          : "Explanation"}
                      </span>
                      <span
                        class="chip model-chip"
                        data-tip="which model wrote this"
                      >
                        {x.produced_by === "cloud" ? "CLOUD" : "LOCAL"}
                      </span>
                      <For each={x.sources}>
                        {(src) => (
                          <span class="chip src-chip">
                            {src.replace(/_/g, " ")}
                          </span>
                        )}
                      </For>
                      <Show
                        when={
                          t().signals.length &&
                          t().signals[t().signals.length - 1].id !== x.watermark
                        }
                      >
                        <span
                          class="chip chip-stale"
                          data-tip="activity has landed since this was written"
                        >
                          STALE
                        </span>
                      </Show>
                    </div>
                    <div class="md" innerHTML={renderSummary(x.markdown)} />
                    {/* Claims the dossier check removed. Shown, not swallowed. */}
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
              <Show when={explainNote()}>
                <p class="muted">{explainNote()}</p>
              </Show>

              <Show when={scores()}>
                {(report) => (
                  <div class="scores">
                    <div class="explain-head">
                      <span class="explain-label">Likely location</span>
                      <Show when={report().origin_repo}>
                        <span class="chip">
                          filed in {report().origin_repo}
                        </span>
                      </Show>
                      {/* A thin answer from a half-built index is a different thing
                          from a thin answer from a complete one. */}
                      <Show when={report().index_note}>
                        <span class="chip chip-stale">
                          {report().index_note}
                        </span>
                      </Show>
                    </div>
                    <Show
                      when={report().candidates.length}
                      fallback={
                        <p class="muted">
                          Nothing in the index resembles this yet.
                        </p>
                      }
                    >
                      <For each={report().candidates}>
                        {(c) => (
                          <div class="score-row">
                            <div class="score-head">
                              <span class="score-pct">
                                {Math.round(c.score * 100)}%
                              </span>
                              <span class="score-target">
                                {c.repo}
                                <Show when={c.component && c.component !== "."}>
                                  <span class="score-component">
                                    {" "}
                                    / {c.component}
                                  </span>
                                </Show>
                                <Show when={c.commit}>
                                  <span class="score-commit">
                                    {" "}
                                    @{c.commit!.slice(0, 8)}
                                  </span>
                                </Show>
                              </span>
                            </div>
                            {/* The evidence is the point: without it the operator can't
                                tell a strong semantic match from a lucky substring. */}
                            <For each={c.evidence}>
                              {(e) => (
                                <div class="score-evidence">
                                  <span class={`chip pass-${e.pass}`}>
                                    {e.pass}
                                  </span>
                                  <span>{e.detail}</span>
                                </div>
                              )}
                            </For>
                          </div>
                        )}
                      </For>
                      <p class="muted score-caveat">
                        Ranked hypotheses over the code index — not a confirmed
                        cause.
                      </p>
                    </Show>
                  </div>
                )}
              </Show>
              {/* A stored summary the backend has judged unusable — it recited the
                  prompt, pasted the evidence, or copied the discussion — must not be
                  rendered as if it were content. `last_reasoned_at` set with
                  `decorated.summary` false is exactly that case, and the board row
                  already says "not summarised": this is what stops the two disagreeing. */}
              <Show
                when={t().summary && !summaryFailed()}
                fallback={
                  <p class="summary muted">
                    <Show
                      when={summaryFailed()}
                      fallback={<>No summary yet.</>}
                    >
                      The last summary pass produced nothing usable — it repeated its
                      own instructions or the evidence rather than summarising. Re-run
                      it from Analyse.
                    </Show>
                  </p>
                }
              >
                <div
                  class="summary md"
                  innerHTML={renderSummary(t().summary!)}
                />
              </Show>
              <div class="chips">
                <For each={t().keys}>
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
                        <a
                          class="chip chip-link"
                          href={href}
                          target="_blank"
                          rel="noreferrer"
                        >
                          {e.kind}:{e.value}
                        </a>
                      </Show>
                    );
                  }}
                </For>
              </div>
              <div class="tags">
                <For
                  each={t().tags}
                  fallback={<span class="muted">no tags</span>}
                >
                  {(tag) => <span class="chip tag">{tag}</span>}
                </For>
                <button
                  class="linkish"
                  disabled={busy() !== ""}
                  onClick={() => {
                    const next = prompt(
                      "Tags (comma-separated):",
                      t().tags.join(", "),
                    );
                    if (next === null) return;
                    const tags = next
                      .split(",")
                      .map((s) => s.trim())
                      .filter(Boolean);
                    run(
                      "tags",
                      () =>
                        api.tool("set_subject_tags", {
                          subject_key: props.id,
                          tags,
                        }),
                      true,
                    );
                  }}
                >
                  {busy() === "tags"
                    ? "…"
                    : t().tags_pinned
                      ? "edit tags"
                      : "tags (auto)"}
                </button>
              </div>
              {/* Triage first and on its own, then everything the AI can be asked to
                  do, behind one disclosure.

                  This was eleven identically-weighted buttons and two model selects in
                  a single wrapped row: ACK SNOOZE RECONSIDER WHERE? EXPLAIN Second opinion
                  DRAFT POSTMORTEM SAVE AS MEMORY OPEN IN CHAT. Nothing said which one
                  the operator wanted, which cost money, or which two did nearly the
                  same thing. Triage is the decision this screen exists to support, so
                  it gets the primary button; the analyses are a menu that names its own
                  cost. */}
              <div class="thread-actions">
                <Show
                  when={isHandled()}
                  fallback={
                    <>
                      <button
                        class="primary"
                        disabled={busy() !== ""}
                        onClick={() => setHandled("acknowledged")}
                      >
                        Ack
                      </button>
                      <button
                        disabled={busy() !== ""}
                        onClick={() => setHandled("snoozed")}
                      >
                        Snooze
                      </button>
                    </>
                  }
                >
                  {/* Named for what it undoes — "Reopen" against three different states
                      tells you nothing about which one you are leaving. */}
                  <button
                    class="primary"
                    disabled={busy() !== ""}
                    onClick={() => setHandled("open")}
                  >
                    {thread()?.handled === "snoozed"
                      ? "Un-snooze"
                      : thread()?.handled === "resolved"
                        ? "Un-resolve"
                        : "Un-ack"}
                  </button>
                </Show>

                <details class="menu">
                  <summary>Analyse ▾</summary>
                  <div class="menu-body">
                    {/* One pass over everything under this subject: its events, every PR
                        attempting it with the critique and what reviewers said, the
                        proposed causes, the triage, the attached context. On a PR it
                        explains just that change. Free when nothing has changed since the
                        last one — the workflow key collides and the explanation already
                        on screen is the answer. */}
                    <button
                      disabled={busy() !== ""}
                      onClick={() =>
                        run("explain", async () => {
                          const r = await api.explain(props.id);
                          if (!r.submitted) setExplainNote(r.note);
                        })
                      }
                    >
                      {busy() === "explain" ? "Explaining…" : "Explain"}
                      <span class="cost">on this machine</span>
                    </button>
                    {/* The only action here that reaches a cloud model, which is why it
                        says so. Same dossier, same rules, different model — so the
                        difference between the two answers is the model and nothing else. */}
                    <button
                      class="cloud-btn"
                      disabled={busy() !== "" || awaitingSecond()}
                      data-tip={
                        cloudOpinion()
                          ? "A cloud second opinion is already below; asking again re-reads the same dossier"
                          : "Ask the cloud model for its own read of the same dossier"
                      }
                      onClick={() =>
                        run("second", async () => {
                          const r = await api.explain(props.id, true);
                          // `submitted: false` means the key collided — nothing has changed,
                          // so the answer already on screen *is* the answer. Saying so beats
                          // a spinner that never resolves.
                          if (r.submitted) setAwaitingSecond(true);
                          else setExplainNote(r.note);
                        })
                      }
                    >
                      {awaitingSecond()
                        ? "Asking Claude…"
                        : cloudOpinion()
                          ? "Second opinion ✓"
                          : "Second opinion"}
                      <span class="cost cost-metered">metered call</span>
                    </button>
                    {/* Where should I even start? Ranks repo / component / commit over
                        the code index — the question asked long before "why did this
                        break", and the one a 147-repo org can't answer by searching.
                        Named for the question now; it used to be labelled "WHERE?". */}
                    <button
                      disabled={busy() !== ""}
                      data-tip="Rank which repo, component and change this is likely about"
                      onClick={() =>
                        run("score", async () => {
                          setScores(await api.scoreIssue(props.id));
                        })
                      }
                    >
                      {busy() === "score"
                        ? "Locating…"
                        : "Find the likely code"}
                      <span class="cost">on this machine</span>
                    </button>
                    {/* Not on a pull request. A postmortem is written about something
                        that went wrong; a PR is the attempt to put it right, and drafting
                        one from a change's timeline produces a postmortem of the fix. */}
                    <Show when={t().rank !== "pull_request"}>
                      <button
                        disabled={busy() !== ""}
                        onClick={() =>
                          run("postmortem", async () => {
                            // Save on generate: a drafted postmortem is persisted to
                            // memory (linked to the thread) as soon as it's produced.
                            const r = await api.tool<{
                              draft: string;
                              saved_memory: unknown;
                            }>("draft_postmortem", {
                              subject_key: props.id,
                              save: true,
                            });
                            setPostmortem(r.draft);
                            setPmSaved(!!r.saved_memory);
                          })
                        }
                      >
                        {busy() === "postmortem"
                          ? "Drafting…"
                          : "Draft postmortem"}
                        <span class="cost">on this machine</span>
                      </button>
                    </Show>
                    <button
                      disabled={busy() !== ""}
                      data-tip="Distill this thread into a one-sentence memory"
                      onClick={() =>
                        run("distill", async () => {
                          const m = await api.tool<Memory>("distill_memory", {
                            subject_key: props.id,
                          });
                          setSavedMemory(m.summary);
                        })
                      }
                    >
                      {busy() === "distill" ? "Saving…" : "Save as memory"}
                      <span class="cost">on this machine</span>
                    </button>

                    {/* Re-run on a model you pick. The two selects live here because
                        this is the only action they configure — at the top of the panel
                        they read as a global setting for the whole screen. */}
                    <div class="menu-sep">Re-run on a specific model</div>
                    <div class="model-bar">
                      <select
                        value={provider()}
                        onChange={(e) => setProvider(e.currentTarget.value)}
                      >
                        <For each={PROVIDERS}>
                          {(p) => <option value={p.id}>{p.label}</option>}
                        </For>
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
                              {models.loading
                                ? "loading…"
                                : models.error
                                  ? "unavailable"
                                  : "no models"}
                            </option>
                          }
                        >
                          <For each={models()}>
                            {(m) => <option value={m}>{m}</option>}
                          </For>
                        </Show>
                      </select>
                    </div>
                    <button
                      disabled={busy() !== ""}
                      data-tip="Re-run the whole analysis on the model selected above"
                      onClick={() =>
                        run(
                          "reanalyze",
                          () =>
                            api.tool("reanalyze", {
                              subject_key: props.id,
                              provider: provider(),
                              model: model() || undefined,
                            }),
                          true,
                        )
                      }
                    >
                      {busy() === "reanalyze"
                        ? "Re-analysing…"
                        : "Re-analyse from scratch"}
                    </button>
                  </div>
                </details>

                <button
                  disabled={busy() !== ""}
                  data-tip="Start a chat seeded with this thread"
                  onClick={() => {
                    setChatSeed({
                      prompt: `Let's dig into the thread "${t().title}" (thread id: ${props.id}). Summarize what's happening and suggest next steps.`,
                      tags: t().tags,
                    });
                    props.onOpenChat();
                  }}
                >
                  Open in chat
                </button>
              </div>
              <Show when={savedMemory()}>
                <div class="saved-memory">
                  <span class="chip tag">memory</span> {savedMemory()}
                  <button class="linkish" onClick={() => setSavedMemory(null)}>
                    dismiss
                  </button>
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
                        data-tip="Copy raw Markdown"
                        onClick={copyPostmortem}
                      >
                        {pmCopied() ? "✓ copied" : "📋 copy"}
                      </button>
                      <button
                        class="linkish"
                        onClick={() => setPostmortem(null)}
                      >
                        dismiss
                      </button>
                    </span>
                  </div>
                  <div class="md" innerHTML={renderMarkdown(postmortem()!)} />
                </div>
              </Show>
            </section>

            <section class="panel timeline-panel">
              <h3>Timeline</h3>
              {/* Evidence, folded — the same rule the patches now follow. Two events cost 333px
                  and six buttons on a real subject, above the verdict and the options, and a
                  timeline is what you consult *after* deciding something looks wrong rather than
                  the first thing you read. The count and the span are on the summary line, so
                  folding it never hides that there is history to look at. */}
              <details class="timeline-fold">
                <summary>
                  <span class="tl-count">
                    {t().signals.length} event{t().signals.length === 1 ? "" : "s"}
                  </span>
                  <Show when={t().signals.length}>
                    <span class="muted">
                      {new Date(
                        t().signals[t().signals.length - 1].occurred_at,
                      ).toLocaleDateString()}
                      {t().signals.length > 1
                        ? ` – ${new Date(t().signals[0].occurred_at).toLocaleDateString()}`
                        : ""}
                    </span>
                  </Show>
                </summary>
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
                        {/* Source *and* kind. The pill used to say only "GITHUB", which
                            told the reader the one thing every entry here already had in
                            common and left out the one that varies — whether this thread is
                            an issue or a pull request. Read from the same `KIND_LABEL` the
                            board row uses, so the detail view cannot name a thread something
                            the row it was opened from didn't.

                            Only for GitHub signals on a GitHub subject: a Slack message
                            attached to a PR (they get moved, see `subject::attach`) is still
                            a Slack event, and labelling it "GitHub PR" would be a lie about
                            where it came from. */}
                        <span class={`src src-${s.source}`}>
                          {s.source === "github" && t().rank !== "slack_thread"
                            ? KIND_LABEL[t().rank].toUpperCase()
                            : s.source.toUpperCase()}
                        </span>
                        <Show when={timelineOutcome(s)}>
                          {(outcome) => (
                            <span
                              class={`tl-outcome outcome-${outcome().kind}`}
                              data-tip={`System outcome: ${outcome().label}`}
                            >
                              <span aria-hidden="true">
                                {outcome().kind === "success" ||
                                outcome().kind === "recovered"
                                  ? "✓"
                                  : "!"}
                              </span>
                              {outcome().label}
                            </span>
                          )}
                        </Show>
                        <time>{new Date(s.occurred_at).toLocaleString()}</time>
                        {/* No "gone upstream" pill. It is bookkeeping about the
                            notification rather than about the work — every superseded
                            entry in a five-event timeline carries it, so it marked
                            almost everything and distinguished nothing. The reconciler
                            still uses the flag; it just isn't news to the reader. */}
                      </div>
                      <div class="tl-content">
                        {/* Titles originate upstream and can contain Markdown. Render
                            them with the same safe Markdown pipeline as summaries,
                            rather than showing their syntax as plain text. */}
                        <div
                          class="tl-title md"
                          innerHTML={renderMarkdown(s.title)}
                        />
                        <Show when={failureSuggestion(s)}>
                          {(advice) => (
                            <div class="tl-advice">
                              <span>MuggleBot suggestion</span>
                              <p>{advice()}</p>
                            </div>
                          )}
                        </Show>
                        <div class="tl-tags">
                          <For each={s.tags}>
                            {(tag) => <span class="chip tag">{tag}</span>}
                          </For>
                        </div>
                        <div class="tl-actions">
                          <Show when={signalHref(s)}>
                            <a
                              class="tl-source"
                              href={signalHref(s)}
                              target="_blank"
                              rel="noreferrer"
                            >
                              {/* "Open source" set in caps read as open-source
                                  software rather than "open the source of this
                                  event", which is what it does. */}
                              Open where it happened ↗
                            </a>
                          </Show>
                          {/* Full alert content (Value/Labels/annotations) lives
                              behind a pop-out so the timeline stays scannable. */}
                          <Show when={hasDetails(s)}>
                            <button
                              class="tl-details"
                              onClick={() => setDetail(s)}
                            >
                              details
                            </button>
                          </Show>
                          {/* Triage applies to the subject, not to one of its
                              notifications — see `setHandled`. The per-event
                              buttons that used to be here couldn't express
                              anything coherent. */}
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
                    run(
                      "split",
                      async () => {
                        await api.tool("split_thread", {
                          subject_key: props.id,
                          signal_ids: [...selected()],
                        });
                        setSelected(new Set<string>());
                      },
                      true,
                    )
                  }
                >
                  Split selected ({selected().size})
                </button>
              </div>
              </details>
            </section>

            {/* What the browser read off any linked dashboard. MuggleBot drives
                the operator's signed-in Chrome read-only; the manual paste box is
                the fallback for when it can't reach it. */}
            <Show when={browserInvestigations()?.length}>
              <section class="panel browser-investigations">
                <h3>Dashboard readings</h3>
                <For each={browserInvestigations()}>
                  {(inv) => (
                    <div class="browser-investigation">
                      <div class="browser-investigation-head">
                        <span
                          class={`state state-${BROWSER_STATE[inv.status]}`}
                        >
                          {inv.status}
                        </span>
                        {/* Which tier answered, because it changes how much the reading is
                            worth: a `grafana` reading quotes series that were checked, a
                            `browser` reading describes a rendered page and cannot be. */}
                        <span
                          class="read-via"
                          classList={{ verified: inv.method === "grafana" }}
                          data-tip={
                            inv.method === "grafana"
                              ? "Read from Grafana's API — every figure below was checked against the series"
                              : "Read off the rendered page in your signed-in Chrome — figures are as the model saw them"
                          }
                        >
                          {inv.method === "grafana" ? "series" : "page"}
                        </span>
                        <a href={inv.url} target="_blank" rel="noreferrer">
                          open dashboard ↗
                        </a>
                        <Show when={inv.attempts > 1}>
                          <span class="muted">attempt {inv.attempts}</span>
                        </Show>
                      </div>
                      <Show when={inv.status === "running"}>
                        <p class="muted">
                          {inv.method === "grafana"
                            ? "Querying the series behind this alert…"
                            : "Reading the page in your authenticated Chrome…"}
                        </p>
                      </Show>
                      <Show when={inv.status === "pending"}>
                        <p class="muted">
                          {inv.method === "grafana"
                            ? "Queued — resolving the alert rule, then its queries."
                            : "Queued — the browser worker takes one page at a time."}
                        </p>
                      </Show>
                      <Show when={inv.error}>
                        <p class="browser-error">{inv.error}</p>
                      </Show>
                      <Show
                        when={inv.findings}
                        fallback={
                          <Show when={inv.status === "failed"}>
                            <div class="browser-findings-entry">
                              <textarea
                                placeholder="Paste findings by hand if the browser can't reach the page…"
                                value={browserFindings()[inv.id] ?? ""}
                                onInput={(e) =>
                                  setBrowserFindings((current) => ({
                                    ...current,
                                    [inv.id]: e.currentTarget.value,
                                  }))
                                }
                              />
                              <button
                                disabled={
                                  !browserFindings()[inv.id]?.trim() ||
                                  busy() !== ""
                                }
                                onClick={() =>
                                  run(
                                    "browser findings",
                                    async () => {
                                      await api.tool(
                                        "record_browser_investigation",
                                        {
                                          id: inv.id,
                                          findings: browserFindings()[inv.id],
                                        },
                                      );
                                      await refetchBrowserInvestigations();
                                    },
                                    true,
                                  )
                                }
                              >
                                RECORD FINDINGS
                              </button>
                            </div>
                          </Show>
                        }
                      >
                        <>
                          <div
                            class="browser-findings md"
                            innerHTML={renderMarkdown(inv.findings!)}
                          />
                          {/* The series the conclusion was drawn from, folded. This is what
                              makes the reading checkable rather than merely plausible — but
                              the conclusion is the answer and the numbers are what back it,
                              so the same rule as the diff applies: unfold the judgement,
                              fold the evidence. */}
                          <GrafanaSeries raw={inv.evidence} />
                        </>
                      </Show>
                    </div>
                  )}
                </For>
              </section>
            </Show>

            {/* The change itself, when this subject *is* a pull request.
                A PR subject has no attempt rows of its own — `pull_requests` is keyed by the
                issue a PR attempts, so a PR is never listed under itself — and the result was a
                PR whose diff had been read, stored, and then shown nowhere. Its own diff is the
                single most relevant thing on the page, so it leads, unfolded. */}
            <Show when={t().rank === "pull_request"}>
              <section class="panel attempts-panel">
                <h3>
                  The change
                  <span class="muted">
                    {" "}
                    — this pull request's diff and review
                  </span>
                </h3>
                <DiffPane subjectKey={props.id} expand />
              </section>
            </Show>

            {/* The attempts, with their diffs — the same renderer the board uses.
                Clicking into an issue used to lose the pull requests attempting it, which
                is the opposite of what clicking in is for: the card is the summary and
                this is supposed to be the whole story. The diff comes from the pull
                request's own object state, so this costs a state read rather than an API
                call and a model pass.

                Two sources, unioned: the view's own `pull_requests`, and the PR-fix rows the
                triage panel fetches per triaged issue. Either can be populated without the
                other — a judged PR lands in the first, a triage pass fills the second — and
                "any pull request means show me the diff" has to hold whichever one has it. */}
            <Show when={attempts().length}>
              <section class="panel attempts-panel">
                <h3>
                  {attempts().length} attempt
                  {attempts().length === 1 ? "" : "s"}
                  <span class="muted"> — pull requests on this work</span>
                </h3>
                <For each={attempts()}>
                  {(pr) => (
                    <Attempt
                      expand
                      pr={pr}
                      onExplain={() =>
                        run("explain-pr", () =>
                          api.tool("explain", { subject_key: prKey(pr) }),
                        )
                      }
                    />
                  )}
                </For>
              </section>
            </Show>

            {/* Assigned to you: what the code says, and what your options are.
                Patch options are proposals — nothing here has been applied. */}
            <For each={triage()}>
              {(t) => (
                // Queued or running, this is one line: there is nothing to read yet,
                // and a panel that says "queued" in a heading, a status pill and a
                // paragraph says it three times.
                <Show
                  when={t.status !== "pending" && t.status !== "running"}
                  fallback={
                    <div class="not-run">
                      <span class="muted" classList={{ thinking: t.status === "running" }}>
                        {t.status === "running"
                          ? "Reading the code for this issue…"
                          : "Queued for code triage — one issue is read at a time."}
                      </span>
                    </div>
                  }
                >
                <section class="panel issue-triage">
                  <div class="panel-head">
                    <h3>Assigned · {t.issue_key}</h3>
                    <div class="row">
                      <span class={`state state-${TRIAGE_STATE[t.status]}`}>
                        {t.status}
                      </span>
                      <button
                        disabled={busy() !== ""}
                        onClick={() =>
                          run("re-triage", async () => {
                            await api.tool("retriage_issue", {
                              issue_key: t.issue_key,
                            });
                            await refetchTriage();
                          })
                        }
                      >
                        Re-triage
                      </button>
                    </div>
                  </div>

                  <Show when={t.error}>
                    <p class="browser-error">{t.error}</p>
                  </Show>

                  {/* The plain-English gloss leads: it's the part you read at a
                      glance. The technical detail sits underneath it. */}
                  <Show when={t.plain_summary}>
                    <div class="triage-plain">{t.plain_summary}</div>
                  </Show>
                  <Show when={t.characterization}>
                    <div
                      class="triage-analysis md"
                      innerHTML={renderMarkdown(t.characterization!)}
                    />
                  </Show>

                  <Show when={t.patches.length}>
                    <h4 class="triage-heading">
                      {t.patches.length} possible approach
                      {t.patches.length === 1 ? "" : "es"}
                      <span class="muted"> — proposals, nothing applied</span>
                    </h4>
                    {/* Each approach folded to its own headline. Three unfolded proposals ran
                        to 2,638px — 48% of the page once the patches were folded — and choosing
                        between three options means comparing them, which you cannot do when the
                        first fills the screen. The head carries what the comparison turns on:
                        what it does, how much work, how confident, and the mechanism, which is
                        the check on whether the answer is real or generic. The rest — files,
                        sketch, risk — is what you read *after* picking one. */}
                    <For each={t.patches}>
                      {(p, i) => (
                        <details class="patch">
                          <summary class="patch-head">
                            <span class="patch-index">{i() + 1}</span>
                            <span class="patch-title">{p.title}</span>
                            <span class={`chip effort-${p.effort}`}>{p.effort}</span>
                            <span
                              class="rc-confidence"
                              data-tip="The model's confidence — a proposal, not a verdict"
                            >
                              {Math.round(p.confidence * 100)}%
                            </span>
                            <Show when={p.mechanism}>
                              <span class="patch-via" data-tip={p.mechanism!}>
                                via {p.mechanism}
                              </span>
                            </Show>
                          </summary>
                          <div class="patch-approach">{p.approach}</div>
                          <Show when={p.new_dependency}>
                            <div class="patch-dep">
                              adds a new dependency: <code>{p.new_dependency}</code>
                            </div>
                          </Show>
                          <Show when={p.files.length}>
                            <div class="rc-files">{p.files.join(" · ")}</div>
                          </Show>
                          <Show when={p.sketch}>
                            <pre class="rc-fragment">{p.sketch}</pre>
                          </Show>
                          <Show when={p.risk}>
                            <div class="patch-risk">
                              <span class="rc-label">risk</span> {p.risk}
                            </div>
                          </Show>
                        </details>
                      )}
                    </For>
                  </Show>

                  {/* Somebody may already be fixing this. Shown after the options
                      but before the provenance, because it can make the options
                      moot — and the critique is the part that matters, not the
                      PR's own claim to close the issue. */}
                  <Show when={prFixes()?.[t.issue_key]?.length}>
                    <h4 class="triage-heading">
                      Already being fixed?
                      <span class="muted">
                        {" "}
                        — open pull requests that may cover this; the diff and
                        review for each are under Attempts above
                      </span>
                    </h4>
                    <For each={prFixes()![t.issue_key]}>
                      {(pr) => (
                        <div class={`pr-fix pr-${pr.verdict}`}>
                          <div class="patch-head">
                            <span
                              class={`rc-relation pr-verdict-${pr.verdict}`}
                            >
                              {pr.verdict}
                            </span>
                            <a
                              class="rc-ref"
                              href={pr.pr_url ?? "#"}
                              target="_blank"
                              rel="noreferrer"
                            >
                              {pr.pr_repo}#{pr.pr_number} ↗
                            </a>
                            <Show when={pr.pr_author}>
                              <span class="muted">by {pr.pr_author}</span>
                            </Show>
                            <Show when={pr.pr_state === "draft"}>
                              <span class="chip">draft</span>
                            </Show>
                            <span
                              class="rc-confidence"
                              data-tip="Confidence in this judgment"
                            >
                              {Math.round(pr.confidence * 100)}%
                            </span>
                          </div>
                          <div class="patch-title">{pr.pr_title}</div>
                          <Show when={pr.implementation}>
                            <div class="patch-approach">
                              <span class="rc-label">implements</span>{" "}
                              {pr.implementation}
                            </div>
                          </Show>
                          <Show when={pr.critique}>
                            <div class="pr-critique">
                              <span class="rc-label">critique</span>{" "}
                              {pr.critique}
                            </div>
                          </Show>
                          {/* What reviewers said, on its own line rather than folded
                              into the critique: a human who read the change and pushed
                              back is better evidence than a model's reading of the
                              same diff. */}
                          <Show when={pr.conversation}>
                            <div class="attempt-conversation pr-critique">
                              <span class="rc-label">reviewers</span>{" "}
                              {pr.conversation}
                            </div>
                          </Show>
                          {/* Each entry carries its own justification, so it reads
                              as a claim you can check rather than a bare list. */}
                          <Show when={pr.also_fixes.length}>
                            <div class="pr-also">
                              <span class="rc-label">also resolves</span>
                              <For each={pr.also_fixes}>
                                {(entry) => (
                                  <div class="pr-also-entry">{entry}</div>
                                )}
                              </For>
                            </div>
                          </Show>
                          <Show when={pr.files.length}>
                            <div class="rc-files">
                              {pr.files.slice(0, 8).join(" · ")}
                            </div>
                          </Show>
                          <div class="muted pr-tier">
                            <Show
                              when={
                                pr.analyzed_by && pr.analyzed_by !== "local"
                              }
                            >
                              judged by the {pr.analyzed_by} tier ·{" "}
                            </Show>
                            {/* Worth stating where it's read: this critique is a note
                                in MuggleBot's store. It is never posted to the PR. */}
                            never posted to GitHub
                          </div>
                        </div>
                      )}
                    </For>
                  </Show>

                  {/* The citation for everything above: which commit, which files. */}
                  <Show when={t.head_sha || t.files.length}>
                    <div class="triage-provenance">
                      <Show when={t.head_sha}>
                        <span class="rc-label">read at</span>
                        <code>{t.head_sha!.slice(0, 8)}</code>
                      </Show>
                      <Show when={t.files.length}>
                        <span class="rc-label">from</span>
                        <span>
                          {t.files.length} file{t.files.length === 1 ? "" : "s"}
                        </span>
                        <details>
                          <summary class="muted">show</summary>
                          <div class="rc-files">{t.files.join("\n")}</div>
                        </details>
                      </Show>
                    </div>
                  </Show>
                </section>
                </Show>
              )}
            </For>

            {/* Root cause: hypotheses with citations, never conclusions.

                Un-run, this is one line rather than a panel. A heading, a paragraph
                explaining what the feature would do and a button is a full panel's
                worth of chrome around no content — and it sat between the diff and
                the relations, pushing what the subject actually says off screen. */}
            <Show
              when={rootCause()}
              fallback={
                <div class="not-run">
                  <span class="muted">
                    No root-cause search yet — look for the issue, PR, or commit
                    behind this across the indexed repositories.
                  </span>
                  <button
                    disabled={busy() !== ""}
                    onClick={() =>
                      run("investigate", async () => {
                        await api.tool("investigate_root_cause", {
                          subject_key: props.id,
                        });
                        await refetchRootCause();
                      })
                    }
                  >
                    Investigate
                  </button>
                </div>
              }
            >
              <section class="panel root-cause">
                <div class="panel-head">
                  <h3>Root cause</h3>
                  <button
                    disabled={busy() !== "" || rootCause()?.status === "running"}
                    onClick={() =>
                      run("investigate", async () => {
                        await api.tool("investigate_root_cause", {
                          subject_key: props.id,
                        });
                        await refetchRootCause();
                      })
                    }
                  >
                    Re-investigate
                  </button>
                </div>
                <Show when={rootCause()}>
                  {(report) => (
                    <>
                    <Show when={report().status === "running"}>
                      <p class="muted thinking">Investigating…</p>
                    </Show>
                    <Show when={report().symptoms.length}>
                      <div class="rc-meta">
                        <span class="rc-label">searched</span>
                        <For each={report().symptoms}>
                          {(term) => <span class="chip">{term}</span>}
                        </For>
                      </div>
                    </Show>
                    <Show when={report().repos.length}>
                      <div class="rc-meta">
                        <span class="rc-label">in</span>
                        <For each={report().repos}>
                          {(repo) => (
                            <a
                              class="chip repo"
                              href={`https://github.com/${repo}`}
                              target="_blank"
                              rel="noreferrer"
                            >
                              {repo}
                            </a>
                          )}
                        </For>
                      </div>
                    </Show>
                    <Show when={report().verdict}>
                      <div
                        class="rc-verdict md"
                        innerHTML={renderMarkdown(report().verdict!)}
                      />
                    </Show>
                    <Show when={report().error}>
                      <p class="browser-error">{report().error}</p>
                    </Show>
                    <For each={report().candidates}>
                      {(c) => (
                        <div class={`rc-candidate rc-${c.relation}`}>
                          <div class="rc-head">
                            <span
                              class={`rc-relation rc-relation-${c.relation}`}
                            >
                              {c.relation}
                            </span>
                            <span class="rc-kind">
                              {c.kind.replace("_", " ")}
                            </span>
                            <Show
                              when={c.url}
                              fallback={
                                <span class="rc-ref">{c.reference}</span>
                              }
                            >
                              <a
                                class="rc-ref"
                                href={c.url!}
                                target="_blank"
                                rel="noreferrer"
                              >
                                {c.reference} ↗
                              </a>
                            </Show>
                            <span
                              class="rc-confidence"
                              data-tip="How confident the model is — a hypothesis, not a verdict"
                            >
                              {Math.round(c.confidence * 100)}%
                            </span>
                          </div>
                          <div class="rc-title">{c.title}</div>
                          <Show when={c.rationale}>
                            <div class="muted">{c.rationale}</div>
                          </Show>
                          <div class="rc-facts">
                            <Show when={c.state}>
                              <span class="chip">{c.state}</span>
                            </Show>
                            <Show when={c.author}>
                              <span class="muted">{c.author}</span>
                            </Show>
                            <Show when={c.when}>
                              <span class="muted">{c.when!.slice(0, 10)}</span>
                            </Show>
                            <For each={c.labels}>
                              {(l) => <span class="chip tag">{l}</span>}
                            </For>
                          </div>
                          <Show when={c.files.length}>
                            <div class="rc-files">
                              {c.files.slice(0, 8).join(" · ")}
                            </div>
                          </Show>
                          <Show when={c.fragments?.length}>
                            <pre class="rc-fragment">
                              {c.fragments!.join("\n")}
                            </pre>
                          </Show>
                        </div>
                      )}
                    </For>
                    <Show
                      when={
                        report().status === "complete" &&
                        !report().candidates.length
                      }
                    >
                      <p class="muted">
                        Nothing in the searched repositories explains this — it
                        looks unreported.
                      </p>
                      </Show>
                    </>
                  )}
                </Show>
              </section>
            </Show>

            {/* Who this work is with. Placed after the analysis panels and before live
                assist: it is a question you ask once you know what the change *is*, and the
                answer is about people rather than about the code. */}
            <PersonaPredict subjectKey={props.id} />

            <Show when={threadHints().length}>
              <section class="panel">
                <h3>Live assist</h3>
                <For each={threadHints()}>
                  {(h) => (
                    <div
                      class={`hint hint-${h.kind}`}
                      classList={{ flag: h.kind === "flag" }}
                    >
                      <div class="hint-head">
                        <span class="hint-kind">
                          {h.kind === "flag"
                            ? (h.flag_type ?? "flag").replace("_", " ")
                            : h.kind}
                        </span>
                        <span class="muted">
                          {Math.round(h.confidence * 100)}%
                        </span>
                      </div>
                      <div>{h.text}</div>
                      <Show when={h.rationale}>
                        <div class="muted">{h.rationale}</div>
                      </Show>
                      <Show when={h.citations.length}>
                        <div class="cites">cites: {h.citations.join(", ")}</div>
                      </Show>
                      <div class="row">
                        <button
                          onClick={() =>
                            run("dismiss", async () => {
                              await api.tool("dismiss_hint", { id: h.id });
                              removeHint(h.id);
                            })
                          }
                        >
                          DISMISS
                        </button>
                        <button
                          onClick={() =>
                            run("dismiss", async () => {
                              await api.tool("dismiss_hint", {
                                id: h.id,
                                false_positive: true,
                              });
                              removeHint(h.id);
                            })
                          }
                        >
                          FALSE POSITIVE
                        </button>
                      </div>
                    </div>
                  )}
                </For>
              </section>
            </Show>

            <Show when={relations().shown.length || relations().hidden.length}>
              <section class="panel">
                <div class="panel-head">
                  <h3>Related</h3>
                  <Show when={relations().hidden.length}>
                    <button
                      class="linkish"
                      onClick={() => setAllRelations((v) => !v)}
                    >
                      {allRelations()
                        ? "show fewer"
                        : `${relations().hidden.length} more`}
                    </button>
                  </Show>
                </div>
                <For each={relations().shown}>
                  {(e) => {
                    // The edge can point at a thread that's off the active board
                    // (resolved/snoozed) or merged away — only offer navigation
                    // when the target actually exists in the loaded board.
                    const target = () => subjects[other(e)];
                    return (
                      <div class={`edge edge-${e.kind}`}>
                        <div class="edge-line">
                          <span class="edge-kind">{e.kind}</span>
                          <Show
                            when={target()}
                            fallback={
                              <span class="muted" data-tip={other(e)}>
                                {other(e)} · off board
                              </span>
                            }
                          >
                            <button
                              class="linkish"
                              onClick={() => props.onOpen(other(e))}
                            >
                              {target()!.title}
                            </button>
                          </Show>
                          {/* Only a human pin is worth labelling. Every LLM edge here
                              scores 80-95%, so printing the number ranked nothing and
                              lent a guess the authority of a measurement. */}
                          <Show when={e.provenance === "user"}>
                            <span class="chip">pinned</span>
                          </Show>
                        </div>
                        <div class="muted">{e.rationale}</div>
                      </div>
                    );
                  }}
                </For>
              </section>
            </Show>

            <Show when={t().context.length}>
              <section class="panel">
                <h3>Attached context</h3>
                <For each={t().context}>
                  {(c) => (
                    <div class="ctx-item">
                      <span class="chip">{c.kind}</span>{" "}
                      {c.summary ?? c.content}
                    </div>
                  )}
                </For>
              </section>
            </Show>

            <section class="panel">
              {/* Named for what it does. "Actions" sent a reader to the bottom of a
                  twelve-thousand-pixel page hunting for Ack and Snooze, which are in the
                  header where you land. */}
              <h3>Attach context</h3>
              <div class="form">
                <label>Attach context</label>
                <textarea
                  placeholder="free text…"
                  value={ctxText()}
                  onInput={(e) => setCtxText(e.currentTarget.value)}
                />
                <button
                  disabled={!ctxText().trim() || busy() !== ""}
                  onClick={() =>
                    run(
                      "attach",
                      async () => {
                        await api.tool("attach_thread_context", {
                          subject_key: props.id,
                          text: ctxText(),
                        });
                        setCtxText("");
                      },
                      true,
                    )
                  }
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
                  onClick={() =>
                    run(
                      "attach",
                      async () => {
                        await api.tool("attach_thread_context", {
                          subject_key: props.id,
                          url: ctxUrl(),
                        });
                        setCtxUrl("");
                      },
                      true,
                    )
                  }
                >
                  ATTACH URL
                </button>
              </div>
              <div class="form">
                <label>Relate to thread</label>
                <select
                  value={relateId()}
                  onChange={(e) => setRelateId(e.currentTarget.value)}
                >
                  <option value="">— choose a subject —</option>
                  <For each={otherThreads()}>
                    {(t) => <option value={t.key}>{t.title}</option>}
                  </For>
                </select>
                <div class="row">
                  <select
                    value={relateKind()}
                    onChange={(e) => setRelateKind(e.currentTarget.value)}
                  >
                    <option value="related">related</option>
                    <option value="same">same (merge)</option>
                    <option value="distinct">distinct</option>
                  </select>
                  <button
                    disabled={!relateId().trim() || busy() !== ""}
                    onClick={() =>
                      run(
                        "relate",
                        async () => {
                          const merged = relateKind() === "same";
                          await api.tool("relate", {
                            thread_a: props.id,
                            thread_b: relateId(),
                            kind: relateKind(),
                          });
                          setRelateId("");
                          if (merged) props.onBack();
                        },
                        true,
                      )
                    }
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
