import { For, Show, createSignal, onCleanup } from "solid-js";
import { dispatchesFor } from "../state";
import type { Dispatch, DispatchState } from "../types";

/// What the AI is doing for one subject, right now.
///
/// Every expensive pass is submitted and returns immediately, so pressing INVESTIGATE or
/// 2ND OPINION used to do nothing visible: the work was queued behind a concurrency
/// limit, or refused as a duplicate because the same key already ran, or it failed inside
/// the handler — and all three looked like a button that flashed.
///
/// So this shows the state machine rather than a spinner. A spinner would only have
/// covered the case that was already working, and the two it missed are the ones worth
/// seeing: nothing-needed-to-happen, and it-broke-and-here-is-why.
export default function DispatchStrip(props: { subjectKey: string }) {
  const rows = () => dispatchesFor(props.subjectKey);
  const active = () =>
    rows().filter((d) => d.state === "queued" || d.state === "running");
  /// Recent finished rows, so a failure or a duplicate stays readable after the fact —
  /// which is the whole point. Bounded because this is a status strip, not a log.
  const recent = () =>
    rows()
      .filter((d) => !isActive(d))
      .slice(0, 4);

  return (
    <Show when={rows().length}>
      <div class="dispatch-strip">
        <For each={[...active(), ...recent()]}>
          {(d) => <Row dispatch={d} />}
        </For>
      </div>
    </Show>
  );
}

function isActive(d: Dispatch) {
  return d.state === "queued" || d.state === "running";
}

function Row(props: { dispatch: Dispatch }) {
  const d = () => props.dispatch;
  return (
    <div class={`dispatch dispatch-${d().state}`} title={d().detail ?? ""}>
      <span class={`dispatch-state ds-${d().state}`}>{label(d().state)}</span>
      <span class="dispatch-kind">{pretty(d().kind)}</span>
      <Show when={isActive(d())} fallback={<Took dispatch={d()} />}>
        <Elapsed since={d().started_at} />
      </Show>
      <Show when={d().detail && d().state !== "queued"}>
        <span
          class={d().state === "failed" ? "dispatch-error" : "muted"}
          /* The message, not just the fact of a failure: "no such repo restatedev/foo"
             is actionable and "failed" is not. */
        >
          {d().detail}
        </span>
      </Show>
    </div>
  );
}

function label(s: DispatchState): string {
  switch (s) {
    case "queued":
      return "QUEUED";
    case "running":
      return "RUNNING";
    case "done":
      return "DONE";
    case "duplicate":
      return "ALREADY DONE";
    case "failed":
      return "FAILED";
  }
}

/// `RootCause` → `ROOT CAUSE`. The backend keeps the workflow's real name so a log line
/// and a strip row can be matched up; the split happens here.
function pretty(kind: string): string {
  return kind.replace(/([a-z])([A-Z])/g, "$1 $2").toUpperCase();
}

/// A live-ticking age for in-flight work.
///
/// Ticking, not static, because "queued 4s ago" and "queued 6 minutes ago" mean different
/// things — the first is normal and the second says the queue is stuck. A timestamp the
/// reader has to subtract from the clock themselves does not carry that.
function Elapsed(props: { since: string }) {
  const [now, setNow] = createSignal(Date.now());
  const timer = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(timer));
  const secs = () =>
    Math.max(0, Math.round((now() - new Date(props.since).getTime()) / 1000));
  return <span class="dispatch-age">{duration(secs())}</span>;
}

function Took(props: { dispatch: Dispatch }) {
  const secs = () => {
    const { started_at, finished_at } = props.dispatch;
    if (!finished_at) return null;
    return Math.max(
      0,
      Math.round(
        (new Date(finished_at).getTime() - new Date(started_at).getTime()) /
          1000,
      ),
    );
  };
  return (
    <Show when={secs() !== null}>
      <span class="dispatch-age">{duration(secs()!)}</span>
    </Show>
  );
}

function duration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ${secs % 60}s`;
  return `${Math.floor(mins / 60)}h ${mins % 60}m`;
}
