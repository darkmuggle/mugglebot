import { createSignal, For, onMount, Show } from "solid-js";
import { api } from "../api";
import type { DiffFile, PrDiffReport } from "../types";

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

                <For each={isShut() ? [] : d.files}>
                  {(f) => {
                    const key = `${d.repo}#${d.number}:${f.path}`;
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
                        <Show when={isOpen(key) && f.patch}>
                          <Patch patch={f.patch!} />
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
            <button
              class="explain-btn"
              disabled={busy()}
              data-tip="read the diff again from GitHub"
              onClick={() => load({ refresh: true })}
            >
              {busy() ? "READING DIFF…" : "RE-READ"}
            </button>
          }
        >
          <button
            class="explain-btn"
            disabled={busy()}
            onClick={() => load({})}
          >
            {busy() ? "READING DIFF…" : "DIFF"}
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
function Patch(props: { patch: string }) {
  const lines = () => props.patch.split("\n");
  return (
    <pre class="diff-patch">
      <For each={lines()}>
        {(line) => (
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
        )}
      </For>
    </pre>
  );
}

export type { DiffFile };
