import { createMemo, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import Board from "./components/Board";
import Chat from "./components/Chat";
import ConfigPage from "./components/Config";
import ContextLibrary from "./components/Context";
import MemoryEditor from "./components/Memory";
import TagEditor from "./components/Tags";
import ThreadDetail from "./components/ThreadDetail";
import { connect, connected, disconnect, health, hints, redAlert, signals, threads } from "./state";

type View = "board" | "memory" | "context" | "tags" | "chat" | "config";

const SOURCES = ["github", "slack", "granola"] as const;

export default function App() {
  const [view, setView] = createSignal<View>("board");
  const [selected, setSelected] = createSignal<string | null>(null);
  // Active board filter: show only threads carrying a signal from this source.
  // null = no filter (show all). Toggled from the SOURCES rail.
  const [sourceFilter, setSourceFilter] = createSignal<string | null>(null);

  onMount(connect);
  onCleanup(disconnect);

  const openThread = (id: string) => {
    setSelected(id);
    setView("board");
  };

  // Jump to the chat view (used by "open in chat" — the seed is set on shared
  // state before this fires, and the Chat component consumes it on mount).
  const openChat = () => setView("chat");

  const healthFor = (src: string) => health().find((h) => h.source === src);
  const flags = createMemo(() => hints().filter((h) => h.kind === "flag"));

  // `sep: true` draws a divider above the item, splitting the interactive views
  // (board, chat) from the reference/knowledge views (memory, context, tags, …).
  const nav: { id: View; label: string; sep?: boolean }[] = [
    { id: "board", label: "BOARD" },
    { id: "chat", label: "CHAT" },
    { id: "memory", label: "MEMORY", sep: true },
    { id: "context", label: "CONTEXT" },
    { id: "tags", label: "TAGS" },
    { id: "config", label: "CONFIG" },
  ];

  return (
    <div class="lcars" classList={{ "red-alert": redAlert() !== null }}>
      <header class="lcars-top">
        <div class="elbow" />
        <div class="title">MUGGLEBOT</div>
        <Show when={redAlert()} fallback={<div class="bar" />}>
          <div class="bar alert-bar">RED ALERT · {redAlert()!.message}</div>
        </Show>
        <div class="status" classList={{ online: connected() }}>
          {connected() ? "LINK ESTABLISHED" : "RECONNECTING…"}
        </div>
      </header>

      <div class="lcars-body">
        <nav class="rail">
          <For each={nav}>
            {(n) => (
              <>
                <Show when={n.sep}>
                  <div class="rail-divider" />
                </Show>
                <button
                  class="pill nav-pill"
                  classList={{ active: view() === n.id }}
                  onClick={() => {
                    if (n.id === "board") setSelected(null);
                    setView(n.id);
                  }}
                >
                  {n.label}
                </button>
              </>
            )}
          </For>

          <div class="rail-sep">SOURCES</div>
          <For each={SOURCES}>
            {(src) => {
              const h = () => healthFor(src);
              // Click to filter the board to this source; click the active one
              // again to clear. Also jumps back to the board from any other view.
              const toggle = () => {
                setSourceFilter((f) => (f === src ? null : src));
                setSelected(null);
                setView("board");
              };
              return (
                <div
                  class="source-row"
                  classList={{ active: sourceFilter() === src }}
                  title={h()?.detail ?? ""}
                  onClick={toggle}
                >
                  <span
                    class="dot"
                    classList={{ on: h()?.ok === true, off: h()?.ok === false, idle: !h() }}
                  />
                  <span class={`src src-${src}`}>{src.toUpperCase()}</span>
                </div>
              );
            }}
          </For>

          <div class="rail-spacer" />
          <div class="count">{Object.keys(threads).length}</div>
          <div class="count-label">THREADS</div>
          <div class="subcount">
            {Object.keys(signals).length} signals · {hints().length} hints
          </div>
        </nav>

        <main class="main">
          <Show when={flags().length && view() !== "board"}>
            <div class="flag-strip">
              ⚠ {flags().length} active flag(s) — see the thread on the board.
            </div>
          </Show>
          <Switch>
            <Match when={view() === "board"}>
              <Show
                when={selected()}
                fallback={<Board onOpen={openThread} sourceFilter={sourceFilter()} />}
              >
                <ThreadDetail
                  id={selected()!}
                  onBack={() => setSelected(null)}
                  onOpen={openThread}
                  onOpenChat={openChat}
                />
              </Show>
            </Match>
            <Match when={view() === "memory"}>
              <MemoryEditor />
            </Match>
            <Match when={view() === "context"}>
              <ContextLibrary />
            </Match>
            <Match when={view() === "tags"}>
              <TagEditor />
            </Match>
            <Match when={view() === "chat"}>
              <Chat />
            </Match>
            <Match when={view() === "config"}>
              <ConfigPage />
            </Match>
          </Switch>
        </main>
      </div>
    </div>
  );
}
