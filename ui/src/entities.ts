// Deep links for entity chips. GitHub-derived entities (repo/pr/issue/…) point
// at their page on github.com; a bare Slack channel or meeting isn't browsable.
import type { CloudEntity, ResolutionKey } from "./types";

// "owner/name#123" → ["owner/name", "123"]; undefined if not that shape.
function repoAndNumber(value: string): [string, string] | undefined {
  const m = value.match(/^([^/\s]+\/[^/\s#]+)#(\d+)$/);
  return m ? [m[1], m[2]] : undefined;
}

// GitHub username rule (alphanumeric + single internal hyphens, ≤39 chars).
const GH_LOGIN = /^[a-z\d](?:[a-z\d]|-(?=[a-z\d])){0,38}$/i;

/// A Restate Cloud id in either spelling: `env_201k…` as the control plane writes it, or
/// `env-201k…` as Kubernetes — and therefore every alert label — has to. Alert-derived
/// entity chips are almost always the hyphenated form.
const CLOUD_ID = /^(acc|env|org|grp|rgn|usr|key)[-_]([A-Za-z\d]{10,})$/;

/// The canonical, underscore-separated id, or undefined when this is not a platform id.
/// Matches `cloud::scan`'s normalization so a chip links to the same entity the context box
/// and the Cloud page hold.
export function canonicalCloudId(value: string): string | undefined {
  const m = value.trim().match(CLOUD_ID);
  return m ? `${m[1]}_${m[2]}` : undefined;
}

/// Ancestors worth naming in a hover, in the order they answer "whose is this, and where does
/// it run". The environment's own fields come first; these follow.
const TIP_ANCESTORS = ["account", "organization", "region", "nuon-install"];

/// A hover summary for a Restate Cloud entity chip: what it is, and who it belongs to.
///
/// An alert chip reading `environment:env-201kya2b2kmkxykq38sqb5gsfpm` is an opaque token in
/// the one place it matters most — the summary of a firing. The answer is already on the
/// subject (`SubjectView.cloud` carries the whole resolved closure), so this is a lookup
/// rather than a fetch: hovering costs nothing and needs no round trip.
///
/// Returns undefined when the chip is not a platform id or the estate has not been warmed for
/// it, which leaves the chip with no tooltip rather than an empty one.
export function cloudTip(
  value: string,
  cloud: CloudEntity[] | undefined,
): string | undefined {
  const id = canonicalCloudId(value);
  if (!id || !cloud?.length) return undefined;
  const byId = new Map(cloud.map((e) => [e.id, e]));
  const entity = byId.get(id);
  if (!entity) return undefined;

  const head = [entity.kind, entity.name ?? entity.id, entity.status]
    .filter(Boolean)
    .join(" · ");
  const lines = [head, ...entity.fields.map((f) => `${f.label}: ${f.value}`)];

  // Walk up. The edges only ever point at parents, so this terminates without a seen-set;
  // one is kept anyway because a cache written by an older build need not be well-formed.
  const seen = new Set([entity.id]);
  const queue = [...entity.related];
  const found = new Map<string, CloudEntity>();
  while (queue.length) {
    const next = queue.shift()!;
    if (seen.has(next)) continue;
    seen.add(next);
    const e = byId.get(next);
    if (!e) continue;
    if (!found.has(e.kind)) found.set(e.kind, e);
    queue.push(...e.related);
  }
  for (const kind of TIP_ANCESTORS) {
    const e = found.get(kind);
    if (e) lines.push(`${kind}: ${e.name ?? e.id}`);
  }
  return lines.join("\n");
}

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
    // Alert labels name the environment, account or organization the firing is about. Those
    // chips used to be the only dead ones on the panel — and they are the ones an operator
    // most wants to follow, because "whose environment is this" is the first question a
    // cloud alert raises. An in-app hash rather than an external URL: the answer is the
    // Cloud page, which already holds the whole estate.
    case "environment":
    case "account":
    case "organization": {
      const id = canonicalCloudId(v);
      return id ? `#/cloud/${id}` : undefined;
    }
    default:
      return undefined;
  }
}
