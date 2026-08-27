import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import type { Tag } from "../types";
import { Empty, Item, Panel } from "./ui";

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
    <Panel
      title="Vocabulary"
      meta={`${tags()?.length ?? 0} tags`}
    >
      <p class="muted panel-lede">
        A tag's summary is the description the classifier reads to decide which tags apply to
        an incoming issue. Summaries for automatic tags are generated once, then edited here.
      </p>
      <For
        each={tags()}
        fallback={
          <Empty kind="clear">
            No tags yet — the vocabulary is built from what you add to Memory and Context.
          </Empty>
        }
      >
        {(t) => (
          <Item
            title={<span class="chip tag">{t.name}</span>}
            actions={
              <>
                <Show
                  when={editing() === t.name}
                  fallback={<button onClick={() => startEdit(t)}>Edit</button>}
                >
                  <button disabled={busy() === t.name} onClick={() => save(t.name)}>
                    {busy() === t.name ? "…" : "Save"}
                  </button>
                  <button onClick={() => setEditing(null)}>Cancel</button>
                </Show>
                <button onClick={() => merge(t.name)}>Merge</button>
                <button class="destructive" onClick={() => remove(t.name)}>
                  Delete
                </button>
              </>
            }
          >
            <Show
              when={editing() === t.name}
              fallback={
                /* An unsummarised tag is a tag the classifier cannot use, so it says so
                   rather than rendering as an empty line. */
                <Show when={t.summary} fallback={<span class="muted">No summary yet — the classifier has nothing to match on.</span>}>
                  {t.summary}
                </Show>
              }
            >
              <textarea
                placeholder="What does this tag cover? The classifier reads this."
                value={draft()}
                onInput={(e) => setDraft(e.currentTarget.value)}
              />
            </Show>
          </Item>
        )}
      </For>
    </Panel>
  );
}
