import { For, onCleanup, onMount, Show } from "solid-js";
import { entityHref } from "../entities";
import { renderMessage } from "../markdown";
import type { Signal } from "../types";

// Older GitHub signals may carry the REST API subject URL; keep those clickable
// while newer signals store the web URL directly.
export function signalHref(signal: Signal): string | undefined {
  if (signal.source !== "github" || !signal.url) return signal.url ?? undefined;
  try {
    const url = new URL(signal.url);
    if (url.hostname !== "api.github.com") return signal.url;
    const [, repos, owner, repo, resource, identifier] = url.pathname.split("/");
    if (repos !== "repos" || !owner || !repo || !identifier) return signal.url;
    const route = { issues: "issues", pulls: "pull", discussions: "discussions", commits: "commit" }[
      resource
    ];
    return route ? `https://github.com/${owner}/${repo}/${route}/${identifier}` : signal.url;
  } catch {
    return signal.url;
  }
}

function ciLogHref(signal: Signal): string | undefined {
  if (!signal.raw || typeof signal.raw !== "object") return undefined;
  const value = (signal.raw as Record<string, unknown>).ci_log_url;
  return typeof value === "string" && value.startsWith("https://github.com/") ? value : undefined;
}

/// A pop-out modal showing one signal in full — the complete alert content
/// (Value / Labels / Annotations / links) that the board and timeline keep
/// collapsed so their rows stay scannable. Closes on ✕, backdrop click, or Esc.
export function SignalModal(props: { signal: Signal; onClose: () => void }) {
  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });
  const s = () => props.signal;
  return (
    <div class="modal-backdrop" onClick={props.onClose}>
      <div class="modal" role="dialog" aria-modal="true" onClick={(e) => e.stopPropagation()}>
        <div class="modal-head">
          <span class={`src src-${s().source}`}>{s().source.toUpperCase()}</span>
          <span class={`state state-${s().state}`}>{s().state}</span>
          <button class="modal-close" title="Close (Esc)" onClick={props.onClose}>
            ✕
          </button>
        </div>
        <h3 class="modal-title">{s().title}</h3>
        <div class="modal-meta">
          <time>{new Date(s().occurred_at).toLocaleString()}</time>
          <Show when={signalHref(s())}>
            <a href={signalHref(s())} target="_blank" rel="noreferrer">
              open source ↗
            </a>
          </Show>
          <Show when={ciLogHref(s())}>
            <a href={ciLogHref(s())} target="_blank" rel="noreferrer">
              open build log ↗
            </a>
          </Show>
        </div>
        <Show when={s().body?.trim()}>
          <div class="md modal-body" innerHTML={renderMessage(s().body!)} />
        </Show>
        <Show when={s().entities.length}>
          <div class="chips">
            <For each={s().entities}>
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
        </Show>
      </div>
    </div>
  );
}
