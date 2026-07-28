import { Show } from "solid-js";
import DiffPane from "./DiffPane";
import type { PrFix } from "../types";

/// A pull request's subject key, from the critique that names it. The critique stores
/// the PR as `repo` + `number`; the subject form uses `!` so an issue and a PR with the
/// same number stay distinct.
export function prKey(pr: PrFix): string {
  return `${pr.pr_repo}!${pr.pr_number}`;
}

/// One attempt at the issue: what it does, what MuggleBot makes of it, and what
/// reviewers said — the last of which is deliberately given its own line, because a
/// human who read the change and objected outranks a model's reading of the same diff.
///
/// None of this is ever posted to GitHub. It is a note in MuggleBot's own store,
/// rendered here and nowhere else.
/// Whether this attempt's diff is worth unfolding without being asked.
///
/// An outstanding or merged PR is the meat of the fix — the reason you opened the subject —
/// so hiding it behind a disclosure triangle hides the answer. A PR closed *without* merging
/// is a dead end: worth listing, not worth the screen.
///
/// An unknown state counts as outstanding. Erring towards showing the change is the right
/// error: the cost is scrolling, and the cost the other way is missing what happened.
function worthUnfolding(pr: PrFix): boolean {
  return pr.pr_state !== "closed";
}

export default function Attempt(props: {
  pr: PrFix;
  onExplain: () => void;
  /// Unfold the diff — patches and all — rather than a file list to click through. Set in the
  /// click-in view, where screen space is the point; left off on the board, where a card is a
  /// list row.
  expand?: boolean;
}) {
  const pr = () => props.pr;
  return (
    <div class={`attempt verdict-${pr().verdict}`}>
      <div class="attempt-head">
        <span class={`verdict verdict-${pr().verdict}`}>
          {pr().verdict.toUpperCase()}
        </span>
        <span class="attempt-conf">{Math.round(pr().confidence * 100)}%</span>
        <Show
          when={pr().pr_url}
          fallback={<span class="attempt-ref">{prKey(pr())}</span>}
        >
          <a
            class="attempt-ref"
            href={pr().pr_url!}
            target="_blank"
            rel="noreferrer"
            onClick={(e) => e.stopPropagation()}
          >
            {pr().pr_repo}#{pr().pr_number} ↗
          </a>
        </Show>
        <span class="attempt-title">{pr().pr_title}</span>
        <Show when={pr().pr_author}>
          <span class="attempt-author">{pr().pr_author}</span>
        </Show>
        <Show when={pr().pr_state}>
          <span class="chip">{pr().pr_state}</span>
        </Show>
        <button
          class="explain-btn"
          data-tip="Explain just this pull request"
          onClick={props.onExplain}
        >
          EXPLAIN
        </button>
      </div>
      <Show when={pr().implementation}>
        <div class="attempt-row">
          <span class="attempt-key">IMPLEMENTS</span>
          <span>{pr().implementation}</span>
        </div>
      </Show>
      {/* The diff, per attempt. Keyed to this PR rather than the issue so one click reads one
          change — an issue with five attempts would otherwise fetch five diffs to show one. */}
      <DiffPane
        subjectKey={`${pr().pr_repo}!${pr().pr_number}`}
        expand={props.expand && worthUnfolding(pr())}
      />
      <Show when={pr().critique}>
        <div class="attempt-row">
          <span class="attempt-key">CRITIQUE</span>
          <span>{pr().critique}</span>
        </div>
      </Show>
      <Show when={pr().conversation}>
        <div class="attempt-row attempt-conversation">
          <span class="attempt-key">REVIEWERS</span>
          <span>{pr().conversation}</span>
        </div>
      </Show>
      <Show when={pr().also_fixes.length}>
        <div class="attempt-row">
          <span class="attempt-key">ALSO</span>
          <span>{pr().also_fixes.join(" · ")}</span>
        </div>
      </Show>
      <Show when={pr().analyzed_by}>
        <div class="attempt-foot">
          judged by {pr().analyzed_by} · never posted to GitHub
        </div>
      </Show>
    </div>
  );
}
