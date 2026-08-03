import {
  createEffect,
  createMemo,
  createSignal,
  For,
  Match,
  onCleanup,
  onMount,
  Show,
  Switch,
} from "solid-js";
import Board from "./components/Board";
import Incidents from "./components/Incidents";
import Chat from "./components/Chat";
import ConfigPage from "./components/Config";
import ContextLibrary from "./components/Context";
import MemoryEditor from "./components/Memory";
import RepoIndexView from "./components/RepoIndex";
import TagEditor from "./components/Tags";
import SubjectDetail from "./components/SubjectDetail";
import TooltipLayer from "./components/Tooltip";
import { connect, connected, disconnect, health, hints, redAlert, signals, subjects } from "./state";

type View =
  | "board"
  | "incidents"
  | "memory"
  | "context"
  | "tags"
  | "index"
  | "chat"
  | "config";

const SOURCES = ["github", "slack", "granola", "incident"] as const;

const VIEWS: View[] = [
  "board",
  "incidents",
  "memory",
  "context",
  "tags",
  "index",
  "chat",
  "config",
];

/// Where you are, as a URL: `#/board`, `#/chat`, `#/t/restatedev/nuon-byoc!140`.
///
/// This used to live only in component signals, which meant a refresh dropped you
/// back on the board, the browser's Back button did nothing, and a thread could not
/// be linked to from Slack or a ticket — for a tool whose whole job is pointing at
/// one piece of work, that last one is the real cost. A subject key can contain `/`
/// and `#`, so it is encoded rather than interpolated.
function parseHash(): { view: View; selected: string | null } {
  const raw = location.hash.replace(/^#\/?/, "");
  if (raw.startsWith("t/")) {
    return { view: "board", selected: decodeURIComponent(raw.slice(2)) };
  }
  const view = VIEWS.find((v) => v === raw);
  return { view: view ?? "board", selected: null };
}

function toHash(view: View, selected: string | null): string {
  // The open thread is a *board* location. Keying the hash off `selected` alone left
  // the URL pointing at a thread after navigating to Chat, so a refresh went back to
  // the thread rather than to where the operator actually was.
  return view === "board" && selected
    ? `#/t/${encodeURIComponent(selected)}`
    : `#/${view}`;
}

export default function App() {
  const initial = parseHash();
  const [view, setView] = createSignal<View>(initial.view);
  const [selected, setSelected] = createSignal<string | null>(initial.selected);
  // Active board filter: show only subjects carrying a signal from this source.
  // null = no filter (show all). Toggled from the SOURCES rail.
  const [sourceFilter, setSourceFilter] = createSignal<string | null>(null);

  onMount(connect);
  onCleanup(disconnect);

  // Push the location whenever it changes, and follow it when the user navigates
  // (Back/Forward, or a pasted link). Writing the same hash we just read is a no-op,
  // so the two directions don't fight.
  createEffect(() => {
    const next = toHash(view(), selected());
    if (location.hash !== next) location.hash = next;
  });
  const onHash = () => {
    const { view: v, selected: s } = parseHash();
    setView(v);
    setSelected(s);
  };
  onMount(() => window.addEventListener("hashchange", onHash));
  onCleanup(() => window.removeEventListener("hashchange", onHash));

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
    { id: "board", label: "Board" },
    // Its own top-level view, beside Board rather than under it: "what is on fire" is a
    // different question from "what does my work need", and the two lists are disjoint.
    { id: "incidents", label: "Incidents" },
    { id: "chat", label: "Chat" },
    { id: "memory", label: "Memory", sep: true },
    { id: "context", label: "Context" },
    { id: "tags", label: "Tags" },
    { id: "index", label: "Code index" },
    { id: "config", label: "Config" },
  ];

  return (
    <div class="lcars" classList={{ "red-alert": redAlert() !== null }}>
      {/* One LCARS gesture (the elbow), the wordmark, and the link state as a dot.
          The band used to also carry a full-width gradient panel that displayed
          nothing, and spell the connection out as LINK ESTABLISHED — 56px of the
          most valuable strip on the screen, spent on set-dressing. */}
      <header class="lcars-top">
        <div class="elbow" />
        <div class="title">MUGGLEBOT</div>
        <Show when={redAlert()} fallback={<div class="bar" />}>
          <div class="bar alert-bar">Red alert · {redAlert()!.message}</div>
        </Show>
        <div
          class="status"
          classList={{ online: connected() }}
          data-tip={connected() ? "Link established" : "Reconnecting…"}
        />
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

          <div class="rail-sep">Sources</div>
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
                  data-tip={h()?.detail ?? ""}
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
          {/* Totals, small. The count that gets acted on ("2 to decide") is in the
              board header, beside the rows it counts. */}
          <div class="subcount">
            {Object.keys(subjects).length} threads
            <br />
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
                <SubjectDetail
                  id={selected()!}
                  onBack={() => setSelected(null)}
                  onOpen={openThread}
                  onOpenChat={openChat}
                />
              </Show>
            </Match>
            <Match when={view() === "incidents"}>
              <Incidents onOpen={openThread} />
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
            <Match when={view() === "index"}>
              {/* The chat hand-off: the seed is set on shared state, and switching views is
                  what makes the Chat component consume it on mount. */}
              <RepoIndexView onChat={openChat} />
            </Match>
            <Match when={view() === "config"}>
              <ConfigPage />
            </Match>
          </Switch>
        </main>
      </div>

      {/* One layer for every `data-tip` in the app, at the root so a tooltip is never
          clipped by the panel that raised it. */}
      <TooltipLayer />
    </div>
  );
}
