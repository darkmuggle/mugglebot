import { createEffect, createResource, createSignal, For, onMount, Show } from "solid-js";
import { api } from "../api";
import { looksLikeMarkdown, renderMarkdown } from "../markdown";
import { chatSeed, setChatSeed } from "../state";
import type { ChatBubble, ChatImage, ChatTurn, Tag } from "../types";

// UI-facing provider labels; the id is what the backend maps to a reasoner.
//
// Local is FIRST because it is the default, and the default is what gets used. Picking
// Anthropic here is the operator asking a cloud model by name — which is the only way one
// is ever used — so it must be a choice, not what happens when nobody chooses.
const PROVIDERS = [
  { id: "ollama_local", label: "Ollama (Local)" },
  { id: "anthropic", label: "Anthropic" },
  { id: "openai", label: "OpenAI" },
  { id: "ollama", label: "Ollama Cloud" },
] as const;

function fileToImage(file: File): Promise<ChatImage> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const url = reader.result as string; // data:<mime>;base64,<data>
      const [meta, base64] = url.split(",");
      const media_type = meta.slice(5).split(";")[0] || "image/png";
      resolve({ media_type, base64 });
    };
    reader.onerror = reject;
    reader.readAsDataURL(file);
  });
}

/** Derive a chat title from its first user message. */
function titleFor(history: ChatBubble[]): string {
  const first = history.find((b) => b.role === "user" && b.content.trim());
  const text = first?.content.trim() ?? "";
  return text ? text.slice(0, 48) : "New chat";
}

export default function Chat() {
  const [history, setHistory] = createSignal<ChatBubble[]>([]);
  const [input, setInput] = createSignal("");
  const [pending, setPending] = createSignal<ChatImage[]>([]);
  const [busy, setBusy] = createSignal(false);

  // The chat we're editing. A fresh id is minted per new conversation and only
  // persisted once it has messages.
  const [chatId, setChatId] = createSignal<string>(crypto.randomUUID());
  const [chats, { refetch: refetchChats }] = createResource(() => api.listChats());

  // Provider + dynamically-loaded model list. Models refetch whenever the
  // provider changes; we default the selection to the first available model.
  const [provider, setProvider] = createSignal<string>(PROVIDERS[0].id);
  const [model, setModel] = createSignal<string>("");
  const [models, { refetch: refetchModels }] = createResource(provider, (p) => api.models(p));
  createEffect(() => {
    const list = models();
    if (list && list.length && !list.includes(model())) setModel(list[0]);
  });

  // Routing tags attached to this chat. Their tag-matched memory and context are
  // folded in server-side as grounding, so the agent starts with the relevant
  // runbooks/lessons already in hand.
  const [tags] = createResource(() => api.tool<Tag[]>("list_tags"));
  const [selectedTags, setSelectedTags] = createSignal<string[]>([]);
  const toggleTag = (name: string) =>
    setSelectedTags((prev) =>
      prev.includes(name) ? prev.filter((t) => t !== name) : [...prev, name],
    );

  // Consume a "open in chat" hand-off from the board: start a fresh conversation
  // seeded with the thread's prompt and tags, then clear the seed so it fires once.
  onMount(() => {
    const seed = chatSeed();
    if (!seed) return;
    setChatSeed(null);
    setChatId(crypto.randomUUID());
    setHistory([]);
    setInput(seed.prompt);
    setSelectedTags(seed.tags);
  });

  const attach = async (files: FileList | null) => {
    if (!files) return;
    for (const f of Array.from(files)) {
      try {
        const img = await fileToImage(f);
        setPending((p) => [...p, img]);
      } catch {
        /* skip */
      }
    }
  };

  const newChat = () => {
    setChatId(crypto.randomUUID());
    setHistory([]);
    setInput("");
    setPending([]);
    setSelectedTags([]);
  };

  const openChat = async (id: string) => {
    if (id === chatId() || busy()) return;
    try {
      const c = await api.getChat(id);
      setChatId(c.id);
      setHistory(c.messages ?? []);
      setInput("");
      setPending([]);
    } catch {
      /* ignore load failure */
    }
  };

  const removeChat = async (id: string, e: MouseEvent) => {
    e.stopPropagation();
    try {
      await api.deleteChat(id);
    } finally {
      if (id === chatId()) newChat();
      refetchChats();
    }
  };

  // Persist the current transcript, then refresh the list so the ordering and
  // title reflect the latest activity.
  const persist = async (h: ChatBubble[]) => {
    if (h.length === 0) return;
    try {
      await api.saveChat(chatId(), titleFor(h), h);
      refetchChats();
    } catch {
      /* a failed save shouldn't break the live chat */
    }
  };

  const send = async () => {
    const text = input().trim();
    if ((!text && pending().length === 0) || busy()) return;
    const userBubble: ChatBubble = { role: "user", content: text, images: pending() };
    const next = [...history(), userBubble];
    setHistory(next);
    setInput("");
    setPending([]);
    setBusy(true);
    try {
      const turns: ChatTurn[] = next.map((b) => ({ role: b.role, content: b.content, images: b.images }));
      const resp = await api.chat(turns, provider(), model() || undefined, selectedTags());
      const withReply = [...next, { role: "assistant" as const, content: resp.answer, images: [], tools: resp.tool_calls }];
      setHistory(withReply);
      persist(withReply);
    } catch (e) {
      const withErr = [...next, { role: "assistant" as const, content: `error: ${e}`, images: [] }];
      setHistory(withErr);
      persist(withErr);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="chat-layout">
      <aside class="chat-list">
        <button class="chat-new" onClick={newChat}>+ NEW CHAT</button>
        <div class="chat-list-scroll">
          <For each={chats()} fallback={<div class="chat-list-empty">No saved chats yet.</div>}>
            {(c) => (
              <div
                class="chat-list-item"
                classList={{ active: c.id === chatId() }}
                onClick={() => openChat(c.id)}
              >
                <span class="chat-list-title">{c.title}</span>
                <button class="chat-del" title="Delete chat" onClick={(e) => removeChat(c.id, e)}>✕</button>
              </div>
            )}
          </For>
        </div>
      </aside>

      <div class="chat">
        <div class="chat-log">
          <Show when={history().length} fallback={<div class="empty">Ask about the board, a service, or drop a screenshot.</div>}>
            <For each={history()}>
              {(b) => (
                <div class={`bubble ${b.role}`}>
                  <Show when={b.tools?.length}>
                    <div class="tool-trace">
                      <For each={b.tools}>{(t) => <span class="chip">{t.tool}</span>}</For>
                    </div>
                  </Show>
                  <Show
                    when={b.role === "assistant" && looksLikeMarkdown(b.content)}
                    fallback={<div class="bubble-text">{b.content}</div>}
                  >
                    <div class="bubble-text md" innerHTML={renderMarkdown(b.content)} />
                  </Show>
                  <Show when={b.images.length}>
                    <div class="thumbs">
                      <For each={b.images}>
                        {(img) => <img alt="attachment" src={`data:${img.media_type};base64,${img.base64}`} />}
                      </For>
                    </div>
                  </Show>
                </div>
              )}
            </For>
          </Show>
          <Show when={busy()}>
            <div class="bubble assistant"><div class="bubble-text muted">thinking…</div></div>
          </Show>
        </div>

        <div class="chat-input">
          <div class="model-bar">
            <select value={provider()} onChange={(e) => setProvider(e.currentTarget.value)}>
              <For each={PROVIDERS}>{(p) => <option value={p.id}>{p.label}</option>}</For>
            </select>
            <select
              value={model()}
              // Only truly disabled when we have no list at all. A refetch (see
              // onFocus) sets `models.loading` while keeping the prior value, so
              // we stay enabled and don't collapse the open dropdown to one entry.
              disabled={!(models()?.length)}
              // Refresh the list at selection time so newly available models show.
              onFocus={() => refetchModels()}
              onChange={(e) => setModel(e.currentTarget.value)}
            >
              <Show
                when={models()?.length}
                fallback={
                  <option>
                    {models.loading ? "loading…" : models.error ? "unavailable" : "no models"}
                  </option>
                }
              >
                <For each={models()}>{(m) => <option value={m}>{m}</option>}</For>
              </Show>
            </select>
          </div>
          <Show when={tags()?.length}>
            <div class="tag-bar" title="Attach tags to ground the chat in matching memory & context">
              <span class="tag-bar-label">CONTEXT</span>
              <For each={tags()}>
                {(tag) => (
                  <button
                    type="button"
                    class="chip tag tag-toggle"
                    classList={{ on: selectedTags().includes(tag.name) }}
                    title={tag.summary}
                    onClick={() => toggleTag(tag.name)}
                  >
                    {tag.name}
                  </button>
                )}
              </For>
            </div>
          </Show>
          <Show when={pending().length}>
            <div class="thumbs">
              <For each={pending()}>
                {(img) => <img alt="pending" src={`data:${img.media_type};base64,${img.base64}`} />}
              </For>
            </div>
          </Show>
          <div class="row">
            <label class="attach-btn">
              📎
              <input type="file" accept="image/*" multiple hidden onChange={(e) => attach(e.currentTarget.files)} />
            </label>
            <textarea
              class="grow"
              placeholder="Message MuggleBot…"
              value={input()}
              onInput={(e) => setInput(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  send();
                }
              }}
            />
            <button disabled={busy()} onClick={send}>SEND</button>
          </div>
        </div>
      </div>
    </div>
  );
}
