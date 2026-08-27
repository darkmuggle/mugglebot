import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import type { ContextSource } from "../types";
import { Empty, Field, Form, Item, Panel } from "./ui";

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
    <>
      <Panel title="Sources" meta={`${sources()?.length ?? 0} in the library`}>
        <For
          each={sources()}
          fallback={
            <Empty kind="clear">
              Nothing in the library yet. Add a runbook, an architecture note or an internal
              doc below — it is fetched, summarised and made available to grounding.
            </Empty>
          }
        >
          {(c) => (
            <Item
              title={
                <>
                  <span class="chip">{c.kind}</span>
                  <span class="item-loc">{c.location}</span>
                </>
              }
              actions={
                <>
                  <button disabled={busy() === c.id} onClick={() => refresh(c.id)}>
                    {busy() === c.id ? "…" : "Refresh"}
                  </button>
                  <button class="destructive" onClick={() => remove(c.id)}>
                    Remove
                  </button>
                </>
              }
              foot={
                <>
                  <span class="tags">
                    <For each={c.tags} fallback={<span class="muted">no tags</span>}>
                      {(t) => <span class="chip tag">{t}</span>}
                    </For>
                  </span>
                  <button class="linkish" onClick={() => editTags(c.id, c.tags)}>
                    {c.tags_pinned ? "edit tags" : "tags (auto)"}
                  </button>
                  <span>
                    ·{" "}
                    {c.fetched_at
                      ? `fetched ${new Date(c.fetched_at).toLocaleString()}`
                      : "never fetched"}{" "}
                    · refresh {c.refresh_interval}
                    {c.credential ? ` · authed(${c.credential})` : ""}
                  </span>
                </>
              }
            >
              {/* Said out loud rather than left blank: a source with no summary has not been
                  read yet, and is not yet doing the job it was added for. */}
              <Show when={c.summary} fallback={<span class="muted">Not summarised yet.</span>}>
                {c.summary}
              </Show>
            </Item>
          )}
        </For>
      </Panel>

      <Panel title="Add a source" tone="quiet">
        <Form
          actions={
            <button class="primary" disabled={busy() === "add"} onClick={add}>
              {busy() === "add" ? "Fetching…" : "Add and ingest"}
            </button>
          }
        >
          <Field label="Location" hint={kind() === "url" ? "fetched over HTTP" : "read from disk"}>
            <div class="toolbar">
              <select
                value={kind()}
                onChange={(e) => setKind(e.currentTarget.value as "url" | "file")}
              >
                <option value="url">URL</option>
                <option value="file">File</option>
              </select>
              <input
                placeholder={
                  kind() === "url"
                    ? "https://runbooks.internal/oncall"
                    : "~/notes/architecture.md"
                }
                value={location()}
                onInput={(e) => setLocation(e.currentTarget.value)}
              />
            </div>
          </Field>
          <Show when={kind() === "url"}>
            <Field label="Credential" hint="optional — the name of a secret on the Config screen">
              <div class="toolbar">
                <input
                  placeholder="stored credential account"
                  value={credential()}
                  onInput={(e) => setCredential(e.currentTarget.value)}
                />
                <input
                  placeholder="header (default Authorization)"
                  value={header()}
                  onInput={(e) => setHeader(e.currentTarget.value)}
                />
              </div>
            </Field>
          </Show>
          <Field label="Tags" hint="comma-separated — auto-suggested if blank">
            <input value={tags()} onInput={(e) => setTags(e.currentTarget.value)} />
          </Field>
        </Form>
      </Panel>
    </>
  );
}
