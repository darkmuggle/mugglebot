import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import { day, phase, short } from "../repoindex";
import type { CommitSummaryRow, RepoIndexProgress, RepoKind } from "../types";

/// Commits pulled for the dedicated view. Higher than the list panel's old inline expander
/// asked for, because this screen is the one place you come to actually read them — the server
/// clamps at 200 either way.
const COMMIT_LIMIT = 100;

/// The view is split rather than stacked. Everything the index holds about a repo on one screen
/// is four unrelated questions answered at once — what is this, what is inside it, what points
/// at it, what has changed — and the commit summaries alone drown the other three.
type Tab = "overview" | "components" | "commits";

/// One repo's index contents, as its own screen.
export default function RepoDetailView(props: {
  repo: string;
  /// The list row, when we came from the list: lets the header show status and counts
  /// immediately instead of blank until the fetch lands.
  row?: RepoIndexProgress;
  onBack: () => void;
  /// Start an agent in a checkout of this repo, optionally seeded with one commit.
  onAgent: (sha?: string) => void;
  onRetag: (kind: RepoKind | null) => void;
}) {
  const [detail] = createResource(
    () => props.repo,
    (repo) => api.repoIndexDetail(repo, COMMIT_LIMIT),
  );
  const [tab, setTab] = createSignal<Tab>("overview");
  /// Which commits have been opened. Collapsed by default: the subject line is what you scan,
  /// the behavioural summary is what you read once you've found the change you care about.
  const [openShas, setOpenShas] = createSignal<Set<string>>(new Set());

  const toggle = (sha: string) => {
    const next = new Set(openShas());
    next.has(sha) ? next.delete(sha) : next.add(sha);
    setOpenShas(next);
  };

  const ph = createMemo(() => (props.row ? phase(props.row) : null));
  const deps = createMemo(() => {
    const d = detail();
    return (d?.depends_on.length ?? 0) + (d?.depended_on_by.length ?? 0);
  });

  const TabBtn = (p: { id: Tab; label: string; count?: number }) => (
    <button
      class="rd-tab"
      classList={{ active: tab() === p.id }}
      onClick={() => setTab(p.id)}
    >
      {p.label}
      <Show when={p.count !== undefined}>
        <span class="rd-tab-n">{p.count}</span>
      </Show>
    </button>
  );

  return (
    <div class="repo-detail-view">
      <div class="rd-head">
        <button class="pill" onClick={props.onBack}>
          ‹ INDEX
        </button>
        <h3 class="rd-name">{props.repo}</h3>
        <Show when={ph()}>
          {(p) => <span class={`chip ${p().cls}`}>{p().label}</span>}
        </Show>
        <Show when={props.row?.language ?? detail()?.entry?.language}>
          {(lang) => <span class="chip src-chip">{lang()}</span>}
        </Show>
        <span class="rd-spacer" />
        <select
          class="kind-pick"
          classList={{ pinned: !!props.row?.kind_pinned }}
          title={
            props.row?.kind_pinned
              ? "Tagged by you — the crawl will not overwrite it"
              : "Guessed from the name and topics; pick one to pin it"
          }
          value={props.row?.kind ?? ""}
          onChange={(e) =>
            props.onRetag(
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
        <button
          class="explain-btn"
          title="Check this repo out and run an agent in it"
          onClick={() => props.onAgent()}
        >
          AGENT
        </button>
      </div>

      <Show when={detail()} fallback={<p class="muted">reading the index…</p>}>
        {(d) => (
          <>
            {/* Counts on one line. They are the reason you opened the repo, and as a strip
                they stay legible while the tab below changes underneath them. */}
            <div class="rd-facts">
              <span>
                <b>{d().components.length}</b> components
              </span>
              <span>
                <b>{props.row?.commits_summarized ?? d().commit_summaries.length}</b>
                {props.row ? ` of ${props.row.commits_cached}` : ""} commits
                summarized
              </span>
              <span>
                <b>{deps()}</b> dependency edges
              </span>
              <span class="muted">
                history {day(d().history_back_to)} →{" "}
                {day(props.row?.last_commit ?? null)}
              </span>
              <Show when={d().entry?.indexed_sha}>
                {(sha) => (
                  <span class="chip" title="the commit the repo card was written from">
                    {short(sha())}
                  </span>
                )}
              </Show>
            </div>

            <div class="rd-tabs">
              <TabBtn id="overview" label="OVERVIEW" />
              <TabBtn
                id="components"
                label="COMPONENTS"
                count={d().components.length}
              />
              <TabBtn
                id="commits"
                label="COMMITS"
                count={d().commit_summaries.length}
              />
            </div>

            <Show when={tab() === "overview"}>
              <div class="rd-body">
                <Show
                  when={d().entry?.summary}
                  fallback={
                    <p class="muted">
                      no repo card yet — scoring has nothing to match an
                      incident's words against here
                    </p>
                  }
                >
                  <div class="rd-card">{d().entry!.summary}</div>
                </Show>

                <Show when={d().entry?.description}>
                  <p class="rd-desc">{d().entry!.description}</p>
                </Show>

                <div class="rd-section-label">DEPENDENCIES</div>
                <Show
                  when={d().depends_on.length || d().depended_on_by.length}
                  fallback={
                    <p class="muted">
                      no edges to indexed repos — an edge to somewhere MuggleBot
                      can't look would propagate a score to nowhere, so those
                      aren't recorded
                    </p>
                  }
                >
                  <div class="rd-deps">
                    <For each={d().depends_on}>
                      {(e) => (
                        <div class="dep-row">
                          <span class="chip">↗ depends on</span>
                          <span>{e.to_repo}</span>
                          <span class="muted">
                            via <code>{e.dep_name}</code> in {e.source}
                          </span>
                        </div>
                      )}
                    </For>
                    <For each={d().depended_on_by}>
                      {(e) => (
                        <div class="dep-row">
                          <span class="chip">↘ used by</span>
                          <span>{e.from_repo}</span>
                          <span class="muted">
                            via <code>{e.dep_name}</code> in {e.source}
                          </span>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>

                {/* A glance at what has moved, without the summaries. Enough to tell whether
                    the repo is alive; the commits tab is where you read why. */}
                <Show when={d().commit_summaries.length}>
                  <div class="rd-section-label">RECENT</div>
                  <div class="rd-recent">
                    <For each={d().commit_summaries.slice(0, 5)}>
                      {(c) => (
                        <div class="rd-recent-row" onClick={() => setTab("commits")}>
                          <span class="muted">{day(c.committed_at)}</span>
                          <span class="commit-subject">
                            {c.subject ?? short(c.sha)}
                          </span>
                          <span class="muted">{c.author}</span>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
              </div>
            </Show>

            <Show when={tab() === "components"}>
              <div class="rd-body rd-comps">
                <For
                  each={d().components}
                  fallback={
                    <p class="muted">
                      none carded — scoring cannot route to this repo's
                      internals yet
                    </p>
                  }
                >
                  {(c) => (
                    <div class="rd-comp">
                      <div class="comp-path">{c.path}</div>
                      <Show when={c.purpose}>
                        <div class="comp-purpose">{c.purpose}</div>
                      </Show>
                      {/* The symptoms line is the routing key: it is what an incident's
                          words are matched against. */}
                      <Show when={c.symptoms}>
                        <div class="comp-symptoms">{c.symptoms}</div>
                      </Show>
                    </div>
                  )}
                </For>
              </div>
            </Show>

            <Show when={tab() === "commits"}>
              <div class="rd-body rd-commits">
                <Show
                  when={d().commit_summaries.length}
                  fallback={
                    <p class="muted">
                      none yet
                      {d().history_back_to === null
                        ? " — history has not been walked for this repo"
                        : ""}
                    </p>
                  }
                >
                  <For each={d().commit_summaries}>
                    {(c) => (
                      <CommitRow
                        c={c}
                        open={openShas().has(c.sha)}
                        onToggle={() => toggle(c.sha)}
                        onAgent={() => props.onAgent(c.sha)}
                      />
                    )}
                  </For>
                </Show>
              </div>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}

/// One commit: a scannable line, with the behavioural summary behind a click.
///
/// Collapsed by default because a hundred paragraphs is not a list — the subject and the date
/// are what you search by, and the summary is what you read once.
function CommitRow(props: {
  c: CommitSummaryRow;
  open: boolean;
  onToggle: () => void;
  onAgent: () => void;
}) {
  const noop = () => props.c.summary.startsWith("(no code changes");
  return (
    <div
      class="rd-commit"
      classList={{ "commit-noop": noop(), open: props.open }}
    >
      <div class="rd-commit-line" onClick={props.onToggle}>
        <span class="rd-caret">{props.open ? "▾" : "▸"}</span>
        <span class="muted rd-date">{day(props.c.committed_at)}</span>
        <span class="commit-subject">{props.c.subject ?? short(props.c.sha)}</span>
        <span class="muted rd-author">{props.c.author}</span>
        <For each={props.c.components}>
          {(p) => <span class="chip src-chip">{p}</span>}
        </For>
      </div>
      <Show when={props.open}>
        <div class="rd-commit-body">
          <div class="commit-summary">{props.c.summary}</div>
          <div class="rd-commit-actions">
            <Show
              when={props.c.url}
              fallback={<span class="chip">{short(props.c.sha)}</span>}
            >
              <a
                class="chip"
                href={props.c.url!}
                target="_blank"
                rel="noreferrer"
              >
                {short(props.c.sha)}
              </a>
            </Show>
            {/* Per commit, because "what did this change break" is asked about one change. */}
            <button
              class="explain-btn"
              title="Run an agent on this commit, seeded with the index's summary of it"
              onClick={props.onAgent}
            >
              AGENT
            </button>
          </div>
        </div>
      </Show>
    </div>
  );
}
