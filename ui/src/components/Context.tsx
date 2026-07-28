import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import type { ContextSource } from "../types";

export default function ContextLibrary() {
  const [sources, { refetch }] = createResource(() => api.tool<ContextSource[]>("list_context"));
  const [kind, setKind] = createSignal<"url" | "file">("url");
  const [location, setLocation] = createSignal("");
  const [credential, setCredential] = createSignal("");
  const [header, setHeader] = createSignal("");
  const [tags, setTags] = createSignal("");
  const [busy, setBusy] = createSignal("");

  const parseTags = (raw: string): string[] =>
    raw
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);

  const add = async () => {
    if (!location().trim()) return;
    setBusy("add");
    try {
      const parsed = parseTags(tags());
      const args: Record<string, unknown> =
        kind() === "url"
          ? { url: location(), credential: credential() || undefined, header: header() || undefined }
          : { path: location() };
      if (parsed.length) args.tags = parsed;
      await api.tool("add_context", args);
      setLocation("");
      setCredential("");
      setHeader("");
      setTags("");
      refetch();
    } catch (e) {
      alert(`add failed: ${e}`);
    } finally {
      setBusy("");
    }
  };

  const editTags = async (id: string, current: string[]) => {
    const next = prompt("Tags (comma-separated):", current.join(", "));
    if (next === null) return;
    await api.tool("tag_context", { id, tags: parseTags(next) });
    refetch();
  };

  const refresh = async (id: string) => {
    setBusy(id);
    try {
      await api.tool("refresh_context", { id });
      refetch();
    } finally {
      setBusy("");
    }
  };

  const remove = async (id: string) => {
    if (!confirm("Remove this source?")) return;
    await api.tool("remove_context", { id });
    refetch();
  };

  return (
    <div class="page">
      <section class="panel">
        <h3>Add context source</h3>
        <div class="form">
          <div class="row">
            <select value={kind()} onChange={(e) => setKind(e.currentTarget.value as "url" | "file")}>
              <option value="url">URL</option>
              <option value="file">File</option>
            </select>
            <input
              class="grow"
              placeholder={kind() === "url" ? "https://runbooks.internal/oncall" : "~/notes/architecture.md"}
              value={location()}
              onInput={(e) => setLocation(e.currentTarget.value)}
            />
          </div>
          <Show when={kind() === "url"}>
            <div class="row">
              <input placeholder="stored credential account (optional)" value={credential()} onInput={(e) => setCredential(e.currentTarget.value)} />
              <input placeholder="header (default Authorization)" value={header()} onInput={(e) => setHeader(e.currentTarget.value)} />
            </div>
          </Show>
          <div class="row">
            <input
              class="grow"
              placeholder="tags, comma-separated (optional — auto-suggested if blank)"
              value={tags()}
              onInput={(e) => setTags(e.currentTarget.value)}
            />
          </div>
          <button disabled={busy() === "add"} onClick={add}>
            {busy() === "add" ? "Fetching…" : "Add & ingest"}
          </button>
        </div>
      </section>

      <section class="panel">
        <h3>LIBRARY ({sources()?.length ?? 0})</h3>
        <For each={sources()} fallback={<p class="muted">No sources yet.</p>}>
          {(c) => (
            <div class="mem-item">
              <div class="mem-head">
                <strong>
                  <span class="chip">{c.kind}</span> {c.location}
                </strong>
                <span class="tl-actions">
                  <button disabled={busy() === c.id} onClick={() => refresh(c.id)}>
                    {busy() === c.id ? "…" : "refresh"}
                  </button>
                  <button onClick={() => remove(c.id)}>remove</button>
                </span>
              </div>
              <div>{c.summary ?? "(not yet summarized)"}</div>
              <div class="tags">
                <For each={c.tags} fallback={<span class="muted">no tags</span>}>
                  {(t) => <span class="chip tag">{t}</span>}
                </For>
                <button class="linkish" onClick={() => editTags(c.id, c.tags)}>
                  {c.tags_pinned ? "edit tags" : "tags (auto)"}
                </button>
              </div>
              <div class="cites">
                {c.fetched_at ? `fetched ${new Date(c.fetched_at).toLocaleString()}` : "never fetched"} · refresh {c.refresh_interval}
                {c.credential ? ` · authed(${c.credential})` : ""}
              </div>
            </div>
          )}
        </For>
      </section>
    </div>
  );
}
