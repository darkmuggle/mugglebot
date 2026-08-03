//! Subjects — the durable pieces of work the board is built from.
//!
//! A subject is what a signal is *about*, and there are three kinds, ranked:
//!
//! > **GitHub issue > pull request > Slack thread**
//!
//! Each is keyed by its real upstream identity rather than a synthetic id, which
//! is what lets any watcher, workflow, or tool address one without a lookup table.
//! Attribution climbs as far *up* that ranking as it can resolve (see [`resolve`]),
//! and the highest rank that resolves owns the signal.
//!
//! Everything else a signal mentions — repo, environment, service, channel,
//! person, branch, commit — is a *resolution key* and context. Nothing is keyed on
//! those: they're long-lived and shared, so keying a subject on one collapses a
//! repository's whole history into a single card.
//!
//! This replaces the earlier synthetic `Thread`, which was invented by the
//! grouping engine and keyed by an internal id.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

use crate::signal::{ResolutionKey, Severity, Signal};

pub mod attach;
pub mod projection;
pub mod resolve;
pub mod store;

pub use attach::Attributor;

/// How authoritative a subject is. Declaration order defines the ordering, so
/// `rank > other.rank` works directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectRank {
    /// A Slack conversation — a subject only when no GitHub artifact resolves.
    SlackThread,
    /// One attempt at the work.
    PullRequest,
    /// The durable statement of what the work is. GitHub Discussions share this
    /// rank: a discussion is also a standing statement of a problem rather than an
    /// attempt at one. Their key form differs (`~` vs `#`) because issue and
    /// discussion numbering are independent, so `repo#5` and `repo~5` are
    /// genuinely different things.
    Issue,
    /// An incident.io incident. Highest rank, and it never loses one: production being
    /// broken outranks every artifact describing it.
    ///
    /// The rank is close to moot in practice, because an incident is **never merged into
    /// another subject** — see `Attributor`. It relates to the issues, pull requests and
    /// commits it turns out to be about by *edge*, which is what keeps it on its own board
    /// instead of disappearing into whatever it was eventually filed as.
    Incident,
}

impl SubjectRank {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectRank::SlackThread => "slack_thread",
            SubjectRank::PullRequest => "pull_request",
            SubjectRank::Issue => "issue",
            SubjectRank::Incident => "incident",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "slack_thread" => Some(SubjectRank::SlackThread),
            "pull_request" => Some(SubjectRank::PullRequest),
            "issue" => Some(SubjectRank::Issue),
            "incident" => Some(SubjectRank::Incident),
            _ => None,
        }
    }
}

/// The identity of a subject, and its address everywhere: a SQLite column, a URL
/// path segment, an MCP argument, and (from Phase 2) a Restate virtual-object key.
///
/// One canonical string form with a validating parser beats structured access,
/// because every one of those consumers wants the string.
///
/// | Form | Kind |
/// |---|---|
/// | `owner/repo#412` | issue |
/// | `owner/repo~7` | discussion (issue rank) |
/// | `owner/repo!987` | pull request |
/// | `C02ABC/1721822400.001` | Slack thread (`channel/thread_ts`) |
/// | `incident:INC-448` | incident.io incident |
///
/// The incident form leads with a literal prefix rather than a sigil because every sigil is
/// taken and an incident reference has no repo to hang one off. It is matched first, so
/// nothing else can claim it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubjectKey(String);

/// Marks an incident key. A prefix rather than a sigil — see the table above.
pub const INCIDENT_PREFIX: &str = "incident:";

impl SubjectKey {
    pub fn issue(repo: &str, number: u64) -> Self {
        Self(format!("{repo}#{number}"))
    }

    pub fn discussion(repo: &str, number: u64) -> Self {
        Self(format!("{repo}~{number}"))
    }

    pub fn pull_request(repo: &str, number: u64) -> Self {
        Self(format!("{repo}!{number}"))
    }

    pub fn slack_thread(channel_and_ts: &str) -> Self {
        Self(channel_and_ts.to_string())
    }

    /// `incident:INC-448`, from incident.io's own human reference.
    ///
    /// Keyed on the reference, not the ULID: the reference is what appears in Slack, in
    /// alert titles and in what people type, so it is the identity that lets a signal from
    /// anywhere else resolve onto the same subject. The ULID is carried on the signal for
    /// API calls.
    pub fn incident(reference: &str) -> Self {
        Self(format!("{INCIDENT_PREFIX}{}", reference.trim()))
    }

    /// Parse a key, rejecting anything whose kind can't be determined. Called on
    /// every externally-supplied key (MCP arguments, URL segments) so a typo fails
    /// loudly instead of creating an unreachable subject.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            bail!("empty subject key");
        }
        let key = Self(s.to_string());
        // `rank` is what makes a key meaningful; if it can't be determined, the key
        // isn't one.
        key.try_rank()?;
        Ok(key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn rank(&self) -> SubjectRank {
        // Keys in the store were validated on the way in.
        self.try_rank().unwrap_or(SubjectRank::SlackThread)
    }

    fn try_rank(&self) -> Result<SubjectRank> {
        let s = &self.0;
        // First, and by prefix: an incident reference contains none of the sigils below, so
        // without this branch `incident:INC-448` fails to parse rather than being
        // misclassified — but it must be claimed here before `/` can see a `repo/name`.
        if s.starts_with(INCIDENT_PREFIX) {
            Ok(SubjectRank::Incident)
        } else if s.contains('#') || s.contains('~') {
            Ok(SubjectRank::Issue)
        } else if s.contains('!') {
            Ok(SubjectRank::PullRequest)
        } else if s.contains('/') {
            // `channel/thread_ts` — the only remaining shape.
            Ok(SubjectRank::SlackThread)
        } else {
            bail!("'{s}' is not a subject key (expected owner/repo#N, owner/repo~N, owner/repo!N, channel/ts, or incident:INC-N)")
        }
    }

    /// `owner/repo` for a GitHub subject; `None` for a Slack thread.
    pub fn repo(&self) -> Option<&str> {
        let idx = self.0.find(['#', '~', '!'])?;
        Some(&self.0[..idx])
    }

    /// The upstream number for a GitHub subject.
    pub fn number(&self) -> Option<u64> {
        let idx = self.0.find(['#', '~', '!'])?;
        self.0[idx + 1..].parse().ok()
    }

    /// The incident.io reference (`INC-448`) for an incident key, else `None`.
    pub fn incident_reference(&self) -> Option<&str> {
        self.0.strip_prefix(INCIDENT_PREFIX)
    }

    /// A GitHub *discussion* (`owner/repo~7`) rather than an issue (`owner/repo#7`).
    ///
    /// Both carry [`SubjectRank::Issue`], and both answer `number()`, so anything that
    /// reaches the REST issues API has to tell them apart: discussion 7 and issue 7 are
    /// unrelated objects in the same repo, and discussions are not on that API at all.
    /// Asking it for a discussion's comments returns some other conversation entirely.
    pub fn is_discussion(&self) -> bool {
        self.0.contains('~')
    }
}

impl fmt::Display for SubjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the operator has done about a subject.
///
/// This lives on the *subject*, not on each signal, which is the point: half a
/// PR's CI failures being acknowledged was never a coherent thing to express, and
/// the old "a thread is only as handled as its least-handled member" min-fold
/// existed to paper over that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Handled {
    Open,
    Seen,
    Acknowledged,
    Snoozed,
    Resolved,
}

impl Handled {
    pub fn as_str(self) -> &'static str {
        match self {
            Handled::Open => "open",
            Handled::Seen => "seen",
            Handled::Acknowledged => "acknowledged",
            Handled::Snoozed => "snoozed",
            Handled::Resolved => "resolved",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" | "unseen" => Some(Handled::Open),
            "seen" => Some(Handled::Seen),
            "acknowledged" => Some(Handled::Acknowledged),
            "snoozed" => Some(Handled::Snoozed),
            "resolved" => Some(Handled::Resolved),
            _ => None,
        }
    }

    /// Settled work: never re-analyzed on a cloud model, and muted for
    /// notifications. Only the local reopen classifier may look at it.
    pub fn is_handled(self) -> bool {
        matches!(
            self,
            Handled::Acknowledged | Handled::Snoozed | Handled::Resolved
        )
    }
}

/// A durable piece of work, plus what MuggleBot knows about it.
///
/// Small and hot by design: bodies, artifacts, and embeddings live in their own
/// tables and are referenced from here. From Phase 2 this is the state of a
/// Restate virtual object, which is the other reason to keep it small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subject {
    pub key: SubjectKey,
    pub rank: SubjectRank,
    pub title: String,
    /// Deterministic one-liner always; replaced by the LLM summary once a
    /// reasoning pass runs.
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_reasoned_at: Option<DateTime<Utc>>,
    /// The operator is active here (live assist).
    pub live: bool,
    /// Categorical routing tags from the shared vocabulary.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tags were set by a human and must not be overwritten by the classifier.
    #[serde(default)]
    pub tags_pinned: bool,
    /// Operator triage state.
    pub handled: Handled,
    pub snoozed_until: Option<DateTime<Utc>>,
    /// Set when this subject was merged into another: activity forwards there and
    /// it no longer appears on the board on its own.
    pub same_as: Option<SubjectKey>,
    /// Parent issue, for a PR resolved through a closing keyword.
    pub parent: Option<SubjectKey>,
    /// Deterministic merge key within the Slack rank — an environment id. Two alert threads
    /// naming the same environment are the same incident, and this is what makes that a lookup
    /// rather than a model judgment.
    ///
    /// A field on the record rather than a column, now that the subject *is* its object's state:
    /// the `subjects` table was the only thing carrying it.
    #[serde(default)]
    pub merge_key: Option<String>,
}

impl Subject {
    /// A fresh subject for `key`, titled from the signal that created it.
    pub fn new(key: SubjectKey, s: &Signal, now: DateTime<Utc>) -> Self {
        Self {
            rank: key.rank(),
            key,
            title: title_from(s),
            summary: None,
            created_at: now,
            updated_at: now,
            last_reasoned_at: None,
            live: false,
            tags: Vec::new(),
            tags_pinned: false,
            handled: Handled::Open,
            snoozed_until: None,
            same_as: None,
            parent: None,
            merge_key: None,
        }
    }
}

/// A subject with its members and derived attributes, as returned to clients.
#[derive(Debug, Clone, Serialize)]
pub struct SubjectView {
    #[serde(flatten)]
    pub subject: Subject,
    /// The one line the board row shows: `summary` reduced to a single plain-text
    /// sentence (see [`headline_from`]), or `None` when there is no usable summary
    /// yet — which the row says out loud rather than filling with truncated markdown.
    ///
    /// Derived on read, not stored: it must not be able to disagree with `summary`.
    pub headline: Option<String>,
    /// A pull request's review decision — `approved`, `changes_requested`, `commented` —
    /// or `None` when nobody has reviewed it (and on anything that isn't a PR).
    ///
    /// The board shows this instead of asking for attention on work a human has already
    /// signed off. Derived from the signal feed, so it moves when the review does.
    pub review_state: Option<String>,
    /// This pull request has cleared **every** gate — approved, and nothing still failing.
    /// See [`crate::subject::projection::gates_passed`] for what that does and doesn't
    /// claim. False on anything that isn't a signed-off PR.
    ///
    /// Derived here rather than re-derived per surface: the board row's badge, the detail
    /// panel, and `attention.reason` are all asserting the same thing, and three copies of
    /// the rule is three chances for them to disagree in front of the operator.
    pub gates_passed: bool,
    pub signals: Vec<Signal>,
    /// Resolution keys and context drawn from the members, for display.
    pub keys: Vec<ResolutionKey>,
    pub severity: Severity,
    pub edges: Vec<crate::correlation::Edge>,
    pub context: Vec<crate::correlation::SubjectContext>,
    /// Child PRs (on an issue) and contributing Slack threads/meetings.
    #[serde(default)]
    pub children: Vec<SubjectKey>,
    /// The attempts at this issue: each open PR with what it implements, MuggleBot's
    /// critique of the diff, and what reviewers actually said.
    ///
    /// On the view rather than fetched separately because the nesting *is* the answer
    /// to "what's the state of this?" — an issue whose PRs you have to click through
    /// to see reads as an issue nobody is working on.
    #[serde(default)]
    pub pull_requests: Vec<crate::store::PrFix>,
    /// Distilled explanations of this subject and everything under it — the local one the
    /// board writes on its own, and the cloud one if the operator asked for a second
    /// opinion. Both, so the panel can show them side by side and label which is which.
    pub explanations: Vec<crate::store::Explanation>,
    /// Does this need the operator, and has the AI actually looked at it?
    pub attention: Attention,
}

/// The two questions the board exists to answer.
///
/// Triage state is bookkeeping — it records what you *did*, which is not what you
/// want to read at a glance. What you want is: **does this need me**, and **has the
/// AI been over it** (and at whose expense).
#[derive(Debug, Clone, Serialize)]
pub struct Attention {
    /// Needs a human. Derived — not a stored flag to keep in sync.
    pub needed: bool,
    /// Why, in a few words, so the badge is explainable rather than mysterious.
    pub reason: Option<String>,
    /// Which AI decorations exist. An undecorated subject is one you're reading raw.
    pub decorated: Decorations,
}

/// Per-facet record of what the AI has produced for a subject, and where the work
/// ran.
///
/// Split by tier because "has the AI paid attention" and "what did it cost me" are
/// different questions: `local_passes` ran on this machine (fans up, battery down),
/// `cloud_passes` is metered.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Decorations {
    /// A grounded summary has been written (not just the deterministic one-liner).
    pub summary: bool,
    /// Routing tags were classified.
    pub tags: bool,
    /// A dashboard behind a linked alert was actually read.
    pub dashboard: bool,
    /// Root-cause investigation status: `complete`, `running`, `failed`, or absent.
    pub root_cause: Option<String>,
    /// Assigned-issue triage status, if this subject is an assigned issue.
    pub triage: Option<String>,
    /// How many associated pull requests have been judged.
    pub prs_judged: usize,
    /// Completed AI artifacts produced on-device.
    pub local_passes: u32,
    /// Completed AI artifacts that cost a metered call.
    pub cloud_passes: u32,
}

impl Decorations {
    /// Has the AI done anything at all here?
    pub fn any(&self) -> bool {
        self.summary
            || self.tags
            || self.dashboard
            || self.root_cause.is_some()
            || self.triage.is_some()
            || self.prs_judged > 0
    }
}

/// Resolution-key kinds that exist only as internal grouping keys and carry no
/// display value (opaque ids like a Slack conversation ts). Kept on the signal,
/// hidden from the view.
const HIDDEN_KINDS: &[&str] = &["slack_thread"];

/// The distinct resolution keys across a subject's members, for display.
pub fn union_keys(signals: &[Signal]) -> Vec<ResolutionKey> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for s in signals {
        for k in &s.keys {
            if HIDDEN_KINDS.contains(&k.kind.to_ascii_lowercase().as_str()) {
                continue;
            }
            let dedup = format!(
                "{}:{}",
                k.kind.to_ascii_lowercase(),
                k.value.to_ascii_lowercase()
            );
            if seen.insert(dedup) {
                out.push(k.clone());
            }
        }
    }
    out
}

pub fn title_from(s: &Signal) -> String {
    let t = s.title.trim();
    if t.is_empty() {
        format!("{} · {}", s.source, s.external_id)
    } else {
        t.to_string()
    }
}

/// How long a board row's headline is allowed to be before it is cut. Sized to the
/// row, not to the model: past roughly this the line wraps and the row stops being
/// one line tall, which is the entire point of it.
const HEADLINE_MAX: usize = 160;

/// Fragments of the summariser's own instructions. A summary containing one of
/// these is the model reciting the brief instead of doing it.
const PROMPT_ECHOES: &[&str] = &[
    "(blast radius",
    "(current outcome",
    "(what to do now",
    "labeled sections",
    "at most 120 characters",
    "only when the evidence includes",
    "give the call —",
];

/// Is this summary content, or a failed pass wearing content's clothes?
///
/// Three failures show up in practice and all are worse than no summary at all,
/// because storing any of them also sets `last_reasoned_at` and so convinces the
/// board it has a real summary and nothing retries:
///
/// 1. **Prompt echo** — the model repeats the section brief back, so the board
///    shows the operator `**Impact:** (blast radius)`.
/// 2. **Evidence dump** — the model pastes the `[sig:ID] source · kind · time:` lines
///    it was given. Legitimate summaries *do* cite `[sig:ID]` inline, so the tell is
///    not the citation but the signal-line shape following it.
/// 3. **Transcript** — asked to resolve the conversation, the model pastes the
///    conversation instead. This is the one that looks most like success: a wall of
///    real quotes from real people, with nobody's question actually answered. The
///    whole point of the Conversation section is that somebody has to take a side,
///    so a section that only reproduces the discussion has done none of the work.
pub fn is_usable_summary(summary: &str) -> bool {
    let text = summary.trim();
    if text.is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if PROMPT_ECHOES.iter().any(|e| lower.contains(e)) {
        return false;
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    // A pasted evidence line: a signal citation plus the ` · `-separated header that
    // `signal_line` emits.
    let dumped = lines
        .iter()
        .filter(|l| l.contains("[sig:") && l.matches(" · ").count() >= 2)
        .count();
    // One such line in a long summary is a heavily-cited sentence; half of them is a
    // transcript.
    if dumped * 2 >= lines.len().max(1) {
        return false;
    }
    // A pasted comment header, the shape `Comment::render` emits: `[review] alice
    // (APPROVED) 2026-07-27:` or `[discussion] bob 2026-07-27:`. Two or more of these
    // is the conversation reproduced rather than resolved. One can legitimately appear
    // when a call quotes the comment it is answering.
    quoted_comment_headers(&lines) < 2
}

/// How many lines open with a rendered comment header (see `github::Comment::render`).
fn quoted_comment_headers(lines: &[&str]) -> usize {
    lines
        .iter()
        .filter(|l| {
            let t = l.trim().trim_start_matches(['-', '*', '>', ' ']);
            let Some(rest) = t.strip_prefix('[') else {
                return false;
            };
            let Some((kind, after)) = rest.split_once(']') else {
                return false;
            };
            // The three kinds the GitHub client emits, and nothing else — `[sig:…]`
            // citations and markdown links must not match.
            matches!(kind, "review" | "discussion" | "review_comment") && after.contains(':')
        })
        .count()
}

/// The one line the board shows for a subject.
///
/// Prefers the `**Headline:**` the summariser is asked to open with; falls back to
/// the first sentence of `**Status:**`, then to the first sentence of anything. The
/// result is plain text: citations, markdown emphasis and links are stripped,
/// because a row is not a place to render them.
pub fn headline_from(summary: Option<&str>) -> Option<String> {
    let text = summary.map(str::trim).filter(|s| !s.is_empty())?;
    if !is_usable_summary(text) {
        return None;
    }
    let normalised = split_labels(text);
    let candidate = labelled_section(&normalised, "headline")
        .or_else(|| labelled_section(&normalised, "status"))
        .unwrap_or_else(|| normalised.clone());
    let flat = strip_for_row(&candidate);
    let sentence = first_sentence(&flat);
    let out = truncate_on_word(sentence, HEADLINE_MAX);
    (!out.is_empty()).then_some(out)
}

/// Put every `**Label:**` at the start of its own line.
///
/// The prompt asks for the sections separated by blank lines, and a model that runs them
/// together on one line used to defeat section extraction entirely: the line-based scan
/// found `Headline:` and then read to the end of the line, so a board row showed the
/// headline, the status and the impact concatenated. Normalising first means the
/// extractor only has to handle one shape.
/// Not a byte index in sight. Summaries are full of em-dashes and arrows, and the first
/// version of this sliced at fixed offsets — `after[..l.len()]` landed inside a `—` and
/// panicked, which took down every board read, because the projection is what serves
/// them. `str::get` returns `None` on a non-boundary instead of aborting.
fn split_labels(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut last = 0;
    for (i, _) in text.match_indices("**") {
        // `match_indices` yields non-overlapping matches, but `last` also guards the
        // closing `**` of a label we have already consumed.
        if i < last {
            continue;
        }
        let after = &text[i + 2..];
        let is_label = LABELS
            .iter()
            .any(|l| after.get(..l.len()).is_some_and(|p| p.eq_ignore_ascii_case(l)));
        out.push_str(&text[last..i]);
        if is_label && !out.is_empty() && !out.ends_with('\n') {
            out.push_str("\n\n");
        }
        out.push_str("**");
        last = i + 2;
    }
    out.push_str(&text[last..]);
    out
}

/// The section labels the summariser is asked for, without their markdown.
const LABELS: &[&str] = &[
    "headline:",
    "status:",
    "impact:",
    "conversation:",
    "next:",
    "next steps:",
];

/// The body of a `**Label:**` section, up to the next blank line or next label.
fn labelled_section(text: &str, label: &str) -> Option<String> {
    let needle = format!("{label}:");
    let mut out = String::new();
    let mut found = false;
    for line in text.lines() {
        let bare = line.trim().trim_start_matches(['*', '#', '-', ' ']);
        let lower = bare.to_ascii_lowercase();
        if found {
            // A blank line or the next label ends the section.
            if line.trim().is_empty() || is_label_line(bare) {
                break;
            }
            out.push(' ');
            out.push_str(line.trim());
            continue;
        }
        if lower.starts_with(&needle) {
            found = true;
            out.push_str(bare[needle.len()..].trim_start_matches(['*', ' ']).trim());
        }
    }
    found.then(|| out.trim().to_string()).filter(|s| !s.is_empty())
}

/// Does this line open one of the summariser's labelled sections?
fn is_label_line(bare: &str) -> bool {
    let lower = bare.to_ascii_lowercase();
    LABELS.iter().any(|l| lower.starts_with(l))
}

/// Reduce summary markdown to plain single-line text fit for a table row.
fn strip_for_row(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Drop `[sig:…]`, `[mem:…]`, `[cause:REF]` citations wholesale, but keep
            // the text of a real markdown link `[text](url)`.
            '[' => {
                let mut inner = String::new();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == ']' {
                        break;
                    }
                    inner.push(n);
                }
                let is_citation = ["sig:", "mem:", "ctx:", "cause:", "browser:"]
                    .iter()
                    .any(|p| inner.starts_with(p));
                if !is_citation {
                    out.push_str(&inner);
                }
                // Swallow a following `(…)` target either way.
                if chars.peek() == Some(&'(') {
                    for n in chars.by_ref() {
                        if n == ')' {
                            break;
                        }
                    }
                }
            }
            // Emphasis and code markers only. `_` and `#` are deliberately absent: both
            // are rarer as markup here than inside the things these summaries quote, and
            // stripping them turned `review_requested` into `reviewrequested` and `#25`
            // into `25`. Leading `#` heading markers are handled by `labelled_section`,
            // which trims them off the line.
            '*' | '`' => {}
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    // Removing an inline `[sig:…]` leaves the space that preceded it stranded in front
    // of the sentence's own punctuation — "the last blocker ." — so close those up.
    let mut tidy = String::with_capacity(flat.len());
    let mut chars = flat.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' '
            && chars
                .peek()
                .is_some_and(|n| matches!(n, '.' | ',' | ';' | ':' | '!' | '?' | ')'))
        {
            continue;
        }
        tidy.push(c);
    }
    tidy
}

/// The first sentence, or the whole thing if it has no terminator. Abbreviations
/// aren't worth handling here — the result is a row preview, not a citation.
fn first_sentence(text: &str) -> &str {
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?')
            // Not a decimal point or a version number.
            && bytes.get(i + 1).is_none_or(|n| n.is_ascii_whitespace())
        {
            return text[..=i].trim_end();
        }
    }
    text
}

/// Cut at a word boundary and mark the cut, so a row never ends mid-word.
fn truncate_on_word(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let stem = match cut.rfind(' ') {
        Some(i) if i > max / 2 => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{}…", stem.trim_end_matches([',', ';', ':', ' ']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_forms_parse_to_their_ranks() {
        let cases = [
            ("restatedev/restate#412", SubjectRank::Issue),
            ("restatedev/restate~7", SubjectRank::Issue),
            ("restatedev/restate!987", SubjectRank::PullRequest),
            ("C02ABC/1721822400.001", SubjectRank::SlackThread),
        ];
        for (raw, rank) in cases {
            let k = SubjectKey::parse(raw).expect(raw);
            assert_eq!(k.rank(), rank, "{raw}");
            assert_eq!(k.as_str(), raw);
        }
    }

    #[test]
    fn a_bare_word_is_not_a_key() {
        // Rejecting these is the point: a subject nobody can address is worse than
        // an error, because it silently accumulates activity nothing displays.
        for bad in ["", "   ", "restate", "412"] {
            assert!(SubjectKey::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn issue_outranks_pr_outranks_slack() {
        assert!(SubjectRank::Issue > SubjectRank::PullRequest);
        assert!(SubjectRank::PullRequest > SubjectRank::SlackThread);
    }

    #[test]
    fn discussion_and_issue_numbering_do_not_collide() {
        let issue = SubjectKey::issue("o/r", 5);
        let discussion = SubjectKey::discussion("o/r", 5);
        assert_ne!(issue, discussion);
        assert_eq!(issue.rank(), discussion.rank());
        assert_eq!(issue.number(), discussion.number());
        // Same rank and same number, different objects — so anything that reaches the
        // REST issues API for comments has to tell them apart.
        assert!(discussion.is_discussion());
        assert!(!issue.is_discussion());
        assert!(!SubjectKey::pull_request("o/r", 5).is_discussion());
        assert!(!SubjectKey::slack_thread("C02ABC/1721822400.001").is_discussion());
    }

    #[test]
    fn repo_and_number_come_back_out() {
        let k = SubjectKey::pull_request("restatedev/restate", 987);
        assert_eq!(k.repo(), Some("restatedev/restate"));
        assert_eq!(k.number(), Some(987));
        let slack = SubjectKey::slack_thread("C02ABC/1721822400.001");
        assert_eq!(slack.repo(), None);
        assert_eq!(slack.number(), None);
    }

    /// The real summary stored for restate-cloud#1179: the model recited the section
    /// brief instead of writing the sections, and the board rendered it.
    const PROMPT_ECHO: &str = "**Status:** (current outcome, including whether a later \
         success cleared a failure), \r\n**Impact:** (blast radius), and \r\n**Next:** (what \
         to do now, or explicitly say no action is needed). Cite the evidence";

    /// The real summary stored for gcp-gke-sandbox#25: the evidence block, pasted.
    const EVIDENCE_DUMP: &str = "**Status:** [sig:github/24809234539@2026-07-28T11:17:58Z] \
         github · review_requested · 2026-07-28T11:17:58+00:00: linkerd multi replica — \
         restatedev/gcp-gke-sandbox · review_requested · open · @lukebond\n\n\
         [sig:github/assigned/restatedev/gcp-gke-sandbox#25] github · assigned · \
         2026-07-28T11:35:16+00:00: PR: linkerd multi replica — stacked on #24.\n";

    #[test]
    fn a_recited_prompt_is_not_a_summary() {
        assert!(!is_usable_summary(PROMPT_ECHO));
        assert_eq!(headline_from(Some(PROMPT_ECHO)), None);
    }

    #[test]
    fn a_pasted_evidence_block_is_not_a_summary() {
        assert!(!is_usable_summary(EVIDENCE_DUMP));
        assert_eq!(headline_from(Some(EVIDENCE_DUMP)), None);
    }

    #[test]
    fn a_cited_sentence_is_still_a_summary() {
        // The guard must not reject legitimate citation, which is the house style.
        let good = "**Status:** The tunnel conflict is the last blocker [sig:github/9].\n\n\
                    **Impact:** Human UI access only.\n\n**Next:** Resolve it, then merge.";
        assert!(is_usable_summary(good));
        assert_eq!(
            headline_from(Some(good)).as_deref(),
            Some("The tunnel conflict is the last blocker."),
        );
    }

    #[test]
    fn the_headline_section_wins_when_present() {
        let s = "**Headline:** Approved by pcholakov, pending one cleanup.\n\n\
                 **Status:** The PR bumps the ingress image to PR1200.\n\n**Next:** Merge.";
        assert_eq!(
            headline_from(Some(s)).as_deref(),
            Some("Approved by pcholakov, pending one cleanup."),
        );
    }

    #[test]
    fn a_headline_is_one_plain_line() {
        let s = "**Headline:** The `restate-cloud` bump to \
                 [PR1200](https://github.com/restatedev/restate-cloud/pull/1200) is\nready \
                 [sig:github/1]. Second sentence is dropped.";
        assert_eq!(
            headline_from(Some(s)).as_deref(),
            Some("The restate-cloud bump to PR1200 is ready."),
        );
    }

    /// The shape the summariser now produces: a Conversation section that answers each
    /// participant, between Impact and Next.
    const WITH_CONVERSATION: &str = "**Headline:** Approved pending the tunnel conflict — \
         drop that change and merge.\n\n\
         **Status:** The image bump is approved [sig:github/9].\n\n\
         **Impact:** Human UI access only.\n\n\
         **Conversation:**\n\
         - pcholakov approved but wants the tunnel conflict resolved first — go with \
         dropping the change; they flagged it as probably unnecessary.\n\
         - darkmuggle asks whether to keep the tag — answer no, the reverse-lookup tool \
         settles it.\n\n\
         **Next:** Drop the tunnel change, then merge.";

    /// What the local model actually produced for nuon-byoc!140 on the first pass with
    /// conversations wired in: it pasted the thread instead of resolving it. Trimmed,
    /// but the header shapes are verbatim.
    const TRANSCRIPT: &str = "**Status:** The incident is under investigation.\n\n\
         **Impact:** A subset of JWT-authenticated users.\n\n\
         **Conversation:**\n\
         [review] pcholakov (APPROVED) 2026-07-27: Approved assuming you'll resolve the \
         tunnel conflict - probably safe to drop that change altogether.\n\n\
         [discussion] pcholakov 2026-07-27: Tunnel tag has probably already overtaken \
         your change. Might be worth checking and resolving it.\n\n\
         [discussion] darkmuggle 2026-07-27: That is the current tunnel on the main \
         branch. I'll wait to pull the trigger.\n\n\
         **Next:** The engineer will continue investigating.";

    #[test]
    fn a_pasted_thread_is_not_a_resolved_conversation() {
        // The failure that looks most like success: real quotes from real people, and
        // nobody's question answered. Storing it would also mark the subject summarised.
        assert!(!is_usable_summary(TRANSCRIPT));
        assert_eq!(headline_from(Some(TRANSCRIPT)), None);
    }

    #[test]
    fn a_call_may_quote_the_comment_it_answers() {
        // One header is a call citing what it responds to, which is not a transcript.
        let resolved = "**Headline:** Drop the tunnel change, then merge.\n\n\
             **Status:** Approved with one condition.\n\n\
             **Conversation:**\n\
             - pcholakov is blocking on the tunnel conflict ([review] pcholakov \
             (APPROVED) 2026-07-27:) — go with dropping it; they called it redundant.\n\
             - darkmuggle is waiting for a second pair of eyes — answer yes, deploy \
             together to canary.\n\n\
             **Next:** Drop the change and merge.";
        assert!(is_usable_summary(resolved));
        assert_eq!(
            headline_from(Some(resolved)).as_deref(),
            Some("Drop the tunnel change, then merge."),
        );
    }

    #[test]
    fn labels_run_together_on_one_line_still_split() {
        // What the local model actually returned: every label inline, no blank lines. The
        // line-based extractor read to the end of the line and produced a "headline" that
        // was three sections concatenated.
        let inline = "**Headline:** Restate-cloud image bump to PR1200 **Status:** Approved \
             pending the tunnel conflict **Impact:** Low, only affects staging. \
             **Next:** Drop that change and merge.";
        assert_eq!(
            headline_from(Some(inline)).as_deref(),
            Some("Restate-cloud image bump to PR1200"),
        );
    }

    #[test]
    fn multibyte_text_around_a_label_does_not_panic() {
        // The regression that took the board API down: an em-dash sitting where a label
        // comparison sliced, so `after[..\"next steps:\".len()]` cut mid-character.
        // Every one of these panicked before; the assertion is mostly that we return.
        for text in [
            "**Headline:** Restate-cloud — bump the image and merge.",
            "**Status:** approved — merge.\n\n**Next:** — drop the tunnel change",
            "**—** an em-dash where a label would be",
            "**Headline:** ok**",
            "**",
            "****",
            "**Headline:**",
            "→**Next:**→",
            "**Conversation:** @a wants → go with it",
        ] {
            let _ = headline_from(Some(text));
            let _ = split_labels(text);
            let _ = is_usable_summary(text);
        }
        // And the useful case still works with multibyte either side of the label.
        assert_eq!(
            headline_from(Some("**Headline:** Approved — drop it. **Status:** x")).as_deref(),
            Some("Approved — drop it."),
        );
    }

    #[test]
    fn splitting_labels_leaves_ordinary_bold_alone() {
        // `**Bottom line**` is not one of the section labels and must not gain a break.
        let text = "**Headline:** All good. Some **emphasis** mid-sentence.";
        let split = split_labels(text);
        assert!(!split.contains("Some\n\n**emphasis**"), "{split:?}");
        assert_eq!(headline_from(Some(text)).as_deref(), Some("All good."));
    }

    #[test]
    fn a_conversation_section_does_not_leak_into_the_headline() {
        // `Status:` must stop at the next label. Before `Conversation:` was a known
        // label, extraction ran straight through it and the row showed the whole thread.
        assert!(is_usable_summary(WITH_CONVERSATION));
        assert_eq!(
            headline_from(Some(WITH_CONVERSATION)).as_deref(),
            Some("Approved pending the tunnel conflict — drop that change and merge."),
        );
    }

    #[test]
    fn the_status_section_still_ends_at_the_next_label() {
        // Same summary with the headline removed: the fallback path has to stop at
        // `Impact:` rather than swallowing the conversation behind it.
        let no_headline = WITH_CONVERSATION
            .split_once("**Status:**")
            .map(|(_, rest)| format!("**Status:**{rest}"))
            .expect("status section");
        assert_eq!(
            headline_from(Some(&no_headline)).as_deref(),
            Some("The image bump is approved."),
        );
    }

    #[test]
    fn the_conversation_instructions_are_not_a_summary() {
        // The new section brought its own instructions to recite back.
        for echo in [
            "**Conversation:** (only when the evidence includes a conversation)",
            "For each participant, name them and give the CALL — go with their approach.",
        ] {
            assert!(!is_usable_summary(echo), "{echo:?} passed the guard");
        }
    }

    #[test]
    fn an_identifier_survives_emphasis_stripping() {
        // Live regression: `_` was treated as emphasis, so a summary quoting a signal
        // kind rendered "the reviewrequested signal" on the board.
        let s = "**Headline:** The review_requested signal names PR #25.";
        assert_eq!(
            headline_from(Some(s)).as_deref(),
            Some("The review_requested signal names PR #25."),
        );
    }

    #[test]
    fn a_long_headline_is_cut_on_a_word() {
        let long = format!("**Headline:** {}", "alpha ".repeat(60));
        let got = headline_from(Some(&long)).expect("headline");
        assert!(got.chars().count() <= HEADLINE_MAX + 1, "{got:?}");
        assert!(got.ends_with('…'), "{got:?}");
        assert!(!got.contains("alph…"), "cut mid-word: {got:?}");
    }

    #[test]
    fn no_summary_means_no_headline() {
        assert_eq!(headline_from(None), None);
        assert_eq!(headline_from(Some("   ")), None);
    }

    #[test]
    fn handled_states_that_settle_work() {
        assert!(!Handled::Open.is_handled());
        assert!(!Handled::Seen.is_handled());
        for h in [Handled::Acknowledged, Handled::Snoozed, Handled::Resolved] {
            assert!(h.is_handled(), "{h:?}");
        }
    }
}
