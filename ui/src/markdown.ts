// Markdown → HTML for LLM output (chat replies, drafted postmortems, thread
// summaries), backed by markdown-it.
//
// Safety: `html: false` means raw HTML in the (model-authored, possibly
// signal-echoing) source is escaped, never emitted — so nothing in the text can
// inject live markup. markdown-it's default link validator also blocks dangerous
// hrefs (javascript:/vbscript:/data:), so the string is safe to pass to innerHTML.

import MarkdownIt from "markdown-it";

// Open links in a new tab. Hrefs are already validated by markdown-it, so this
// only adds target/rel. Applied to any instance we build.
function openLinksInNewTab(inst: MarkdownIt): MarkdownIt {
  const defaultLinkOpen =
    inst.renderer.rules.link_open ??
    ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
  inst.renderer.rules.link_open = (tokens, idx, options, env, self) => {
    tokens[idx].attrSet("target", "_blank");
    tokens[idx].attrSet("rel", "noreferrer");
    return defaultLinkOpen(tokens, idx, options, env, self);
  };
  return inst;
}

const md = openLinksInNewTab(
  new MarkdownIt({
    html: false, // escape raw HTML in the source rather than emitting it
    linkify: true, // turn bare URLs into links
    breaks: false,
  }),
);

// Same safety posture, but `breaks: true` so single newlines become <br> —
// Slack messages use bare newlines as hard line breaks (multi-line alerts,
// field lists), which the LLM-output renderer above deliberately collapses.
const mdBreaks = openLinksInNewTab(
  new MarkdownIt({ html: false, linkify: true, breaks: true }),
);

/** Render a Markdown string to a safe HTML string. */
export function renderMarkdown(src: string): string {
  return md.render(src ?? "");
}

/** Render a raw source message (e.g. a Slack body) preserving line breaks. */
export function renderMessage(src: string): string {
  return mdBreaks.render(src ?? "");
}

/** Heuristic: does this text carry Markdown structure worth rendering? */
export function looksLikeMarkdown(s: string): boolean {
  return /(^|\n)\s{0,3}#{1,6}\s|(^|\n)\s*[-*+]\s+|(^|\n)\s*\d+[.)]\s+|```|`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\([^)]+\)|(^|\n)\s*>\s/.test(
    s,
  );
}
