import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import type { Memory, MemoryHit } from "../types";
import { Empty, Field, Form, Item, Panel, Toolbar } from "./ui";

export default function MemoryEditor() {
  const [memories, { refetch }] = createResource(() => api.tool<Memory[]>("list_memories"));
  const [text, setText] = createSignal("");
  const [summary, setSummary] = createSignal("");
  const [links, setLinks] = createSignal("");
  const [tags, setTags] = createSignal("");
  const [query, setQuery] = createSignal("");
  const [results, setResults] = createSignal<MemoryHit[] | null>(null);
  const [editing, setEditing] = createSignal<string | null>(null);
  const [editText, setEditText] = createSignal("");

  const parseTags = (raw: string): string[] =>
    raw
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);

  const add = async () => {
    if (!text().trim()) return;
    const parsed = parseTags(tags());
    await api.tool("put_memory", {
      text: text(),
      summary: summary() || undefined,
      links: links().split(",").map((s) => s.trim()).filter(Boolean),
      tags: parsed.length ? parsed : undefined,
    });
    setText("");
    setSummary("");
    setLinks("");
    setTags("");
    refetch();
  };

  const editTags = async (id: string, current: string[]) => {
    const next = prompt("Tags (comma-separated):", current.join(", "));
    if (next === null) return;
    await api.tool("tag_memory", { id, tags: parseTags(next) });
    refetch();
  };

  const search = async () => {
    if (!query().trim()) {
      setResults(null);
      return;
    }
    setResults(await api.tool<MemoryHit[]>("search_memory", { query: query() }));
  };

  const saveEdit = async (id: string) => {
    await api.tool("edit_memory", { id, text: editText() });
    setEditing(null);
    refetch();
  };

  const del = async (id: string) => {
    if (!confirm("Delete this memory?")) return;
    await api.tool("delete_memory", { id });
    refetch();
  };

  return (
    <>
      <Panel title="Recall">
        <Toolbar>
          <input
            placeholder="Search by meaning, not keyword…"
            value={query()}
            onInput={(e) => setQuery(e.currentTarget.value)}
            onKeyDown={(e) => e.key === "Enter" && search()}
          />
          <button onClick={search}>Recall</button>
          <Show when={results()}>
            <button
              onClick={() => {
                setQuery("");
                setResults(null);
              }}
            >
              Clear
            </button>
          </Show>
        </Toolbar>
        <Show when={results()}>
          {/* The score is shown because recall is semantic: a weak match is still returned,
              and without the number there is no way to tell it from a strong one. */}
          <div class="stack">
            <For
              each={results()}
              fallback={<Empty kind="clear">Nothing recalled for that.</Empty>}
            >
              {(m) => (
                <Item title={m.summary} actions={<span class="score">{m.score.toFixed(2)}</span>}>
                  {m.text}
                </Item>
              )}
            </For>
          </div>
        </Show>
      </Panel>

      <Panel title="Memory" meta={`${memories()?.length ?? 0} stored`}>
        <For
          each={memories()}
          fallback={
            <Empty kind="clear">
              Nothing remembered yet. Add a fact below, or distil one from a thread in Chat.
            </Empty>
          }
        >
          {(m) => (
            <Item
              title={m.summary}
              actions={
                <>
                  <Show
                    when={editing() !== m.id}
                    fallback={<button onClick={() => saveEdit(m.id)}>Save</button>}
                  >
                    <button
                      onClick={() => {
                        setEditing(m.id);
                        setEditText(m.text);
                      }}
                    >
                      Edit
                    </button>
                  </Show>
                  <button class="destructive" onClick={() => del(m.id)}>
                    Delete
                  </button>
                </>
              }
              foot={
                <>
                  <span class="tags">
                    <For each={m.tags} fallback={<span class="muted">no tags</span>}>
                      {(t) => <span class="chip tag">{t}</span>}
                    </For>
                  </span>
                  <button class="linkish" onClick={() => editTags(m.id, m.tags)}>
                    {m.tags_pinned ? "edit tags" : "tags (auto)"}
                  </button>
                  <Show when={m.links.length}>
                    <span>· links: {m.links.join(", ")}</span>
                  </Show>
                </>
              }
            >
              <Show when={editing() === m.id} fallback={m.text}>
                <textarea value={editText()} onInput={(e) => setEditText(e.currentTarget.value)} />
              </Show>
            </Item>
          )}
        </For>
      </Panel>

      <Panel title="Add memory" tone="quiet">
        <Form actions={<button class="primary" onClick={add}>Save</button>}>
          <Field label="Fact" hint="a lesson, a confirmed approach, something that surprised you">
            <textarea value={text()} onInput={(e) => setText(e.currentTarget.value)} />
          </Field>
          <Field label="Summary" hint="optional — one line; generated if left blank">
            <input value={summary()} onInput={(e) => setSummary(e.currentTarget.value)} />
          </Field>
          <Field label="Links" hint="signal or thread ids, comma-separated">
            <input value={links()} onInput={(e) => setLinks(e.currentTarget.value)} />
          </Field>
          <Field label="Tags" hint="comma-separated — auto-suggested if blank">
            <input value={tags()} onInput={(e) => setTags(e.currentTarget.value)} />
          </Field>
        </Form>
      </Panel>
    </>
  );
}
