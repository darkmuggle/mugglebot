import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import type { Memory, MemoryHit } from "../types";

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
    <div class="page">
      <section class="panel">
        <h3>Add memory</h3>
        <div class="form">
          <textarea placeholder="A fact, lesson, or confirmed approach…" value={text()} onInput={(e) => setText(e.currentTarget.value)} />
          <input placeholder="one-line summary (optional)" value={summary()} onInput={(e) => setSummary(e.currentTarget.value)} />
          <input placeholder="links: signal/thread ids, comma-separated" value={links()} onInput={(e) => setLinks(e.currentTarget.value)} />
          <input placeholder="tags, comma-separated (optional — auto-suggested if blank)" value={tags()} onInput={(e) => setTags(e.currentTarget.value)} />
          <button onClick={add}>Save</button>
        </div>
      </section>

      <section class="panel">
        <h3>Search</h3>
        <div class="row">
          <input placeholder="semantic recall…" value={query()} onInput={(e) => setQuery(e.currentTarget.value)} onKeyDown={(e) => e.key === "Enter" && search()} />
          <button onClick={search}>Recall</button>
          <Show when={results()}>
            <button onClick={() => { setQuery(""); setResults(null); }}>CLEAR</button>
          </Show>
        </div>
        <Show when={results()}>
          <For each={results()}>
            {(m) => (
              <div class="mem-item">
                <div class="mem-head"><strong>{m.summary}</strong><span class="muted">{(m.score).toFixed(2)}</span></div>
                <div>{m.text}</div>
              </div>
            )}
          </For>
        </Show>
      </section>

      <section class="panel">
        <h3>MEMORY ({memories()?.length ?? 0})</h3>
        <For each={memories()} fallback={<p class="muted">Empty.</p>}>
          {(m) => (
            <div class="mem-item">
              <div class="mem-head">
                <strong>{m.summary}</strong>
                <span class="tl-actions">
                  <Show when={editing() !== m.id} fallback={<button onClick={() => saveEdit(m.id)}>save</button>}>
                    <button onClick={() => { setEditing(m.id); setEditText(m.text); }}>edit</button>
                  </Show>
                  <button onClick={() => del(m.id)}>delete</button>
                </span>
              </div>
              <Show when={editing() === m.id} fallback={<div>{m.text}</div>}>
                <textarea value={editText()} onInput={(e) => setEditText(e.currentTarget.value)} />
              </Show>
              <div class="tags">
                <For each={m.tags} fallback={<span class="muted">no tags</span>}>
                  {(t) => <span class="chip tag">{t}</span>}
                </For>
                <button class="linkish" onClick={() => editTags(m.id, m.tags)}>
                  {m.tags_pinned ? "edit tags" : "tags (auto)"}
                </button>
              </div>
              <Show when={m.links.length}>
                <div class="cites">links: {m.links.join(", ")}</div>
              </Show>
            </div>
          )}
        </For>
      </section>
    </div>
  );
}
