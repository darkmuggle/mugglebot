import { createResource, For, Show } from "solid-js";
import { api } from "../api";
import type { SubjectView } from "../types";

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
export default function Incidents(props: { onOpen: (key: string) => void }) {
  const [report, { refetch }] = createResource(() => api.incidents());

  return (
    <div class="panel">
      <h2 class="panel-title">Incidents</h2>
      <p class="muted">
        Open incidents from incident.io — <code>triage</code>, <code>active</code> and{" "}
        <code>post-incident</code>. Closed upstream, gone from here; acknowledging one
        does not remove it.
      </p>

      <Show
        when={!report.error}
        fallback={
          <p class="muted">
            Could not read incidents: {String(report.error)}. This needs an{" "}
            <code>incident</code> API key on the Config page.
          </p>
        }
      >
        <Show
          when={report()?.incidents?.length}
          fallback={
            <p class="lane-empty">
              {report.loading
                ? "Reading incidents…"
                : /* Two different nothings, and the operator should be able to tell them
                     apart: an empty board is good news, an unconfigured one is not. */
                  "Nothing open. If you expected incidents here, check that [sources.incident] is enabled and an `incident` key is stored."}
            </p>
          }
        >
          <div class="inc-list">
            <For each={report()!.incidents}>
              {(inc) => <IncidentRow t={inc} onOpen={props.onOpen} />}
            </For>
          </div>
        </Show>
      </Show>

      <button class="explain-btn" disabled={report.loading} onClick={() => void refetch()}>
        {report.loading ? "Reading…" : "Refresh"}
      </button>
    </div>
  );
}

/// One incident: what it is, and the code it has been mapped to.
function IncidentRow(props: { t: SubjectView; onOpen: (key: string) => void }) {
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

  return (
    <div
      class={`lane-row rank-incident sev-${t().severity}`}
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
      <span class="row-kind">{reference()}</span>
      <span class="row-mid">
        <span class="row-titleline">
          <span class="row-title">{t().title}</span>
          <Show when={status()}>
            <span class="badge badge-blocked">{status()}</span>
          </Show>
        </span>
        {/* The mapping. `root_cause` is the same engine that maps an issue to code:
            deepseek builds the candidate graph, Opus 5 judges it. Its absence is stated
            rather than left blank — "not analysed yet" and "nothing matched" are
            different answers. */}
        <Show
          when={t().attention.decorated.root_cause === "complete"}
          fallback={
            <span class="row-headline none">
              {t().attention.decorated.root_cause
                ? `Code mapping: ${t().attention.decorated.root_cause}`
                : "Not yet mapped to code"}
            </span>
          }
        >
          <span class="row-headline">
            {t().headline ?? "Mapped to code — open for the ranked candidates"}
          </span>
        </Show>
      </span>
      <time class="row-when">{new Date(t().updated_at).toLocaleString()}</time>
    </div>
  );
}
