import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";
import { Empty, Field, Form, Page, Panel, Toolbar } from "./ui";

export default function ConfigPage() {
  const [creds, { refetch: refetchCreds }] = createResource(() =>
    api.secrets().then((r) => r.secrets),
  );
  const [account, setAccount] = createSignal("github");
  const [secret, setSecret] = createSignal("");
  const [credMsg, setCredMsg] = createSignal("");

  // Editable TOML config.
  const [loaded, { refetch: reloadConfig }] = createResource(() => api.configRaw());
  const [draft, setDraft] = createSignal<string | null>(null);
  const [saveMsg, setSaveMsg] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  // The textarea shows the local edit if any, else the loaded file.
  const text = () => draft() ?? loaded() ?? "";
  const dirty = () => draft() !== null && draft() !== loaded();

  const save = async () => {
    setSaving(true);
    setSaveMsg("");
    try {
      const r = await api.saveConfig(text());
      setSaveMsg(r.message);
      setDraft(null);
      await reloadConfig();
    } catch (e) {
      setSaveMsg(`${e}`);
    } finally {
      setSaving(false);
    }
  };

  const saveCred = async () => {
    if (!account().trim() || !secret()) return;
    try {
      await api.setSecret(account(), secret());
      setSecret("");
      setCredMsg(`Stored '${account()}' in the local database — restart MuggleBot to apply source-token changes.`);
      refetchCreds();
    } catch (e) {
      setCredMsg(`Error: ${e}`);
    }
  };

  const removeCred = async (acc: string) => {
    if (!confirm(`Delete credential '${acc}'?`)) return;
    await api.deleteSecret(acc);
    setCredMsg(`Deleted '${acc}' from the local database — restart MuggleBot to apply source-token changes.`);
    refetchCreds();
  };

  const missing = () => (creds() ?? []).filter((s) => !s.set).length;

  return (
    <Page
      title="Config"
      subtitle="Credentials live in MuggleBot's local SQLite database, never in the config file. Treat the data directory as sensitive, and restart after changing a source token."
    >
      <Panel
        title="Credentials"
        /* The count that matters is the missing one — a source with no token is a source that
           has silently never worked, and that is not visible from a list of green dots. */
        meta={
          missing() ? (
            <span class="meta-warn">{missing()} missing</span>
          ) : (
            `${(creds() ?? []).length} set`
          )
        }
      >
        <p class="muted panel-lede">
          Reasoning uses the Claude/Codex CLI — no LLM API keys are needed here.{" "}
          <code>ollama</code> is the optional Ollama&nbsp;Cloud key.
        </p>
        <For each={creds() ?? []} fallback={<Empty kind="waiting">Reading credentials…</Empty>}>
          {(s) => (
            <div class="cred-row">
              <span class={`dot ${s.set ? "on" : "off"}`} />
              <span class="cred-name">{s.name}</span>
              <span class="muted">
                {s.set
                  ? s.updated_at
                    ? `set ${new Date(s.updated_at).toLocaleDateString()}`
                    : "set"
                  : "missing"}
              </span>
              <Show when={s.set}>
                <button class="destructive" onClick={() => removeCred(s.name)}>
                  Delete
                </button>
              </Show>
            </div>
          )}
        </For>

        <Form>
          <Field label="Store a credential" hint="the account name a source or context entry refers to">
            <Toolbar>
              <input
                placeholder="account (e.g. github)"
                value={account()}
                onInput={(e) => setAccount(e.currentTarget.value)}
              />
              <input
                type="password"
                placeholder="secret / token"
                value={secret()}
                onInput={(e) => setSecret(e.currentTarget.value)}
              />
              <button onClick={saveCred}>Store</button>
            </Toolbar>
          </Field>
          <Show when={credMsg()}>
            <div class="muted">{credMsg()}</div>
          </Show>
        </Form>
      </Panel>

      <Panel
        title="config.toml"
        /* Dirty state on the panel head, not only on the button label. "SAVE" vs "Saved" on
           the same control was the only thing distinguishing an edited file from a saved one,
           and it is the least visible pixel on the screen. */
        meta={dirty() ? <span class="meta-warn">unsaved changes</span> : "saved"}
        actions={
          <>
            <Show when={dirty()}>
              <button onClick={() => setDraft(null)}>Revert</button>
            </Show>
            <button class="primary" disabled={saving() || !dirty()} onClick={save}>
              {saving() ? "Saving…" : "Save"}
            </button>
          </>
        }
      >
        <p class="muted panel-lede">
          Invalid TOML is rejected. Most changes — sources, reasoner routing, intervals — apply
          on the next restart.
        </p>
        <Show when={loaded.error}>
          <Empty kind="error">Could not load config: {String(loaded.error)}</Empty>
        </Show>
        <textarea
          class="config-edit"
          spellcheck={false}
          value={text()}
          onInput={(e) => setDraft(e.currentTarget.value)}
        />
        <Show when={saveMsg()}>
          <div class="muted save-msg">{saveMsg()}</div>
        </Show>
      </Panel>
    </Page>
  );
}
