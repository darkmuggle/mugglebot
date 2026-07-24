import { createResource, createSignal, For, Show } from "solid-js";
import { api } from "../api";

export default function ConfigPage() {
  const [creds, { refetch: refetchCreds }] = createResource(() => api.credentials());
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
      await api.setCredential(account(), secret());
      setSecret("");
      setCredMsg(`stored '${account()}' in the local database — restart MuggleBot to apply source-token changes`);
      refetchCreds();
    } catch (e) {
      setCredMsg(`error: ${e}`);
    }
  };

  const removeCred = async (acc: string) => {
    if (!confirm(`Delete credential '${acc}'?`)) return;
    await api.deleteCredential(acc);
    setCredMsg(`deleted '${acc}' from the local database — restart MuggleBot to apply source-token changes`);
    refetchCreds();
  };

  return (
    <div class="page">
      <section class="panel">
        <h3>CREDENTIALS (LOCAL DATABASE)</h3>
        <p class="muted">
          Secrets are stored in MuggleBot's local SQLite database, never the config file. Treat the
          MuggleBot data directory as sensitive, and restart after changing a source token. Reasoning
          uses the Claude/Codex CLI — no LLM API keys needed; <code>ollama</code> is the optional
          Ollama&nbsp;Cloud key.
        </p>
        <For each={Object.entries(creds() ?? {})}>
          {([acc, present]) => (
            <div class="cred-row">
              <span class={`dot ${present ? "on" : "off"}`} />
              <span class="cred-name">{acc}</span>
              <span class="muted">{present ? "set" : "missing"}</span>
              <Show when={present}>
                <button onClick={() => removeCred(acc)}>delete</button>
              </Show>
            </div>
          )}
        </For>
        <div class="form">
          <div class="row">
            <input placeholder="account (e.g. github)" value={account()} onInput={(e) => setAccount(e.currentTarget.value)} />
            <input class="grow" type="password" placeholder="secret / token" value={secret()} onInput={(e) => setSecret(e.currentTarget.value)} />
            <button onClick={saveCred}>STORE</button>
          </div>
          <Show when={credMsg()}>
            <div class="muted">{credMsg()}</div>
          </Show>
        </div>
      </section>

      <section class="panel">
        <div class="mem-head">
          <h3>CONFIGURATION (config.toml)</h3>
          <span class="tl-actions">
            <button disabled={saving() || !dirty()} onClick={save}>
              {saving() ? "SAVING…" : dirty() ? "SAVE" : "SAVED"}
            </button>
            <Show when={dirty()}>
              <button onClick={() => setDraft(null)}>REVERT</button>
            </Show>
          </span>
        </div>
        <p class="muted">
          Edit and save the daemon's TOML. Invalid TOML is rejected. Most changes (sources, reasoner
          routing, intervals) apply on the next restart.
        </p>
        <Show when={loaded.error}>
          <div class="flag-strip">Could not load config: {String(loaded.error)}</div>
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
      </section>
    </div>
  );
}
