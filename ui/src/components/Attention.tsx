import { For, Show } from "solid-js";
import type { Attention as AttentionData, Decorations } from "../types";

/// The AI-decoration facets, in the order they happen.
///
/// Each is rendered filled when present and hollow when not, so "the AI hasn't
/// looked at this yet" is visible at a glance rather than inferred from an empty
/// panel further down the page.
function facets(
  d: Decorations,
): { key: string; label: string; on: boolean; title: string }[] {
  return [
    {
      key: "tags",
      label: "TAG",
      on: d.tags,
      title: "Routing tags classified (local)",
    },
    {
      key: "summary",
      label: "SUM",
      on: d.summary,
      title: "Grounded summary written (cloud)",
    },
    {
      key: "dash",
      label: "DASH",
      on: d.dashboard,
      title: "Dashboard behind an alert link was read",
    },
    {
      key: "cause",
      label: "CAUSE",
      on: d.root_cause === "complete",
      title:
        d.root_cause === null
          ? "Root cause not investigated"
          : `Root-cause investigation: ${d.root_cause}`,
    },
    {
      key: "triage",
      label: "CODE",
      on: d.triage === "complete",
      title:
        d.triage === null
          ? "Not an assigned issue (no code triage)"
          : `Assigned-issue triage: ${d.triage}`,
    },
    {
      key: "prs",
      label: "PR",
      on: d.prs_judged > 0,
      title: d.prs_judged
        ? `${d.prs_judged} associated pull request(s) judged`
        : "No associated pull requests judged",
    },
  ];
}

/// "Does this need me, and has the AI been over it?"
export function AttentionBadge(props: {
  attention: AttentionData;
  compact?: boolean;
}) {
  const d = () => props.attention.decorated;
  const untouched = () => !facets(d()).some((f) => f.on);
  return (
    <div class="attention">
      <Show
        when={props.attention.needed}
        fallback={
          <span class="att att-clear" title="Nothing here is asking for you">
            CLEAR
          </span>
        }
      >
        <span
          class="att att-needed"
          title={props.attention.reason ?? "Needs your attention"}
        >
          NEEDS YOU
        </span>
      </Show>
      <Show
        when={
          props.attention.reason && props.attention.needed && !props.compact
        }
      >
        <span class="att-reason">{props.attention.reason}</span>
      </Show>

      <span
        class="facets"
        title={untouched() ? "The AI has not analyzed this yet" : "AI analysis"}
      >
        <For each={facets(d())}>
          {(f) => (
            <span
              class={`facet ${f.on ? "facet-on" : "facet-off"}`}
              title={f.title}
            >
              {f.label}
            </span>
          )}
        </For>
      </span>

      {/* "…and melted my macbook": local work is the fans, cloud work is the bill. */}
      <Show when={d().local_passes > 0 || d().cloud_passes > 0}>
        <span class="passes">
          <Show when={d().local_passes > 0}>
            <span
              class="pass pass-local"
              title={`${d().local_passes} pass(es) ran on this machine`}
            >
              ⌂{d().local_passes}
            </span>
          </Show>
          <Show when={d().cloud_passes > 0}>
            <span
              class="pass pass-cloud"
              title={`${d().cloud_passes} metered cloud call(s)`}
            >
              ☁{d().cloud_passes}
            </span>
          </Show>
        </span>
      </Show>
    </div>
  );
}
