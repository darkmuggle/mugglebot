//! `Explain` — distil a subject *and everything under it* into something readable.
//!
//! Keyed `{subject}@{watermark}`. Nothing new has arrived → same key → the previous
//! explanation comes back with no model call.
//!
//! The point is the **nesting**. Clicking a pull request should explain that PR: what
//! it changes, what MuggleBot thinks of it, what reviewers said. Clicking the issue
//! above it should explain the whole situation: what the problem is, which PRs are
//! attempting it and how each is doing, what the root cause looks like, what the
//! triage proposed, and what conversations are attached. Those are the same operation
//! at two levels of the hierarchy, which is why one workflow does both — it gathers
//! from the subject's rank downwards.
//!
//! Assembly is deliberately deterministic: the context is built by reading the store,
//! and the model's only job is to write it up. A model asked to *find* the context
//! would invent some, and an explanation that cites a PR that doesn't exist reads as
//! authoritative and sends you hunting.

use std::sync::Arc;

use restate_sdk::prelude::*;

use super::{split_versioned, WorkflowOps};
use crate::restate::scopes;

pub struct Explain {
    ops: Arc<WorkflowOps>,
}

impl Explain {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }

    /// Explaining reads a lot of stored text and writes prose over it — the routed
    /// tier's job, not the local coder's.
    pub const SCOPE: &'static str = scopes::CLOUD_LLM;
}

#[restate_sdk::workflow]
impl Explain {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("Explain", &key, self.explain(ctx)).await
    }
}

impl Explain {
    async fn explain(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let (subject, watermark) = split_versioned(ctx.key());
        let (subject, watermark) = (subject.to_string(), watermark.to_string());

        // Step 1: gather. Pure store reads, so a retry after a model failure doesn't
        // re-walk the hierarchy.
        let ops = self.ops.clone();
        let subject_for_gather = subject.clone();
        let gathered = ctx
            .run(|| {
                let ops = ops.clone();
                let subject = subject_for_gather.clone();
                async move {
                    let g =
                        gather(&ops, &subject).map_err(|e| TerminalError::new(format!("{e:#}")))?;
                    Ok(Json(g))
                }
            })
            .await?
            .into_inner();

        // Step 2: write it up. Separate step, so a rate limit here doesn't repeat
        // step 1 — and the *cost* of this pass is the whole reason the key versions on
        // the watermark.
        let ops = self.ops.clone();
        let subject_for_write = subject.clone();
        let watermark_for_write = watermark.clone();
        let gathered_for_write = gathered.clone();
        let written = ctx
            .run(|| {
                let ops = ops.clone();
                let subject = subject_for_write.clone();
                let watermark = watermark_for_write.clone();
                let gathered = gathered_for_write.clone();
                async move {
                    let out = write_up(&ops, &subject, &watermark, gathered)
                        .await
                        .map_err(|e| HandlerError::from(anyhow::anyhow!("{e:#}")))?;
                    Ok(Json(out))
                }
            })
            .await?
            .into_inner();

        Ok(Json(written))
    }
}

/// `SecondOpinion` — the same dossier, written by a cloud model, because the operator asked.
///
/// This is the **only** workflow that reaches a cloud model, and it only ever runs from a
/// button press. Everything else MuggleBot does runs on the local model, so the question
/// "did this cost money?" has one answer: only if someone clicked.
///
/// It is deliberately the *same* gather step as [`Explain`] — same dossier, same rules, same
/// verification — so the two answers differ by model and nothing else. A second opinion that
/// also changed the evidence would tell you nothing about the first one.
///
/// Keyed `{subject}@{watermark}` like `Explain`, so pressing the button twice on unchanged
/// work is a key collision that returns the answer already paid for.
pub struct SecondOpinion {
    ops: Arc<WorkflowOps>,
}

impl SecondOpinion {
    pub fn new(ops: Arc<WorkflowOps>) -> Self {
        Self { ops }
    }

    /// The cloud queue: this is the one path that lands in it.
    pub const SCOPE: &'static str = scopes::CLOUD_LLM;
}

#[restate_sdk::workflow]
impl SecondOpinion {
    /// Reports itself to the dispatch strip: the submitting call returned as soon as the
    /// ingress accepted this, so `Running` / `Done` / `Failed` can only come from here.
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<serde_json::Value>> {
        let key = ctx.key().to_string();
        super::tracked("SecondOpinion", &key, self.second_opinion(ctx)).await
    }
}

impl SecondOpinion {
    async fn second_opinion(
        &self,
        ctx: WorkflowContext<'_>,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let (subject, watermark) = split_versioned(ctx.key());
        let (subject, watermark) = (subject.to_string(), watermark.to_string());

        let ops = self.ops.clone();
        let subject_for_gather = subject.clone();
        let gathered = ctx
            .run(|| {
                let ops = ops.clone();
                let subject = subject_for_gather.clone();
                async move {
                    let g =
                        gather(&ops, &subject).map_err(|e| TerminalError::new(format!("{e:#}")))?;
                    Ok(Json(g))
                }
            })
            .await?
            .into_inner();

        let ops = self.ops.clone();
        let subject_for_write = subject.clone();
        let watermark_for_write = watermark.clone();
        let written = ctx
            .run(|| {
                let ops = ops.clone();
                let subject = subject_for_write.clone();
                let watermark = watermark_for_write.clone();
                let gathered = gathered.clone();
                async move {
                    let out = write_with(
                        &ops,
                        ops.cloud.as_ref(),
                        crate::store::EXPLAIN_CLOUD,
                        &subject,
                        &watermark,
                        gathered,
                    )
                    .await
                    .map_err(|e| HandlerError::from(anyhow::anyhow!("{e:#}")))?;
                    Ok(Json(out))
                }
            })
            .await?
            .into_inner();

        Ok(Json(written))
    }
}

/// Assemble everything known about a subject **and everything under it**.
///
/// Every field is a store read. The model that writes the explanation is handed this
/// and told to add nothing — an explanation citing a PR that doesn't exist reads as
/// authoritative and sends the operator hunting.
pub fn gather(ops: &WorkflowOps, subject_key: &str) -> anyhow::Result<GatheredContext> {
    let board = ops.attributor.as_ref();
    let Some(view) = board.subject_view(subject_key)? else {
        anyhow::bail!("no subject {subject_key}");
    };
    let mut g = GatheredContext {
        subject: subject_key.to_string(),
        rank: view.subject.rank.as_str().to_string(),
        title: view.subject.title.clone(),
        summary: view.subject.summary.clone(),
        tags: view.subject.tags.clone(),
        handled: view.subject.handled.as_str().to_string(),
        severity: format!("{:?}", view.severity).to_lowercase(),
        ..Default::default()
    };

    // The subject's own activity, oldest first so the write-up reads chronologically.
    let mut signals = view.signals.clone();
    signals.sort_by_key(|s| s.occurred_at);
    g.events = signals
        .iter()
        .map(|s| EventLine {
            id: s.id.clone(),
            when: s.occurred_at.to_rfc3339(),
            source: s.source.as_str().to_string(),
            kind: format!("{:?}", s.kind),
            title: s.title.clone(),
            body: s.body.as_deref().map(|b| truncate(b, 900)),
        })
        .collect();

    g.attached_context = view
        .context
        .iter()
        .map(|c| {
            c.summary
                .clone()
                .unwrap_or_else(|| truncate(&c.content, 400))
        })
        .collect();
    g.related = view
        .edges
        .iter()
        .map(|e| {
            let other = if e.subject_a == subject_key {
                &e.subject_b
            } else {
                &e.subject_a
            };
            format!(
                "{other} ({}, {:.0}%): {}",
                e.kind.as_str(),
                e.confidence * 100.0,
                e.rationale
            )
        })
        .collect();

    if let Some(rc) = ops.store.get_root_cause(subject_key)? {
        g.root_cause = Some(RootCauseLine {
            status: rc.status.clone(),
            verdict: rc.verdict.clone(),
            candidates: rc
                .candidates
                .as_array()
                .map(|a| a.iter().filter_map(candidate_line).collect())
                .unwrap_or_default(),
        });
    }
    if let Some(t) = ops.store.issue_triage_for_subject(subject_key)?.first() {
        g.triage = Some(TriageLine {
            status: t.status.clone(),
            characterization: t.characterization.clone(),
            plain_summary: t.plain_summary.clone(),
            approaches: t
                .patches
                .as_array()
                .map(|a| a.iter().filter_map(approach_line).collect())
                .unwrap_or_default(),
        });
    }

    // ---- the sub-contexts: the attempts at this issue ------------------------
    //
    // This is what makes explaining an *issue* different from explaining a PR: the
    // issue's explanation has to account for every attempt at it, including what
    // reviewers said about each one.
    for fix in ops.store.pr_fixes_for_issue(subject_key)? {
        g.pull_requests.push(PullRequestLine {
            reference: fix.reference(),
            url: fix.pr_url.clone(),
            title: fix.pr_title.clone(),
            author: fix.pr_author.clone(),
            state: fix.pr_state.clone(),
            verdict: fix.verdict.clone(),
            confidence: fix.confidence,
            implementation: fix.implementation.clone(),
            critique: fix.critique.clone(),
            conversation: fix.conversation.clone(),
            also_fixes: fix.also_fixes.clone(),
            analyzed_by: fix.analyzed_by.clone(),
        });
    }

    // A child PR that got a card of its own contributes its activity too — otherwise
    // explaining the issue would miss the CI failures that only ever named the PR.
    for child in &view.children {
        if let Ok(Some(cv)) = board.subject_view(child.as_str()) {
            g.child_subjects.push(RelatedSubject {
                key: child.to_string(),
                title: cv.subject.title.clone(),
                summary: cv.subject.summary.clone(),
                event_count: cv.signals.len(),
                severity: format!("{:?}", cv.severity).to_lowercase(),
            });
        }
    }

    // A PR explained on its own still wants the problem it is attempting.
    if let Some(parent) = &view.subject.parent {
        if let Ok(Some(pv)) = board.subject_view(parent.as_str()) {
            g.parent = Some(RelatedSubject {
                key: parent.to_string(),
                title: pv.subject.title.clone(),
                summary: pv.subject.summary.clone(),
                event_count: pv.signals.len(),
                severity: format!("{:?}", pv.severity).to_lowercase(),
            });
        }
    }

    for inv in ops.store.browser_investigations_for_subject(subject_key)? {
        if let Some(f) = inv.findings.filter(|f| !f.trim().is_empty()) {
            g.dashboard_readings.push(truncate(&f, 900));
        }
    }
    Ok(g)
}

/// Write the dossier up and store it.
async fn write_up(
    ops: &WorkflowOps,
    subject_key: &str,
    watermark: &str,
    g: GatheredContext,
) -> anyhow::Result<serde_json::Value> {
    write_with(
        ops,
        ops.explainer.as_ref(),
        crate::store::EXPLAIN_LOCAL,
        subject_key,
        watermark,
        g,
    )
    .await
}

/// Write the explanation with a given model, label it with who wrote it, and verify it
/// against the dossier before storing.
///
/// One function for both workflows on purpose: the prompt, the section list built from the
/// dossier, and the verification are the *contract*, not an implementation detail of the
/// local path. A cloud model gets no license to invent a link either.
async fn write_with(
    ops: &WorkflowOps,
    model: &dyn crate::reasoner::Reasoner,
    produced_by: &str,
    subject_key: &str,
    watermark: &str,
    g: GatheredContext,
) -> anyhow::Result<serde_json::Value> {
    let sources = g.sources();
    let sections = sections_for(subject_key, &g);
    // Note: unlike the board summary, this prompt does *not* ask for links to the
    // comments it cites. This dossier carries the **distilled** conversation, which has
    // no URLs in it, so any link would be an invented one and `verify` would strip it
    // straight back out — asking for one just manufactures removals.
    let system = format!(
        "You are explaining one piece of engineering work to the engineer who owns it, from a \
         dossier that has already been assembled for you. Write Markdown, no preamble.\n\
         Write EXACTLY these sections, in this order, and no others:\n{}\n\
         Rules:\n\
         - Use ONLY what the dossier contains. If it isn't in the dossier, it does not exist: \
         no invented pull request, file, person, number, or link. Write a link only if the \
         dossier gives you its URL.\n\
         - Never claim a reviewer said anything unless the dossier quotes them. Where it says \
         a PR has no review discussion, say nobody has reviewed it.\n\
         - A verdict's confidence is MuggleBot's confidence in its own judgment. It is NOT how \
         much of the problem the PR solves.\n\
         - A proposed cause is a hypothesis: write \"likely\", never \"caused by\".\n\
         - Where a reviewer objected, lead with the objection. A human who read the change and \
         pushed back is better evidence than any model's reading of the same diff.\n\
         - Every position anyone takes in a conversation gets an answer. Name the person, say what \
         they want in a few words, and then make the call in the imperative: go with their approach, \
         push back and why, fix what they flagged, or answer their question. Do not paste or quote \
         the discussion — reporting that people talked, without deciding anything, is the failure \
         this rule exists to prevent. If the dossier does not settle a disagreement, say which way \
         to lean and the one fact that would settle it. A blocking reviewer outranks the rest of the \
         thread; a maintainer's decision outranks a suggestion.\n\
         - If the dossier is thin, say so in one line rather than padding it.\n\
         - Do not echo these instructions or the dossier's section headings.",
        sections
            .iter()
            .map(|s| format!("  - {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut req = crate::reasoner::CompletionRequest::single(g.render());
    req.system = Some(system);
    req.max_tokens = 1400;
    // Explaining is exactly the case AGENTS.md reserves the top tier for: a fluent,
    // plausible, wrong explanation is worse than no explanation, because the operator
    // acts on it. So it bypasses the difficulty grader — which graded this "medium"
    // and kept it on the local coder model, where it invented reviewer approval.
    let raw = model.complete(&req).await?.trim().to_string();
    if raw.is_empty() {
        anyhow::bail!("the reasoner returned an empty explanation");
    }
    // The prompt asks for no fabrication; this enforces it. See [`verify`].
    let (markdown, removed) = verify(&raw, &g);
    if markdown.is_empty() {
        anyhow::bail!("nothing survived verification against the dossier");
    }
    if !removed.is_empty() {
        tracing::info!(
            "explain {subject_key} ({produced_by}): {}",
            removed.join("; ")
        );
    }
    ops.store.put_explanation(
        subject_key,
        watermark,
        &markdown,
        produced_by,
        &sources,
        &removed,
    )?;
    Ok(serde_json::json!({
        "subject_key": subject_key,
        "watermark": watermark,
        "markdown": markdown,
        "sources": sources,
        "produced_by": produced_by,
        "unsupported_removed": removed,
    }))
}

/// Strip from an explanation the things the dossier cannot support.
///
/// The local model is the only one that writes explanations now, and on its first live run
/// a 33B model produced four fabrications in one page: a markdown link to
/// `link_to_pr`, a claim that "reviewers said this approach is effective" about a PR with no
/// reviews, a reading of a verdict's confidence as "only fixes 90% of it", and an "Attempts"
/// section on a dossier with no attempts. The prompt now forbids all four. This is the check
/// that the prompt worked, because a prompt is a request and this is a guarantee.
///
/// Deterministic on purpose — no second model call. Each removal is reported so the panel can
/// say what was taken out rather than quietly showing a shorter answer.
///
/// It only ever *removes*. A verifier that rewrote prose would introduce the very thing it
/// exists to catch.
pub fn verify(markdown: &str, g: &GatheredContext) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    let known_urls = g.known_urls();
    let reviewed = g.any_review_discussion();

    // 1. Links the dossier never gave. Keep the text, drop the destination: the sentence is
    //    usually still true, and a link to `link_to_pr` is the part that wastes a click.
    let (mut out, stripped) = strip_unknown_links(markdown, &known_urls);
    if stripped > 0 {
        notes.push(format!(
            "{stripped} link{} removed (not in the dossier)",
            if stripped == 1 { "" } else { "s" }
        ));
    }

    // 2. Claims about reviewers when nothing reviewed anything. Sentence-level, because the
    //    surrounding paragraph is usually about the diff and is fine.
    if !reviewed {
        let (kept, dropped) = drop_sentences(&out, |sentence| {
            let l = sentence.to_ascii_lowercase();
            (l.contains("reviewer") || l.contains("reviewers")) && !is_negated(&l)
        });
        if dropped > 0 {
            out = kept;
            notes.push(format!(
                "{dropped} claim{} about reviewers removed (nothing here has been reviewed)",
                if dropped == 1 { "" } else { "s" }
            ));
        }
    }

    // 3. Sections the dossier has no material for. The prompt builds the section list from
    //    the dossier, so any heading outside it was invented.
    let (kept, dropped) = drop_unsupported_sections(&out, g);
    if !dropped.is_empty() {
        out = kept;
        notes.push(format!(
            "section{} removed with nothing behind {}: {}",
            if dropped.len() == 1 { "" } else { "s" },
            if dropped.len() == 1 { "it" } else { "them" },
            dropped.join(", ")
        ));
    }

    (out.trim().to_string(), notes)
}

/// Whether a sentence negates what it mentions.
///
/// Used to tell "reviewers approved this" from "reviewers have not weighed in" — the second
/// is the *correct* thing to say about an unreviewed PR and must survive.
///
/// This started as a list of the exact phrasings a model might use ("no review", "nobody has
/// reviewed", …) and that was the wrong shape: natural language has unbounded ways to negate,
/// so an allowlist silently deleted true sentences it hadn't anticipated — measured on a live
/// run, "Reviewers have not weighed in." A negation token anywhere in the sentence is coarser
/// and fails the right way. Keeping a suspicious sentence is recoverable, because it is
/// visible; deleting a true one is not, because the operator never learns what went.
fn is_negated(lower: &str) -> bool {
    const NEGATIONS: &[&str] = &[
        " no ",
        "no ",
        " not",
        "n't",
        " none",
        " nobody",
        " never",
        " without",
        " yet",
        " absent",
        " lacks",
        " lacking",
        " missing",
        " unreviewed",
    ];
    // Padded so `no` doesn't match inside `notice` and `not` doesn't match inside `nothing`
    // at a word boundary we didn't intend.
    let padded = format!(" {lower} ");
    NEGATIONS.iter().any(|n| padded.contains(n))
}

/// Rewrite `[text](url)` to `text` for any URL the dossier didn't supply. Returns the
/// count removed.
fn strip_unknown_links(markdown: &str, known: &[String]) -> (String, usize) {
    let mut out = String::with_capacity(markdown.len());
    let mut removed = 0usize;
    let bytes: Vec<char> = markdown.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        // A link starts at `[` that is not an image (`![`) and has `](` after the text.
        if bytes[i] == '[' && (i == 0 || bytes[i - 1] != '!') {
            if let Some((text, url, next)) = parse_link(&bytes, i) {
                let ok = known.iter().any(|k| k == &url);
                if ok {
                    out.push_str(&format!("[{text}]({url})"));
                } else {
                    // A citation marker like [sig:abc] is not a link and never reaches here,
                    // since it has no `](`.
                    out.push_str(&text);
                    removed += 1;
                }
                i = next;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    (out, removed)
}

/// Parse a markdown inline link starting at `open`. Returns (text, url, index after).
fn parse_link(chars: &[char], open: usize) -> Option<(String, String, usize)> {
    let close = (open + 1..chars.len()).find(|&i| chars[i] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&i| chars[i] == ')')?;
    let text: String = chars[open + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((text, url.trim().to_string(), end + 1))
}

/// Drop whole sentences matching `unsupported`, preserving everything else including
/// markdown structure. Returns the kept text and how many sentences went.
fn drop_sentences(markdown: &str, unsupported: impl Fn(&str) -> bool) -> (String, usize) {
    let mut dropped = 0usize;
    let mut out_lines: Vec<String> = Vec::new();
    for line in markdown.lines() {
        // Headings are not prose and are handled by the section check.
        if line.trim_start().starts_with('#') {
            out_lines.push(line.to_string());
            continue;
        }
        let mut kept = String::new();
        for sentence in split_sentences(line) {
            if unsupported(&sentence) {
                dropped += 1;
            } else {
                kept.push_str(&sentence);
            }
        }
        // A line that was *entirely* an unsupported claim disappears rather than becoming
        // a stray bullet marker.
        if kept.trim().is_empty() && !line.trim().is_empty() {
            continue;
        }
        out_lines.push(kept);
    }
    (out_lines.join("\n"), dropped)
}

/// Split on sentence ends, keeping the delimiter and any trailing space so reassembly is
/// lossless for the sentences that stay.
fn split_sentences(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?') {
            // Not a sentence end inside a number or an abbreviation like `e.g.`
            let next_is_space = chars.peek().is_none_or(|n| n.is_whitespace());
            if next_is_space {
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() && n != '\n' {
                        cur.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Remove a `**Heading**` block whose subject matter isn't in the dossier at all.
/// Is this subject itself a pull request?
///
/// On a PR page the diff and its review lead the page under "The change", so anything
/// that narrates the same PR a second time is a duplicate rather than context.
fn subject_is_pr(subject_key: &str) -> bool {
    crate::subject::SubjectKey::parse(subject_key)
        .is_ok_and(|k| k.rank() == crate::subject::SubjectRank::PullRequest)
}

/// The sections to ask for, built from what the dossier actually holds.
///
/// Naming every possible section up front reliably produced all of them — including an
/// "Attempts" section on a dossier with no attempts in it.
fn sections_for(subject_key: &str, g: &GatheredContext) -> Vec<String> {
    let mut sections = vec!["**Bottom line** — one line".to_string()];
    if !g.events.is_empty() {
        sections.push("**What happened** — from the events, citing [sig:ID]".into());
    }
    if g.root_cause.is_some() {
        sections.push("**Why** — the proposed causes, as hypotheses, citing [cause:REF]".into());
    }
    if !g.pull_requests.is_empty() && !subject_is_pr(subject_key) {
        sections.push(
            "**The attempts** — one short block per pull request: what it does, whether it \
             fixes this, and what reviewers said (or that nobody has reviewed it)"
                .into(),
        );
    }
    if g.triage.is_some() {
        sections.push("**Options** — the proposed approaches, with their risk".into());
    }
    sections.push("**What to do next** — one or two concrete moves".into());
    sections
}

fn drop_unsupported_sections(markdown: &str, g: &GatheredContext) -> (String, Vec<String>) {
    // Only headings whose material is *absent* are candidates. Anything not listed here is
    // left alone — the check is for invented sections, not a style rule.
    let forbidden: Vec<(&str, bool)> = vec![
        ("attempt", g.pull_requests.is_empty()),
        ("pull request", g.pull_requests.is_empty()),
        ("why", g.root_cause.is_none()),
        ("cause", g.root_cause.is_none()),
        ("option", g.triage.is_none()),
        ("what happened", g.events.is_empty()),
    ];
    let mut dropped = Vec::new();
    let mut out = Vec::new();
    let mut skipping: Option<String> = None;
    for line in markdown.lines() {
        if let Some(heading) = heading_text(line) {
            let lower = heading.to_ascii_lowercase();
            let unsupported = forbidden
                .iter()
                .any(|(needle, absent)| *absent && lower.contains(needle));
            if unsupported {
                dropped.push(heading.clone());
                skipping = Some(heading);
                continue;
            }
            skipping = None;
        }
        if skipping.is_none() {
            out.push(line.to_string());
        }
    }
    (out.join("\n"), dropped)
}

/// The heading text of a line, for either `## Heading` or a `**Heading**` lead-in.
fn heading_text(line: &str) -> Option<String> {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix('#') {
        return Some(rest.trim_start_matches('#').trim().to_string());
    }
    // Only list markers, not `*` — trimming asterisks here would eat the `**` this then
    // looks for, and every bold heading would read as ordinary prose.
    let stripped = t.trim_start_matches(['-', ' ']);
    let inner = stripped.strip_prefix("**")?;
    let end = inner.find("**")?;
    Some(inner[..end].trim().to_string())
}

fn candidate_line(c: &serde_json::Value) -> Option<String> {
    Some(format!(
        "{} [{}] {:.0}%: {}",
        c.get("reference")?.as_str()?,
        c.get("relation")?.as_str()?,
        c.get("confidence")?.as_f64()? * 100.0,
        c.get("rationale")?.as_str()?
    ))
}

fn approach_line(p: &serde_json::Value) -> Option<String> {
    Some(format!(
        "{} (risk {}, effort {}): {}",
        p.get("title")?.as_str()?,
        p.get("risk").and_then(|r| r.as_str()).unwrap_or("?"),
        p.get("effort").and_then(|r| r.as_str()).unwrap_or("?"),
        p.get("approach")?.as_str()?
    ))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Everything the explainer was given.
///
/// Journalled between the two steps, so a retry of the write-up uses exactly the
/// dossier the first attempt gathered rather than a newer one — an explanation that
/// half-describes two different states of the world is worse than a stale one.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GatheredContext {
    pub subject: String,
    pub rank: String,
    pub title: String,
    pub summary: Option<String>,
    pub tags: Vec<String>,
    pub handled: String,
    pub severity: String,
    pub events: Vec<EventLine>,
    pub attached_context: Vec<String>,
    pub related: Vec<String>,
    pub root_cause: Option<RootCauseLine>,
    pub triage: Option<TriageLine>,
    pub pull_requests: Vec<PullRequestLine>,
    pub child_subjects: Vec<RelatedSubject>,
    pub parent: Option<RelatedSubject>,
    pub dashboard_readings: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventLine {
    pub id: String,
    pub when: String,
    pub source: String,
    pub kind: String,
    pub title: String,
    pub body: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RootCauseLine {
    pub status: String,
    pub verdict: Option<String>,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TriageLine {
    pub status: String,
    pub characterization: Option<String>,
    pub plain_summary: Option<String>,
    pub approaches: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PullRequestLine {
    pub reference: String,
    pub url: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub state: Option<String>,
    pub verdict: String,
    pub confidence: f64,
    pub implementation: Option<String>,
    pub critique: Option<String>,
    pub conversation: Option<String>,
    pub also_fixes: Vec<String>,
    pub analyzed_by: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelatedSubject {
    pub key: String,
    pub title: String,
    pub summary: Option<String>,
    pub event_count: usize,
    pub severity: String,
}

impl GatheredContext {
    /// Which facets went into the explanation — the citation strip on the board, and
    /// the honest answer to "how much did it actually have to go on?".
    /// Every URL the dossier actually supplied. The allow-list for links in the write-up:
    /// a URL not in here was invented, whatever it looks like.
    pub fn known_urls(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .pull_requests
            .iter()
            .filter_map(|p| p.url.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Whether anything here carries review discussion. When nothing does, a sentence about
    /// what reviewers thought cannot be true.
    pub fn any_review_discussion(&self) -> bool {
        self.pull_requests.iter().any(|p| {
            p.conversation
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty())
        })
    }

    pub fn sources(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.events.is_empty() {
            out.push("events".into());
        }
        if self.summary.is_some() {
            out.push("summary".into());
        }
        if !self.pull_requests.is_empty() {
            out.push("pr_critiques".into());
        }
        if self.pull_requests.iter().any(|p| p.conversation.is_some()) {
            out.push("pr_conversations".into());
        }
        if self.root_cause.is_some() {
            out.push("root_cause".into());
        }
        if self.triage.is_some() {
            out.push("triage".into());
        }
        if !self.attached_context.is_empty() {
            out.push("attached_context".into());
        }
        if !self.dashboard_readings.is_empty() {
            out.push("dashboard".into());
        }
        if self.parent.is_some() {
            out.push("parent_issue".into());
        }
        if !self.child_subjects.is_empty() {
            out.push("child_subjects".into());
        }
        out
    }

    /// The dossier, as the prompt sees it. Sections the dossier can't support are
    /// omitted rather than emitted empty — an empty heading invites the model to fill
    /// it in.
    pub fn render(&self) -> String {
        let mut s = format!(
            "=== {} ({}) ===\n{}\nSeverity: {} · Triage: {}\n",
            self.subject, self.rank, self.title, self.severity, self.handled
        );
        if !self.tags.is_empty() {
            s.push_str(&format!("Tags: {}\n", self.tags.join(", ")));
        }
        if let Some(summary) = &self.summary {
            s.push_str(&format!("\nCurrent summary:\n{summary}\n"));
        }
        if let Some(p) = &self.parent {
            s.push_str(&format!(
                "\n=== THE ISSUE THIS PULL REQUEST IS AN ATTEMPT TO FIX ===\n\
                 ISSUE {} (an issue, not a pull request; nothing here says it was \
                 merged) — {}\n{}\n",
                p.key,
                p.title,
                p.summary.as_deref().unwrap_or("(no summary yet)")
            ));
        }
        if !self.events.is_empty() {
            s.push_str("\n=== WHAT HAPPENED (oldest first) ===\n");
            for e in &self.events {
                s.push_str(&format!(
                    "- [{}] {} · {} · {}: {}\n",
                    e.id, e.when, e.source, e.kind, e.title
                ));
                if let Some(body) = &e.body {
                    s.push_str(&format!("  {}\n", body.replace('\n', "\n  ")));
                }
            }
        }
        if !self.pull_requests.is_empty() {
            s.push_str("\n=== THE ATTEMPTS (pull requests) ===\n");
            for p in &self.pull_requests {
                s.push_str(&format!(
                    "- PULL REQUEST {} \"{}\" by {} (state: {})\n  link: {}\n",
                    p.reference,
                    p.title,
                    p.author.as_deref().unwrap_or("unknown"),
                    p.state.as_deref().unwrap_or("open"),
                    p.url.as_deref().unwrap_or("(none — do not write a link)")
                ));
                // Spelled out, because "verdict fixes at 90%" was read by a small model
                // as "fixes 90% of the problem" — which is a different claim, and a
                // wrong one.
                s.push_str(&format!(
                    "  MuggleBot's verdict: this PR {} the issue. Its confidence in \
                     that verdict: {:.0}% (judged by the {} tier).\n",
                    match p.verdict.as_str() {
                        "fixes" => "FIXES",
                        "partial" => "PARTIALLY addresses",
                        "related" => "is RELATED to but does not fix",
                        _ => "is UNRELATED to",
                    },
                    p.confidence * 100.0,
                    p.analyzed_by.as_deref().unwrap_or("local")
                ));
                if let Some(i) = &p.implementation {
                    s.push_str(&format!("  implements: {i}\n"));
                }
                if let Some(c) = &p.critique {
                    s.push_str(&format!("  critique: {c}\n"));
                }
                // Absence is stated, never omitted. Leaving the line out invited a
                // model to invent reviewer approval for a PR nobody had reviewed —
                // which is the single worst thing this feature could fabricate.
                match &p.conversation {
                    Some(c) => s.push_str(&format!("  what reviewers said: {c}\n")),
                    None => s.push_str(
                        "  what reviewers said: NOTHING — this PR has no review \
                         discussion. Do not claim reviewers approved or objected.\n",
                    ),
                }
                if !p.also_fixes.is_empty() {
                    s.push_str(&format!("  also resolves: {}\n", p.also_fixes.join(", ")));
                }
            }
        }
        if !self.child_subjects.is_empty() {
            s.push_str("\n=== ACTIVITY ON THOSE ATTEMPTS ===\n");
            for c in &self.child_subjects {
                s.push_str(&format!(
                    "- {} — {} ({} event(s), {})\n  {}\n",
                    c.key,
                    c.title,
                    c.event_count,
                    c.severity,
                    c.summary.as_deref().unwrap_or("(no summary yet)")
                ));
            }
        }
        if let Some(rc) = &self.root_cause {
            s.push_str(&format!("\n=== PROPOSED CAUSES ({}) ===\n", rc.status));
            if let Some(v) = &rc.verdict {
                s.push_str(&format!("{v}\n"));
            }
            for c in &rc.candidates {
                s.push_str(&format!("- {c}\n"));
            }
        }
        if let Some(t) = &self.triage {
            s.push_str(&format!(
                "\n=== TRIAGE AGAINST THE CODE ({}) ===\n",
                t.status
            ));
            if let Some(c) = &t.characterization {
                s.push_str(&format!("{c}\n"));
            }
            for a in &t.approaches {
                s.push_str(&format!("- approach: {a}\n"));
            }
        }
        if !self.dashboard_readings.is_empty() {
            s.push_str("\n=== WHAT THE DASHBOARDS SHOWED ===\n");
            for d in &self.dashboard_readings {
                s.push_str(&format!("- {d}\n"));
            }
        }
        if !self.attached_context.is_empty() {
            s.push_str("\n=== CONTEXT THE OPERATOR ATTACHED ===\n");
            for c in &self.attached_context {
                s.push_str(&format!("- {c}\n"));
            }
        }
        if !self.related.is_empty() {
            s.push_str("\n=== RELATED WORK ===\n");
            for r in &self.related {
                s.push_str(&format!("- {r}\n"));
            }
        }
        s.push_str("\n=== END OF DOSSIER ===\n");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> GatheredContext {
        GatheredContext {
            subject: "o/r#412".into(),
            rank: "issue".into(),
            title: "Connection pool exhausted".into(),
            summary: Some("Pool saturates under load".into()),
            handled: "open".into(),
            severity: "warning".into(),
            pull_requests: vec![PullRequestLine {
                reference: "o/r#987".into(),
                url: Some("https://github.com/o/r/pull/987".into()),
                title: "Raise pool ceiling".into(),
                author: Some("alice".into()),
                state: Some("open".into()),
                verdict: "partial".into(),
                confidence: 0.7,
                implementation: Some("Bumps max_connections to 200".into()),
                critique: Some("Papers over the leak".into()),
                conversation: Some("bob is blocking: wants the leak fixed first".into()),
                also_fixes: vec![],
                analyzed_by: Some("local".into()),
            }],
            ..Default::default()
        }
    }

    /// An unreviewed dossier: no PR conversation anywhere.
    fn unreviewed() -> GatheredContext {
        let mut g = ctx();
        g.pull_requests[0].conversation = None;
        g
    }

    /// The four fabrications a 33B model produced on the first live run of this feature.
    /// The prompt forbids each one; these assert the *guarantee* rather than the request,
    /// because now that explanations are written locally by default there is no metered
    /// tier standing between a fabrication and the operator.
    #[test]
    fn an_invented_link_loses_its_destination_not_its_sentence() {
        let g = ctx();
        let (out, notes) = verify("See [o/r#991](link_to_pr) for the follow-up.", &g);
        assert_eq!(out, "See o/r#991 for the follow-up.");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("1 link removed"), "{notes:?}");
    }

    #[test]
    fn a_link_the_dossier_supplied_survives_untouched() {
        let g = ctx();
        let text = "See [o/r#987](https://github.com/o/r/pull/987) for the fix.";
        let (out, notes) = verify(text, &g);
        assert_eq!(out, text);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn invented_reviewer_approval_is_removed_when_nobody_reviewed() {
        let (out, notes) = verify(
            "The PR bumps the ceiling. Reviewers said this approach is effective. \
             It does not fix the leak.",
            &unreviewed(),
        );
        assert!(!out.contains("Reviewers said"), "{out}");
        // The sentences around it are untouched — the claim was the problem, not the
        // paragraph.
        assert!(out.contains("The PR bumps the ceiling."), "{out}");
        assert!(out.contains("It does not fix the leak."), "{out}");
        assert!(
            notes.iter().any(|n| n.contains("about reviewers")),
            "{notes:?}"
        );
    }

    /// Stating that nobody reviewed it is the *correct* thing to say, and must survive.
    /// The phrasings a model actually reaches for when it says nobody reviewed something.
    /// Every one of these is *true* about an unreviewed PR, so deleting any of them is worse
    /// than the fabrication the check exists to stop — the operator sees a shorter
    /// explanation and never learns a correct sentence was taken out of it.
    #[test]
    fn true_statements_about_the_absence_of_reviews_all_survive() {
        for line in [
            "Nobody has reviewed it.",
            "No reviewers have commented.",
            "Reviewers have not weighed in.",
            "The PR has not yet been reviewed by anyone.",
            "There is no reviewer feedback.",
            "No reviewer has looked at it yet.",
            "This dossier has no diff, no reviewer, and no logs.",
            "No reviewers are assigned.",
        ] {
            let (out, notes) = verify(line, &unreviewed());
            assert_eq!(out, line, "deleted a true statement: {line}");
            assert!(notes.is_empty(), "{line} -> {notes:?}");
        }
    }

    /// ...and the positive claims still go.
    #[test]
    fn positive_reviewer_claims_still_go_when_nothing_was_reviewed() {
        for line in [
            "Reviewers said this approach is effective.",
            "Reviewers approved it.",
            "Two reviewers signed off on the change.",
        ] {
            let (out, notes) = verify(line, &unreviewed());
            assert!(out.is_empty(), "should have gone: {line} -> {out}");
            assert_eq!(notes.len(), 1, "{line} -> {notes:?}");
        }
    }

    #[test]
    fn saying_nobody_has_reviewed_it_is_not_a_reviewer_claim() {
        let text = "Nobody has reviewed it yet.";
        let (out, notes) = verify(text, &unreviewed());
        assert_eq!(out, text);
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// With a real conversation in the dossier, reviewer claims are legitimate and are
    /// left alone — including the objection, which the prompt says to lead with.
    #[test]
    fn reviewer_claims_stand_when_there_is_a_conversation() {
        let text = "Reviewers objected: bob wants the leak fixed first.";
        let (out, notes) = verify(text, &ctx());
        assert_eq!(out, text);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_pr_is_not_asked_to_narrate_itself_as_an_attempt() {
        // On a PR page the diff and its review lead the page, so "The attempts" would be
        // the third telling of the same change.
        let g = ctx();
        assert!(!g.pull_requests.is_empty(), "fixture must have attempts");
        let pr = sections_for("restatedev/nuon-byoc!140", &g);
        assert!(!pr.iter().any(|s| s.contains("The attempts")), "{pr:#?}");
        // On the issue those PRs attempt, it is exactly what the reader wants.
        let issue = sections_for("restatedev/nuon-byoc#1200", &g);
        assert!(
            issue.iter().any(|s| s.contains("The attempts")),
            "{issue:#?}"
        );
    }

    #[test]
    fn a_section_with_nothing_behind_it_is_dropped_with_its_body() {
        let mut g = ctx();
        g.pull_requests.clear();
        let (out, notes) = verify(
            "**Bottom line** — the pool saturates.\n\
             **The attempts**\n\
             - o/r#987 raises the ceiling.\n\
             **What to do next** — fix the leak.",
            &g,
        );
        assert!(!out.contains("The attempts"), "{out}");
        assert!(
            !out.contains("raises the ceiling"),
            "the section body must go with its heading: {out}"
        );
        assert!(out.contains("Bottom line"), "{out}");
        assert!(
            out.contains("What to do next"),
            "a later real section must resume: {out}"
        );
        assert!(
            notes.iter().any(|n| n.contains("The attempts")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_section_the_dossier_supports_is_kept() {
        let (out, _) = verify("**The attempts**\n- o/r#987 raises the ceiling.", &ctx());
        assert!(out.contains("The attempts"));
        assert!(out.contains("raises the ceiling"));
    }

    /// Citation markers are not links and must never be mistaken for one — they are how
    /// the explanation points at its evidence.
    #[test]
    fn citation_markers_are_left_alone() {
        let text = "The pool saturated [sig:abc123] under load [ctx:def456].";
        let (out, notes) = verify(text, &ctx());
        assert_eq!(out, text);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_clean_explanation_is_returned_unchanged_with_no_notes() {
        let text = "**Bottom line** — the pool saturates under load.\n\
                    **What to do next** — fix the leak in the error path.";
        let (out, notes) = verify(text, &ctx());
        assert_eq!(out, text);
        assert!(
            notes.is_empty(),
            "a good explanation must not be annotated: {notes:?}"
        );
    }

    #[test]
    fn the_dossier_carries_the_critique_and_what_reviewers_said_separately() {
        let rendered = ctx().render();
        // Both must reach the prompt, and distinctly: the critique is MuggleBot's
        // reading of the diff, the conversation is a human's. Collapsing them would
        // lose exactly the distinction that makes a reviewer's objection worth more.
        assert!(rendered.contains("critique: Papers over the leak"));
        assert!(rendered.contains("what reviewers said: bob is blocking"));
        // Spelled out rather than "partial at 70%", which a small model read as
        // "fixes 70% of the problem" — a different claim, and a wrong one.
        assert!(rendered.contains("PARTIALLY addresses the issue"));
        assert!(rendered.contains("confidence in that verdict: 70%"));
    }

    /// The absence of a review discussion has to be *stated*. Omitting the line let a
    /// model invent reviewer approval for a PR nobody had looked at — the single worst
    /// thing this feature could fabricate.
    #[test]
    fn a_pr_with_no_reviews_says_so_rather_than_going_quiet() {
        let mut g = ctx();
        g.pull_requests[0].conversation = None;
        let rendered = g.render();
        assert!(rendered.contains("what reviewers said: NOTHING"));
        assert!(rendered.contains("Do not claim reviewers approved or objected"));
        assert!(!g.sources().contains(&"pr_conversations".to_string()));
    }

    /// A missing URL must be stated too, or the model writes `[o/r#991](link_to_pr)`.
    #[test]
    fn a_pr_with_no_url_tells_the_model_not_to_invent_one() {
        let mut g = ctx();
        g.pull_requests[0].url = None;
        assert!(g.render().contains("do not write a link"));
    }

    #[test]
    fn sources_report_what_it_actually_had_to_go_on() {
        let s = ctx().sources();
        assert!(s.contains(&"pr_critiques".to_string()));
        assert!(s.contains(&"pr_conversations".to_string()));
        assert!(s.contains(&"summary".to_string()));
        // Nothing claims a root cause or a dashboard reading it never saw.
        assert!(!s.contains(&"root_cause".to_string()));
        assert!(!s.contains(&"dashboard".to_string()));
    }

    #[test]
    fn a_pr_explained_alone_still_names_the_problem_it_attempts() {
        let mut g = ctx();
        g.rank = "pull_request".into();
        g.subject = "o/r!987".into();
        g.pull_requests.clear();
        g.parent = Some(RelatedSubject {
            key: "o/r#412".into(),
            title: "Connection pool exhausted".into(),
            summary: Some("Pool saturates under load".into()),
            event_count: 3,
            severity: "warning".into(),
        });
        let rendered = g.render();
        // Labelled as an *issue*, explicitly: without that, a model described the
        // parent issue as a pull request that had been merged.
        assert!(rendered.contains("THE ISSUE THIS PULL REQUEST IS AN ATTEMPT TO FIX"));
        assert!(rendered.contains("ISSUE o/r#412"));
        assert!(rendered.contains("not a pull request"));
        assert!(g.sources().contains(&"parent_issue".to_string()));
    }

    #[test]
    fn a_thin_dossier_renders_no_empty_sections() {
        // An empty heading is an invitation to fill it in, which is how an explanation
        // acquires a root cause nobody proposed.
        let g = GatheredContext {
            subject: "C1/1.2".into(),
            rank: "slack_thread".into(),
            title: "TLS expiry".into(),
            handled: "open".into(),
            severity: "critical".into(),
            ..Default::default()
        };
        let rendered = g.render();
        for absent in [
            "THE ATTEMPTS",
            "PROPOSED CAUSES",
            "TRIAGE AGAINST THE CODE",
            "WHAT THE DASHBOARDS SHOWED",
            "RELATED WORK",
        ] {
            assert!(!rendered.contains(absent), "{absent} should be omitted");
        }
        assert!(g.sources().is_empty());
    }
}
