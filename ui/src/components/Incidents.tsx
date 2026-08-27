import { createResource, For, Show } from "solid-js";
import { api } from "../api";
import { ago, stamp } from "../format";
import type { SubjectView } from "../types";
import { Empty, Page, Panel } from "./ui";

/// The incidents board — what is on fire, and what code it is probably about.
///
/// Deliberately **not** the main board. That board answers "what does my work need from me"
/// and is driven by what you have read; this one answers "what is broken" and is driven by
/// what incident.io says. An incident leaves here when it is closed upstream, not when you
/// acknowledge it — so the list is a mirror of production rather than of your inbox.
///
/// Read from its own endpoint for the same reason: `board_views()` excludes incidents and
/// `incident_views()` is only incidents, so neither screen can quietly show a slice of the
/// other's list.
///
/// It renders as **cards**, like the board, because that separation is about *what the list
/// is*, not about how a row should look. It used to draw `.lane-row` — the board's previous
/// row shape — with a CSS comment claiming the two lined up. They stopped lining up the day
/// the board moved to cards, and this was the only screen left holding that language alive.
export default function Incidents(props: { onOpen: (key: string) => void }) {
  const [report, { refetch }] = createResource(() => api.incidents());
  const list = () => report()?.incidents ?? [];

  return (
    <Page
      title="Incidents"
      subtitle={
        <>
          Open incidents from incident.io — <code>triage</code>, <code>active</code> and{" "}
          <code>post-incident</code>. Closed upstream, gone from here; acknowledging one does
          not remove it.
        </>
      }
      actions={
        <button disabled={report.loading} onClick={() => void refetch()}>
          {report.loading ? "Reading…" : "Refresh"}
        </button>
      }
    >
      <Show
        when={!report.error}
        fallback={
          <Empty kind="error">
            Could not read incidents: {String(report.error)}. This needs an <code>incident</code>{" "}
            API key on the Config screen.
          </Empty>
        }
      >
        <Show
          when={list().length}
          fallback={
            /* Three different nothings, and the operator must be able to tell them apart: an
               empty board is good news, an unconfigured one is not, and a loading one is
               neither. They used to render as one grey sentence. */
            <Show
              when={!report.loading}
              fallback={<Empty kind="waiting">Reading incidents…</Empty>}
            >
              <Empty kind="clear">
                Nothing open. If you expected incidents here, check that{" "}
                <code>[sources.incident]</code> is enabled and an <code>incident</code> key is
                stored.
              </Empty>
            </Show>
          }
        >
          <Panel title="Open" meta={`${list().length} incident${list().length === 1 ? "" : "s"}`}>
            <div class="card-grid">
              <For each={list()}>
                {(inc) => <IncidentCard t={inc} onOpen={props.onOpen} />}
              </For>
            </div>
          </Panel>
        </Show>
      </Show>
    </Page>
  );
}

/// One incident: what it is, and the code it has been mapped to.
///
/// Same blocks as a board card, in the same order, so the two screens are scanned the same
/// way: head (what and how stale) → title (what it is) → headline (what is already known).
function IncidentCard(props: { t: SubjectView; onOpen: (key: string) => void }) {
  const t = () => props.t;
  /// The incident.io reference, from the key (`incident:INC-448`).
  const reference = () => t().key.replace(/^incident:/, "");
  /// The upstream lifecycle, from the newest signal that carries it. Shown rather than the
  /// operator's own triage state, because upstream is what decides whether this is here.
  const status = () => {
    const raws = [...t().signals]
      .sort((a, b) => b.occurred_at.localeCompare(a.occurred_at))
      .map((s) => s.raw as Record<string, unknown> | undefined);
    for (const raw of raws) {
      const s = raw?.["status"];
      if (typeof s === "string" && s) return s;
    }
    return null;
  };
  const mapped = () => t().attention.decorated.root_cause === "complete";

  return (
    <article
      class={`card rank-incident sev-${t().severity}`}
      role="button"
      tabindex="0"
      onClick={() => props.onOpen(t().key)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          props.onOpen(t().key);
        }
      }}
    >
      <div class="card-head">
        <span class="card-type">INC</span>
        <span class="card-ref">{reference()}</span>
        <span class="card-spacer" />
        <Show when={status()}>
          <span class="badge badge-blocked">{status()}</span>
        </Show>
        <time class="card-when" data-tip={stamp(t().updated_at)}>
          {ago(t().updated_at)}
        </time>
      </div>

      <h3 class="card-title">{t().title}</h3>

      {/* The mapping. `root_cause` is the same engine that maps an issue to code: deepseek
          builds the candidate graph, Opus 5 judges it. Its absence is stated rather than left
          blank — "not analysed yet" and "nothing matched" are different answers. */}
      <Show
        when={mapped()}
        fallback={
          <p class="card-headline none">
            {t().attention.decorated.root_cause
              ? `Code mapping: ${t().attention.decorated.root_cause}`
              : "Not yet mapped to code"}
          </p>
        }
      >
        <p class="card-headline">
          {t().headline ?? "Mapped to code — open for the ranked candidates"}
        </p>
      </Show>
    </article>
  );
}
