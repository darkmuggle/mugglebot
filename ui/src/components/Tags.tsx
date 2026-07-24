import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import type { Tag } from "../types";

export default function TagEditor() {
  const [tags, { refetch }] = createResource(() => api.tool<Tag[]>("list_tags"));
  const [editing, setEditing] = createSignal<string | null>(null);
  const [draft, setDraft] = createSignal("");
  const [busy, setBusy] = createSignal("");

  const startEdit = (t: Tag) => {
    setEditing(t.name);
    setDraft(t.summary);
  };

  const save = async (name: string) => {
    setBusy(name);
    try {
      await api.tool("edit_tag", { name, summary: draft() });
      setEditing(null);
      refetch();
    } catch (e) {
      alert(`save failed: ${e}`);
    } finally {
      setBusy("");
    }
  };

  const remove = async (name: string) => {
    if (!confirm(`Remove tag "${name}"? It is stripped from all content that carried it.`)) return;
    await api.tool("delete_tag", { name });
    refetch();
  };

  const merge = async (name: string) => {
    const into = prompt(`Merge "${name}" into which tag? (also renames if new)`);
    if (into === null || !into.trim()) return;
    await api.tool("merge_tags", { from: name, into });
    refetch();
  };

  return (
    <div class="page">
      <section class="panel">
        <h3>TAG VOCABULARY ({tags()?.length ?? 0})</h3>
        <p class="muted">
          Each tag's summary is the description the classifier reads to decide which tags apply to
          an incoming issue. Summaries for automatic tags are generated once, then edited here.
        </p>
        <For each={tags()} fallback={<p class="muted">No tags yet — add context or memory to build the vocabulary.</p>}>
          {(t) => (
            <div class="mem-item">
              <div class="mem-head">
                <strong>
                  <span class="chip tag">{t.name}</span>
                </strong>
                <span class="tl-actions">
                  <Show
                    when={editing() === t.name}
                    fallback={<button onClick={() => startEdit(t)}>edit</button>}
                  >
                    <button disabled={busy() === t.name} onClick={() => save(t.name)}>
                      {busy() === t.name ? "…" : "save"}
                    </button>
                    <button onClick={() => setEditing(null)}>cancel</button>
                  </Show>
                  <button onClick={() => merge(t.name)}>merge</button>
                  <button onClick={() => remove(t.name)}>delete</button>
                </span>
              </div>
              <Show
                when={editing() === t.name}
                fallback={<div classList={{ muted: !t.summary }}>{t.summary || "(no summary yet)"}</div>}
              >
                <textarea
                  placeholder="What does this tag cover? The classifier reads this."
                  value={draft()}
                  onInput={(e) => setDraft(e.currentTarget.value)}
                />
              </Show>
            </div>
          )}
        </For>
      </section>
    </div>
  );
}
