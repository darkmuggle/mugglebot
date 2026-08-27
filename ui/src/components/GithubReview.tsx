import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { api } from "../api";
import type { BranchReview, BranchPass, PlacedComment, PersonaSummary } from "../types";
import { Empty, Panel, SectionHead, Toolbar } from "./ui";

const POLL_MS = 3000;

const PROVIDERS = [
  { id: "claude", label: "Claude" },
  { id: "chatgpt", label: "ChatGPT" },
  { id: "ollama_local", label: "Local" },
] as const;

/// How a reviewer is named on screen.
///
/// A persona pass is labelled with the person's slug and, everywhere it appears, marked as a
/// *rehearsal*. That is not decoration: the sentence "this swallows the error" reads identically
/// whether a model wrote it or a colleague is predicted to, and only the label says which.
function reviewerLabel(by: PlacedComment["by"]): string {
  return typeof by === "string" ? "model" : `@${by.persona}`;
}
function isPersona(by: PlacedComment["by"]): boolean {
  return typeof by !== "string";
}

/// One review note, inline under the line it is about.
function Note(props: { comment: PlacedComment }) {
  const c = () => props.comment;
  return (
    <div
      class={`review-note note-${c().severity}`}
      classList={{ "note-inline": true, "note-persona": isPersona(c().by) }}
    >
      {/* Who, first. On a diff interleaving three reviewers, "who said this" is what decides
          how much weight the sentence carries — and it must be readable before the sentence,
          not after it. */}
      <span
        class="note-by"
        classList={{ persona: isPersona(c().by) }}
        data-tip={
          isPersona(c().by)
            ? "A rehearsal — what this person would probably say. Not their words, and never posted."
            : "The model's own read of the code"
        }
      >
        {reviewerLabel(c().by)}
        {isPersona(c().by) ? " would say" : ""}
      </span>
      <span class={`note-sev sev-${c().severity}`}>{c().severity.toUpperCase()}</span>
      <span class="note-text">{c().note}</span>
      {/* Which traits the prediction rested on. Their presence is what separates "he'd object
          to this" from "he objects to this kind of thing, and here it is". */}
      <Show when={c().because.length}>
        <span class="note-because" data-tip="Persona traits this prediction rests on">
          {c().because.map((t) => t.split("/")[1] ?? t).join(", ")}
        </span>
      </Show>
    </div>
  );
}

/// One file's patch, with every reviewer's comments inline.
function Patch(props: { patch: string; comments: PlacedComment[] }) {
  const lines = () => props.patch.split("\n");
  const at = (i: number) => props.comments.filter((c) => c.patch_index === i);
  return (
    <pre class="diff-patch">
      <For each={lines()}>
        {(line, i) => (
          <>
            <div
              class="dl"
              classList={{
                "dl-add": line.startsWith("+") && !line.startsWith("+++"),
                "dl-del": line.startsWith("-") && !line.startsWith("---"),
                "dl-hunk": line.startsWith("@@"),
              }}
            >
              {line || " "}
            </div>
            {/* Directly under the line, which is the whole point: the same sentence in a
                summary is worth a fraction of what it is worth here. */}
            <For each={at(i())}>{(c) => <Note comment={c} />}</For>
          </>
        )}
      </For>
    </pre>
  );
}

/// One reviewer's pass: what they concluded, and the controls for it.
function PassCard(props: { pass: BranchPass; headSha: string; onChanged: () => void }) {
  const p = () => props.pass;
  const [busy, setBusy] = createSignal(false);
  const act = async (tool: string, args: Record<string, unknown>) => {
    setBusy(true);
    try {
      await api.tool(tool, { id: p().id, ...args });
      props.onChanged();
    } finally {
      setBusy(false);
    }
  };
  const stale = () => !!props.headSha && p().head_sha !== props.headSha;
  return (
    <div
      class="pass-card"
      classList={{ dismissed: p().dismissed, [`status-${p().status}`]: true }}
    >
      <div class="pass-head">
        <span
          class="pass-by"
          classList={{ persona: isPersona(p().by) }}
          data-tip={
            isPersona(p().by)
              ? "A rehearsal — what this person would probably say. Never posted anywhere."
              : "The model's own read of the code"
          }
        >
          {reviewerLabel(p().by)}
        </span>
        <span class="pass-model">
          {p().provider}/{p().model}
        </span>
        <Show when={p().recommendation}>
          <span class={`pass-rec rec-${p().recommendation!.replace(/\s+/g, "_")}`}>
            {p().recommendation}
          </span>
        </Show>
        <span class="pass-count">{p().comments.length} inline</span>
        {/* A pass is about a commit. When the branch moves the review does not follow it —
            saying so is the difference between stale and wrong. */}
        <Show when={stale()}>
          <span class="pass-stale" data-tip={`Reviewed ${p().head_sha.slice(0, 8)}; the branch has moved since`}>
            stale
          </span>
        </Show>
        <span class="pass-spacer" />
        <button disabled={busy()} onClick={() => void act("rerun_branch_review", {})}>
          ask again
        </button>
        <button
          disabled={busy()}
          onClick={() => void act("dismiss_branch_review", { dismissed: !p().dismissed })}
        >
          {p().dismissed ? "restore" : "dismiss"}
        </button>
      </div>
      <Show when={p().status === "pending" || p().status === "running"}>
        <p class="muted">
          {isPersona(p().by)
            ? "Reading the diff as this person would…"
            : "Reading the diff…"}
        </p>
      </Show>
      <Show when={p().error}>
        <p class="pass-error">{p().error}</p>
      </Show>
      <Show when={p().summary}>
        <p class="pass-summary">
          <Show when={isPersona(p().by)}>
            <span class="rehearsal">rehearsal · never posted</span>
          </Show>
          {p().summary}
        </p>
      </Show>
    </div>
  );
}

/// Which branch review is open is a **location**, not component state.
///
/// It used to be a local signal, so leaving the screen unmounted the component and lost it:
/// you came back to an empty pane with the review still sitting on the server, which reads as
/// the app having discarded a review you waited on. Every other "what am I looking at" in this
/// app is in the URL — an open subject, a resolved cloud entity — for exactly this reason, and
/// a branch review is the one that costs the most to reproduce.
export default function GithubReview(props: {
  openKey: string | null;
  onOpenKey: (key: string | null) => void;
}) {
  const [branch, setBranch] = createSignal("");
  const [provider, setProvider] = createSignal<string>("claude");
  const [model, setModel] = createSignal("");
  const [chosen, setChosen] = createSignal<string[]>([]);
  const [generic, setGeneric] = createSignal(true);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  const openKey = () => props.openKey;
  const setOpenKey = (key: string | null) => props.onOpenKey(key);

  // Through `api.personas()`, the helper the Personas view already uses. `list_personas`
  // answers `{ enabled, personas: [...] }` rather than a bare array, and reading it as one is
  // what produced "No personas yet" with three personas in the store — going through the shared
  // helper means that envelope is decoded in one place and cannot drift again.
  const [personas] = createResource(async () =>
    api
      .listPersonas()
      .then((r) => r.personas ?? [])
      .catch(() => [] as PersonaSummary[]),
  );
  const [models] = createResource(provider, async (p: string) =>
    api.models(p).catch(() => [] as string[]),
  );
  const [branches, { refetch: refetchBranches }] = createResource(async () =>
    api.tool<{ branch_key: string; repo: string; head: string }[]>(
      "list_branch_reviews",
      { limit: 25 },
    ),
  );
  const [review, { refetch: refetchReview }] = createResource(openKey, async (key: string) =>
    api.tool<BranchReview>("get_branch_review", { branch_key: key }),
  );

  // A panel fills in behind you. Poll only while something is in flight — a screen that polls
  // at rest is a screen that wakes the machine for nothing.
  const inFlight = () =>
    (review()?.passes ?? []).some((p) => p.status === "pending" || p.status === "running");
  const timer = setInterval(() => {
    if (inFlight()) void refetchReview();
  }, POLL_MS);
  onCleanup(() => clearInterval(timer));

  const toggle = (slug: string) =>
    setChosen((c) => (c.includes(slug) ? c.filter((s) => s !== slug) : [...c, slug]));

  const dispatch = async () => {
    if (!branch().trim() || busy()) return;
    setBusy(true);
    setError("");
    try {
      const out = await api.tool<{ branch_key: string; open_pull_request: number | null }>(
        "review_branch",
        {
          branch: branch(),
          personas: chosen(),
          provider: provider(),
          ...(model() ? { model: model() } : {}),
          generic: generic(),
        },
      );
      setOpenKey(out.branch_key);
      await Promise.all([refetchBranches(), refetchReview()]);
    } catch (e) {
      // The branch parser and the resolve step both write their messages for this box —
      // "no branch `x` in owner/repo", "that compare URL has no owner/repo in it".
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  /// Comments for one file, from every pass still showing.
  const commentsFor = (path: string): PlacedComment[] =>
    (review()?.visible ?? []).filter((c) => c.path === path);
  /// Comments that could not be placed on a line — shown above the file rather than dropped,
  /// which is what makes an unresolvable anchor visible instead of silently wrong.
  const unplacedFor = (path: string): PlacedComment[] =>
    commentsFor(path).filter((c) => c.patch_index === null || c.patch_index === undefined);

  return (
    <>
      <Panel title="Review a branch" meta="rehearsal, not a posted review">
        <p class="muted panel-lede">
          Point at any branch. The model reads the code; each persona you pick rehearses the
          review that person would leave. Every comment lands on the line its author quoted.
        </p>

        <Toolbar>
          <input
            type="text"
            placeholder="owner/repo@branch, or a GitHub branch or compare URL…"
            value={branch()}
            onInput={(e) => setBranch(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void dispatch();
            }}
          />
          <select
            value={provider()}
            onChange={(e) => {
              setProvider(e.currentTarget.value);
              setModel("");
            }}
          >
            <For each={PROVIDERS}>{(p) => <option value={p.id}>{p.label}</option>}</For>
          </select>
          <select value={model()} onChange={(e) => setModel(e.currentTarget.value)}>
            <option value="">default model</option>
            <For each={models() ?? []}>{(m) => <option value={m}>{m}</option>}</For>
          </select>
          <button
            class="primary"
            disabled={!branch().trim() || busy()}
            onClick={() => void dispatch()}
          >
            {busy() ? "Reading…" : "Review"}
          </button>
        </Toolbar>

        <SectionHead title="Review as" level={3} />
        <div class="review-panel">
          <label class="review-generic">
            <input
              type="checkbox"
              checked={generic()}
              onChange={(e) => setGeneric(e.currentTarget.checked)}
            />
            model's own review
          </label>
          <Show
            when={(personas() ?? []).length}
            fallback={
              <span class="muted">No personas yet — model someone to review as them.</span>
            }
          >
            <For each={personas() ?? []}>
              {(p) => (
                <button
                  class="persona-pick"
                  classList={{ on: chosen().includes(p.slug) }}
                  data-tip={p.role ?? undefined}
                  onClick={() => toggle(p.slug)}
                >
                  {p.display_name}
                </button>
              )}
            </For>
          </Show>
        </div>
        <Show when={error()}>
          <Empty kind="error">{error()}</Empty>
        </Show>
      </Panel>

      <Show when={(branches() ?? []).length}>
        <div class="review-history">
          <For each={branches() ?? []}>
            {(b) => (
              <button
                class="branch-chip"
                classList={{ on: openKey() === b.branch_key }}
                onClick={() => setOpenKey(b.branch_key)}
              >
                {b.repo} <span class="muted">@</span> {b.head}
              </button>
            )}
          </For>
        </div>
      </Show>

      <Show when={review()}>
        {/* The reviewers, then the diff they reviewed. Verdicts first: what each concluded is
            the answer, and the diff is what backs it — the same rule the subject view follows. */}
        <Panel
          title={`${review()!.repo} @ ${review()!.head}`}
          meta={
            <>
              vs {review()!.base} · {review()!.head_sha.slice(0, 8)}
              <For each={review()!.tally}>
                {(t) => (
                  <>
                    {" · "}
                    {t.reviewer}: {t.comments}
                  </>
                )}
              </For>
            </>
          }
        >
          <For each={review()!.passes}>
            {(p) => (
              <PassCard
                pass={p}
                headSha={review()!.head_sha}
                onChanged={() => void refetchReview()}
              />
            )}
          </For>
        </Panel>

        <Show when={review()!.diff?.files?.length}>
          <Panel
            title="The change"
            meta={`${review()!.diff!.file_count} file(s), +${review()!.diff!.additions} −${review()!.diff!.deletions}`}
          >
            <For each={review()!.diff!.files}>
              {(f) => (
                <details class="review-file" open>
                  <summary>
                    <code>{f.path}</code>
                    <span class="muted">
                      {" "}
                      +{f.additions} −{f.deletions}
                    </span>
                    <Show when={commentsFor(f.path).length}>
                      <span class="file-comment-count">
                        {commentsFor(f.path).length} comment(s)
                      </span>
                    </Show>
                  </summary>
                  {/* Comments that matched no line, above the hunks. An unanchored note here is
                      about the file; attaching it to whatever line happened to be nearby would
                      make it a note about the wrong code. */}
                  <For each={unplacedFor(f.path)}>{(c) => <Note comment={c} />}</For>
                  <Show
                    when={f.patch}
                    fallback={<p class="muted">No patch — the file was too large to include.</p>}
                  >
                    <Patch patch={f.patch!} comments={commentsFor(f.path)} />
                  </Show>
                </details>
              )}
            </For>
          </Panel>
        </Show>
      </Show>
    </>
  );
}
