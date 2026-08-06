import {
  createEffect,
  createResource,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import { api } from "../api";
import type {
  DiffFile,
  PrDiffReport,
  Recommendation,
  ReviewComment,
} from "../types";

/// Providers a re-review can be sent to. Same list and order as the subject panel's
/// RECONSIDER control — one vocabulary for "pick a model", wherever it is offered.
const PROVIDERS = [
  { id: "ollama_local", label: "Ollama (Local)" },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "ollama", label: "Ollama Cloud" },
] as const;

/// A pull request's diff, summarized first and readable underneath.
///
/// Summary above the files on purpose. The question a reviewer opens a diff with is "what does
/// this do and where is the risk", and a file list answers neither — it answers "how big is it",
/// which the counts already say in one line.
///
/// **Opens itself from object state.** The diff lives on the pull request's own virtual object,
/// so showing it costs one state read — cheap enough to do on render, for every attempt on an
/// issue. What is *not* cheap is reading a diff GitHub hasn't been asked for yet: that is an API
/// call plus a local model pass, so a PR with nothing stored still shows a button. The
/// distinction is the whole reason `stored_only` exists.
export default function DiffPane(props: {
  subjectKey: string;
  /// Show the patches without being asked.
  ///
  /// The click-in view sets this for an outstanding or merged PR: that diff is the meat of
  /// the fix, and a disclosure triangle over it hides the answer the view exists to give.
  /// The board leaves it off — a card is a list row, and sixty unfurled patches in one is a
  /// wall.
  expand?: boolean;
}) {
  const [report, setReport] = createSignal<PrDiffReport | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  /// Per-file overrides of the default expansion. Absent means "whatever `expand` says", so a
  /// click always wins over the default in either direction — the reader can fold a noisy file
  /// away in the click-in view, and open one on a board card.
  const [open, setOpen] = createSignal<Record<string, boolean | undefined>>({});
  const isOpen = (key: string) => open()[key] ?? !!props.expand;
  /// Which diff blocks are collapsed, by `repo#number`.
  ///
  /// Per block rather than one flag for the pane, because an issue with several attempts renders
  /// several blocks and the point of collapsing is to fold away the ones you have already read.
  ///
  /// Collapsing keeps the fetched report: re-expanding is instant and costs neither an API call
  /// nor another model pass, which is the whole reason this is a toggle and not a reload.
  const [shut, setShut] = createSignal<Record<string, boolean>>({});
  /// Whether the raw patches are shown, by `repo#number`. **Folded by default, always.**
  ///
  /// `expand` used to unfold these too, on the reasoning that the patches are the substance of
  /// the click-in view and a disclosure triangle over them hides the answer. That was right when
  /// there was no review — the patch *was* the only answer available. There is a review now, so
  /// the answer is the verdict and its findings, and the patch is the evidence behind it.
  /// Unfolding evidence by default is what made one pull request's detail view 12,126px tall,
  /// **6,319px of it unified diff** — 52% of the page — with the verdict, the proposed approaches
  /// and the Actions row all buried beneath it.
  ///
  /// So: unfold the judgement, fold the evidence. `expand` still governs whether individual
  /// files start open *once the patches are shown*.
  const [patches, setPatches] = createSignal<Record<string, boolean>>({});
  const patchesOn = (id: string) => patches()[id] ?? false;
  const togglePatches = (id: string) =>
    setPatches((prev) => ({ ...prev, [id]: !patchesOn(id) }));

  /// Provider/model for a re-review, and the dispatch result.
  ///
  /// Defaults to the on-device provider deliberately: the first press of a button whose
  /// label doesn't name a model should not be the thing that sends a diff off the machine.
  const [provider, setProvider] = createSignal<string>(PROVIDERS[0].id);
  const [model, setModel] = createSignal<string>("");
  const [models, { refetch: refetchModels }] = createResource(provider, (p) =>
    api.models(p),
  );
  createEffect(() => {
    const list = models();
    if (list && list.length && !list.includes(model())) setModel(list[0]);
  });
  /// What the last re-review dispatch said, per `repo#number`. `dispatched: false` is not an
  /// error — it means this model has already reviewed this diff and the verdict on screen is
  /// its answer — so it needs its own message rather than the error channel.
  const [dispatch, setDispatch] = createSignal<Record<string, string>>({});
  /// PRs whose re-review is in flight, and the model asked for. Cleared when a review
  /// produced by that model shows up — comparing the model rather than the timestamp,
  /// because `produced_by` is the one field that distinguishes the answer we are waiting
  /// for from the one already on screen.
  const [awaiting, setAwaiting] = createSignal<Record<string, string>>({});

  const reReview = async (repo: string, number: number) => {
    const id = `${repo}#${number}`;
    setBusy(true);
    setError("");
    try {
      // Picking the model that produced the review already on screen means "do it again",
      // not "do nothing" — a button labelled Re-review has to review. Without this the key
      // is already spent and the press is silently free, which reads as broken. Any *other*
      // model keeps the free-by-default behaviour: that press is a genuine new question.
      const again =
        report()?.diffs.find((d) => `${d.repo}#${d.number}` === id)?.review
          ?.produced_by === model();
      const r = await api.tool<{ dispatched: boolean; model: string }>(
        "pr_review",
        {
          subject_key: `${repo}!${number}`,
          provider: provider(),
          model: model(),
          force: again,
        },
      );
      setDispatch((prev) => ({
        ...prev,
        [id]: r.dispatched
          ? `reviewing on ${r.model} — the verdict replaces the one above when it lands`
          : `${r.model} has already reviewed this diff; the verdict above is its answer`,
      }));
      // Wait for the new verdict. The existing poll only runs when a diff has *no* review,
      // and a re-review replaces one — so it needs its own reason to keep looking.
      if (r.dispatched) {
        setAwaiting((prev) => ({ ...prev, [id]: r.model }));
      }
    } catch (e) {
      setError(`${e}`);
    } finally {
      setBusy(false);
    }
  };

  const load = async (opts: { storedOnly?: boolean; refresh?: boolean }) => {
    setBusy(true);
    setError("");
    try {
      setReport(await api.prDiff(props.subjectKey, opts));
    } catch (e) {
      setError(`${e}`.replace(/^Error:\s*/, ""));
    } finally {
      setBusy(false);
    }
  };

  // The stored read on render. Silent about failure: a board card must not sprout an error
  // because Restate was restarting, and the button below is the way through either way.
  onMount(() => {
    void load({ storedOnly: true });
  });

  /// Whether some attempt's diff has never been read. Drives the button, so it offers the
  /// expensive read exactly when there is something the cheap one couldn't answer.
  const unread = () => {
    const r = report();
    if (!r) return true;
    return r.diffs.length < r.target_count;
  };

  /// Whether some shown diff has no review beside it — the model failed to produce one, or the
  /// diff predates reviews existing. Offered as its own button because the fix is a model pass
  /// over a diff already in hand, not another read of GitHub.
  const needsReview = () =>
    (report()?.diffs ?? []).some((d) => !d.review && !d.error);

  /// Poll for a review that is still being written.
  ///
  /// The review is several model passes and runs in the background, so the diff arrives first
  /// and the verdict lands a minute or two later. Polling object state is a ~40ms read, which
  /// is cheap enough to do while the answer is outstanding and stops the moment it arrives —
  /// the alternative is a pane that stays wrong until the operator thinks to reload.
  ///
  /// Only while something is actually pending, and only after a diff has been shown: an idle
  /// pane makes no requests at all.
  /// A re-review that has landed: its `produced_by` is the model we asked for.
  createEffect(() => {
    const r = report();
    if (!r) return;
    const pending = awaiting();
    if (!Object.keys(pending).length) return;
    const done = { ...pending };
    let changed = false;
    for (const d of r.diffs) {
      const id = `${d.repo}#${d.number}`;
      if (done[id] && d.review?.produced_by === done[id]) {
        delete done[id];
        changed = true;
      }
    }
    if (changed) setAwaiting(done);
  });

  createEffect(() => {
    const pending = Object.keys(awaiting()).length > 0;
    if (!report() || busy() || (!needsReview() && !pending)) return;
    const timer = setInterval(() => {
      void load({ storedOnly: true });
    }, 8000);
    onCleanup(() => clearInterval(timer));
  });

  const toggle = (key: string) =>
    setOpen((prev) => ({ ...prev, [key]: !(prev[key] ?? !!props.expand) }));

  const toggleBlock = (key: string) =>
    setShut((prev) => ({ ...prev, [key]: !prev[key] }));

  return (
    <div class="diff-pane" onClick={(e) => e.stopPropagation()}>
      <Show when={report()?.diffs.length}>
        <For each={report()!.diffs}>
          {(d) => {
            const id = `${d.repo}#${d.number}`;
            const isShut = () => !!shut()[id];
            return (
              <div class="diff-block">
                {/* The header is the toggle. It stays visible when collapsed, so a folded diff
                    still reports its size — a pane that vanishes entirely leaves no sign there
                    is a diff to read. */}
                <div
                  class="diff-head diff-toggle"
                  onClick={() => toggleBlock(id)}
                >
                  <span class="muted">{isShut() ? "▸" : "▾"}</span>
                  <span class="explain-label">
                    DIFF · {d.repo}#{d.number}
                  </span>
                  {/* The recommendation sits in the header so it survives folding: "should this
                      land" is the one thing worth reading without opening anything. */}
                  <Show when={d.review}>
                    <span
                      class={`rec rec-${d.review!.recommendation}`}
                      data-tip={`reviewed by ${d.review!.produced_by} · never posted to GitHub`}
                    >
                      {REC_LABEL[d.review!.recommendation]}
                    </span>
                    <Show when={d.review!.comments.length}>
                      <span class="muted">
                        {d.review!.comments.length} note
                        {d.review!.comments.length === 1 ? "" : "s"}
                      </span>
                    </Show>
                  </Show>
                  <Show when={!d.error}>
                    <span class="diff-stat">
                      {d.file_count} file{d.file_count === 1 ? "" : "s"}
                    </span>
                    <span class="diff-add">+{d.additions}</span>
                    <span class="diff-del">−{d.deletions}</span>
                    {/* A diff that silently stops is how a reader concludes a change is
                        smaller than it is. */}
                    <Show when={d.truncated}>
                      <span
                        class="chip chip-stale"
                        data-tip="more files than were fetched"
                      >
                        TRUNCATED
                      </span>
                    </Show>
                    {/* When it was read, because a stored diff is only as fresh as the last
                        activity on the PR — and a force-push notifies nobody. */}
                    <Show when={d.fetched_at}>
                      <span class="muted" data-tip={d.fetched_at}>
                        read {age(d.fetched_at!)}
                      </span>
                    </Show>
                  </Show>
                </div>

                <Show when={d.error && !isShut()}>
                  <div class="diff-error">{d.error}</div>
                </Show>

                {/* The summary leads, because "what does this do" is the question. */}
                <Show when={d.summary && !isShut()}>
                  <div class="diff-summary">{d.summary}</div>
                </Show>

                {/* Then the review — the part that takes a position. Kept distinct from the
                    summary above it: one explains the change, the other says what to do about
                    it, and merging them makes the advice read as description. */}
                <Show when={d.review?.rationale && !isShut()}>
                  <div
                    class={`review-rationale rec-border-${d.review!.recommendation}`}
                  >
                    <span class="rc-label">Review</span>
                    <span>{d.review!.rationale}</span>
                    <span class="muted review-by">
                      — {d.review!.produced_by}
                    </span>
                  </div>
                </Show>

                {/* Re-review on a model you name. Placed under the verdict rather than in the
                    header because that is where the reason to press it is felt: you have just
                    read a recommendation and want a second reader on it. Collapsed by default —
                    an always-open pair of selects reads as configuration for the whole pane. */}
                <Show when={!isShut() && !d.error}>
                  <details class="menu review-again">
                    <summary>Re-review on another model ▾</summary>
                    <div class="menu-body">
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
                        class="explain-btn"
                        disabled={busy() || !model()}
                        data-tip="Review this diff again on the model selected — runs in the background, no GitHub call"
                        onClick={() => void reReview(d.repo, d.number)}
                      >
                        {awaiting()[id] ? "Reviewing…" : "Re-review"}
                      </button>
                      {/* "Already reviewed on this model" is an answer, not a failure, so it
                          says so here rather than in the error line. */}
                      <Show when={dispatch()[id]}>
                        <span class="muted">{dispatch()[id]}</span>
                      </Show>
                    </div>
                  </details>
                </Show>

                {/* Every finding, listed. Until now a review's notes were rendered only against
                    the line they were anchored to — inside the patches — so folding the patches
                    hid the findings, and *not* folding them buried the verdict under 6,000px of
                    diff. The findings are the review's actual content, so they belong beside the
                    verdict where the reader already is. */}
                <Show when={!isShut() && d.review?.comments.length}>
                  <ul class="review-findings">
                    <For each={d.review!.comments}>
                      {(c: ReviewComment) => (
                        <li class={`finding sev-${c.severity}`}>
                          <span class="finding-sev">{c.severity}</span>
                          <Show when={c.path}>
                            <code class="finding-path">{c.path}</code>
                          </Show>
                          <span class="finding-note">{c.note}</span>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>

                {/* The evidence, behind one control that says how much of it there is. */}
                <Show when={!isShut() && !d.error && d.files.length}>
                  <button
                    class="patch-toggle"
                    data-tip="The patches behind the review — folded by default so the verdict is not buried under them"
                    onClick={() => togglePatches(id)}
                  >
                    {patchesOn(id) ? "▾ hide" : "▸ show"} the {d.file_count} patch
                    {d.file_count === 1 ? "" : "es"}
                    <span class="diff-add">+{d.additions}</span>
                    <span class="diff-del">−{d.deletions}</span>
                  </button>
                </Show>

                <For each={isShut() || !patchesOn(id) ? [] : d.files}>
                  {(f) => {
                    const key = `${d.repo}#${d.number}:${f.path}`;
                    const notes = () =>
                      (d.review?.comments ?? []).filter(
                        (c) => c.path === f.path,
                      );
                    /// Notes the backend could not pin to a line. Rendered under the file
                    /// header rather than dropped: the note is still about this file, and
                    /// guessing a line would be worse than admitting we don't have one.
                    const loose = () =>
                      notes().filter((c) => c.patch_index === undefined);
                    return (
                      <div class="diff-file">
                        <div class="diff-file-head" onClick={() => toggle(key)}>
                          <span class="diff-path">{f.path}</span>
                          <span class="diff-add">+{f.additions}</span>
                          <span class="diff-del">−{f.deletions}</span>
                          <Show
                            when={f.patch}
                            fallback={
                              /* Two different facts, told apart: GitHub had no hunk, or we
                                 chose not to keep one. Only the first says something about
                                 the change. */
                              <Show
                                when={f.patch_omitted}
                                fallback={
                                  <span
                                    class="muted"
                                    data-tip="binary, or no textual hunk"
                                  >
                                    —
                                  </span>
                                }
                              >
                                <span
                                  class="muted"
                                  data-tip="the patch was not kept, to bound what one PR costs in state — open it on GitHub"
                                >
                                  not stored
                                </span>
                              </Show>
                            }
                          >
                            <span class="muted">{isOpen(key) ? "▾" : "▸"}</span>
                          </Show>
                        </div>
                        <For each={loose()}>{(c) => <Note comment={c} />}</For>
                        <Show when={isOpen(key) && f.patch}>
                          <Patch patch={f.patch!} comments={notes()} />
                        </Show>
                      </div>
                    );
                  }}
                </For>
              </div>
            );
          }}
        </For>
      </Show>

      {/* The read, offered when state couldn't answer — and a re-read when it could but the
          PR may have moved since. */}
      <div class="diff-head">
        <Show
          when={unread()}
          fallback={
            <Show
              when={needsReview()}
              fallback={
                <button
                  class="explain-btn"
                  disabled={busy()}
                  data-tip="read the diff again from GitHub"
                  onClick={() => load({ refresh: true })}
                >
                  {busy() ? "Reading diff…" : "Re-read"}
                </button>
              }
            >
              <button
                class="explain-btn"
                disabled={busy()}
                data-tip="review the stored diff — runs in the background, no GitHub call"
                onClick={() => load({})}
              >
                {busy() ? "Reviewing…" : "REVIEW"}
              </button>
              {/* Said while the poll is waiting, so the pane does not look finished-and-empty
                  for the minute or two a review takes. */}
              <span class="muted">review in progress…</span>
            </Show>
          }
        >
          <button
            class="explain-btn"
            disabled={busy()}
            onClick={() => load({})}
          >
            {busy() ? "Reading diff…" : "DIFF"}
          </button>
        </Show>
        <Show when={error()}>
          <span class="diff-error">{error()}</span>
        </Show>
        <Show when={report() && report()!.target_count === 0}>
          <span class="muted">no pull request is attached to this yet</span>
        </Show>
      </div>
    </div>
  );
}

/// A coarse age, for a timestamp whose exact value is in the tooltip.
function age(iso: string): string {
  const secs = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (secs < 90) return "just now";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.round(mins / 60);
  if (hours < 48) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

/// One file's hunk, coloured by line kind.
///
/// Rendered line by line rather than as one block so additions and deletions are distinguishable
/// — an uncoloured unified diff is the one presentation that makes a patch harder to read than the
/// file it came from.
function Patch(props: { patch: string; comments?: ReviewComment[] }) {
  const lines = () => props.patch.split("\n");
  /// Notes by the patch line they hang under, so a line with two remarks shows both.
  const at = (i: number) =>
    (props.comments ?? []).filter((c) => c.patch_index === i);
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
            {/* Inline, directly under the line it is about — the position that makes a review
                comment worth more than the same sentence in a summary. */}
            <For each={at(i())}>{(c) => <Note comment={c} inline />}</For>
          </>
        )}
      </For>
    </pre>
  );
}

/// One review note.
///
/// Severity is a colour and a word, not an icon: "blocker" and "nit" have to be
/// distinguishable at a glance without a legend, and a reader who ignores nits should be able
/// to ignore them by colour.
function Note(props: { comment: ReviewComment; inline?: boolean }) {
  const c = () => props.comment;
  return (
    <div
      class={`review-note note-${c().severity}`}
      classList={{ "note-inline": props.inline }}
    >
      <span class={`note-sev sev-${c().severity}`}>
        {c().severity.toUpperCase()}
      </span>
      <span class="note-text">{c().note}</span>
    </div>
  );
}

/// The header label per recommendation. `REQUEST CHANGES` rather than an emoji or a colour
/// alone, because the folded card is where this gets read and a bare colour is not a claim.
const REC_LABEL: Record<Recommendation, string> = {
  approve: "APPROVE",
  comment: "COMMENT",
  request_changes: "REQUEST CHANGES",
};

export type { DiffFile };
