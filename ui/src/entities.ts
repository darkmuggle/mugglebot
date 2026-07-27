// Deep links for entity chips. GitHub-derived entities (repo/pr/issue/…) point
// at their page on github.com; a bare Slack channel or meeting isn't browsable.
import type { ResolutionKey } from "./types";

// "owner/name#123" → ["owner/name", "123"]; undefined if not that shape.
function repoAndNumber(value: string): [string, string] | undefined {
  const m = value.match(/^([^/\s]+\/[^/\s#]+)#(\d+)$/);
  return m ? [m[1], m[2]] : undefined;
}

// GitHub username rule (alphanumeric + single internal hyphens, ≤39 chars).
const GH_LOGIN = /^[a-z\d](?:[a-z\d]|-(?=[a-z\d])){0,38}$/i;

/** A browser URL for an entity chip, or undefined when it isn't linkable. */
export function entityHref(e: ResolutionKey): string | undefined {
  const v = e.value.trim();
  switch (e.kind) {
    case "repo":
      return /^[^/\s]+\/[^/\s]+$/.test(v) ? `https://github.com/${v}` : undefined;
    case "pr": {
      const rn = repoAndNumber(v);
      return rn ? `https://github.com/${rn[0]}/pull/${rn[1]}` : undefined;
    }
    case "issue": {
      const rn = repoAndNumber(v);
      return rn ? `https://github.com/${rn[0]}/issues/${rn[1]}` : undefined;
    }
    case "discussion": {
      const rn = repoAndNumber(v);
      return rn ? `https://github.com/${rn[0]}/discussions/${rn[1]}` : undefined;
    }
    case "commit": {
      const m = v.match(/^([^/\s]+\/[^/\s@]+)@([0-9a-f]+)$/i);
      return m ? `https://github.com/${m[1]}/commit/${m[2]}` : undefined;
    }
    case "branch": {
      // "owner/name@branch" — the branch may itself contain slashes.
      const at = v.indexOf("@");
      if (at <= 0 || at === v.length - 1) return undefined;
      return `https://github.com/${v.slice(0, at)}/tree/${v.slice(at + 1)}`;
    }
    case "person":
      // Best-effort GitHub profile (github signals attribute by login).
      return GH_LOGIN.test(v) ? `https://github.com/${v}` : undefined;
    default:
      return undefined;
  }
}
