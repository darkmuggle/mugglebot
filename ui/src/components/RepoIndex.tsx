import {
  createMemo,
  createResource,
  createSignal,
  For,
  onCleanup,
  Show,
} from "solid-js";
import { api } from "../api";
import {
  carded,
  day,
  KIND_LABEL,
  KIND_ORDER,
  kindOf,
  pct,
  phase,
  PHASE_ORDER,
  present,
} from "../repoindex";
import { indexProgress } from "../state";
import AgentSession from "./AgentSession";
import RepoDetailView from "./RepoDetail";
import type {
  IndexProgressEvent,
  IndexStatus,
  RepoKind,
  RepoIndexProgress,
} from "../types";

/// How often the panel re-reads the parts that have no push behind them.
///
/// Per-repo progress arrives over the WebSocket the moment a batch lands, so this poll is only
/// for the two things Restate has no event for: the **in-flight strip** (`sys_invocation` state)
/// and the arrival of repos the client has never seen. Slower than the old 5s because it is no
/// longer what makes the numbers move.
const POLL_MS = 20_000;

/// Columns the list can be ordered by.
type SortKey =
  "repo" | "language" | "status" | "components" | "commits" | "last_commit";

export default function RepoIndexView(props: { onChat?: () => void }) {
  const [status, { refetch }] = createResource<IndexStatus>(() =>
    api.indexStatus(),
  );
  /// The repo whose own screen is showing. The list and the repo view are alternatives rather
  /// than an expander: everything the index holds about a repo needs the whole width, and
  /// inline it buried the rest of the list under one repo's commit history.
  const [openRepo, setOpenRepo] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal("");
  const [error, setError] = createSignal("");
  // Show every repo, or only the ones the index has actually touched. Default is touched:
  // on a 147-repo org the untouched rows are the majority and say nothing individually —
  // the count of them in the header is the informative form.
  const [showAll, setShowAll] = createSignal(false);

  const timer = setInterval(refetch, POLL_MS);
  onCleanup(() => clearInterval(timer));

  /// The fetched baseline with pushed progress overlaid.
  ///
  /// A memo so the overlay is recomputed once per change rather than per row render, and so a
  /// push for a repo the last fetch didn't include still appears — which is what happens when
  /// the org crawl adds a repo and its indexer ticks before the next poll.
  const merged = createMemo<RepoIndexProgress[]>(() => {
    const s = status();
    if (!s) return [];
    const byName = new Map(s.repos.map((r) => [r.full_name, r]));
    const live_rows = indexProgress as Record<string, IndexProgressEvent>;
    for (const [repo, live] of Object.entries(live_rows)) {
      const base = byName.get(repo);
      byName.set(repo, {
        // A pushed row carries only progress, so anything descriptive — language, the repo
        // card, whether it is archived — comes from the baseline and survives the overlay.
        full_name: repo,
        summary: base?.summary ?? null,
        language: base?.language ?? null,
        archived: base?.archived ?? false,
        indexed_sha: base?.indexed_sha ?? null,
        // Descriptive, so it comes from the baseline: a progress push carries no kind, and
        // defaulting it here would flip a tagged repo back to untagged on every tick.
        kind: base?.kind ?? null,
        kind_pinned: base?.kind_pinned ?? false,
        components: live.components,
        commits_cached: live.commits_cached,
        commits_summarized: live.commits_summarized,
        depends_on: live.dep_edges,
        depended_on_by: base?.depended_on_by ?? 0,
        history_back_to: live.history_back_to,
        last_commit: live.last_commit,
      });
    }
    return [...byName.values()].sort((a, b) =>
      a.full_name.localeCompare(b.full_name),
    );
  });

  const rows = () => (showAll() ? merged() : merged().filter(present));

  /// The list row for the open repo, so its own screen can show status and counts without
  /// waiting on a fetch — and keeps moving as progress is pushed.
  const openRow = createMemo(() =>
    merged().find((r) => r.full_name === openRepo()),
  );

  /// How many repos have reported progress over the socket this session.
  const pushes = createMemo(() => Object.keys(indexProgress).length);

  const [sortBy, setSortBy] = createSignal<SortKey>("repo");
  const [desc, setDesc] = createSignal(false);

  /// Click a column to sort by it; click it again to reverse.
  ///
  /// First click on a *numeric or date* column sorts descending, because "which repos have the
  /// most work left" and "what changed most recently" are the questions being asked — ascending
  /// would put the empty and the ancient at the top.
  const sortOn = (key: SortKey) => {
    if (sortBy() === key) {
      setDesc(!desc());
      return;
    }
    setSortBy(key);
    setDesc(key !== "repo" && key !== "language");
  };

  const sorted = createMemo(() => {
    const rs = [...rows()];
    const dir = desc() ? -1 : 1;
    const by = sortBy();
    rs.sort((a, b) => {
      switch (by) {
        case "language":
          // Repos with no detected language sort last either way rather than clumping under
          // an empty heading at the top.
          if (!a.language !== !b.language) return a.language ? -1 : 1;
          return dir * (a.language ?? "").localeCompare(b.language ?? "");
        case "status":
          return (
            dir * (PHASE_ORDER[phase(a).label] - PHASE_ORDER[phase(b).label])
          );
        case "components":
          return dir * (a.components - b.components);
        case "commits":
          return dir * (a.commits_summarized - b.commits_summarized);
        case "last_commit":
          // Never-fetched sorts last in both directions: "unknown" is not "old".
          if (!a.last_commit !== !b.last_commit) return a.last_commit ? -1 : 1;
          return dir * (a.last_commit ?? "").localeCompare(b.last_commit ?? "");
        default:
          return dir * a.full_name.localeCompare(b.full_name);
      }
    });
    return rs;
  });

  /// Rows grouped by what the repos are for, in the order they deserve attention.
  const groups = createMemo(() => {
    const by: Record<RepoKind, RepoIndexProgress[]> = {
      code: [],
      example: [],
      docs: [],
    };
    for (const r of sorted()) by[kindOf(r)].push(r);
    return KIND_ORDER.map((kind) => ({ kind, rows: by[kind] })).filter(
      (g) => g.rows.length,
    );
  });

  /// Which CLI drives a session. Ollama is absent because it has no agent mode — no working
  /// directory, no tool use, no event stream — and offering it would only produce a refusal.
  const [agentTool, setAgentTool] = createSignal("claude");
  const [session, setSession] = createSignal<{
    id: string;
    repo: string;
    tool: string;
  } | null>(null);

  /// Check the repo out and run an agent in it, streaming here.
  ///
  /// For a commit, the index's summary of that change seeds the question — the agent then has the
  /// actual diff available to it, which is the difference between this and the chat: chat reasons
  /// over summaries, an agent reads the files.
  const openAgent = async (repo: string, sha?: string) => {
    setError("");
    try {
      let prompt: string | undefined;
      if (sha) {
        const ctx = await api.chatContext(repo, sha);
        prompt = ctx.prompt;
      }
      const s = await api.startAgentSession(repo, agentTool(), prompt);
      setSession({ id: s.session_id, repo: s.repo, tool: s.tool });
    } catch (e) {
      setError(`${e}`.replace(/^Error:\s*/, ""));
    }
  };

  const retag = async (repo: string, kind: RepoKind | null) => {
    setError("");
    try {
      await api.setRepoKind(repo, kind);
      refetch();
    } catch (e) {
      setError(`${e}`.replace(/^Error:\s*/, ""));
    }
  };

  const Th = (props: {
    col: SortKey;
    label: string;
    title: string;
    num?: boolean;
  }) => (
    <span
      class="idx-th"
      classList={{ "idx-num": props.num, active: sortBy() === props.col }}
      title={props.title}
      onClick={() => sortOn(props.col)}
    >
      {props.label}
      <Show when={sortBy() === props.col}>{desc() ? " ▾" : " ▴"}</Show>
    </span>
  );

  const refresh = async () => {
    setBusy("refresh");
    setError("");
    try {
      const r = await api.refreshRepoIndex();
      if (!r.summarized)
        setError("no repo cards needed re-writing — nothing has moved");
    } catch (e) {
      setError(`${e}`.replace(/^Error:\s*/, ""));
    } finally {
      setBusy("");
      refetch();
    }
  };

  const errorStrip = () => (
    <Show when={error()}>
      <div class="flag-strip action-error">
        <span>{error()}</span>
        <button class="linkish" onClick={() => setError("")}>
          dismiss
        </button>
      </div>
    </Show>
  );

  /// The live transcript, shown above whichever screen started it so switching back to the list
  /// doesn't kill the thing you are watching.
  const agentPane = () => (
    <Show when={session()}>
      {(s) => (
        <AgentSession
          sessionId={s().id}
          repo={s().repo}
          tool={s().tool}
          onClose={() => setSession(null)}
        />
      )}
    </Show>
  );

  return (
    <div class="page">
      <Show
        when={openRepo() === null}
        fallback={
          <section class="panel repo-index">
            {errorStrip()}
            {agentPane()}
            <RepoDetailView
              repo={openRepo()!}
              row={openRow()}
              onBack={() => setOpenRepo(null)}
              onAgent={(sha) => void openAgent(openRepo()!, sha)}
              onRetag={(kind) => void retag(openRepo()!, kind)}
            />
          </section>
        }
      >
        <section class="panel repo-index">
          <div class="panel-head">
            <h3>CODE INDEX</h3>
            {/* Whether the numbers are arriving by push. Worth showing, because a stalled index
                and a dropped WebSocket look identical on a panel of static numbers. */}
            <span
              class="chip"
              classList={{ "ph-done": pushes() > 0, "ph-idle": pushes() === 0 }}
              title={
                pushes() > 0
                  ? `${pushes()} repo(s) reporting live; progress arrives as each batch lands`
                  : "no live updates yet — progress appears as batches land"
              }
            >
              {pushes() > 0 ? `LIVE · ${pushes()}` : "LIVE"}
            </span>
            <div class="row">
              <select
                class="kind-pick"
                title="Which agent CLI runs in the checkout"
                value={agentTool()}
                onChange={(e) => setAgentTool(e.currentTarget.value)}
              >
                <option value="claude">claude</option>
                <option value="codex">codex</option>
              </select>
              <button disabled={busy() !== ""} onClick={refresh}>
                {busy() === "refresh" ? "REFRESHING…" : "REFRESH CARDS"}
              </button>
              <button onClick={() => setShowAll(!showAll())}>
                {showAll() ? "HIDE UNTOUCHED" : "SHOW ALL"}
              </button>
            </div>
          </div>

          {errorStrip()}
          {agentPane()}

          <Show
            when={status()}
            fallback={<p class="muted">reading the index…</p>}
          >
            {(s) => (
              <>
                {/* The one-line answer to "is this thing built?". Counts rather than a
                  percentage: the denominators grow as the walk proceeds, so a percentage
                  would climb and fall for reasons that have nothing to do with progress. */}
                <div class="index-totals">
                  <span>
                    <b>{s().totals.repos_with_components}</b> of{" "}
                    {s().totals.repos} repos carded
                  </span>
                  <span>
                    <b>{s().totals.components}</b> components
                  </span>
                  <span>
                    <b>{s().totals.commits_summarized}</b> of{" "}
                    {s().totals.commits_cached} commits summarized
                  </span>
                  <span>
                    <b>{s().totals.dep_edges}</b> dependency edges
                  </span>
                  <Show when={s().totals.repos_untouched}>
                    <span
                      class="chip chip-stale"
                      title="scoring cannot reach these repos at all"
                    >
                      {s().totals.repos_untouched} untouched
                    </span>
                  </Show>
                </div>

                {/* What is happening right now. This half comes from Restate, not SQLite, and
                  it is the difference between "the index is thin" and "the index is thin and
                  nothing is working on it". */}
                <div class="index-active">
                  <span class="explain-label">IN FLIGHT</span>
                  <Show
                    when={s().active.length}
                    fallback={
                      <span class="muted">
                        nothing running — indexing ticks on a durable timer, so
                        this is empty between batches
                      </span>
                    }
                  >
                    <For each={s().active}>
                      {(inv) => (
                        <div
                          class="inv-row"
                          classList={{ "inv-failed": !!inv.failure }}
                        >
                          <span class="chip">{inv.status}</span>
                          <span class="inv-repo">{inv.repo}</span>
                          <span class="muted">{inv.handler}</span>
                          <Show when={inv.scope}>
                            <span class="chip src-chip">{inv.scope}</span>
                          </Show>
                          <Show when={inv.failure}>
                            <span class="inv-why">{inv.failure}</span>
                          </Show>
                        </div>
                      )}
                    </For>
                  </Show>
                </div>

                {/* A header, so a unit is written once instead of repeated on every row — and so
                    the columns can be clicked to sort. */}
                <div class="repo-head">
                  <span class="idx-status">
                    <Th
                      col="status"
                      label="STATUS"
                      title="Where this repo is in the index"
                    />
                  </span>
                  <Th col="repo" label="REPO" title="Sort by name" />
                  <span class="repo-facts">
                    <Th
                      col="components"
                      label="COMPONENTS"
                      title="Component cards written"
                      num
                    />
                    <Th
                      col="commits"
                      label="COMMITS"
                      title="Commit summaries written / commits fetched locally"
                      num
                    />
                    <span
                      class="idx-th idx-num idx-deps"
                      title="Dependency edges out ↗ / in ↘"
                    >
                      DEPS
                    </span>
                    <span
                      class="idx-th idx-num"
                      title="How far BACK the walk has reached — the oldest commit fetched. History is walked backwards from HEAD, so this is not the last commit."
                    >
                      HISTORY FROM
                    </span>
                    <Th
                      col="last_commit"
                      label="LAST COMMIT"
                      title="The newest commit fetched — when this repo last changed"
                      num
                    />
                  </span>
                  <span class="mini-bar-spacer" />
                  <span class="kind-pick-spacer" />
                  <span class="repo-lang">
                    <Th
                      col="language"
                      label="LANGUAGE"
                      title="Sort by language"
                    />
                  </span>
                  <span class="repo-open-spacer" />
                </div>

                <For
                  each={groups()}
                  fallback={<p class="muted">no repo has been indexed yet</p>}
                >
                  {(g) => (
                    <div class="repo-rows">
                      {/* Grouped so the twenty repos that can actually page you are legible among
                          the hundred that cannot. */}
                      <div class="kind-head">
                        {KIND_LABEL[g.kind]}{" "}
                        <span class="muted">({g.rows.length})</span>
                      </div>
                      <For each={g.rows}>
                        {(r) => {
                          const ph = phase(r);
                          return (
                            <div class="repo-row">
                              <div
                                class="repo-line"
                                title="Open this repo's index"
                                onClick={() => setOpenRepo(r.full_name)}
                              >
                                <span class={`chip ${ph.cls}`}>{ph.label}</span>
                                <span class="repo-name">{r.full_name}</span>
                                {/* Units live in the header now. "1 comp" needed explaining, which
                                is the sign a label is doing the reader's work for them. */}
                                <span class="repo-facts">
                                  <span class="idx-num">{r.components}</span>
                                  <span class="idx-num">
                                    {r.commits_summarized}/{r.commits_cached}
                                  </span>
                                  <span class="idx-num idx-deps">
                                    <Show
                                      when={r.depends_on || r.depended_on_by}
                                      fallback={"—"}
                                    >
                                      ↗{r.depends_on} ↘{r.depended_on_by}
                                    </Show>
                                  </span>
                                  <span class="idx-num">
                                    {day(r.history_back_to)}
                                  </span>
                                  <span class="idx-num">
                                    {day(r.last_commit)}
                                  </span>
                                </span>
                                {/* The bar is over the commits it has *fetched*, and only when it
                              has fetched some — see `phase`. */}
                                <Show when={r.commits_cached > 0}>
                                  <span class="mini-bar">
                                    <span
                                      class="mini-fill"
                                      style={{
                                        width: `${pct(r.commits_summarized, r.commits_cached)}%`,
                                      }}
                                    />
                                  </span>
                                </Show>
                                {/* Tagging is per row rather than a bulk editor: the guess is right
                                most of the time, and the correction is a one-off for the few it
                                is not. A select rather than a cycle button, so the options and
                                the pinned state are both visible. */}
                                <select
                                  class="kind-pick"
                                  classList={{ pinned: r.kind_pinned }}
                                  title={
                                    r.kind_pinned
                                      ? "Tagged by you — the crawl will not overwrite it"
                                      : "Guessed from the name and topics; pick one to pin it"
                                  }
                                  value={r.kind ?? ""}
                                  onClick={(e) => e.stopPropagation()}
                                  onChange={(e) =>
                                    retag(
                                      r.full_name,
                                      e.currentTarget.value === ""
                                        ? null
                                        : (e.currentTarget.value as RepoKind),
                                    )
                                  }
                                >
                                  <option value="">auto</option>
                                  <option value="code">code</option>
                                  <option value="example">example</option>
                                  <option value="docs">docs</option>
                                </select>
                                {/* Far right, and always rendered even when unknown, so the column
                                lines up down the list instead of the row above borrowing it. */}
                                <span class="repo-lang">
                                  <Show
                                    when={r.language}
                                    fallback={<span class="muted">—</span>}
                                  >
                                    <span class="chip src-chip">
                                      {r.language}
                                    </span>
                                  </Show>
                                </span>
                                {/* The whole row opens the repo; this is the affordance saying so. */}
                                <span class="repo-open">›</span>
                              </div>
                            </div>
                          );
                        }}
                      </For>
                    </div>
                  )}
                </For>
              </>
            )}
          </Show>
        </section>
      </Show>
    </div>
  );
}
