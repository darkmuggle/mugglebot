//! Slack thread analysis — two models, no synthesis, and the quotes have to be real.
//!
//! Paste a permalink; get an assessment of what actually happened in that thread. This is
//! deliberately **not** a watcher. Threads already become subjects when they involve you;
//! this is the other thing — a specific conversation you want read properly, asked for by
//! hand.
//!
//! # Why two models, run blind to each other
//!
//! One model asked "was anyone unreasonable here" will produce a confident answer either
//! way, and its particular flavour of agreeableness is a property of that model. So the
//! same prompt goes to Claude and to ChatGPT **concurrently**, neither seeing the other's
//! output, and [`agreement`] then computes where they land on the same person over the same
//! message.
//!
//! There is no third synthesis pass, and that is the design. A synthesiser reads two
//! analyses and writes one, which means disagreement — the most informative thing here —
//! gets smoothed into a paragraph that sounds more settled than the evidence is. Corroborated
//! and contested findings are shown as what they are.
//!
//! # Candour, and the trap in asking for it
//!
//! The point of this is to be told when *you* were the problem. But a prompt that demands
//! criticism gets criticism, invented if necessary, because the model is answering the
//! instruction rather than reading the thread. Three things keep it honest:
//!
//! 1. **Everyone is graded on the same rubric, you included.** The prompt does not say "be
//!    hard on this one participant" — it asks for the same judgement about every person in
//!    the thread. Your findings are then *extracted*, not specially commissioned. A rubric
//!    applied to one person is a rubric bent around them.
//! 2. **"Nothing to call out" is an allowed answer**, and the prompt says so explicitly with
//!    a reason required. Without that, the model manufactures a fault to fill the section.
//! 3. **Every finding cites messages, and quotes must be real.** [`verify`] drops a finding
//!    whose citations don't resolve, and drops one whose quoted text does not appear in the
//!    thread. Fabricating a damning quote is the worst failure this could have, and it is
//!    the one that is cheapest to make impossible.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::persona::{Facet, Profile};

/// Messages one analysis will read. Past this the thread is capped from the *front* — see
/// [`cap`] for why the end is what gets kept.
const MAX_MESSAGES: usize = 200;

/// Characters kept per message. Enough for a paragraph and a code block; a pasted stack
/// trace is not what the analysis turns on.
const MAX_MESSAGE_CHARS: usize = 1_400;

/// Trait lines per participant fed into the prompt. Six people at eighteen traits each is
/// more context than the thread being read, and the thread is the evidence.
const MAX_TRAITS_PER_PARTICIPANT: usize = 6;

const MAX_FINDINGS: usize = 14;

/// A quoted span shorter than this is not checked against the thread — `"no"` or `"LGTM"`
/// will collide with something by accident, and flagging those trains you to ignore the
/// marks.
const MIN_CHECKED_QUOTE: usize = 12;

// ---- the link ----------------------------------------------------------------

/// Which thread, from a pasted Slack permalink.
///
/// Slack's own "Copy link" produces one of two shapes, and the difference matters:
///
/// - `…/archives/C01234ABC/p1712345678123456` — a message. If it *is* a thread root, its ts
///   is the thread.
/// - `…/archives/C01234ABC/p1712345678123456?thread_ts=1712345600.000100&cid=C01234ABC` —
///   a reply *inside* a thread. Here the interesting thing is `thread_ts`, not the message
///   the operator happened to right-click. Reading the ts would fetch a one-message
///   "thread" and analyse a single reply out of context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    pub channel: String,
    /// The thread root's ts — what `conversations.replies` needs.
    pub thread_ts: String,
    /// The message the link actually pointed at, when that is not the root. Worth keeping:
    /// "the bit I was looking at" is context for what the operator wants read.
    pub focus_ts: Option<String>,
}

impl ThreadRef {
    /// A stable key for one thread. Used as the workflow key, so re-analysing the same
    /// thread is a refused invocation rather than four metered model calls.
    pub fn key(&self) -> String {
        format!("{}@{}", self.channel, self.thread_ts)
    }
}

/// `p1712345678123456` → `1712345678.123456`.
///
/// Slack's permalink form is the ts with the dot removed, so the dot goes back in six digits
/// from the end rather than at a fixed offset — ts seconds are ten digits now and will not
/// be forever.
fn ts_from_permalink_segment(seg: &str) -> Option<String> {
    let digits = seg.strip_prefix('p')?;
    if digits.len() < 7 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (secs, micros) = digits.split_at(digits.len() - 6);
    Some(format!("{secs}.{micros}"))
}

/// Read a pasted Slack link. Anything that is not a Slack archive link is an error naming
/// what was expected — this is an operator typing into a box, so the message is the UI.
pub fn parse_link(raw: &str) -> Result<ThreadRef> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("paste a Slack message link");
    }
    let (path, query) = match raw.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw, ""),
    };
    let after = path.split("/archives/").nth(1).with_context(|| {
        format!(
            "that does not look like a Slack link — expected something like \
             https://your-workspace.slack.com/archives/C01234ABC/p1712345678123456 \
             (use Slack's \"Copy link\"), got: {}",
            raw.chars().take(120).collect::<String>()
        )
    })?;
    let mut parts = after.split('/').filter(|s| !s.is_empty());
    let channel = parts
        .next()
        .filter(|c| c.len() > 1)
        .context("the Slack link has no channel id in it")?
        .to_string();
    let message_ts = parts.next().and_then(ts_from_permalink_segment);

    let mut thread_ts = None;
    let mut cid = None;
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "thread_ts" => thread_ts = Some(v.to_string()),
            "cid" => cid = Some(v.to_string()),
            _ => {}
        }
    }

    // `thread_ts` wins: the link points into a thread, and the thread is the unit of
    // analysis. The clicked message is kept as focus rather than thrown away.
    let (root, focus) = match (thread_ts, message_ts) {
        (Some(t), Some(m)) if t != m => (t, Some(m)),
        (Some(t), _) => (t, None),
        (None, Some(m)) => (m, None),
        (None, None) => bail!("the Slack link has no message timestamp in it"),
    };
    Ok(ThreadRef {
        channel: cid.unwrap_or(channel),
        thread_ts: root,
        focus_ts: focus,
    })
}

// ---- the thread ---------------------------------------------------------------

/// One message, as the analysis sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// `m1`, `m2`, … The ordinal the model cites.
    ///
    /// Ordinals rather than timestamps for the same reason persona evidence uses them: a
    /// model asked to copy `1712345678.123456` will get a digit wrong often enough to matter,
    /// and a citation that doesn't resolve is a finding thrown away. `m3` it can manage.
    pub id: String,
    pub ts: String,
    /// Display name where known, else the raw Slack id.
    pub author: String,
    /// Slack user id, when the message came from a human.
    pub user: Option<String>,
    pub text: String,
    /// The operator. Named, because the whole point is being told about your own part.
    pub is_you: bool,
    /// Reaction names, which carry agreement that never got written down.
    pub reactions: Vec<String>,
}

/// A participant, with whatever the persona engine already knows about them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    pub handle: String,
    pub display_name: String,
    pub messages: usize,
    pub is_you: bool,
    /// The persona slug, when one exists.
    pub persona: Option<String>,
    /// Trait lines for the prompt — behaviour observed elsewhere, not in this thread.
    pub traits: Vec<String>,
    pub role: Option<String>,
}

/// A whole thread plus its cast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub reference: ThreadRef,
    pub channel_name: Option<String>,
    pub messages: Vec<Message>,
    pub participants: Vec<Participant>,
    /// Messages dropped by [`MAX_MESSAGES`], declared rather than silently cut.
    pub truncated: usize,
}

impl Thread {
    fn you(&self) -> Option<&Participant> {
        self.participants.iter().find(|p| p.is_you)
    }

    /// Participants the persona engine has never modelled.
    ///
    /// Surfaced in the prompt on purpose: the difference between "this is how they always
    /// behave" and "this is how they behaved once" is the difference between an insight and
    /// a slur, and only the persona profile can tell them apart.
    fn unmodelled(&self) -> Vec<&Participant> {
        self.participants
            .iter()
            .filter(|p| p.persona.is_none() && !p.is_you)
            .collect()
    }
}

// ---- findings ----------------------------------------------------------------

/// What a finding is about. Everyone gets all three, which is what stops the operator's
/// section being either flattery or a beating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stance {
    /// Something this person got right.
    Credit,
    /// Something this person got wrong, and the thread shows it.
    Criticism,
    /// Something that happened, with no verdict attached.
    Observation,
}

impl Stance {
    pub fn as_str(self) -> &'static str {
        match self {
            Stance::Credit => "credit",
            Stance::Criticism => "criticism",
            Stance::Observation => "observation",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "credit" | "praise" | "positive" => Stance::Credit,
            "criticism" | "critique" | "negative" | "callout" => Stance::Criticism,
            _ => Stance::Observation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Which participant this is about, by handle. `None` for a finding about the thread
    /// itself rather than a person.
    pub about: Option<String>,
    pub stance: Stance,
    pub claim: String,
    /// Message ids, resolved from the ordinals the model wrote.
    pub cites: Vec<String>,
    /// Persona trait ids the claim leaned on, lifted out of the prose.
    ///
    /// The profile lines in the prompt are labelled `[tr:ID]`, and a model that uses one
    /// quotes the label back mid-sentence — so a live run produced claims reading
    /// "…matching his documented habit \[tr:pavel/hobby_horses/1c064dc05a9e6cbc\]". The
    /// citation is worth keeping and the 40-character hash in the middle of a sentence is
    /// not, so it moves here and the sentence is left readable.
    #[serde(default)]
    pub from_traits: Vec<String>,
    /// Which model said it — `claude` or `chatgpt`.
    pub source: String,
}

/// A finding after both models have been heard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Corroborated {
    pub finding: Finding,
    /// The other model's matching finding, when there is one.
    pub also: Option<Finding>,
}

impl Corroborated {
    pub fn both_models(&self) -> bool {
        self.also.is_some()
    }
}

/// One analysis, from one model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    pub model: String,
    pub provider: String,
    /// What the thread was about and where it got to, in the model's words.
    pub summary: String,
    /// Whether the thread reached a decision, and what.
    pub outcome: Option<String>,
    pub findings: Vec<Finding>,
    /// Findings dropped by [`verify`], with the reason. Kept so a suspiciously thin
    /// analysis is visibly thin rather than quietly so.
    pub dropped: Vec<Dropped>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dropped {
    pub claim: String,
    pub why: String,
}

/// The whole result: two analyses, and what they agree on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub thread: Thread,
    pub analyses: Vec<Analysis>,
    /// Findings about the operator, corroboration first, criticism before credit. This is
    /// the section that was asked for.
    pub about_you: Vec<Corroborated>,
    /// Everything else, same ordering.
    pub about_others: Vec<Corroborated>,
    /// Where exactly one model made a claim the other did not — not noise, but not
    /// corroborated either.
    pub contested: usize,
    /// Models that were asked and did not answer, with the reason.
    ///
    /// This field exists because of a real failure caught on the first live run: the panel
    /// was configured with a ChatGPT model that account could not use, ChatGPT returned
    /// nothing, and the verdict came back looking like a complete two-model analysis whose
    /// findings simply happened to be uncorroborated. That is the worst shape this feature
    /// could have — the operator asked for two independent readers and would have believed
    /// they got them. A partial panel has to be loud.
    #[serde(default)]
    pub failures: Vec<String>,
}

impl Verdict {
    /// Did every configured model answer? When false, nothing here can be corroborated and
    /// the absence of corroboration says nothing about the findings.
    pub fn full_panel(&self) -> bool {
        self.failures.is_empty() && self.analyses.len() > 1
    }
}

// ---- rendering the thread for a prompt ---------------------------------------

pub fn render_thread(thread: &Thread) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "THREAD in {}\n",
        thread
            .channel_name
            .as_deref()
            .unwrap_or(&thread.reference.channel)
    ));
    if let Some(focus) = &thread.reference.focus_ts {
        if let Some(m) = thread.messages.iter().find(|m| &m.ts == focus) {
            out.push_str(&format!(
                "The operator's link pointed at [{}] specifically.\n",
                m.id
            ));
        }
    }
    out.push_str("\nPARTICIPANTS\n");
    for p in &thread.participants {
        out.push_str(&format!(
            "- @{}{} — {} message(s)",
            p.handle,
            if p.is_you {
                " (THE OPERATOR, i.e. the person who asked for this analysis)"
            } else {
                ""
            },
            p.messages
        ));
        if let Some(role) = p.role.as_deref().filter(|r| !r.trim().is_empty()) {
            out.push_str(&format!(", {role}"));
        }
        out.push('\n');
        if p.traits.is_empty() {
            out.push_str(
                "  (no profile — nothing is known about how they usually behave, so do not \
                 claim a pattern)\n",
            );
        } else {
            for t in &p.traits {
                out.push_str(&format!("  {t}\n"));
            }
        }
    }
    out.push_str("\nMESSAGES\n");
    for m in &thread.messages {
        out.push_str(&format!(
            "[{}] @{}{}: {}\n",
            m.id,
            m.author,
            if m.reactions.is_empty() {
                String::new()
            } else {
                format!(" (reactions: {})", m.reactions.join(", "))
            },
            truncate(&m.text, MAX_MESSAGE_CHARS)
        ));
    }
    if thread.truncated > 0 {
        out.push_str(&format!(
            "\n({} earlier message(s) not included — say so if the answer depends on them.)\n",
            thread.truncated
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let cut: String = t.chars().take(max).collect();
    format!("{cut}… [truncated]")
}

/// The brief. Both models get exactly this, so a difference in their answers is a difference
/// between the models rather than between two prompts.
pub fn brief(thread: &Thread) -> String {
    let you = thread
        .you()
        .map(|p| format!("@{}", p.handle))
        .unwrap_or_else(|| "(the operator did not post in this thread)".into());
    let unmodelled = thread.unmodelled();
    let unmodelled_note = if unmodelled.is_empty() {
        String::new()
    } else {
        format!(
            "\n- These participants have no profile: {}. You may describe what they did in \
             this thread. You may NOT characterise what they are like, because one thread is \
             not a pattern.",
            unmodelled
                .iter()
                .map(|p| format!("@{}", p.handle))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "Read one Slack thread and say what actually happened in it.\n\n\
         The person who asked for this is {you}. They have asked to be told where they were \
         wrong, and they meant it. Treat that as permission to be blunt about them — not as \
         an instruction to find fault.\n\n\
         Assess EVERY participant on the same terms, including {you}. Do not soften the \
         operator's entries and do not sharpen them; a rubric bent around one person is not a \
         rubric. Whether their entries come out worse or better than anyone else's is a \
         result, not a target.\n\n\
         For each finding, pick one stance:\n\
         - `credit` — they got something right.\n\
         - `criticism` — they got something wrong, and the messages show it.\n\
         - `observation` — it happened; no verdict.\n\n\
         Rules, all of them enforced by a checker that runs before anyone reads this:\n\
         - Every finding must cite the messages it rests on, as `[m3]` or `[m3, m7]` in a \
           `cites` array. A finding citing nothing is DELETED. Do not pad.\n\
         - **Never invent a quotation.** Any text you put in double quotes is checked \
           character by character against the thread, and a finding with a quote that isn't \
           there is DELETED. Paraphrase freely; quote only what was written.\n\
         - Judge what was said, not what you infer someone felt. \"He was dismissive\" needs \
           a message that reads as dismissive. \"He was frustrated\" is a guess about a mind.\n\
         - A profile line `[tr:…]` tells you how someone usually behaves. Use it to tell a \
           one-off apart from a pattern. Without one, you cannot tell, and must not imply you \
           can.{unmodelled_note}\n\
         - **\"Nothing to call out\" is a real answer** and often the right one. A thread \
           where people disagreed, resolved it and moved on contains no criticism. If you \
           have nothing on {you}, return no `criticism` findings about them and say why in \
           `summary`. Do not go looking for a fault to fill the section — an invented \
           criticism is the single worst thing you can return here, because it will be \
           believed.\n\
         - If the thread is too short or too fragmentary to support any of this, say that and \
           return few findings or none.\n\n\
         Reply with ONE JSON object and nothing else:\n\
         {{\"summary\":\"what the thread was about and where it got to\",\
         \"outcome\":\"the decision reached, or null if none was\",\
         \"findings\":[{{\"about\":\"handle, or null for the thread as a whole\",\
         \"stance\":\"credit|criticism|observation\",\
         \"claim\":\"one sentence, specific\",\"cites\":[\"m3\"]}}]}}\n\
         At most {MAX_FINDINGS} findings."
    )
}

// ---- parsing and verification ------------------------------------------------

/// Resolve the ordinals in a `cites` array to message ids present in this thread.
fn resolve_cites(node: Option<&serde_json::Value>, index: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        // Accept `m3`, `M3`, `[m3]`, `3`, and `m3, m7` in one string — the model will
        // produce all of these and the citation is worth more than the pedantry.
        for piece in raw.split(|c: char| !c.is_ascii_alphanumeric()) {
            let p = piece.trim();
            if p.is_empty() {
                continue;
            }
            let digits = p.trim_start_matches(['m', 'M']);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let id = format!("m{digits}");
            if index.contains(&id) && !out.contains(&id) {
                out.push(id);
            }
        }
    };
    match node {
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                match item {
                    serde_json::Value::String(s) => push(s),
                    serde_json::Value::Number(n) => push(&n.to_string()),
                    _ => {}
                }
            }
        }
        Some(serde_json::Value::String(s)) => push(s),
        _ => {}
    }
    out
}

/// Lift `[tr:ID]` labels out of a claim, returning the cleaned prose and the ids.
///
/// Tidying rather than censoring: the model is *right* to cite the trait, and the citation is
/// kept — it just belongs beside the claim rather than inside the sentence.
fn split_trait_citations(claim: &str) -> (String, Vec<String>) {
    let mut ids = Vec::new();
    let mut out = String::with_capacity(claim.len());
    let mut rest = claim;
    while let Some(start) = rest.find("[tr:") {
        let Some(end_rel) = rest[start..].find(']') else {
            break;
        };
        let end = start + end_rel;
        let id = rest[start + 4..end].trim().to_string();
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
        out.push_str(&rest[..start]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    // A citation removed mid-sentence leaves " ." or a double space behind.
    let cleaned = out
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" .", ".")
        .replace(" ,", ",")
        .replace("( )", "")
        .trim()
        .to_string();
    (cleaned, ids)
}

/// Double-quoted spans in a claim, long enough to be worth checking.
fn quoted_spans(claim: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = claim.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Straight and curly openers both, because a model writes either.
        if matches!(chars[i], '"' | '\u{201c}') {
            let close = chars[i + 1..]
                .iter()
                .position(|c| matches!(c, '"' | '\u{201d}'));
            if let Some(rel) = close {
                let span: String = chars[i + 1..i + 1 + rel].iter().collect();
                if span.trim().chars().count() >= MIN_CHECKED_QUOTE {
                    out.push(span.trim().to_string());
                }
                i += rel + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Is this quote actually in the thread?
///
/// Compared on collapsed whitespace and case, because a model reflowing a Slack message
/// across a line break has not fabricated anything. Nothing looser than that: the whole
/// value of the check is that it fails on text nobody wrote.
fn quote_is_real(quote: &str, haystack: &str) -> bool {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    norm(haystack).contains(&norm(quote))
}

/// Parse one model's reply and drop what it cannot support.
///
/// Returns the analysis with survivors in `findings` and casualties in `dropped`. Reporting
/// the casualties is the part that matters: an analysis that lost four findings to invented
/// quotes is telling you something about that run, and silently returning two findings looks
/// identical to a thread with only two things worth saying.
pub fn parse_and_verify(
    thread: &Thread,
    provider: &str,
    model: &str,
    reply: &str,
) -> Result<Analysis> {
    let json = crate::reasoner::extract_json(reply)
        .with_context(|| format!("{provider} returned no JSON: {}", first_line(reply)))?;
    let index: BTreeSet<String> = thread.messages.iter().map(|m| m.id.clone()).collect();
    let handles: BTreeSet<String> = thread
        .participants
        .iter()
        .map(|p| p.handle.to_lowercase())
        .collect();
    let corpus = thread
        .messages
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut findings = Vec::new();
    let mut dropped = Vec::new();
    let items = json
        .get("findings")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    for item in items.iter().take(MAX_FINDINGS) {
        let raw_claim = item
            .get("claim")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let (claim, from_traits) = split_trait_citations(&raw_claim);
        if claim.is_empty() {
            continue;
        }
        let cites = resolve_cites(item.get("cites"), &index);
        if cites.is_empty() {
            dropped.push(Dropped {
                claim,
                why: "cited no message in the thread".into(),
            });
            continue;
        }
        let fake: Vec<String> = quoted_spans(&claim)
            .into_iter()
            .filter(|q| !quote_is_real(q, &corpus))
            .collect();
        if !fake.is_empty() {
            dropped.push(Dropped {
                claim,
                why: format!("quoted words nobody wrote: \"{}\"", fake.join("\", \"")),
            });
            continue;
        }
        // An `about` naming someone who is not in the thread is a finding about a person
        // the model invented; keep the claim as being about the thread rather than binding
        // it to a stranger.
        let about = item
            .get("about")
            .and_then(|a| a.as_str())
            .map(|s| s.trim().trim_start_matches('@').to_string())
            .filter(|s| !s.is_empty() && s.to_lowercase() != "null")
            .filter(|s| handles.contains(&s.to_lowercase()));
        findings.push(Finding {
            about,
            stance: Stance::parse(item.get("stance").and_then(|s| s.as_str()).unwrap_or("")),
            claim,
            cites,
            from_traits,
            source: provider.to_string(),
        });
    }

    Ok(Analysis {
        model: model.to_string(),
        provider: provider.to_string(),
        summary: json
            .get("summary")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .trim()
            .to_string(),
        outcome: json
            .get("outcome")
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.to_lowercase() != "null")
            .map(str::to_string),
        findings,
        dropped,
    })
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(empty)")
        .chars()
        .take(160)
        .collect()
}

/// Do two findings say the same thing about the same person?
///
/// Deterministic and deliberately crude: same subject, same stance, and at least one message
/// in common. It is not judging whether the *words* match — that would need a third model,
/// and a model deciding whether two models agree is exactly the laundering this design
/// avoids. Two independent readers flagging the same person over the same message is the
/// signal; what they each chose to emphasise is left visible.
fn same_finding(a: &Finding, b: &Finding) -> bool {
    let subject_matches = match (&a.about, &b.about) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        (None, None) => true,
        _ => false,
    };
    subject_matches && a.stance == b.stance && a.cites.iter().any(|c| b.cites.contains(c))
}

/// Pair up two models' findings, then order them the way they should be read.
///
/// Corroborated first, then criticism before credit before observation. The operator asked
/// to be told where they were wrong; burying that under three compliments would be a way of
/// technically complying.
pub fn agreement(analyses: &[Analysis]) -> (Vec<Corroborated>, Vec<Corroborated>, usize) {
    let (first, second) = match analyses {
        [a] => (a.findings.clone(), Vec::new()),
        [a, b, ..] => (a.findings.clone(), b.findings.clone()),
        [] => (Vec::new(), Vec::new()),
    };
    let mut used_second = vec![false; second.len()];
    let mut all: Vec<Corroborated> = Vec::new();
    for f in first {
        let mate = second
            .iter()
            .enumerate()
            .find(|(i, g)| !used_second[*i] && same_finding(&f, g));
        match mate {
            Some((i, g)) => {
                used_second[i] = true;
                all.push(Corroborated {
                    finding: f,
                    also: Some(g.clone()),
                });
            }
            None => all.push(Corroborated {
                finding: f,
                also: None,
            }),
        }
    }
    // Whatever the second model raised alone is still a finding — it just stands on one
    // reader rather than two.
    for (i, g) in second.into_iter().enumerate() {
        if !used_second[i] {
            all.push(Corroborated {
                finding: g,
                also: None,
            });
        }
    }
    let contested = all.iter().filter(|c| c.also.is_none()).count();

    let rank = |c: &Corroborated| {
        (
            !c.both_models(),
            match c.finding.stance {
                Stance::Criticism => 0,
                Stance::Credit => 1,
                Stance::Observation => 2,
            },
        )
    };
    let you: BTreeSet<String> = Default::default();
    let _ = &you;
    let (mut yours, mut others): (Vec<Corroborated>, Vec<Corroborated>) = (Vec::new(), Vec::new());
    for c in all {
        if c.finding.about.is_some() && c.finding.about.as_deref() == Some(YOU_MARKER) {
            yours.push(c);
        } else {
            others.push(c);
        }
    }
    yours.sort_by_key(rank);
    others.sort_by_key(rank);
    (yours, others, contested)
}

/// The handle substituted for the operator before [`agreement`] splits the findings.
///
/// A sentinel rather than "whatever their Slack handle is": the split has to work when the
/// operator's own handle is unknown or when a model wrote their display name instead, and a
/// mis-split here silently moves the section they asked for.
pub const YOU_MARKER: &str = "__you__";

/// Rewrite `about` to [`YOU_MARKER`] for findings that are about the operator.
///
/// Matched against handle *and* display name, because a model given `@ben` and
/// `Ben Howard` will use either.
pub fn mark_operator(analyses: &mut [Analysis], thread: &Thread) {
    let Some(you) = thread.you() else { return };
    let names: Vec<String> = [you.handle.to_lowercase(), you.display_name.to_lowercase()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    for a in analyses.iter_mut() {
        for f in a.findings.iter_mut() {
            if let Some(about) = &f.about {
                if names.iter().any(|n| n == &about.to_lowercase()) {
                    f.about = Some(YOU_MARKER.to_string());
                }
            }
        }
    }
}

/// Assemble the final verdict from the models' replies.
pub fn assemble(thread: Thread, mut analyses: Vec<Analysis>, failures: Vec<String>) -> Verdict {
    mark_operator(&mut analyses, &thread);
    let (about_you, about_others, contested) = agreement(&analyses);
    Verdict {
        thread,
        analyses,
        about_you,
        about_others,
        contested,
        failures,
    }
}

/// Trait lines for a participant, the ones that bear on how a conversation goes.
///
/// Five of the ten facets, and the cut is the point. `SlackRegister` is *literally* how this
/// person behaves in this medium, and `Escalation` — what they do when they disagree — is the
/// question a contentious thread is asking. `Style`, `HobbyHorses` and `MeetingRegister`
/// carry over.
///
/// Left out: `ReviewsFor`, `Ignores`, `Bar`, `Expertise`, `BlindSpots`. Every one of those is
/// about how someone handles a *diff*, and a profile of six people's review habits is more
/// context than the thread being read — which buries the thread in material that cannot bear
/// on it. Highest confidence first, so the cap keeps the strongest.
pub fn traits_for_prompt(profile: &Profile) -> Vec<String> {
    const RELEVANT: &[Facet] = &[
        Facet::SlackRegister,
        Facet::Escalation,
        Facet::Style,
        Facet::HobbyHorses,
        Facet::MeetingRegister,
    ];
    let mut chosen: Vec<&crate::persona::Trait> = profile
        .traits
        .iter()
        .filter(|t| RELEVANT.contains(&t.facet))
        .collect();
    chosen.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    chosen
        .into_iter()
        .take(MAX_TRAITS_PER_PARTICIPANT)
        .map(|t| t.render())
        .collect()
}

/// Build the participant list from the messages, richest first.
pub fn participants_of(
    messages: &[Message],
    display: &BTreeMap<String, String>,
    profiles: &BTreeMap<String, Profile>,
) -> Vec<Participant> {
    let mut counts: BTreeMap<String, (usize, bool, Option<String>)> = BTreeMap::new();
    for m in messages {
        let key = m.user.clone().unwrap_or_else(|| m.author.clone());
        let entry = counts.entry(key).or_insert((0, false, None));
        entry.0 += 1;
        entry.1 |= m.is_you;
        entry.2 = Some(m.author.clone());
    }
    let mut out: Vec<Participant> = counts
        .into_iter()
        .map(|(key, (n, is_you, author))| {
            let handle = display
                .get(&key)
                .cloned()
                .unwrap_or_else(|| author.clone().unwrap_or_else(|| key.clone()));
            let profile = profiles.get(&key).or_else(|| profiles.get(&handle));
            Participant {
                display_name: profile
                    .map(|p| p.persona.display_name.clone())
                    .unwrap_or_else(|| author.unwrap_or_else(|| handle.clone())),
                messages: n,
                is_you,
                persona: profile.map(|p| p.persona.slug.clone()),
                traits: profile.map(traits_for_prompt).unwrap_or_default(),
                role: profile.and_then(|p| p.persona.role.clone()),
                handle,
            }
        })
        .collect();
    out.sort_by(|a, b| b.messages.cmp(&a.messages).then(a.handle.cmp(&b.handle)));
    out
}

/// Cap the thread, keeping the *end*.
///
/// Where a conversation got to matters more than how it opened, and a thread long enough to
/// need capping is usually long because it went in circles. The root is kept regardless —
/// without it nothing else has a subject.
pub fn cap(messages: Vec<Message>) -> (Vec<Message>, usize) {
    if messages.len() <= MAX_MESSAGES {
        return (messages, 0);
    }
    let dropped = messages.len() - MAX_MESSAGES;
    let mut kept = vec![messages[0].clone()];
    kept.extend(
        messages[messages.len() - (MAX_MESSAGES - 1)..]
            .iter()
            .cloned(),
    );
    (kept, dropped)
}

// ---- the analyser -------------------------------------------------------------

/// Reads a thread and gets two independent opinions on it.
pub struct Analyser {
    store: std::sync::Arc<crate::store::Store>,
    http: reqwest::Client,
    slack_token: Option<String>,
    /// Your own Slack user id, from `[sources.slack].user_id`. Without it nothing can be
    /// attributed to you — and the section the operator asked for is the one about them.
    self_user_id: Option<String>,
    cfg: crate::config::Threads,
    /// Builds a reasoner for a named provider and model. The same factory the re-dispatch
    /// button uses, so "which two models" stays configuration rather than a hardcoded pair.
    factory: crate::restate::workflows::ReasonerFactory,
}

impl Analyser {
    pub fn new(
        store: std::sync::Arc<crate::store::Store>,
        slack_token: Option<String>,
        self_user_id: Option<String>,
        cfg: crate::config::Threads,
        factory: crate::restate::workflows::ReasonerFactory,
    ) -> Self {
        Self {
            store,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            slack_token,
            self_user_id,
            cfg,
            factory,
        }
    }

    /// An inert analyser for test harnesses that need a complete [`crate::tools::Tools`].
    ///
    /// Disabled, no token, and a factory that returns a mock — so `ready()` is false and no
    /// test can reach Slack or a metered model by constructing one. The same reasoning as
    /// `Ingress::offline`: a fixture that quietly works is a fixture that bills you.
    #[cfg(test)]
    pub fn for_tests(store: std::sync::Arc<crate::store::Store>) -> Self {
        Self::new(
            store,
            None,
            None,
            crate::config::Threads {
                enabled: false,
                ..Default::default()
            },
            std::sync::Arc::new(|_, _| {
                std::sync::Arc::new(crate::reasoner::MockReasoner::new("{}"))
            }),
        )
    }

    /// Configured and credentialled.
    pub fn ready(&self) -> bool {
        self.cfg.enabled
            && self
                .slack_token
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty())
    }

    /// The models that will be asked, as `(provider, model)`.
    ///
    /// Two entries normally. One is allowed and works — the verdict just comes back with
    /// nothing corroborated, which is visible rather than silent.
    fn panel(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (provider, model) in [
            ("claude", self.cfg.claude_model.trim()),
            ("chatgpt", self.cfg.chatgpt_model.trim()),
        ] {
            if !model.is_empty() {
                out.push((provider.to_string(), model.to_string()));
            }
        }
        out
    }

    /// Fetch one thread and resolve who is in it.
    pub async fn fetch(&self, reference: &ThreadRef) -> Result<Thread> {
        let token = self
            .slack_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .context("no Slack token stored — thread analysis needs one to read the thread")?;
        let raw = crate::watchers::slack::fetch_thread(
            &self.http,
            token,
            &reference.channel,
            &reference.thread_ts,
        )
        .await?;
        if raw.is_empty() {
            bail!("that thread has no messages the Slack token can see");
        }

        // Display names for the ids in the thread, from the users table the watcher fills.
        let mut display: BTreeMap<String, String> = BTreeMap::new();
        for m in &raw {
            if let Some(uid) = m.user.as_deref() {
                if display.contains_key(uid) {
                    continue;
                }
                if let Ok(Some(u)) = self.store.find_slack_user(uid) {
                    display.insert(uid.to_string(), u.name.clone());
                }
            }
        }

        let messages: Vec<Message> = raw
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let user = m.user.clone();
                let author = user
                    .as_deref()
                    .and_then(|u| display.get(u).cloned())
                    .or_else(|| m.bot_name())
                    // The raw id rather than "unknown": an id can be looked up, and a
                    // participant called "unknown" is one the reader cannot check.
                    .or_else(|| user.clone())
                    .unwrap_or_else(|| "an app".into());
                Message {
                    id: format!("m{}", i + 1),
                    ts: m.ts.clone(),
                    author,
                    is_you: matches!(
                        (self.self_user_id.as_deref(), user.as_deref()),
                        (Some(a), Some(b)) if a == b
                    ),
                    user,
                    text: crate::watchers::slack::readable_text(m),
                    reactions: crate::watchers::slack::reaction_names(m),
                }
            })
            .filter(|m| !m.text.trim().is_empty())
            .collect();
        let (messages, truncated) = cap(messages);

        // Personas for the participants, keyed by both Slack id and handle — a persona may
        // be linked by either, and `handles_on` returns all of them.
        let mut profiles: BTreeMap<String, Profile> = BTreeMap::new();
        let wanted: BTreeSet<String> = messages
            .iter()
            .flat_map(|m| [m.user.clone(), Some(m.author.to_lowercase())])
            .flatten()
            .collect();
        for persona in self.store.list_personas().unwrap_or_default() {
            let handles: Vec<String> = persona
                .handles_on(crate::signal::Source::Slack)
                .map(|h| h.to_string())
                .collect();
            let hit = handles
                .iter()
                .find(|h| wanted.contains(*h) || wanted.contains(&h.to_lowercase()));
            let Some(hit) = hit.cloned() else { continue };
            if let Ok(Some(profile)) = self.profile_of(&persona.slug) {
                // Under every handle it is known by, so participant matching finds it
                // whether the message carried an id or a name.
                for h in handles {
                    profiles.insert(h, profile.clone());
                }
                profiles.insert(hit, profile);
            }
        }

        let participants = participants_of(&messages, &display, &profiles);
        Ok(Thread {
            reference: reference.clone(),
            channel_name: self
                .store
                .find_slack_user(&reference.channel)
                .ok()
                .flatten()
                .map(|u| u.name),
            messages,
            participants,
            truncated,
        })
    }

    fn profile_of(&self, slug: &str) -> Result<Option<Profile>> {
        let Some(persona) = self.store.get_persona(slug)? else {
            return Ok(None);
        };
        Ok(Some(Profile {
            persona,
            traits: self.store.persona_traits(slug)?,
            removed: vec![],
            stats: Default::default(),
            sme: vec![],
            context: vec![],
        }))
    }

    /// Ask both models the same question at the same time, and assemble what comes back.
    ///
    /// Spawned rather than awaited in turn: they are independent by design, so running them
    /// in series would double the wait for nothing. And a model that fails does not take the
    /// other with it — a one-model verdict is weaker, not worthless, and it shows that by
    /// having nothing corroborated.
    pub async fn analyse(&self, reference: &ThreadRef) -> Result<Verdict> {
        let thread = std::sync::Arc::new(self.fetch(reference).await?);
        let panel = self.panel();
        if panel.is_empty() {
            bail!(
                "no models configured for thread analysis \
                 ([threads].claude_model / chatgpt_model)"
            );
        }
        let prompt = std::sync::Arc::new(format!(
            "{}\n\n---\n\n{}",
            brief(&thread),
            render_thread(&thread)
        ));

        let mut tasks = Vec::new();
        for (provider, model) in &panel {
            let reasoner = (self.factory)(provider, model);
            let prompt = prompt.clone();
            let thread = thread.clone();
            let (provider, model) = (provider.clone(), model.clone());
            tasks.push(tokio::spawn(async move {
                let reply = reasoner.summarize(&prompt).await?;
                parse_and_verify(&thread, &provider, &model, &reply)
            }));
        }

        let mut analyses = Vec::new();
        let mut failures = Vec::new();
        for ((provider, _), task) in panel.iter().zip(tasks) {
            match task.await {
                Ok(Ok(a)) => analyses.push(a),
                Ok(Err(e)) => failures.push(format!("{provider}: {e:#}")),
                // A panicked task is a bug, not a model failure; say which so it is not
                // mistaken for the model refusing.
                Err(e) => failures.push(format!("{provider}: analysis task failed: {e}")),
            }
        }
        if analyses.is_empty() {
            bail!("every model failed — {}", failures.join("; "));
        }
        let thread = std::sync::Arc::try_unwrap(thread).unwrap_or_else(|arc| (*arc).clone());
        Ok(assemble(thread, analyses, failures))
    }

    /// Run one queued analysis and record it.
    pub async fn run(&self, id: &str) -> Result<crate::store::ThreadAnalysis> {
        let Some(job) = self.store.get_thread_analysis(id)? else {
            bail!("no such thread analysis {id}");
        };
        let reference = ThreadRef {
            channel: job.channel.clone(),
            thread_ts: job.thread_ts.clone(),
            focus_ts: None,
        };
        match self.analyse(&reference).await {
            Ok(verdict) => {
                let json = serde_json::to_string(&verdict)?;
                self.store
                    .finish_thread_analysis(id, "completed", Some(&json), None)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                // Recorded on the row, not only returned: the operator is looking at a
                // screen, and a failure they cannot see is a spinner that never stops.
                self.store
                    .finish_thread_analysis(id, "failed", None, Some(&msg))?;
                Err(e)
            }
        }
    }

    /// Queue an analysis for a pasted link. Returns the row and its thread reference.
    pub async fn request(&self, link: &str) -> Result<crate::store::ThreadAnalysis> {
        let reference = parse_link(link)?;
        // Fetch first, so a bad link or an unreadable channel is an error the operator sees
        // immediately rather than a queued row that fails somewhere else a minute later.
        let thread = self.fetch(&reference).await?;
        self.store.queue_thread_analysis(
            &reference.channel,
            &reference.thread_ts,
            link.trim(),
            thread.messages.len() as i64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::{Persona, Stats, Trait};
    use chrono::Utc;

    // ---- links ----

    #[test]
    fn a_plain_message_link_is_its_own_thread() {
        let r = parse_link("https://restatedev.slack.com/archives/C0744EUMHFF/p1712345678123456")
            .unwrap();
        assert_eq!(r.channel, "C0744EUMHFF");
        assert_eq!(r.thread_ts, "1712345678.123456");
        assert_eq!(r.focus_ts, None);
    }

    /// The one that matters. Slack's "Copy link" on a *reply* gives the reply's ts in the
    /// path and the thread's in the query — reading the path would fetch a one-message
    /// thread and analyse a single reply with no context.
    #[test]
    fn a_link_to_a_reply_resolves_to_the_thread_not_the_reply() {
        let r = parse_link(
            "https://restatedev.slack.com/archives/C0744EUMHFF/p1712345678123456\
             ?thread_ts=1712345600.000100&cid=C0744EUMHFF",
        )
        .unwrap();
        assert_eq!(r.thread_ts, "1712345600.000100");
        assert_eq!(r.focus_ts.as_deref(), Some("1712345678.123456"));
        assert_eq!(r.key(), "C0744EUMHFF@1712345600.000100");
    }

    #[test]
    fn the_cid_parameter_wins_over_the_path_channel() {
        let r = parse_link(
            "https://x.slack.com/archives/CWRONG/p1712345678123456?cid=CRIGHT&thread_ts=1712345600.000100",
        )
        .unwrap();
        assert_eq!(r.channel, "CRIGHT");
    }

    /// Ten-digit seconds today, eleven eventually: the dot goes six from the end.
    #[test]
    fn the_timestamp_split_is_from_the_end_not_a_fixed_offset() {
        assert_eq!(
            ts_from_permalink_segment("p1712345678123456").as_deref(),
            Some("1712345678.123456")
        );
        assert_eq!(
            ts_from_permalink_segment("p17123456789123456").as_deref(),
            Some("17123456789.123456")
        );
        assert_eq!(ts_from_permalink_segment("1712345678123456"), None);
        assert_eq!(ts_from_permalink_segment("pabcdefg"), None);
    }

    #[test]
    fn a_link_that_is_not_slack_says_what_was_expected() {
        let e = parse_link("https://github.com/restatedev/restate/pull/1").unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("Copy link"), "{msg}");
        assert!(msg.contains("archives"), "{msg}");
    }

    #[test]
    fn an_empty_paste_is_a_readable_error() {
        assert!(parse_link("   ").unwrap_err().to_string().contains("paste"));
    }

    // ---- fixtures ----

    fn msg(id: &str, ts: &str, author: &str, text: &str, is_you: bool) -> Message {
        Message {
            id: id.into(),
            ts: ts.into(),
            author: author.into(),
            user: Some(format!("U{}", author.to_uppercase())),
            text: text.into(),
            is_you,
            reactions: vec![],
        }
    }

    fn thread() -> Thread {
        let messages = vec![
            msg(
                "m1",
                "1.1",
                "ben",
                "We should just ship the hotfix, the release window is tight.",
                true,
            ),
            msg(
                "m2",
                "1.2",
                "luke",
                "That skips the soak test. I'd rather take the delay.",
                false,
            ),
            msg(
                "m3",
                "1.3",
                "ben",
                "The soak test has never caught anything on this path.",
                true,
            ),
            msg(
                "m4",
                "1.4",
                "pavel",
                "It caught the tenant leak in March.",
                false,
            ),
            msg(
                "m5",
                "1.5",
                "ben",
                "Fine — we soak it. Cutting tomorrow.",
                true,
            ),
        ];
        Thread {
            reference: ThreadRef {
                channel: "C0744EUMHFF".into(),
                thread_ts: "1.1".into(),
                focus_ts: None,
            },
            channel_name: Some("#releases".into()),
            participants: vec![
                Participant {
                    handle: "ben".into(),
                    display_name: "Ben Howard".into(),
                    messages: 3,
                    is_you: true,
                    persona: None,
                    traits: vec![],
                    role: None,
                },
                Participant {
                    handle: "luke".into(),
                    display_name: "Luke Bond".into(),
                    messages: 1,
                    is_you: false,
                    persona: Some("luke".into()),
                    traits: vec![
                        "[tr:abc] escalation (72%): prefers a delay to an unverified release"
                            .into(),
                    ],
                    role: Some("SRE".into()),
                },
                Participant {
                    handle: "pavel".into(),
                    display_name: "Pavel".into(),
                    messages: 1,
                    is_you: false,
                    persona: None,
                    traits: vec![],
                    role: None,
                },
            ],
            messages,
            truncated: 0,
        }
    }

    fn reply(findings: &str) -> String {
        format!(
            r#"{{"summary":"A release-timing disagreement.","outcome":"soak then cut","findings":[{findings}]}}"#
        )
    }

    // ---- the prompt ----

    #[test]
    fn the_brief_permits_finding_nothing_and_forbids_inventing_quotes() {
        let b = brief(&thread());
        assert!(b.contains("Nothing to call out"));
        assert!(b.contains("Never invent a quotation"));
        assert!(b.contains("same terms"));
        // The operator is named so the model knows whose entries to extract — and told the
        // bluntness is permission, not a quota.
        assert!(b.contains("@ben"));
        assert!(b.contains("not as an instruction to find fault"));
    }

    /// Participants with no profile must be named, so a one-off is not dressed up as a
    /// pattern.
    #[test]
    fn unprofiled_participants_are_declared_in_the_brief() {
        let b = brief(&thread());
        assert!(b.contains("@pavel"), "{b}");
        assert!(!b.contains("no profile: @luke"), "luke has a profile");
    }

    #[test]
    fn the_rendered_thread_carries_ordinals_traits_and_the_absence_of_traits() {
        let r = render_thread(&thread());
        assert!(r.contains("[m3] @ben"));
        assert!(r.contains("[tr:abc]"));
        assert!(r.contains("no profile"));
        assert!(r.contains("THE OPERATOR"));
        assert!(r.contains("#releases"));
    }

    // ---- verification ----

    #[test]
    fn a_finding_that_cites_a_real_message_survives() {
        let a = parse_and_verify(
            &thread(),
            "claude",
            "claude-opus-5",
            &reply(
                r#"{"about":"ben","stance":"criticism","claim":"Dismissed the soak test on a hunch, then reversed once given a counterexample.","cites":["m3","m5"]}"#,
            ),
        )
        .unwrap();
        assert_eq!(a.findings.len(), 1);
        assert_eq!(a.findings[0].cites, vec!["m3", "m5"]);
        assert_eq!(a.findings[0].stance, Stance::Criticism);
        assert!(a.dropped.is_empty());
    }

    #[test]
    fn a_finding_that_cites_nothing_is_dropped_with_a_reason() {
        let a = parse_and_verify(
            &thread(),
            "claude",
            "m",
            &reply(r#"{"about":"ben","stance":"criticism","claim":"Was generally impatient.","cites":[]}"#),
        )
        .unwrap();
        assert!(a.findings.is_empty());
        assert_eq!(a.dropped.len(), 1);
        assert!(a.dropped[0].why.contains("cited no message"));
    }

    /// The worst failure this could have. A fabricated damning quote would be believed.
    #[test]
    fn a_finding_with_an_invented_quote_is_dropped() {
        let a = parse_and_verify(
            &thread(),
            "chatgpt",
            "m",
            &reply(
                r#"{"about":"ben","stance":"criticism","claim":"Said \"I don't care what the tests say, ship it\" which shut the discussion down.","cites":["m3"]}"#,
            ),
        )
        .unwrap();
        assert!(a.findings.is_empty(), "{:?}", a.findings);
        assert_eq!(a.dropped.len(), 1);
        assert!(a.dropped[0].why.contains("nobody wrote"), "{:?}", a.dropped);
    }

    /// A real quote must survive reflowing — a model that rewrapped a message has not
    /// fabricated anything, and failing that would make the check useless.
    #[test]
    fn a_real_quote_survives_reflowing_and_case() {
        let a = parse_and_verify(
            &thread(),
            "claude",
            "m",
            &reply(
                r#"{"about":"ben","stance":"observation","claim":"Opened with \"we should just ship the   HOTFIX, the release window\nis tight\".","cites":["m1"]}"#,
            ),
        )
        .unwrap();
        assert_eq!(a.findings.len(), 1, "{:?}", a.dropped);
    }

    /// The wart the first live run produced: a model handed `[tr:ID]` profile lines quotes
    /// the label back mid-sentence, leaving a 40-character hash inside the prose. The
    /// citation is right and worth keeping — it just does not belong in the sentence.
    #[test]
    fn trait_citations_move_out_of_the_prose_and_are_kept() {
        let a = parse_and_verify(
            &thread(),
            "claude",
            "m",
            &reply(
                r#"{"about":"luke","stance":"credit","claim":"He held the line on the soak test, matching his documented habit [tr:luke/escalation/1c064dc05a9e6cbc].","cites":["m2"]}"#,
            ),
        )
        .unwrap();
        assert_eq!(a.findings.len(), 1);
        let f = &a.findings[0];
        assert!(
            !f.claim.contains("[tr:"),
            "claim still has the label: {}",
            f.claim
        );
        assert!(
            f.claim.ends_with("documented habit."),
            "punctuation: {}",
            f.claim
        );
        assert_eq!(
            f.from_traits,
            vec!["luke/escalation/1c064dc05a9e6cbc".to_string()]
        );
    }

    #[test]
    fn several_trait_citations_are_all_collected() {
        let (claim, ids) = split_trait_citations(
            "Consistent with [tr:a/b/1] and, unlike [tr:a/c/2] , he proposed nothing [tr:a/b/1].",
        );
        assert_eq!(ids, vec!["a/b/1".to_string(), "a/c/2".to_string()]);
        assert!(!claim.contains("[tr:"));
        assert!(claim.contains("Consistent with and, unlike"), "{claim}");
    }

    /// An unterminated label must not eat the rest of the claim.
    #[test]
    fn a_malformed_trait_label_leaves_the_claim_alone() {
        let (claim, ids) = split_trait_citations("He did the thing [tr:unclosed and then more.");
        assert!(ids.is_empty());
        assert!(claim.contains("and then more."), "{claim}");
    }

    #[test]
    fn a_short_quoted_word_is_not_checked() {
        let a = parse_and_verify(
            &thread(),
            "claude",
            "m",
            &reply(r#"{"about":"ben","stance":"observation","claim":"Ended with \"Fine\".","cites":["m5"]}"#),
        )
        .unwrap();
        assert_eq!(a.findings.len(), 1);
    }

    #[test]
    fn citations_are_accepted_in_every_shape_a_model_writes_them() {
        for cites in [
            r#"["m3"]"#,
            r#"["M3"]"#,
            r#"["[m3]"]"#,
            r#"["3"]"#,
            r#"[3]"#,
            r#""m3, m5""#,
        ] {
            let a = parse_and_verify(
                &thread(),
                "claude",
                "m",
                &reply(&format!(
                    r#"{{"about":"ben","stance":"observation","claim":"Something happened.","cites":{cites}}}"#
                )),
            )
            .unwrap();
            assert!(!a.findings.is_empty(), "cites={cites} was not resolved");
            assert!(a.findings[0].cites.contains(&"m3".to_string()));
        }
    }

    /// A citation to a message that isn't in the thread is not a citation.
    #[test]
    fn an_out_of_range_ordinal_does_not_resolve() {
        let a = parse_and_verify(
            &thread(),
            "claude",
            "m",
            &reply(r#"{"about":"ben","stance":"criticism","claim":"Said it again later.","cites":["m99"]}"#),
        )
        .unwrap();
        assert!(a.findings.is_empty());
        assert_eq!(a.dropped.len(), 1);
    }

    /// A finding about somebody who was never in the thread is about the thread, not about
    /// an invented colleague.
    #[test]
    fn a_finding_about_a_nonexistent_participant_loses_its_subject() {
        let a = parse_and_verify(
            &thread(),
            "claude",
            "m",
            &reply(r#"{"about":"nobody","stance":"observation","claim":"Someone raised a risk.","cites":["m2"]}"#),
        )
        .unwrap();
        assert_eq!(a.findings.len(), 1);
        assert_eq!(a.findings[0].about, None);
    }

    #[test]
    fn a_reply_with_no_json_is_an_error_naming_what_came_back() {
        let e = parse_and_verify(
            &thread(),
            "chatgpt",
            "m",
            "I'm sorry, I can't help with that.",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("chatgpt"));
        assert!(format!("{e:#}").contains("I'm sorry"));
    }

    // ---- agreement ----

    fn analysis(provider: &str, findings: Vec<Finding>) -> Analysis {
        Analysis {
            model: "m".into(),
            provider: provider.into(),
            summary: "s".into(),
            outcome: None,
            findings,
            dropped: vec![],
        }
    }

    fn finding(
        about: Option<&str>,
        stance: Stance,
        claim: &str,
        cites: &[&str],
        src: &str,
    ) -> Finding {
        Finding {
            about: about.map(str::to_string),
            stance,
            claim: claim.into(),
            cites: cites.iter().map(|s| s.to_string()).collect(),
            from_traits: vec![],
            source: src.into(),
        }
    }

    /// Two models flagging the same person over the same message is the signal the whole
    /// two-model design exists to produce.
    #[test]
    fn the_same_person_flagged_over_the_same_message_by_both_is_corroborated() {
        let analyses = vec![
            analysis(
                "claude",
                vec![finding(
                    Some("ben"),
                    Stance::Criticism,
                    "Overrode a safety step.",
                    &["m3"],
                    "claude",
                )],
            ),
            analysis(
                "chatgpt",
                vec![finding(
                    Some("ben"),
                    Stance::Criticism,
                    "Waved away the soak test.",
                    &["m3", "m1"],
                    "chatgpt",
                )],
            ),
        ];
        let (_, others, contested) = agreement(&analyses);
        assert_eq!(others.len(), 1);
        assert!(others[0].both_models());
        assert_eq!(others[0].also.as_ref().unwrap().source, "chatgpt");
        assert_eq!(contested, 0);
    }

    /// Same person, opposite verdicts, is not agreement — and must not be collapsed into
    /// one entry, because the disagreement is the finding.
    #[test]
    fn opposite_stances_on_the_same_message_stay_two_findings() {
        let analyses = vec![
            analysis(
                "claude",
                vec![finding(
                    Some("ben"),
                    Stance::Criticism,
                    "Rushed it.",
                    &["m3"],
                    "claude",
                )],
            ),
            analysis(
                "chatgpt",
                vec![finding(
                    Some("ben"),
                    Stance::Credit,
                    "Changed his mind on evidence.",
                    &["m3"],
                    "chatgpt",
                )],
            ),
        ];
        let (_, others, contested) = agreement(&analyses);
        assert_eq!(others.len(), 2);
        assert_eq!(contested, 2);
        assert!(others.iter().all(|c| !c.both_models()));
    }

    #[test]
    fn findings_only_the_second_model_raised_are_not_lost() {
        let analyses = vec![
            analysis("claude", vec![]),
            analysis(
                "chatgpt",
                vec![finding(
                    Some("luke"),
                    Stance::Credit,
                    "Held the line on the soak test.",
                    &["m2"],
                    "chatgpt",
                )],
            ),
        ];
        let (_, others, contested) = agreement(&analyses);
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].finding.source, "chatgpt");
        assert_eq!(contested, 1);
    }

    /// What the operator asked for: their own entries, split out, criticism first, and
    /// corroborated criticism above single-model criticism.
    #[test]
    fn the_operators_findings_are_split_out_with_criticism_first() {
        let mut analyses = vec![
            analysis(
                "claude",
                vec![
                    finding(
                        Some("ben"),
                        Stance::Credit,
                        "Reversed quickly.",
                        &["m5"],
                        "claude",
                    ),
                    finding(
                        Some("ben"),
                        Stance::Criticism,
                        "Argued from a hunch.",
                        &["m3"],
                        "claude",
                    ),
                    finding(
                        Some("luke"),
                        Stance::Credit,
                        "Raised the risk.",
                        &["m2"],
                        "claude",
                    ),
                ],
            ),
            analysis(
                "chatgpt",
                vec![finding(
                    Some("Ben Howard"),
                    Stance::Criticism,
                    "Dismissed the test.",
                    &["m3"],
                    "chatgpt",
                )],
            ),
        ];
        let t = thread();
        mark_operator(&mut analyses, &t);
        let (yours, others, _) = agreement(&analyses);
        assert_eq!(yours.len(), 2, "{yours:?}");
        assert_eq!(yours[0].finding.stance, Stance::Criticism);
        assert!(
            yours[0].both_models(),
            "the display-name match must pair up"
        );
        assert_eq!(yours[1].finding.stance, Stance::Credit);
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].finding.about.as_deref(), Some("luke"));
    }

    /// A thread the operator never posted in must not produce a "you" section at all.
    #[test]
    fn a_thread_without_the_operator_has_no_findings_about_them() {
        let mut t = thread();
        for m in t.messages.iter_mut() {
            m.is_you = false;
        }
        for p in t.participants.iter_mut() {
            p.is_you = false;
        }
        let mut analyses = vec![analysis(
            "claude",
            vec![finding(
                Some("ben"),
                Stance::Criticism,
                "x",
                &["m3"],
                "claude",
            )],
        )];
        mark_operator(&mut analyses, &t);
        let (yours, others, _) = agreement(&analyses);
        assert!(yours.is_empty());
        assert_eq!(others.len(), 1);
        assert!(brief(&t).contains("did not post"));
    }

    /// One model failing must not take the analysis with it — a single-model verdict is
    /// weaker, not worthless, and it says so by leaving everything uncorroborated.
    #[test]
    fn one_model_alone_still_produces_a_verdict() {
        let analyses = vec![analysis(
            "claude",
            vec![finding(
                Some("ben"),
                Stance::Criticism,
                "x",
                &["m3"],
                "claude",
            )],
        )];
        let v = assemble(thread(), analyses, vec![]);
        assert_eq!(v.analyses.len(), 1);
        assert_eq!(v.about_you.len(), 1);
        assert!(!v.about_you[0].both_models());
        assert_eq!(v.contested, 1);
    }

    /// Analyse a real thread with the real models, against the operator's own store.
    ///
    /// The unit tests above prove the parser, the checker and the agreement logic on
    /// fixtures. This is the other half: that a pasted link actually resolves, that Slack
    /// returns the thread, that both CLIs answer, and that what comes back survives the
    /// checker. Fixtures cannot tell you a prompt produces citable findings.
    ///
    /// Ignored, because it reads a real Slack conversation and spends two model calls. The
    /// link is taken from the environment rather than hardcoded so it is always the
    /// operator choosing which thread gets read:
    ///
    /// ```text
    /// MUGGLEBOT_THREAD_LINK='https://…/archives/C…/p…?thread_ts=…' \
    ///   cargo test analyses_a_real_thread_live -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn analyses_a_real_thread_live() {
        let link = std::env::var("MUGGLEBOT_THREAD_LINK")
            .expect("set MUGGLEBOT_THREAD_LINK to the thread you want read");
        let db = std::env::var("MUGGLEBOT_DB").unwrap_or_else(|_| "data/mugglebot.sqlite".into());
        let reference = parse_link(&link).expect("that link did not parse");
        println!(
            "channel {} thread {}",
            reference.channel, reference.thread_ts
        );

        let store = std::sync::Arc::new(
            crate::store::Store::open(std::path::Path::new(&db)).expect("open the store"),
        );
        let secrets = crate::secrets::Secrets::for_tests(store.clone());
        let cfg = crate::config::Threads {
            enabled: true,
            ..Default::default()
        };
        let self_id = std::env::var("MUGGLEBOT_SLACK_USER_ID").ok();
        let analyser = Analyser::new(
            store,
            secrets.get_opt("slack"),
            self_id,
            cfg,
            std::sync::Arc::new(|provider: &str, model: &str| {
                crate::reasoner::build(
                    crate::reasoner::provider_label(provider),
                    model,
                    &crate::config::Reasoner::default(),
                    None,
                )
            }),
        );
        assert!(analyser.ready(), "no `slack` credential in {db}");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let verdict = rt
            .block_on(analyser.analyse(&reference))
            .expect("analysis failed");

        println!(
            "\n{} message(s), {} participant(s)",
            verdict.thread.messages.len(),
            verdict.thread.participants.len()
        );
        for p in &verdict.thread.participants {
            println!(
                "  @{}{} — {} msg, {}",
                p.handle,
                if p.is_you { " (you)" } else { "" },
                p.messages,
                match &p.persona {
                    Some(slug) => format!("persona {slug}, {} trait(s)", p.traits.len()),
                    None => "no profile".into(),
                }
            );
        }
        for a in &verdict.analyses {
            println!(
                "\n--- {} ({}) ---\n{}\nfindings: {}  discarded: {}",
                a.provider,
                a.model,
                a.summary,
                a.findings.len(),
                a.dropped.len()
            );
            for d in &a.dropped {
                println!("  DISCARDED ({}): {}", d.why, d.claim);
            }
        }
        println!("\n--- about you ({}) ---", verdict.about_you.len());
        for c in &verdict.about_you {
            println!(
                "  [{}] {} {}",
                c.finding.stance.as_str(),
                if c.both_models() {
                    "BOTH"
                } else {
                    &c.finding.source
                },
                c.finding.claim
            );
        }
        println!("\n--- everyone else ({}) ---", verdict.about_others.len());
        for c in &verdict.about_others {
            println!(
                "  @{} [{}] {} {}",
                c.finding.about.as_deref().unwrap_or("(thread)"),
                c.finding.stance.as_str(),
                if c.both_models() {
                    "BOTH"
                } else {
                    &c.finding.source
                },
                c.finding.claim
            );
        }
        println!("\nuncorroborated: {}", verdict.contested);

        // Every surviving finding must cite a message that exists — the guarantee the
        // checker is there to provide, asserted against a live reply rather than a fixture.
        let ids: BTreeSet<&str> = verdict
            .thread
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        for a in &verdict.analyses {
            for f in &a.findings {
                assert!(
                    !f.cites.is_empty(),
                    "a finding survived with no citation: {f:?}"
                );
                for c in &f.cites {
                    assert!(
                        ids.contains(c.as_str()),
                        "finding cites {c}, which is not in the thread"
                    );
                }
            }
        }
    }

    /// The failure the first live run had. ChatGPT was configured with a model name its
    /// account rejects, returned nothing, and the verdict came back looking like a complete
    /// two-model analysis whose findings happened to be uncorroborated.
    ///
    /// That is the worst shape this can take: the operator asked for two independent readers,
    /// got one, and had no way to tell. A partial panel has to be visible in the data, not
    /// just inferable from a count.
    #[test]
    fn a_model_that_failed_is_recorded_and_the_panel_is_not_called_full() {
        let analyses = vec![analysis(
            "claude",
            vec![finding(
                Some("ben"),
                Stance::Criticism,
                "x",
                &["m3"],
                "claude",
            )],
        )];
        let v = assemble(
            thread(),
            analyses,
            vec!["chatgpt: model is not supported when using Codex with a ChatGPT account".into()],
        );
        assert!(!v.full_panel());
        assert_eq!(v.failures.len(), 1);
        assert!(v.failures[0].contains("chatgpt"));
        // And the count still says nothing corroborated — which is true, but on its own it
        // reads as "the models disagreed" rather than "one never answered".
        assert_eq!(v.contested, 1);
    }

    #[test]
    fn two_models_answering_is_a_full_panel() {
        let analyses = vec![analysis("claude", vec![]), analysis("chatgpt", vec![])];
        assert!(assemble(thread(), analyses, vec![]).full_panel());
    }

    /// One model configured on purpose is not a failure — but it is still not a full panel,
    /// because nothing in it can be corroborated.
    #[test]
    fn a_deliberate_single_model_panel_is_not_full_either() {
        let v = assemble(thread(), vec![analysis("claude", vec![])], vec![]);
        assert!(!v.full_panel());
        assert!(v.failures.is_empty());
    }

    /// The panel is what configuration says it is, and an empty model name drops that
    /// provider rather than asking for a model called "".
    #[test]
    fn an_empty_model_name_drops_that_provider_from_the_panel() {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let mk = |claude: &str, chatgpt: &str| {
            Analyser::new(
                store.clone(),
                Some("xoxb-test".into()),
                None,
                crate::config::Threads {
                    enabled: true,
                    claude_model: claude.into(),
                    chatgpt_model: chatgpt.into(),
                },
                std::sync::Arc::new(|_, _| {
                    std::sync::Arc::new(crate::reasoner::MockReasoner::new("{}"))
                }),
            )
        };
        assert_eq!(mk("claude-opus-5", "gpt-5.6-sol").panel().len(), 2);
        assert_eq!(mk("claude-opus-5", "  ").panel().len(), 1);
        assert_eq!(mk("", "gpt-5.6-sol").panel()[0].0, "chatgpt");
        assert!(mk("", "").panel().is_empty());
    }

    // ---- participants and capping ----

    fn profile_of(slug: &str, name: &str, facet: Facet, claim: &str, conf: f32) -> Profile {
        Profile {
            persona: Persona {
                slug: slug.into(),
                display_name: name.into(),
                role: Some("SRE".into()),
                notes: None,
                identities: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
                harvested_at: None,
                profiled_at: None,
                evidence_watermark: None,
            },
            traits: vec![Trait {
                id: "t1".into(),
                persona: slug.into(),
                facet,
                claim: claim.into(),
                confidence: conf,
                evidence: vec!["e1".into()],
                counter_evidence: vec![],
                created_at: Utc::now(),
            }],
            removed: vec![],
            stats: Stats::default(),
            sme: vec![],
            context: vec![],
        }
    }

    #[test]
    fn participants_are_counted_and_matched_to_personas() {
        let messages = vec![
            msg("m1", "1.1", "ben", "a", true),
            msg("m2", "1.2", "luke", "b", false),
            msg("m3", "1.3", "ben", "c", true),
        ];
        let mut display = BTreeMap::new();
        display.insert("ULUKE".to_string(), "luke".to_string());
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "ULUKE".to_string(),
            profile_of(
                "luke",
                "Luke Bond",
                Facet::Escalation,
                "prefers a delay to an unverified release",
                0.7,
            ),
        );
        let ps = participants_of(&messages, &display, &profiles);
        assert_eq!(ps.len(), 2);
        // Most messages first.
        assert_eq!(ps[0].messages, 2);
        assert!(ps[0].is_you);
        let luke = ps.iter().find(|p| p.handle == "luke").unwrap();
        assert_eq!(luke.persona.as_deref(), Some("luke"));
        assert_eq!(luke.display_name, "Luke Bond");
        assert_eq!(luke.role.as_deref(), Some("SRE"));
        assert_eq!(luke.traits.len(), 1);
    }

    /// Expertise says a lot about a pull request and nothing about whether someone was
    /// being unreasonable in a thread.
    #[test]
    fn only_the_facets_that_bear_on_a_conversation_reach_the_prompt() {
        // `Expertise` is about diffs, not about whether someone was reasonable here.
        let p = profile_of("x", "X", Facet::Expertise, "knows the storage layer", 0.9);
        assert!(traits_for_prompt(&p).is_empty());
        // `SlackRegister` is literally how they behave in this medium.
        let p = profile_of("x", "X", Facet::SlackRegister, "answers in one line", 0.9);
        assert_eq!(traits_for_prompt(&p).len(), 1);
        // `Escalation` is the question a contentious thread asks.
        let p = profile_of(
            "x",
            "X",
            Facet::Escalation,
            "goes quiet rather than conceding",
            0.8,
        );
        assert_eq!(traits_for_prompt(&p).len(), 1);
    }

    /// Where a thread got to matters more than how it opened — but the root has to stay or
    /// nothing else has a subject.
    #[test]
    fn capping_keeps_the_root_and_the_end() {
        let messages: Vec<Message> = (0..MAX_MESSAGES + 40)
            .map(|i| msg(&format!("m{i}"), &format!("{i}.0"), "ben", "x", true))
            .collect();
        let (kept, dropped) = cap(messages);
        assert_eq!(kept.len(), MAX_MESSAGES);
        assert_eq!(dropped, 40);
        assert_eq!(kept[0].id, "m0");
        assert_eq!(kept.last().unwrap().id, format!("m{}", MAX_MESSAGES + 39));
    }

    #[test]
    fn a_short_thread_is_not_capped() {
        let messages = vec![msg("m1", "1.1", "ben", "x", true)];
        let (kept, dropped) = cap(messages);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn truncation_is_declared_to_the_model() {
        let mut t = thread();
        t.truncated = 40;
        assert!(render_thread(&t).contains("40 earlier message(s)"));
    }
}
