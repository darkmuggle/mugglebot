import { createEffect, createSignal, onCleanup, onMount, Show } from "solid-js";

/// One tooltip layer for the whole app.
///
/// Everything used to hang off the browser's `title` attribute, which is the worst of both
/// worlds: a second's delay before anything appears, no styling, no wrapping control, and — on
/// the elements marked `cursor: help` — a question-mark cursor promising an explanation that
/// most people never waited long enough to see. The affordance was there and the answer wasn't.
///
/// So the app writes `data-tip="…"` instead, and this reads it. One delegated listener rather
/// than props threaded through every component: the tips live on markdown-rendered HTML too
/// (the citation markers in a summary), where there is no component to pass anything to.
///
/// Rendered `position: fixed` at the document root on purpose. A tooltip drawn inside its own
/// panel gets clipped by the first ancestor that scrolls, and most of the dense places that
/// need one — the board rows, the commit list, the index table — scroll.
const SHOW_DELAY_MS = 90;
/// Gap between the anchor and the bubble.
const OFFSET = 8;
/// Viewport margin the bubble is kept inside.
const MARGIN = 8;

interface Anchor {
  text: string;
  /// The anchor's box, in viewport coordinates.
  rect: DOMRect;
}

export default function TooltipLayer() {
  const [anchor, setAnchor] = createSignal<Anchor | null>(null);
  const [pos, setPos] = createSignal<{ left: number; top: number; below: boolean }>({
    left: 0,
    top: 0,
    below: false,
  });
  /// Whether the bubble has been measured and placed. Until it has, it is rendered
  /// transparent — a tooltip that flashes at the top-left corner before jumping to its anchor
  /// is a worse first impression than one that appears a frame later.
  const [ready, setReady] = createSignal(false);
  let bubble: HTMLDivElement | undefined;
  let timer: number | undefined;

  const clear = () => {
    if (timer) clearTimeout(timer);
    timer = undefined;
    setAnchor(null);
  };

  const show = (el: HTMLElement) => {
    const text = el.getAttribute("data-tip")?.trim();
    if (!text) return;
    if (timer) clearTimeout(timer);
    // A short delay so sweeping the cursor across a dense row doesn't strobe, but far
    // shorter than the browser's own — the point is that the answer arrives while you are
    // still looking at the thing you asked about.
    timer = window.setTimeout(() => {
      setReady(false);
      setAnchor({ text, rect: el.getBoundingClientRect() });
    }, SHOW_DELAY_MS);
  };

  onMount(() => {
    const over = (e: Event) => {
      const el = (e.target as HTMLElement | null)?.closest?.(
        "[data-tip]",
      ) as HTMLElement | null;
      if (el) show(el);
      else clear();
    };
    // Hiding on any of these rather than on `mouseout` alone: a tip left behind by an element
    // that scrolled away or was clicked through is worse than no tip.
    const hide = () => clear();
    document.addEventListener("mouseover", over);
    document.addEventListener("focusin", over);
    document.addEventListener("mouseleave", hide);
    document.addEventListener("focusout", hide);
    document.addEventListener("click", hide, true);
    document.addEventListener("scroll", hide, true);
    window.addEventListener("blur", hide);
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") hide();
    };
    document.addEventListener("keydown", esc);
    onCleanup(() => {
      document.removeEventListener("mouseover", over);
      document.removeEventListener("focusin", over);
      document.removeEventListener("mouseleave", hide);
      document.removeEventListener("focusout", hide);
      document.removeEventListener("click", hide, true);
      document.removeEventListener("scroll", hide, true);
      window.removeEventListener("blur", hide);
      document.removeEventListener("keydown", esc);
      if (timer) clearTimeout(timer);
    });
  });

  /// Placed after render, because where it fits depends on how big it turned out.
  createEffect(() => {
    const a = anchor();
    if (!a || !bubble) return;
    const w = bubble.offsetWidth;
    const h = bubble.offsetHeight;
    const left = Math.max(
      MARGIN,
      Math.min(
        a.rect.left + a.rect.width / 2 - w / 2,
        window.innerWidth - w - MARGIN,
      ),
    );
    // Above by default; below when there isn't room, so a tip on a top-row element is not
    // pinned half off the screen.
    const below = a.rect.top - h - OFFSET < MARGIN;
    const top = below ? a.rect.bottom + OFFSET : a.rect.top - h - OFFSET;
    setPos({ left, top, below });
    setReady(true);
  });

  return (
    <Show when={anchor()}>
      {(a) => (
        <div
          ref={bubble}
          class="tip"
          classList={{ "tip-below": pos().below, ready: ready() }}
          role="tooltip"
          style={{ left: `${pos().left}px`, top: `${pos().top}px` }}
        >
          {a().text}
        </div>
      )}
    </Show>
  );
}
