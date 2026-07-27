/// Shared vocabulary for the code index: the list panel and the per-repo view have to agree on
/// what a repo's state is called, or the same repo reads as two different things depending on
/// which screen you are looking at.

import type { RepoIndexProgress, RepoKind } from "./types";

/// A repo is "carded" once it has at least one component: that is the point at which scoring
/// can route an issue to it at all. Not the same as complete, and the distinction is the
/// whole reason the index panel exists.
export function carded(r: RepoIndexProgress): boolean {
  return r.components > 0;
}

/// Whether the index has anything at all to say about this repo.
///
/// Wider than `carded` on purpose, because of one case that is the most informative row on the
/// panel: a repo with **inbound dependency edges and no components**. The structural pass will
/// raise it as a candidate — "you depend on this and nothing in the issue mentions it" — and
/// then there is nothing inside it to look at. Filtering on `carded` alone hid exactly that.
export function present(r: RepoIndexProgress): boolean {
  return (
    r.components > 0 ||
    r.commits_cached > 0 ||
    r.depends_on > 0 ||
    r.depended_on_by > 0
  );
}

/// Where a repo has got to, in the order the indexer does the work.
///
/// Deliberately not a percentage. The denominators arrive as the walk proceeds — a repo whose
/// history hasn't been fetched has 0 of 0 commits, which arithmetic reads as 100% — so a bar
/// would show a fully-indexed repo and a completely untouched one identically.
export function phase(r: RepoIndexProgress): { label: string; cls: string } {
  if (r.archived) return { label: "ARCHIVED", cls: "ph-idle" };
  if (!carded(r)) {
    // The graph points here and there is nothing inside to look at. Flagged rather than
    // called "not started", because for scoring this is a dead end being offered as a lead.
    return r.depended_on_by > 0
      ? { label: "TARGET, UNINDEXED", cls: "ph-gap" }
      : { label: "NOT STARTED", cls: "ph-idle" };
  }
  if (r.history_back_to === null) return { label: "CARDING", cls: "ph-work" };
  if (r.commits_cached === 0) return { label: "NO HISTORY", cls: "ph-idle" };
  if (r.commits_summarized < r.commits_cached)
    return { label: "SUMMARIZING", cls: "ph-work" };
  return { label: "INDEXED", cls: "ph-done" };
}

/// Groups, most-consequential first: an issue about production code can page you, one about a
/// demo rarely can, and docs have no runtime behaviour to break at all.
export const KIND_ORDER: RepoKind[] = ["code", "example", "docs"];

export const KIND_LABEL: Record<RepoKind, string> = {
  code: "CODE",
  example: "EXAMPLES & DEMOS",
  docs: "DOCUMENTATION",
};

/// Sort order for the phase labels: the work still to do first, the finished last.
///
/// Explicit rather than alphabetical, because sorting by status is really asking "what needs
/// attention" — and alphabetically that puts ARCHIVED above SUMMARIZING.
export const PHASE_ORDER: Record<string, number> = {
  "TARGET, UNINDEXED": 0,
  "NOT STARTED": 1,
  CARDING: 2,
  "NO HISTORY": 3,
  SUMMARIZING: 4,
  INDEXED: 5,
  ARCHIVED: 6,
};

export function pct(done: number, total: number): number {
  if (total <= 0) return 0;
  return Math.min(100, Math.round((done / total) * 100));
}

export function short(sha: string): string {
  return sha.slice(0, 8);
}

export function day(ts: string | null): string {
  if (!ts) return "—";
  return ts.slice(0, 10);
}

/// The kind a repo is treated as. An untagged repo is code, because assuming something
/// matters is the safe error — a demo mis-filed as code costs attention, and code mis-filed as
/// a demo costs an incident.
export function kindOf(r: RepoIndexProgress): RepoKind {
  return r.kind ?? "code";
}
