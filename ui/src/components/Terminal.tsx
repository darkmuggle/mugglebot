import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { PTY_WS_URL } from "../api";

/// A real terminal, for the operations that are not API calls — see `src/pty.rs`.
///
/// `rcc edit` opens the document in an editor and writes it back when you quit; `rcc
/// reconcile` prints a diff and waits for a yes; the token script reports progress as it
/// runs. None of those can be a button that returns JSON, so this is a genuine PTY: keystrokes
/// down the socket, bytes back, `SIGWINCH` on resize.
///
/// What the socket carries is a command *kind* and its inputs, never a command string. The
/// server builds the argv itself after validating the inputs against the entity cache, which
/// is what stops this endpoint being a remote shell. See the module docs in `src/pty.rs`.

/// The start payload. Mirrors `pty::Request`, which is the only shape the server accepts.
export type PtyRequest =
  | { kind: "rcc_get"; id: string }
  | { kind: "rcc_edit"; id: string }
  /// Several ids in one session: `rcc reconcile` diffs and confirms each in turn, so a bulk
  /// reconcile queues the per-environment prompts rather than skipping them.
  | { kind: "rcc_reconcile"; ids: string[] }
  | {
      kind: "rcc_override";
      id: string;
      /// Omitted rather than null when unchanged — the backend reads absence as "leave it".
      memory_gib?: number;
      storage_gib?: number;
    }
  | {
      kind: "kube_token";
      install: string;
      access_level: string;
      duration: string;
    };

export default function PtyPane(props: {
  request: PtyRequest;
  /// Fired once with the child's exit code and the tail of what it printed.
  ///
  /// The transcript is there because an exit code is not enough to know what happened: `rcc
  /// edit` exits 0 both when it writes and when it prints "No changes.", and only one of
  /// those leaves an unapplied change behind.
  onExit?: (code: number, transcript: string) => void;
  onClose: () => void;
}) {
  let host!: HTMLDivElement;
  const [label, setLabel] = createSignal("starting…");
  const [exited, setExited] = createSignal<number | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  /// Set once the terminal exists. The pane clicks through to it — without a way to take
  /// focus back, one stray click anywhere else on the page leaves an editor you can see,
  /// that is running, and that ignores every key you press.
  let focusTerm: () => void = () => {};

  onMount(() => {
    const term = new Terminal({
      fontSize: 12,
      fontFamily: 'ui-monospace, "SF Mono", Menlo, monospace',
      // Matches the LCARS surface tokens; xterm paints its own background, so leaving it
      // default puts a black rectangle in the middle of the panel.
      theme: { background: "#0e1620", foreground: "#dde5ec", cursor: "#5aa9e6" },
      // NOT convertEol. A pty already sends CRLF, and rewriting LF on top of that shifts
      // full-screen redraws — an editor's cursor addressing stops agreeing with where its
      // text went.
      scrollback: 5000,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);

    /// Fit only when the pane actually has a size.
    ///
    /// This is the bug that made an edit session show a cursor and nothing else. `FitAddon`
    /// divides the container by the measured cell size, and at mount the container has been
    /// inserted but not laid out — so it divides by a zero and the terminal ends up with a
    /// geometry no redraw can land in. The editor was running the whole time, painting into
    /// a grid with no cells.
    const sized = () => {
      if (!host.clientWidth || !host.clientHeight) return false;
      try {
        fit.fit();
      } catch {
        return false;
      }
      return term.cols > 0 && term.rows > 0;
    };

    let ws: WebSocket | undefined;
    let disposed = false;
    let transcript = "";

    const connect = () => {
      if (disposed) return;
      ws = new WebSocket(PTY_WS_URL);

      ws.onopen = () => {
        // Send the *fitted* size. The child is spawned with this geometry, so getting it
        // wrong here means an editor that renders for a window it does not have.
        ws!.send(
          JSON.stringify({
            type: "start",
            ...props.request,
            rows: term.rows,
            cols: term.cols,
          }),
        );
      };
      wire(ws);
    };

    // Wait for layout before connecting, so the process never starts at the wrong size.
    // Bounded: a pane that never gets a size (hidden tab, display:none) falls through to
    // xterm's own defaults rather than never starting at all.
    let attempts = 0;
    const whenSized = () => {
      if (disposed) return;
      if (sized() || ++attempts > 60) connect();
      else requestAnimationFrame(whenSized);
    };
    requestAnimationFrame(whenSized);

    function wire(sock: WebSocket) {
      sock.onmessage = (ev) => {
        const msg = JSON.parse(ev.data);
        if (msg.type === "output") {
          term.write(msg.data);
          // Kept bounded: this exists to be pattern-matched on exit, not to be a second copy
          // of the scrollback. An editor redrawing a large document would otherwise grow it
          // without limit.
          transcript = (transcript + msg.data).slice(-4000);
        }
        else if (msg.type === "started") {
          setLabel(msg.label);
          // The one moment focus is certainly wanted and the terminal is certainly ready:
          // laid out, sized, and with a process on the other end.
          term.focus();
        }
        else if (msg.type === "exit") {
          setExited(msg.code);
          props.onExit?.(msg.code, transcript);
          // Said in the pane rather than only in a badge: a process that exited leaves its
          // last screen up, and without this line a finished editor looks like a live one.
          term.write(
            `\r\n\x1b[${msg.code === 0 ? "32" : "31"}m── exited ${msg.code} ──\x1b[0m\r\n`,
          );
        } else if (msg.type === "error") {
          setError(msg.message);
          term.write(`\r\n\x1b[31m${msg.message}\x1b[0m\r\n`);
        }
      };
      sock.onerror = () => setError("connection failed");
      sock.onclose = () => setExited((c) => c ?? -1);
    }

    const send = (m: unknown) => {
      if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(m));
    };
    term.onData((data) => send({ type: "input", data }));

    // Keep the child's idea of the window in step with the pane's. Without this, an editor
    // renders for an 80×24 terminal inside whatever size the browser actually gave us.
    const resize = () => {
      if (sized()) send({ type: "resize", rows: term.rows, cols: term.cols });
    };
    const observer = new ResizeObserver(resize);
    observer.observe(host);
    window.addEventListener("resize", resize);

    /// Put the keyboard in the terminal.
    ///
    /// xterm reads keystrokes through a hidden textarea, and focusing it only works once the
    /// terminal has been laid out — which, thanks to the deferred fit above, is *after*
    /// mount. Focusing at mount (as this first did) aimed at a zero-sized terminal whose
    /// textarea had not been positioned yet, so the editor drew fine and swallowed nothing,
    /// because every keystroke was still going to the page.
    focusTerm = () => term.focus();

    onCleanup(() => {
      disposed = true;
      observer.disconnect();
      window.removeEventListener("resize", resize);
      // Closing the socket is what kills the child: leaving the page must not strand an
      // editor holding a lock on a control-plane document.
      ws?.close();
      term.dispose();
    });
  });

  return (
    // Clicking anywhere in the pane hands the keyboard back to the terminal — including the
    // margins around it, which is where a click that misses the grid would otherwise land.
    // `mousedown` rather than `click` so focus arrives before the editor sees the button.
    <div class="pty-pane" onMouseDown={() => focusTerm()}>
      <div class="pty-head">
        <code class="pty-label">{label()}</code>
        <Show when={exited() === null}>
          <span class="pty-hint">click to type</span>
        </Show>
        <Show when={exited() !== null}>
          <span
            class="pty-exit"
            classList={{ ok: exited() === 0, bad: exited() !== 0 }}
          >
            exited {exited()}
          </span>
        </Show>
        <Show when={error()}>
          <span class="pty-error">{error()}</span>
        </Show>
        <button class="pty-close" onClick={() => props.onClose()}>
          {exited() === null ? "cancel" : "close"}
        </button>
      </div>
      <div class="pty-term" ref={host} />
    </div>
  );
}
