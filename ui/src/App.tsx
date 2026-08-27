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
import Clock from "./components/Clock";
import ConfigPage from "./components/Config";
import Directory, { type DirectoryTab } from "./components/Directory";
import Knowledge, { type KnowledgeTab } from "./components/Knowledge";
import Personas from "./components/Personas";
import Read, { type ReadTab } from "./components/Read";
import SubjectDetail from "./components/SubjectDetail";
import TooltipLayer from "./components/Tooltip";
import { api } from "./api";
import type { SourceSnooze } from "./types";
import {
  connect,
  connected,
  disconnect,
  health,
  hints,
  redAlert,
  setChatSeed,
  signals,
  subjects,
} from "./state";

/// The destinations, and what question each answers.
///
/// There were twelve. Eight of them were not destinations — they were sub-views that had each
/// been given a rail slot because there was nowhere else to put them, and the rail is the most
/// expensive real estate in the app: every slot is a thing the operator has to rule out before
/// finding the one they want.
///
/// What merged, and why:
///
/// - **Read** (was Threads + Review). The old rail's own comment said it: *"both are 'point at
///   a thing and have it read properly', and both use the personas."* Same interaction, same
///   output shape, same guarantee that nothing is posted upstream. The only difference is what
///   you paste, which the input can work out for itself.
/// - **Knowledge** (was Memory + Context + Tags). One body of grounding seen three ways — and
///   tags are a *property* of the other two, not a peer of them.
/// - **Directory** (was Code index + Cloud). Again from the old rail's comment: *"both answer
///   'what does this name refer to' about a body of things that exists whether or not anything
///   is on fire."*
///
/// What did **not** merge, and why not:
///
/// - **Board and Incidents stay apart.** The tempting merge, and the wrong one. They are
///   disjoint by construction on the server (`board_views()` excludes incidents,
///   `incident_views()` is only incidents) and they answer different questions: what does my
///   work need from me, versus what is broken in production. One is driven by what you have
///   read, the other by what incident.io says. They now *look* alike — both are card grids —
///   which is the consistency that was missing; being one list is not.
/// - **Personas stays.** It is read on request and never competes for attention, and Read
///   already uses it. That is an argument for adjacency, not for absorption.
type View =
  | "board"
  | "incidents"
  | "chat"
  | "read"
  | "personas"
  | "knowledge"
  | "directory"
  | "config";

const VIEWS: View[] = [
  "board",
  "incidents",
  "chat",
  "read",
  "personas",
  "knowledge",
  "directory",
  "config",
];

/// Sources, for the rail's health and snooze strip. `incident` here is `incident_io` on the
/// wire; the board's own filter chips use the wire name directly.
const SOURCES = ["github", "slack", "granola", "incident"] as const;

/// Where you are, as a URL: `#/board`, `#/chat`, `#/t/restatedev/nuon-byoc!140`.
///
/// This used to live only in component signals, which meant a refresh dropped you back on the
/// board, the browser's Back button did nothing, and a thread could not be linked to from Slack
/// or a ticket — for a tool whose whole job is pointing at one piece of work, that last one is
/// the real cost. A subject key can contain `/` and `#`, so it is encoded rather than
/// interpolated.
///
/// `sub` is the tab within a destination. It is in the URL for the same reason the view is: a
/// merge that made `#/tags` unlinkable would have traded one real capability for tidiness.
interface Loc {
  view: View;
  selected: string | null;
  sub: string | null;
}

/// Destinations that were their own rail slot before the merge.
///
/// Kept working rather than redirected away, because these are links that exist in tickets and
/// Slack messages already. A URL that has been published is a promise; renaming the screen
/// behind it is not a reason to break it.
const LEGACY: Record<string, { view: View; sub: string }> = {
  memory: { view: "knowledge", sub: "memory" },
  context: { view: "knowledge", sub: "context" },
  tags: { view: "knowledge", sub: "tags" },
  threads: { view: "read", sub: "thread" },
  review: { view: "read", sub: "branch" },
  index: { view: "directory", sub: "code" },
};

function parseHash(): Loc {
  const raw = location.hash.replace(/^#\/?/, "");
  if (raw.startsWith("t/")) {
    return { view: "board", selected: decodeURIComponent(raw.slice(2)), sub: null };
  }
  // `#/cloud/env_201k…` — a platform entity, which is where an alert's entity chip points.
  // The Cloud screen moved under Directory; the link it publishes did not.
  if (raw.startsWith("cloud/")) {
    return { view: "directory", selected: decodeURIComponent(raw.slice(6)), sub: "cloud" };
  }
  if (raw === "cloud") return { view: "directory", selected: null, sub: "cloud" };

  // Split into at most three parts *without losing the tail*. `raw.split("/", 3)` would not do
  // this: JavaScript's limit argument truncates the result rather than packing the remainder
  // into the last element, so `read/branch/owner%2Frepo@main` silently became `read/branch` and
  // the selection vanished on every reload.
  const slash1 = raw.indexOf("/");
  const head = slash1 === -1 ? raw : raw.slice(0, slash1);
  const after = slash1 === -1 ? "" : raw.slice(slash1 + 1);
  const slash2 = after.indexOf("/");
  const tab = slash2 === -1 ? after : after.slice(0, slash2);
  const rest = slash2 === -1 ? "" : after.slice(slash2 + 1);

  const legacy = LEGACY[head];
  if (legacy) {
    // `#/review/<branch key>` kept working too — the old destination published these.
    return {
      view: legacy.view,
      selected: after ? decodeURIComponent(after) : null,
      sub: legacy.sub,
    };
  }

  const view = VIEWS.find((v) => v === head);
  return {
    view: view ?? "board",
    selected: rest ? decodeURIComponent(rest) : null,
    sub: tab || null,
  };
}

function toHash(view: View, selected: string | null, sub: string | null): string {
  // The open thread is a *board* location. Keying the hash off `selected` alone left the URL
  // pointing at a thread after navigating to Chat, so a refresh went back to the thread rather
  // than to where the operator actually was.
  if (view === "board" && selected) return `#/t/${encodeURIComponent(selected)}`;
  // Directory's Cloud selection is a location too, so a resolved entity can be linked to from a
  // ticket or pasted into a thread. It keeps its original short form.
  if (view === "directory" && sub === "cloud" && selected) {
    return `#/cloud/${encodeURIComponent(selected)}`;
  }
  // Everything else that has something open says so in the third segment — a branch review, a
  // thread analysis. The key can contain `/` and `@`, so it is encoded rather than interpolated.
  if (sub && selected) return `#/${view}/${sub}/${encodeURIComponent(selected)}`;
  return sub ? `#/${view}/${sub}` : `#/${view}`;
}

/// The tab a destination lands on when the URL does not name one.
const DEFAULT_SUB: Partial<Record<View, string>> = {
  read: "thread",
  knowledge: "memory",
  directory: "code",
};

export default function App() {
  const initial = parseHash();
  const [view, setView] = createSignal<View>(initial.view);
  const [selected, setSelected] = createSignal<string | null>(initial.selected);
  const [sub, setSub] = createSignal<string | null>(initial.sub);
  // Active board filter: show only subjects carrying a signal from this source.
  // null = no filter (show all). Owned here rather than by the Board so it survives clicking
  // into a subject and back — the same reasoning as `reviewAs`.
  const [sourceFilter, setSourceFilter] = createSignal<string | null>(null);

  onMount(connect);
  onCleanup(disconnect);

  // Push the location whenever it changes, and follow it when the user navigates (Back/Forward,
  // or a pasted link). Writing the same hash we just read is a no-op, so the two directions
  // don't fight.
  createEffect(() => {
    const next = toHash(view(), selected(), sub());
    if (location.hash !== next) location.hash = next;
  });
  const onHash = () => {
    const loc = parseHash();
    setView(loc.view);
    setSelected(loc.selected);
    setSub(loc.sub);
  };
  onMount(() => window.addEventListener("hashchange", onHash));
  onCleanup(() => window.removeEventListener("hashchange", onHash));

  /// Where each destination was last left — which tab, and what was open on it.
  ///
  /// Folding several rail slots into tabbed destinations quietly cost something the old
  /// navigation gave for free: `Review` was its own slot, so clicking it took you back to the
  /// branch review you were reading. After the merge, clicking `Read` always reset to the first
  /// tab and dropped the open review, which reads as the app having thrown your work away.
  ///
  /// A destination is therefore a place you *return to*, not one you restart. Remembered here
  /// rather than inside each screen because a tab's component unmounts when you leave it — the
  /// state has to outlive the thing that owns it.
  /// Recorded on the way out rather than by an effect watching the location.
  ///
  /// An effect would have to observe `view`, `sub` and `selected` together, and `go` writes all
  /// three — so unless every write is batched there is an instant where the *old* view is paired
  /// with the *new* (cleared) tab, and the effect would helpfully record that the destination
  /// you are leaving had nothing open. Writing it down before anything moves has no such window.
  const lastAt: Partial<Record<View, { sub: string | null; selected: string | null }>> = {};

  /// Move to a destination, landing where it was last left.
  ///
  /// The selection means different things per view — a subject key on the board, an entity id
  /// on Cloud, a branch key under Read — so it is restored *with* the tab it belongs to and
  /// never carried across, which is what produced `#/cloud/<subject key>`.
  const go = (next: View) => {
    lastAt[view()] = { sub: sub(), selected: selected() };
    const at = lastAt[next];
    setSelected(at?.selected ?? null);
    setSub(at?.sub ?? DEFAULT_SUB[next] ?? null);
    setView(next);
  };

  const tabOf = <T extends string>(fallback: T): T => (sub() as T) ?? fallback;

  const openThread = (id: string) => {
    setSelected(id);
    setSub(null);
    setView("board");
  };

  // Jump to the chat view (used by "open in chat" — the seed is set on shared state before this
  // fires, and the Chat component consumes it on mount).
  const openChat = () => go("chat");

  // Open the chat talking *as* a persona. The seed carries the slug, so the pane starts a fresh
  // conversation already in persona mode rather than needing the picker touched.
  const openPersonaChat = (persona: string) => {
    setChatSeed({ prompt: "", tags: [], persona });
    go("chat");
  };

  // Which sources are snoozed, and for how long. Re-read on a slow tick so the countdown in the
  // rail moves and an expiry clears itself without needing a page reload — a snooze whose
  // remaining time is stale reads as a source that has silently broken.
  const [snoozes, setSnoozes] = createSignal<SourceSnooze[]>([]);
  const loadSnoozes = () =>
    api
      .tool<SourceSnooze[]>("list_source_snoozes", {})
      .then(setSnoozes)
      .catch(() => {});
  onMount(() => {
    void loadSnoozes();
    const t = setInterval(loadSnoozes, 20_000);
    onCleanup(() => clearInterval(t));
  });
  // `incident` in the rail is `incident_io` on the wire.
  const wireName = (src: string) => (src === "incident" ? "incident_io" : src);
  const snoozeOf = (src: string) => snoozes().find((s) => s.source === wireName(src));

  const toggleSnooze = async (src: string) => {
    const live = snoozeOf(src);
    const tool = live ? "wake_source" : "snooze_source";
    await api.tool(tool, { source: wireName(src) }).catch(() => {});
    await loadSnoozes();
  };

  const healthFor = (src: string) => health().find((h) => h.source === src);
  const flags = createMemo(() => hints().filter((h) => h.kind === "flag"));

  /// `sep: true` draws a divider above the item, splitting the views you *work in* from the
  /// ones you *look things up in*.
  const nav: { id: View; label: string; sep?: boolean }[] = [
    { id: "board", label: "Board" },
    // Its own destination beside Board rather than under it: "what is on fire" is a different
    // question from "what does my work need", and the two lists are disjoint server-side.
    { id: "incidents", label: "Incidents" },
    { id: "chat", label: "Chat" },
    // Point at a thread or a branch and have it read properly. Beside Chat because it is work
    // you do, not something you look up.
    { id: "read", label: "Read" },
    // Beside Read, which uses it: a persona is something you interact with.
    { id: "personas", label: "Personas" },
    { id: "knowledge", label: "Knowledge", sep: true },
    { id: "directory", label: "Directory" },
    { id: "config", label: "Config" },
  ];

  return (
    <div class="lcars" classList={{ "red-alert": redAlert() !== null }}>
      {/* One LCARS gesture (the elbow), the wordmark, and the link state as a dot. The band
          used to also carry a full-width gradient panel that displayed nothing, and spell the
          connection out as LINK ESTABLISHED — 56px of the most valuable strip on the screen,
          spent on set-dressing. */}
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
                  onClick={() => go(n.id)}
                >
                  {n.label}
                </button>
              </>
            )}
          </For>

          {/* Health and snooze — is this source working, and is it silenced. Both are global
              facts about the daemon, which is what earns them a place in the chrome.

              What used to be here as well was *filtering the board*: clicking a source row
              filtered the board and navigated you to it from wherever you happened to be. A
              control in the global rail that moves you to another screen is a control you
              cannot use without leaving what you were doing, and filtering a list is a property
              of that list. The chips now live in the board's own header. */}
          <div class="rail-sep">Sources</div>
          <For each={SOURCES}>
            {(src) => {
              const h = () => healthFor(src);
              return (
                <div
                  class="source-row"
                  classList={{ snoozed: !!snoozeOf(src) }}
                  data-tip={h()?.detail ?? ""}
                >
                  <span
                    class="dot"
                    classList={{
                      on: h()?.ok === true,
                      off: h()?.ok === false,
                      idle: !h(),
                    }}
                  />
                  <span class={`src src-${src}`}>{src.toUpperCase()}</span>
                  {/* The countdown, not just a muted icon. A snooze you cannot see the end of
                      is a mute, and a mute is indistinguishable from a broken source. */}
                  <Show when={snoozeOf(src)}>
                    {/* Total silence is a different decision from a normal snooze, so it does
                        not look the same: nothing at all is reaching you from this source,
                        including outages. */}
                    <span
                      class="snooze-left"
                      classList={{ total: snoozeOf(src)!.total }}
                    >
                      {snoozeOf(src)!.total ? "⨯" : ""}
                      {snoozeOf(src)!.remaining}
                    </span>
                  </Show>
                  <button
                    class="snooze-btn"
                    classList={{ on: !!snoozeOf(src) }}
                    data-tip={
                      snoozeOf(src)
                        ? snoozeOf(src)!.total
                          ? `Silenced completely — ${snoozeOf(src)!.remaining} left. NOTHING is reaching you from ${src}, criticals included. Click to wake.`
                          : `Snoozed — ${snoozeOf(src)!.remaining} left, ${snoozeOf(src)!.floor} and above still land. Click to wake.`
                        : `Snooze ${src} — takes its own threads off the board and holds new ones back. Critical still lands.`
                    }
                    onClick={() => void toggleSnooze(src)}
                  >
                    {snoozeOf(src) ? "◼" : "z"}
                  </button>
                </div>
              );
            }}
          </For>

          <div class="rail-spacer" />
          {/* Totals, small. The count that gets acted on ("2 to decide") is in the board
              header, beside the rows it counts. */}
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
                fallback={
                  <Board
                    onOpen={openThread}
                    sourceFilter={sourceFilter()}
                    onSourceFilter={setSourceFilter}
                  />
                }
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
            <Match when={view() === "chat"}>
              <Chat />
            </Match>
            <Match when={view() === "read"}>
              <Read
                tab={tabOf<ReadTab>("thread")}
                onTab={setSub}
                selected={selected()}
                onSelect={setSelected}
              />
            </Match>
            <Match when={view() === "personas"}>
              {/* Same chat hand-off as the directory uses: set the seed on shared state, then
                  switch views, and the Chat component consumes it on mount. */}
              <Personas onChat={openPersonaChat} />
            </Match>
            <Match when={view() === "knowledge"}>
              <Knowledge tab={tabOf<KnowledgeTab>("memory")} onTab={setSub} />
            </Match>
            <Match when={view() === "directory"}>
              <Directory
                tab={tabOf<DirectoryTab>("code")}
                onTab={(t) => {
                  // The selection belongs to the Cloud tab. Leaving it set while switching to
                  // Code would put an entity id in a URL that no longer renders one.
                  setSelected(null);
                  setSub(t);
                }}
                selected={selected()}
                onSelect={setSelected}
                onChat={openChat}
              />
            </Match>
            <Match when={view() === "config"}>
              <ConfigPage />
            </Match>
          </Switch>
        </main>
      </div>

      {/* The team's three zones, along the foot of every view. Outside `.lcars-body` so it is
          chrome like the top band — it never scrolls, and the body gives up the height rather
          than sliding under it. */}
      <Clock />

      {/* One layer for every `data-tip` in the app, at the root so a tooltip is never clipped
          by the panel that raised it. */}
      <TooltipLayer />
    </div>
  );
}
