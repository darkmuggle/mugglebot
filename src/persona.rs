//! Personas — a durable model of one *person*, so you can rehearse an interaction
//! before you have it.
//!
//! A subject is what work is *about*. A persona is who the work is *with*. They are two
//! axes, and this module is the second one.
//!
//! # A persona is not a subject
//!
//! [`crate::subject`] states the rule this appears to break: `person` is a resolution key
//! and context, **never** a subject, because a person spans a career and keying work on one
//! collapses unrelated work into a single card. That rule is untouched. A persona:
//!
//! - never appears on the board and never competes for attention;
//! - never owns a signal — attribution still climbs to an issue, PR, Slack thread or
//!   incident, exactly as before;
//! - is read only when the operator asks for it, against a subject they picked.
//!
//! It is a *lens*, not a card. What made `person` unsuitable as a subject — long-lived,
//! shared, spanning everything — is precisely what makes it a good lens: a profile is
//! supposed to accumulate over a career.
//!
//! # Opt-in, one person at a time
//!
//! Personas exist only for people the operator names. The alternative — mint one for every
//! actor the signal log has ever seen — would build several hundred profiles of near
//! strangers, and the profiles that mattered would be indistinguishable from the noise.
//! [`crate::persona::harvest::propose`] ranks candidates by how much you actually interact
//! with them and *proposes*; creating the persona is a decision.
//!
//! # Candour, and what it has to mean here
//!
//! The point of a persona is prediction, and a profile that reads like a performance review
//! predicts nothing. "Thoughtful and collaborative" is true of almost everyone and tells you
//! nothing about whether this reviewer will block your PR. So the profile is a set of
//! [`Trait`]s, each of which must be:
//!
//! 1. **Falsifiable** — a claim about observable behaviour, not about character. "Blocks on
//!    missing tests for anything touching storage" can be checked against their next
//!    review; "cares about quality" cannot.
//! 2. **Cited** — every trait carries the [`Evidence`] ids it was built from, verbatim
//!    excerpts of things the person actually wrote. A trait with no evidence is dropped by
//!    [`verify`], not merely displayed with low confidence.
//! 3. **Contestable** — contradicting excerpts are kept as `counter_evidence` and shown.
//!    A reviewer who blocks on tests four times out of seven is *contested*, and asserting
//!    the pattern flatly would be the more confident and less useful answer.
//!
//! That is what candour buys: with the constraint in place the model says the unflattering
//! thing when the evidence supports it ("does not read past the first file of a large diff"
//! is a real, citable, useful finding). Without it, a model told to be candid about
//! someone's "biases" produces fluent character assassination — which is both unkind and,
//! more to the point for a prediction tool, *unfalsifiable and therefore worthless*.
//!
//! [`verify`] enforces all three deterministically, the same way [`crate::prdiff`]'s
//! `unverifiable` drops review findings the diff cannot support and `explain::verify` strips
//! claims the dossier cannot support. Prompt hardening was not enough in either of those
//! cases and is not enough here.
//!
//! # Counted, never modelled
//!
//! [`Stats`] — how many reviews, what share were approvals, how long their comments run,
//! how often they ask a question rather than issue an instruction — are computed from the
//! evidence rows in Rust. A model asked "what is their approval rate?" invents a
//! plausible number, and a plausible invented number is worse than no number, because the
//! operator has no way to tell it from a counted one.
//!
//! # Nothing is ever posted
//!
//! A prediction is a private rehearsal. Like every critique in this codebase it is never
//! written back to GitHub or Slack — see AGENTS.md → *Nothing is ever written back to
//! GitHub*. A predicted review is explicitly labelled a prediction wherever it is rendered,
//! and it never becomes a quotation: [`verify_prediction`] drops any point that does not
//! cite a trait, so what the operator reads is grounded in the profile rather than being the
//! base model's own opinion of the diff wearing somebody's name.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::signal::Source;

pub mod harvest;
pub mod predict;
pub mod profile;
pub mod sme;

/// Longest excerpt kept per piece of evidence.
///
/// Verbatim up to here, then truncated. Evidence is quoted back to the operator as the
/// citation for a trait, so it has to be the person's actual words — a summarized excerpt
/// would make the citation unfalsifiable, which defeats the point of keeping it.
pub const EXCERPT_CHARS: usize = 1_200;

/// A confidence no single-excerpt trait may exceed.
///
/// One comment is an anecdote. The model routinely returns `0.9` for a pattern it saw once,
/// and a capped number is the honest one — see [`verify`].
const SINGLE_EVIDENCE_CONFIDENCE: f32 = 0.5;

/// The most confidence any trait may claim, however much is behind it.
///
/// A trait predicts what somebody will do next. No quantity of past comments makes that
/// certain, so `1.0` is not a value this can hold — and the first live profile produced four
/// traits asserting exactly that.
const MAX_CONFIDENCE: f32 = 0.85;

/// The confidence ceiling for a claim resting on `n` excerpts.
///
/// Two excerpts is a coincidence, three is a hint, half a dozen is a habit. The curve matters
/// less than the fact that there is one: an unbounded confidence is the model's mood, and this
/// number is rendered as a percentage beside the claim.
fn ceiling(n: usize) -> f32 {
    match n {
        0 => 0.0,
        1 => SINGLE_EVIDENCE_CONFIDENCE,
        2 => 0.6,
        3 => 0.7,
        4..=5 => 0.78,
        _ => MAX_CONFIDENCE,
    }
}

/// Below this a trait is not worth showing at all.
///
/// Raised from 0.15 after a real profile: the tail below about a third was contested,
/// hedged and unactionable — "never comments on CI plumbing beyond version-string sourcing"
/// at 17% is not a finding, it is the model running out of things to say. A profile is read
/// top to bottom before a conversation, so its length is a cost.
const MIN_CONFIDENCE: f32 = 0.35;

// ---- identity ----------------------------------------------------------------

/// How an identity came to be attached to a persona.
///
/// The distinction earns its place because a wrong join is the worst failure this feature
/// has: a persona built from two people's messages predicts neither of them, and nothing
/// about the output looks wrong. So a guess is never silently promoted to a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityProvenance {
    /// The operator said so. Always wins, never re-derived.
    Operator,
    /// An exact upstream join — a Slack profile whose `github` field names the login, or a
    /// matching verified email. Deterministic and safe to apply without asking.
    Exact,
    /// A similarity guess (display name looks like the login). **Proposed only**: it is
    /// stored so the operator can confirm it, and contributes no evidence until they do.
    Proposed,
}

impl IdentityProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityProvenance::Operator => "operator",
            IdentityProvenance::Exact => "exact",
            IdentityProvenance::Proposed => "proposed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "operator" => Some(IdentityProvenance::Operator),
            "exact" => Some(IdentityProvenance::Exact),
            "proposed" => Some(IdentityProvenance::Proposed),
            _ => None,
        }
    }

    /// Whether evidence may be harvested through this identity.
    ///
    /// A `Proposed` join does not qualify. This is the whole safety property of the identity
    /// model: an unconfirmed guess can sit in the table indefinitely without ever putting one
    /// person's words into another person's profile.
    pub fn confirmed(self) -> bool {
        matches!(
            self,
            IdentityProvenance::Operator | IdentityProvenance::Exact
        )
    }
}

/// One handle on one source. `(source, handle)` is globally unique — a GitHub login belongs
/// to at most one persona, which is what stops two personas harvesting the same evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub source: Source,
    /// The upstream handle: a GitHub login, a Slack user id (`U04ABC…`), a Granola speaker
    /// label. Compared case-insensitively — see [`Identity::matches`].
    pub handle: String,
    pub provenance: IdentityProvenance,
    /// Why this join is believed, for the operator deciding whether to confirm it. Free text
    /// (`"slack profile field 'github' = pcholakov"`).
    pub rationale: Option<String>,
}

impl Identity {
    pub fn new(source: Source, handle: impl Into<String>, provenance: IdentityProvenance) -> Self {
        Self {
            source,
            handle: handle.into(),
            provenance,
            rationale: None,
        }
    }

    pub fn with_rationale(mut self, why: impl Into<String>) -> Self {
        self.rationale = Some(why.into());
        self
    }

    /// Whether an actor string from a signal is this identity.
    ///
    /// Case-insensitive, and tolerant of Slack's `<@U04ABC>` wrapping and a leading `@`,
    /// because the actor field is whatever the watcher happened to capture. Being strict here
    /// fails silently — the persona harvests nothing and looks merely quiet.
    pub fn matches(&self, actor: &str) -> bool {
        let clean = |s: &str| {
            s.trim()
                .trim_start_matches("<@")
                .trim_end_matches('>')
                .trim_start_matches('@')
                .to_ascii_lowercase()
        };
        clean(actor) == clean(&self.handle)
    }
}

// ---- the persona ------------------------------------------------------------

/// A person MuggleBot models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    /// Stable id, operator-chosen, defaulting to the GitHub login: `pcholakov`.
    pub slug: String,
    pub display_name: String,
    /// What they do, in the operator's words — "storage lead", "the SRE who owns the
    /// dashboards". Fed to the model verbatim, because it is the one piece of context the
    /// evidence cannot supply.
    pub role: Option<String>,
    /// The operator's own notes. Also verbatim, and deliberately outside the trait model:
    /// these are asserted, not inferred, so [`verify`] has no business filtering them.
    pub notes: Option<String>,
    pub identities: Vec<Identity>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When evidence was last gathered.
    pub harvested_at: Option<DateTime<Utc>>,
    /// When the traits were last distilled, and from how much.
    pub profiled_at: Option<DateTime<Utc>>,
    /// The newest evidence id at the last profile pass. The `PersonaProfile` workflow is
    /// keyed on it, so re-distilling a persona nothing new has been seen of is a refused
    /// key rather than a few hundred model calls.
    pub evidence_watermark: Option<String>,
}

impl Persona {
    /// Identities evidence may actually be harvested through.
    pub fn confirmed_identities(&self) -> impl Iterator<Item = &Identity> {
        self.identities.iter().filter(|i| i.provenance.confirmed())
    }

    /// The first confirmed handle on one source.
    ///
    /// Prefer [`Self::handles_on`] anywhere the answer is used to *fetch*: a person can have
    /// more than one handle on a source — a renamed Slack account, a personal and a work GitHub
    /// login — and taking the first silently ignores the rest. That is not hypothetical: a
    /// persona linked to a wrong Slack handle and then to the right one kept searching the wrong
    /// one, because the wrong one was linked first.
    pub fn handle_on(&self, source: Source) -> Option<&str> {
        self.handles_on(source).next()
    }

    /// Every confirmed handle on one source, in link order.
    pub fn handles_on(&self, source: Source) -> impl Iterator<Item = &str> {
        self.confirmed_identities()
            .filter(move |i| i.source == source)
            .map(|i| i.handle.as_str())
    }

    /// Whether an actor on a source is this person.
    pub fn is_actor(&self, source: Source, actor: &str) -> bool {
        self.confirmed_identities()
            .any(|i| i.source == source && i.matches(actor))
    }

    /// A slug that is safe as a Restate object key and a URL path segment.
    ///
    /// Restate keys tolerate a lot, but a slug is also a workflow-key component split on `@`
    /// (see [`crate::restate::workflows::split_versioned`]), so an `@` in one would hand the
    /// splitter half a persona name.
    pub fn slugify(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        for c in raw.trim().chars() {
            match c {
                'a'..='z' | '0'..='9' | '-' | '_' | '.' => out.push(c),
                'A'..='Z' => out.push(c.to_ascii_lowercase()),
                _ => out.push('-'),
            }
        }
        // Collapse runs and trim, so `Pavel Cholakov!` is `pavel-cholakov` rather than
        // `pavel-cholakov-`.
        let collapsed = out
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        collapsed.trim_matches('.').to_string()
    }
}

// ---- evidence ----------------------------------------------------------------

/// What kind of thing an excerpt is. Kept because the *register* differs enormously: the
/// same person is terse on GitHub and chatty in Slack, and a profile that mixes the two
/// without distinguishing them predicts the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A review's summary body, with its `state`.
    Review,
    /// An inline comment on a line of a diff — the densest signal of what they look for.
    ReviewComment,
    /// A comment on an issue or PR conversation.
    IssueComment,
    /// A pull request they opened, body included: how they explain their own work.
    AuthoredPr,
    /// A Slack message.
    Slack,
    /// A line attributed to them in a meeting transcript.
    Meeting,
}

impl EvidenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceKind::Review => "review",
            EvidenceKind::ReviewComment => "review_comment",
            EvidenceKind::IssueComment => "issue_comment",
            EvidenceKind::AuthoredPr => "authored_pr",
            EvidenceKind::Slack => "slack",
            EvidenceKind::Meeting => "meeting",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "review" => Some(EvidenceKind::Review),
            "review_comment" => Some(EvidenceKind::ReviewComment),
            "issue_comment" => Some(EvidenceKind::IssueComment),
            "authored_pr" => Some(EvidenceKind::AuthoredPr),
            "slack" => Some(EvidenceKind::Slack),
            "meeting" => Some(EvidenceKind::Meeting),
            _ => None,
        }
    }

    /// Whether this is review activity — the denominator for the approval rate.
    pub fn is_review(self) -> bool {
        matches!(self, EvidenceKind::Review | EvidenceKind::ReviewComment)
    }
}

/// One thing the person actually wrote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    /// Deterministic, derived from the upstream identity of the excerpt — so re-harvesting
    /// the same comment updates one row rather than appending a duplicate. See
    /// [`Evidence::make_id`].
    pub id: String,
    pub persona: String,
    pub source: Source,
    pub kind: EvidenceKind,
    /// The subject it happened on, when it maps to one. Lets a prediction say "they said
    /// this on the issue you are looking at".
    pub subject_key: Option<String>,
    pub url: Option<String>,
    /// Verbatim, bounded by [`EXCERPT_CHARS`].
    pub excerpt: String,
    /// `APPROVED` / `CHANGES_REQUESTED` / `COMMENTED` for a review; the file path for an
    /// inline comment; the channel for Slack. Whatever the one extra fact is for this kind.
    pub context: Option<String>,
    /// A review's verdict, kept separately because [`Stats`] counts it.
    pub state: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub ingested_at: DateTime<Utc>,
}

impl Evidence {
    /// `{persona}/{source}/{kind}/{upstream}` — stable across re-harvests.
    ///
    /// The persona is part of the id on purpose. The same GitHub comment is legitimately
    /// evidence for two personas when a review quotes somebody, and keying without the
    /// persona would make the second write clobber the first.
    pub fn make_id(persona: &str, source: Source, kind: EvidenceKind, upstream: &str) -> String {
        format!("{persona}/{}/{}/{upstream}", source.as_str(), kind.as_str())
    }

    /// One line for a prompt, cited by this excerpt's real id.
    ///
    /// For prompts where the model has to *cite* the excerpt, prefer [`Self::render_as`] with a
    /// short ordinal token: the real id is a hundred characters ending in a permalink, and a
    /// model asked to transcribe one will silently "correct" the segment that looks guessable.
    pub fn render(&self) -> String {
        self.render_as(&format!("ev:{}", self.id))
    }

    /// One line for a prompt, cited by an arbitrary short token.
    ///
    /// See [`crate::persona::profile`] for why citations are ordinals rather than ids.
    pub fn render_as(&self, token: &str) -> String {
        let when = self.occurred_at.format("%Y-%m-%d");
        let mut head = format!("[{token}] {when} {}", self.kind.as_str());
        if let Some(state) = self.state.as_deref().filter(|s| !s.is_empty()) {
            head.push_str(&format!(" ({state})"));
        }
        if let Some(ctx) = self.context.as_deref().filter(|c| !c.is_empty()) {
            head.push_str(&format!(" on {ctx}"));
        }
        format!("{head}: {}", self.excerpt.trim())
    }
}

// ---- traits ------------------------------------------------------------------

/// The facets a profile is built from.
///
/// Chosen to be *predictive* rather than descriptive. Each one is a question the operator
/// actually has before an interaction, and each is answerable from excerpts — which is what
/// keeps the model from reaching for personality. There is deliberately no `personality`
/// facet, and adding one would undo the point of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Facet {
    /// What they demonstrably block or insist on.
    ReviewsFor,
    /// What they demonstrably let through without comment. Half of predicting a review is
    /// predicting silence, and a profile of only positives always predicts engagement.
    Ignores,
    /// How hard they are to get an approval from, in their own words. The *rate* is counted
    /// (see [`Stats`]); this is the qualitative half.
    Bar,
    /// Length, directness, questions vs. instructions, hedging.
    Style,
    /// Themes they raise regardless of the diff in front of them. The "bias" the feature is
    /// for — and the facet most in need of [`verify`], since an uncited hobby horse is just
    /// an accusation.
    HobbyHorses,
    /// Where their comments get specific — the mark of someone who knows the area.
    Expertise,
    /// Where their comments stay generic, or where they defer. Stated as an observation
    /// about comments, never as a judgment about the person.
    BlindSpots,
    /// What they do when they disagree: re-request, escalate, concede, go quiet.
    Escalation,
    /// How they behave in Slack, which is usually not how they behave on GitHub.
    SlackRegister,
    /// How they behave in a meeting — what they push for out loud.
    MeetingRegister,
}

impl Facet {
    pub const ALL: &'static [Facet] = &[
        Facet::ReviewsFor,
        Facet::Ignores,
        Facet::Bar,
        Facet::Style,
        Facet::HobbyHorses,
        Facet::Expertise,
        Facet::BlindSpots,
        Facet::Escalation,
        Facet::SlackRegister,
        Facet::MeetingRegister,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Facet::ReviewsFor => "reviews_for",
            Facet::Ignores => "ignores",
            Facet::Bar => "bar",
            Facet::Style => "style",
            Facet::HobbyHorses => "hobby_horses",
            Facet::Expertise => "expertise",
            Facet::BlindSpots => "blind_spots",
            Facet::Escalation => "escalation",
            Facet::SlackRegister => "slack_register",
            Facet::MeetingRegister => "meeting_register",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Facet::ALL.iter().copied().find(|f| f.as_str() == s)
    }

    /// What the operator is actually asking, used as the prompt for this facet.
    pub fn question(self) -> &'static str {
        match self {
            Facet::ReviewsFor => {
                "What does this person demonstrably block on, insist on, or keep asking for \
                 in reviews? Name the specific thing, not the virtue."
            }
            Facet::Ignores => {
                "What passes without comment from them? What kinds of change or problem do \
                 the excerpts show them *not* engaging with?"
            }
            Facet::Bar => {
                "How hard is it to get their approval, and what do they say when they give \
                 or withhold one? Quote the condition, not the sentiment."
            }
            Facet::Style => {
                "How do they write? Length, directness, questions versus instructions, \
                 hedging, whether they propose the fix or only name the problem."
            }
            Facet::HobbyHorses => {
                "What do they raise repeatedly regardless of what the change is about? Only \
                 a theme that recurs across several unrelated excerpts counts."
            }
            Facet::Expertise => {
                "Where do their comments get specific and technically detailed — naming \
                 files, mechanisms, failure modes?"
            }
            Facet::BlindSpots => {
                "Where do their comments stay generic, or where do they explicitly defer to \
                 someone else? This is about the comments, not about the person."
            }
            Facet::Escalation => {
                "What do they do when they disagree or are pushed back on? Re-request \
                 changes, escalate, concede, stop replying?"
            }
            Facet::SlackRegister => {
                "How do they engage in Slack, and how does it differ from how they engage on \
                 GitHub? Do they answer quickly, at length, only when named?"
            }
            Facet::MeetingRegister => {
                "In meetings, what do they push for out loud, and what do they only raise in \
                 writing afterwards?"
            }
        }
    }

    /// The evidence kinds this facet can legitimately be built from.
    ///
    /// A `SlackRegister` trait cited entirely to GitHub reviews is not evidence about Slack,
    /// and [`verify`] drops it. This is the same class of check as `prdiff::unverifiable`:
    /// the model is not lying, it is answering from the wrong material.
    pub fn admits(self, kind: EvidenceKind) -> bool {
        match self {
            Facet::SlackRegister => matches!(kind, EvidenceKind::Slack),
            Facet::MeetingRegister => matches!(kind, EvidenceKind::Meeting),
            Facet::ReviewsFor | Facet::Ignores | Facet::Bar => {
                kind.is_review() || matches!(kind, EvidenceKind::IssueComment)
            }
            // The rest read across everything the person wrote.
            _ => true,
        }
    }
}

/// One falsifiable claim about behaviour, with its citations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trait {
    pub id: String,
    pub persona: String,
    pub facet: Facet,
    /// One sentence, about observable behaviour.
    pub claim: String,
    pub confidence: f32,
    /// [`Evidence`] ids supporting the claim. Never empty — [`verify`] drops it otherwise.
    pub evidence: Vec<String>,
    /// Evidence ids that *contradict* it. Kept and displayed rather than resolved: a
    /// contested pattern is a genuinely different answer from a clean one.
    pub counter_evidence: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl Trait {
    /// Whether the counter-evidence is substantial enough that this should be read as
    /// contested rather than established.
    ///
    /// Two conditions, and the second was learned from a real profile. A third of the evidence
    /// is the proportion; **at least two dissenting excerpts** is the floor. With the
    /// proportion alone, one counter-excerpt against two supporting ones flipped the badge —
    /// and a capable model fills `counter_evidence` diligently, so 30 of 39 traits came back
    /// marked contested. A badge on three quarters of the profile tells the reader nothing,
    /// which is worse than not having it: the genuinely contested claims stop standing out.
    pub fn contested(&self) -> bool {
        if self.counter_evidence.len() < 2 {
            return false;
        }
        let total = self.evidence.len() + self.counter_evidence.len();
        self.counter_evidence.len() * 3 >= total
    }

    /// One line for a prompt, cited so a prediction can point back at it.
    pub fn render(&self) -> String {
        format!(
            "[tr:{}] {} ({:.0}%{}): {}",
            self.id,
            self.facet.as_str(),
            self.confidence * 100.0,
            if self.contested() { ", contested" } else { "" },
            self.claim.trim()
        )
    }
}

/// Something the operator knows about this person that no excerpt can supply.
///
/// "Owns the release process", "prefers async review", "is on sabbatical until March", a link to
/// their team's charter. **Asserted, not inferred**, so it bypasses [`verify`] entirely and is
/// fed to the model verbatim: the filter exists to stop the *model* making unfalsifiable
/// claims, and applying it to something the operator stated would be the filter second-guessing
/// its own author.
///
/// Distinct from [`Persona::notes`], which is the one-line who-they-are beside their name.
/// These accumulate and are individually removable, which is what makes them usable for the
/// thing that actually happens: learning one more fact about somebody every few weeks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub id: String,
    pub persona: String,
    /// `text` — used verbatim — or `url`.
    pub kind: String,
    pub content: String,
    /// For a URL, what was read from it.
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Context {
    /// One line for a prompt.
    pub fn render(&self) -> String {
        match self.summary.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(summary) => format!("{} — {summary}", self.content),
            None => self.content.clone(),
        }
    }
}

/// A trait [`verify`] refused, and why — displayed rather than discarded silently.
///
/// The same reasoning as `subject_explanations.removed`: a profile that had claims taken out
/// of it is one to read more carefully, and a filter nobody can see is a filter nobody can
/// debug. On a first run against a real person this list is usually longer than the profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Removed {
    pub facet: String,
    pub claim: String,
    pub why: String,
}

/// The whole profile: what was kept, what was dropped, and the counted facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub persona: Persona,
    pub traits: Vec<Trait>,
    pub removed: Vec<Removed>,
    pub stats: Stats,
    /// Where their review activity concentrates, and where the model found it deep — the
    /// answer to "who do I ask about this". Counted from the evidence, then annotated from the
    /// `expertise` traits; see [`sme`].
    #[serde(default)]
    pub sme: Vec<sme::SmeArea>,
    /// What the operator knows about them beyond the evidence. Verbatim, unfiltered.
    #[serde(default)]
    pub context: Vec<Context>,
}

impl Profile {
    /// Render for a model prompt — the profile as the predictor reads it.
    ///
    /// Traits are grouped by facet and cited by id, because [`verify_prediction`] requires a
    /// prediction to name the traits it rests on and the model cannot cite what it was not
    /// shown an id for.
    pub fn render(&self) -> String {
        let mut out = format!(
            "PERSONA {} ({})",
            self.persona.display_name, self.persona.slug
        );
        if let Some(role) = self.persona.role.as_deref().filter(|r| !r.is_empty()) {
            out.push_str(&format!("\nRole: {role}"));
        }
        if let Some(notes) = self.persona.notes.as_deref().filter(|n| !n.is_empty()) {
            // Asserted by the operator, so labelled as such: the model should weigh it
            // above an inferred trait, not average it in.
            out.push_str(&format!("\nOperator's note (authoritative): {notes}"));
        }
        if !self.context.is_empty() {
            out.push_str(
                "\n\nWHAT THE OPERATOR KNOWS ABOUT THEM (authoritative — outweighs anything \
                 inferred below, and never contradict it)",
            );
            for c in &self.context {
                out.push_str(&format!("\n  - {}", c.render()));
            }
        }
        out.push_str(&format!("\n\nCOUNTED FACTS\n{}", self.stats.render()));
        // Whether the change in front of them is even their area is often the whole prediction:
        // the honest answer for a storage reviewer looking at a docs change is silence, and
        // without this the model has no way to know which it is looking at.
        if !self.sme.is_empty() {
            out.push_str("\n\nWHERE THEIR REVIEW ACTIVITY CONCENTRATES");
            for area in &self.sme {
                out.push_str(&format!(
                    "\n  {} ({} excerpts, {} reviews, {:.0}% of their activity){}",
                    area.area,
                    area.excerpts,
                    area.reviews,
                    area.share * 100.0,
                    match area.depth.as_deref() {
                        Some(claim) => format!(" — established depth: {claim}"),
                        None => " — presence only; nothing established about depth here".into(),
                    }
                ));
            }
        }
        out.push_str("\n\nOBSERVED TRAITS (cite these by [tr:id])");
        for facet in Facet::ALL {
            let mut group = self.traits.iter().filter(|t| t.facet == *facet).peekable();
            if group.peek().is_none() {
                continue;
            }
            out.push_str(&format!("\n{}:", facet.as_str()));
            for t in group {
                out.push_str(&format!("\n  {}", t.render()));
            }
        }
        if self.traits.is_empty() {
            out.push_str(
                "\n  (nothing established yet — say so rather than guessing what they would do)",
            );
        }
        out
    }

    /// Where the profile is too thin to predict from, as plain sentences.
    ///
    /// Surfaced with every prediction. A prediction from four excerpts and a prediction from
    /// four hundred look identical on screen, and the operator is about to make a decision
    /// based on which one it is.
    pub fn caveats(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.stats.evidence < 10 {
            out.push(format!(
                "Only {} excerpt(s) of this person have been harvested — this is a guess, not a pattern.",
                self.stats.evidence
            ));
        }
        if self.stats.reviews == 0 {
            out.push(
                "No review activity has been harvested, so a predicted code review has nothing \
                 behind it."
                    .into(),
            );
        }
        if self.traits.iter().all(|t| t.contested()) && !self.traits.is_empty() {
            out.push("Every established trait is contested by counter-evidence.".into());
        }
        for facet in [Facet::ReviewsFor, Facet::Bar] {
            if !self.traits.iter().any(|t| t.facet == facet) {
                out.push(format!(
                    "Nothing established for `{}` — the strongest predictor of a review is missing.",
                    facet.as_str()
                ));
            }
        }
        out
    }
}

// ---- counted facts -----------------------------------------------------------

/// Facts computed from the evidence rows. Never modelled — see the module docs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    pub evidence: usize,
    /// `(source, count)`, so "this profile is 90% Slack" is visible at a glance.
    pub by_source: Vec<(String, usize)>,
    pub by_kind: Vec<(String, usize)>,
    pub reviews: usize,
    pub approvals: usize,
    pub changes_requested: usize,
    pub commented: usize,
    /// Median rather than mean: one 4,000-character design review would otherwise report
    /// this person as writing essays.
    pub median_excerpt_chars: usize,
    /// Share of excerpts containing a question mark — asks versus tells.
    pub question_ratio: f32,
    /// Share of review activity that is an inline comment on a line, rather than a summary.
    /// High means they read the diff; low means they respond to the description.
    pub inline_ratio: f32,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
}

impl Stats {
    pub fn compute(evidence: &[Evidence]) -> Self {
        let mut s = Stats {
            evidence: evidence.len(),
            ..Default::default()
        };
        if evidence.is_empty() {
            return s;
        }
        let mut by_source: Vec<(String, usize)> = Vec::new();
        let mut by_kind: Vec<(String, usize)> = Vec::new();
        let mut lengths: Vec<usize> = Vec::with_capacity(evidence.len());
        let mut questions = 0usize;
        let mut inline = 0usize;
        for e in evidence {
            bump(&mut by_source, e.source.as_str());
            bump(&mut by_kind, e.kind.as_str());
            lengths.push(e.excerpt.chars().count());
            if e.excerpt.contains('?') {
                questions += 1;
            }
            if e.kind.is_review() {
                s.reviews += 1;
                if e.kind == EvidenceKind::ReviewComment {
                    inline += 1;
                }
            }
            match e.state.as_deref().map(str::to_ascii_uppercase).as_deref() {
                Some("APPROVED") => s.approvals += 1,
                Some("CHANGES_REQUESTED") => s.changes_requested += 1,
                Some("COMMENTED") => s.commented += 1,
                _ => {}
            }
            s.first_seen = Some(match s.first_seen {
                Some(f) if f <= e.occurred_at => f,
                _ => e.occurred_at,
            });
            s.last_seen = Some(match s.last_seen {
                Some(l) if l >= e.occurred_at => l,
                _ => e.occurred_at,
            });
        }
        by_source.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        by_kind.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        s.by_source = by_source;
        s.by_kind = by_kind;
        lengths.sort_unstable();
        s.median_excerpt_chars = lengths[lengths.len() / 2];
        s.question_ratio = questions as f32 / evidence.len() as f32;
        s.inline_ratio = if s.reviews == 0 {
            0.0
        } else {
            inline as f32 / s.reviews as f32
        };
        s
    }

    /// Approvals as a share of *decided* reviews.
    ///
    /// `None` when nothing has been decided, which is different from zero and must not
    /// render as "never approves anything".
    pub fn approval_rate(&self) -> Option<f32> {
        let decided = self.approvals + self.changes_requested;
        (decided > 0).then(|| self.approvals as f32 / decided as f32)
    }

    fn render(&self) -> String {
        let mut out = format!("{} excerpt(s)", self.evidence);
        if !self.by_source.is_empty() {
            let parts: Vec<String> = self
                .by_source
                .iter()
                .map(|(s, n)| format!("{s} {n}"))
                .collect();
            out.push_str(&format!(" ({})", parts.join(", ")));
        }
        if self.reviews > 0 {
            out.push_str(&format!(
                "\n{} review action(s): {} approved, {} changes requested, {} comment-only",
                self.reviews, self.approvals, self.changes_requested, self.commented
            ));
            if let Some(rate) = self.approval_rate() {
                out.push_str(&format!(
                    "\napproval rate {:.0}% of decided reviews",
                    rate * 100.0
                ));
            }
            out.push_str(&format!(
                "\n{:.0}% of review activity is inline on a line of the diff",
                self.inline_ratio * 100.0
            ));
        }
        out.push_str(&format!(
            "\nmedian excerpt {} chars; {:.0}% contain a question",
            self.median_excerpt_chars,
            self.question_ratio * 100.0
        ));
        if let (Some(f), Some(l)) = (self.first_seen, self.last_seen) {
            out.push_str(&format!(
                "\nobserved {} to {}",
                f.format("%Y-%m-%d"),
                l.format("%Y-%m-%d")
            ));
        }
        out
    }
}

fn bump(counts: &mut Vec<(String, usize)>, key: &str) {
    match counts.iter_mut().find(|(k, _)| k == key) {
        Some((_, n)) => *n += 1,
        None => counts.push((key.to_string(), 1)),
    }
}

// ---- verification ------------------------------------------------------------

/// Claim shapes that are not falsifiable, and so are not traits.
///
/// Two groups, and they fail for the same reason.
///
/// The first is **generic virtue**: "cares about quality", "is collaborative", "is a strong
/// engineer". True of nearly everyone, checkable against nothing, and the default output of
/// any model asked to describe a person. This is the persona equivalent of the empty
/// `approve` verdict that made `prdiff` review findings-first — a fluent answer that carries
/// no information.
///
/// The second is **inferred personal characteristic**: health, politics, religion, sexuality,
/// nationality, age, family, competence-as-a-person. A model told to be "candid about
/// biases" reaches for these immediately, and every one of them is (a) not evidence-bearing
/// on how somebody reviews a pull request and (b) an assertion about a real colleague that
/// no excerpt can support. Dropping them is not squeamishness — it is the same rule as the
/// rest of this filter, applied where it matters most. A claim about *stated* preferences
/// ("says they don't want to own the on-call rotation") is behaviour and survives.
const UNFALSIFIABLE: &[&str] = &[
    // Generic virtue and its opposite.
    "good engineer",
    "great engineer",
    "strong engineer",
    "bad engineer",
    "weak engineer",
    "poor engineer",
    "smart",
    "intelligent",
    "talented",
    "passionate",
    "cares about quality",
    "cares deeply",
    "team player",
    "collaborative",
    "professional",
    "friendly",
    "nice person",
    "difficult person",
    "hard to work with",
    "easy to work with",
    "toxic",
    "arrogant",
    "insecure",
    "lazy",
    "incompetent",
    "well respected",
    "highly respected",
    // Inferred personal characteristics.
    "mental health",
    "depress",
    "anxiet",
    "burn out",
    "burnt out",
    "burned out",
    "political",
    "politics",
    "religio",
    "sexual",
    "gender",
    "race",
    "racial",
    "ethnic",
    "nationalit",
    "immigra",
    "disabilit",
    "neurodiver",
    "autis",
    "adhd",
    "age of",
    "years old",
    "married",
    "divorce",
    "children",
    "family situation",
    "personal life",
    "salary",
    "compensation",
];

/// Keep only the traits the evidence can support, and say what was dropped.
///
/// Four deterministic rules, in the order they fire. Every one of them was reachable in the
/// first real run against a colleague's review history:
///
/// 1. **No evidence, no trait.** The model returns confident claims with an empty citation
///    list — the persona equivalent of `prdiff`'s "not used anywhere in the codebase"
///    findings about symbols the diff had just introduced.
/// 2. **Evidence must exist.** A cited id that is not in the harvested set is a
///    hallucinated citation, which is worse than none: it looks checkable.
/// 3. **The evidence must be the right kind.** A claim about Slack cited to GitHub reviews
///    is answering from the wrong material — see [`Facet::admits`].
/// 4. **The claim must be falsifiable.** See [`UNFALSIFIABLE`].
///
/// Then confidence is *capped*, not filtered: one excerpt cannot support more than
/// [`SINGLE_EVIDENCE_CONFIDENCE`], and counter-evidence pulls the number down in proportion.
/// Capping rather than dropping is deliberate — a thinly-supported observation is still worth
/// showing, as long as it does not claim to be more than it is.
pub fn verify(
    facet: Facet,
    candidates: Vec<Trait>,
    harvested: &[Evidence],
) -> (Vec<Trait>, Vec<Removed>) {
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    for mut t in candidates {
        let claim = t.claim.trim().to_string();
        if claim.is_empty() {
            continue;
        }
        t.claim = claim;

        // 4 first, because it is the cheapest and the most common.
        let lower = t.claim.to_ascii_lowercase();
        if let Some(hit) = UNFALSIFIABLE.iter().find(|p| lower.contains(**p)) {
            removed.push(Removed {
                facet: facet.as_str().into(),
                claim: t.claim,
                why: format!("not falsifiable from evidence (mentions '{hit}')"),
            });
            continue;
        }

        // 1 + 2: every cited id has to name evidence we actually hold.
        let known: Vec<String> = t
            .evidence
            .iter()
            .filter(|id| harvested.iter().any(|e| &e.id == *id))
            .cloned()
            .collect();
        if known.is_empty() {
            removed.push(Removed {
                facet: facet.as_str().into(),
                claim: t.claim,
                why: if t.evidence.is_empty() {
                    "cites no evidence".into()
                } else {
                    "every cited excerpt is one we do not hold".into()
                },
            });
            continue;
        }

        // 3: the cited evidence has to be material this facet can be judged from.
        let admissible: Vec<String> = known
            .into_iter()
            .filter(|id| {
                harvested
                    .iter()
                    .find(|e| &e.id == id)
                    .is_some_and(|e| facet.admits(e.kind))
            })
            .collect();
        if admissible.is_empty() {
            removed.push(Removed {
                facet: facet.as_str().into(),
                claim: t.claim,
                why: format!(
                    "cited evidence is not the kind `{}` can be judged from",
                    facet.as_str()
                ),
            });
            continue;
        }
        t.evidence = admissible;
        t.counter_evidence
            .retain(|id| harvested.iter().any(|e| &e.id == id));

        // Confidence is bounded by what is behind it, not by what the model felt.
        //
        // A ceiling per evidence count, not just a floor at one excerpt. The first live profile
        // came back with four traits at `1.0` — the model asserting certainty about a person's
        // habits from a 34-excerpt sample. That is never available: the claim is about what
        // somebody will do next, and no quantity of past comments makes that certain. The cap
        // is also the honest rendering, since the number is shown as a percentage next to the
        // claim and `100%` invites acting on it without reading the citations.
        let mut confidence = t.confidence.clamp(0.0, 1.0).min(ceiling(t.evidence.len()));
        let total = t.evidence.len() + t.counter_evidence.len();
        if total > 0 {
            confidence *= t.evidence.len() as f32 / total as f32;
        }
        if confidence < MIN_CONFIDENCE {
            removed.push(Removed {
                facet: facet.as_str().into(),
                claim: t.claim,
                why: format!("confidence {confidence:.2} after weighing the evidence behind it"),
            });
            continue;
        }
        t.confidence = confidence;
        kept.push(t);
    }
    (kept, removed)
}

// ---- predictions -------------------------------------------------------------

/// What is being predicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionKind {
    /// The review they would leave on this pull request.
    CodeReview,
    /// The comment they would leave on this issue.
    IssueResponse,
    /// Whether and how they would engage in the Slack thread.
    SlackEngagement,
}

impl PredictionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PredictionKind::CodeReview => "code_review",
            PredictionKind::IssueResponse => "issue_response",
            PredictionKind::SlackEngagement => "slack_engagement",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "code_review" => Some(PredictionKind::CodeReview),
            "issue_response" => Some(PredictionKind::IssueResponse),
            "slack_engagement" => Some(PredictionKind::SlackEngagement),
            _ => None,
        }
    }

    /// The kind that fits a subject, so the UI can offer the right one by default.
    ///
    /// A PR gets a review, an issue gets a response, a Slack thread or incident gets an
    /// engagement prediction. Offering a code review on a Slack thread would produce one —
    /// models are obliging — about a diff that does not exist.
    pub fn for_subject(key: &str) -> Self {
        if key.contains('!') {
            PredictionKind::CodeReview
        } else if key.contains('#') && !key.starts_with("incident:") {
            PredictionKind::IssueResponse
        } else {
            PredictionKind::SlackEngagement
        }
    }
}

/// One thing the person is predicted to say, and the traits that predict it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictedPoint {
    pub text: String,
    /// For a predicted code review: the file it is about.
    pub path: Option<String>,
    /// The line, copied verbatim from the patch. Same anchoring rule as [`crate::prdiff`]:
    /// a model copying a line it is looking at is usually exact, and the same model counting
    /// positions in a hunk is often off by a few.
    pub line: Option<String>,
    /// Trait ids. **Required** — [`verify_prediction`] drops a point that cites none.
    pub because: Vec<String>,
}

/// What a persona is predicted to do about one subject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prediction {
    pub persona: String,
    pub subject_key: String,
    pub kind: PredictionKind,
    /// The subject's watermark this was built from — the newest attributed signal id. A
    /// prediction older than the subject is visibly stale rather than quietly wrong.
    pub watermark: String,
    /// **Whether they would engage at all.** The most useful field, and the one a
    /// prediction tool is most tempted to skip: a predictor that always produces a review
    /// tells you nothing, and the honest answer for a docs PR in front of a storage
    /// reviewer is "nothing, they will not look at this".
    pub would_engage: bool,
    pub confidence: f32,
    /// `approve` / `comment` / `request_changes` — a predicted code review only.
    pub recommendation: Option<String>,
    /// The note they would write, in their register.
    pub summary: String,
    pub points: Vec<PredictedPoint>,
    /// Where the profile was too thin to support this. From [`Profile::caveats`] plus
    /// whatever the verifier removed.
    pub caveats: Vec<String>,
    /// `local` or the cloud model the operator named.
    pub produced_by: String,
    pub created_at: DateTime<Utc>,
}

impl Prediction {
    /// Drop predicted points that are not grounded in the profile.
    ///
    /// The rule: **a point must cite at least one trait we hold.** Without it the output is
    /// the base model's own review of the diff with somebody's name on top — which the
    /// operator already has, better, from [`crate::prdiff`]'s review, and which is actively
    /// misleading when attributed to a colleague.
    ///
    /// This fires a lot, and it is supposed to. A persona with three established traits
    /// cannot predict eight review comments, and the version of this that "worked" before
    /// the check existed was cheerfully doing exactly that.
    pub fn verify(&mut self, traits: &[Trait]) {
        let known: Vec<&str> = traits.iter().map(|t| t.id.as_str()).collect();
        let before = self.points.len();
        for p in &mut self.points {
            p.because.retain(|id| known.contains(&id.as_str()));
        }
        self.points.retain(|p| !p.because.is_empty());
        let dropped = before - self.points.len();
        if dropped > 0 {
            self.caveats.push(format!(
                "{dropped} predicted point(s) were dropped for citing no established trait — \
                 the profile does not support them."
            ));
        }
        // A prediction with nothing left cannot claim engagement. Left as-is it renders as a
        // confident "they will comment" above an empty list, which reads as a rendering bug
        // rather than as the answer.
        if self.points.is_empty() && self.would_engage && self.kind != PredictionKind::CodeReview {
            self.confidence = self.confidence.min(0.3);
        }
        if traits.is_empty() {
            self.would_engage = false;
            self.confidence = 0.0;
            self.summary =
                "Nothing is established about this person yet, so there is nothing to predict from. \
                 Harvest evidence and re-run the profile."
                    .into();
            self.points.clear();
        }
    }

    /// The composite key a prediction is stored under.
    pub fn storage_key(persona: &str, subject: &str, kind: PredictionKind) -> String {
        format!("{persona}\u{1f}{subject}\u{1f}{}", kind.as_str())
    }
}

// ---- the engine --------------------------------------------------------------

/// Everything the persona feature needs, in one handle.
///
/// Bundled rather than wired separately into `WorkflowOps`, the `Persona` object and the tool
/// surface, because all three need the same four things and the alternative is four fields
/// repeated in three places. Same role as [`crate::repos::RepoIndex`] plays for the code
/// index.
pub struct Engine {
    pub store: std::sync::Arc<crate::store::Store>,
    pub harvester: harvest::Harvester,
    pub distiller: profile::Distiller,
    pub predictor: predict::Predictor,
    /// Whether the feature is on. Off means the object's loop never arms and the tools refuse
    /// with a sentence naming the setting, rather than quietly doing nothing.
    pub enabled: bool,
    /// How often a persona whose backfill is finished is re-harvested.
    pub harvest_interval: std::time::Duration,
    /// How often a persona still walking its history takes another step. Shorter, because
    /// every step is a bounded page and the point is to finish.
    pub backfill_interval: std::time::Duration,
    /// Whether a harvest pass that found new material re-profiles on its own.
    pub auto_profile: bool,
    /// How long to wait after somebody's last observed activity before refreshing them.
    ///
    /// A debounce, not a delay: somebody posting nine messages in a minute is one refresh. The
    /// hard cap keeps a busy thread from deferring the refresh indefinitely, which is the same
    /// arrangement the subjects' re-analysis uses.
    pub engagement_debounce: crate::restate::objects::debounce::Debounce,
    /// How many candidates the proposal pass offers.
    pub max_proposals: usize,
}

impl Engine {
    /// One harvest pass over one persona.
    ///
    /// `trigger` decides whether the GitHub half may spend the reserve held for notifications
    /// and operator actions — see [`harvest::Trigger`].
    pub async fn harvest(
        &self,
        slug: &str,
        trigger: harvest::Trigger,
    ) -> anyhow::Result<harvest::Harvested> {
        let persona = self.require(slug)?;
        self.harvester.harvest(&persona, trigger).await
    }

    /// Re-distil one persona's profile from everything harvested.
    pub async fn distil(&self, slug: &str) -> anyhow::Result<Profile> {
        let persona = self.require(slug)?;
        self.distiller.distil(&persona).await
    }

    /// Predict what a persona would do about a subject.
    ///
    /// The dossier and diff are assembled by the caller — see [`predict::Request`] for why
    /// the reasoning half stays free of Restate and GitHub.
    #[allow(clippy::too_many_arguments)]
    pub async fn predict(
        &self,
        slug: &str,
        subject_key: &str,
        kind: PredictionKind,
        watermark: &str,
        dossier: String,
        diff: Option<String>,
        produced_by: &str,
        reasoner: &dyn crate::reasoner::Reasoner,
    ) -> anyhow::Result<Prediction> {
        let Some(profile) = self.store.persona_profile(slug)? else {
            anyhow::bail!("no persona '{slug}'");
        };
        self.predictor
            .predict(
                predict::Request {
                    profile: &profile,
                    subject_key,
                    kind,
                    watermark,
                    dossier,
                    diff,
                    produced_by: produced_by.to_string(),
                },
                reasoner,
            )
            .await
    }

    /// An engine wired for tests: real store, no GitHub, a mock model.
    ///
    /// `enabled` is true, because a test that constructs one is testing the feature. There is
    /// no token and no org, so [`harvest::Harvester::harvest`] gathers only the free half —
    /// which is what makes the harvest tests hermetic.
    #[cfg(test)]
    pub fn for_tests(store: std::sync::Arc<crate::store::Store>) -> Self {
        Self {
            store: store.clone(),
            harvester: harvest::Harvester {
                store: store.clone(),
                github: None,
                github_background: None,
                org: None,
                history_days: 90,
                slack_token: None,
                slack_pages: 1,
            },
            distiller: profile::Distiller {
                store: store.clone(),
                reasoner: std::sync::Arc::new(crate::reasoner::MockReasoner::new("{}")),
                tier: "local".into(),
            },
            predictor: predict::Predictor { store },
            enabled: true,
            harvest_interval: std::time::Duration::from_secs(43_200),
            backfill_interval: std::time::Duration::from_secs(600),
            auto_profile: true,
            max_proposals: 25,
            engagement_debounce: crate::restate::objects::debounce::Debounce {
                quiet: std::time::Duration::from_secs(120),
                max: std::time::Duration::from_secs(900),
            },
        }
    }

    /// The tier that forms opinions about people — the profile's traits *and* the predictions.
    ///
    /// One setting governs both because they are the same contract and fail the same way: cited
    /// claims in a fixed JSON shape, which the local 33B model could not hold. Predicting was
    /// left on the local reasoner when profiling moved to Claude, which meant a profile built by
    /// a capable model was then read by one that could not cite it — the citation-mangling
    /// failure all over again, one layer down.
    pub fn opinion_reasoner(&self) -> std::sync::Arc<dyn crate::reasoner::Reasoner> {
        self.distiller.reasoner.clone()
    }

    /// A persona, or an error naming the slug.
    ///
    /// A missing persona is a `TerminalError` at every call site: nothing a retry does brings
    /// it back, and a handler retrying forever against a slug the operator deleted is the
    /// failure mode a `RepoIndexer` tick had against a renamed repo.
    fn require(&self, slug: &str) -> anyhow::Result<Persona> {
        if !self.enabled {
            anyhow::bail!("personas are disabled — set `[personas] enabled = true`");
        }
        self.store
            .get_persona(slug)?
            .ok_or_else(|| anyhow::anyhow!("no persona '{slug}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, kind: EvidenceKind, excerpt: &str) -> Evidence {
        Evidence {
            id: id.into(),
            persona: "p".into(),
            source: match kind {
                EvidenceKind::Slack => Source::Slack,
                EvidenceKind::Meeting => Source::Granola,
                _ => Source::GitHub,
            },
            kind,
            subject_key: None,
            url: None,
            excerpt: excerpt.into(),
            context: None,
            state: None,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
        }
    }

    fn candidate(facet: Facet, claim: &str, evidence: &[&str], confidence: f32) -> Trait {
        Trait {
            id: format!("tr-{claim:.8}"),
            persona: "p".into(),
            facet,
            claim: claim.into(),
            confidence,
            evidence: evidence.iter().map(|s| s.to_string()).collect(),
            counter_evidence: vec![],
            created_at: Utc::now(),
        }
    }

    /// A slug has to survive being a Restate object key *and* a workflow key component,
    /// which is split on `@` — so an email-shaped name must not keep its `@`.
    #[test]
    fn slugs_are_safe_as_keys() {
        assert_eq!(Persona::slugify("pcholakov"), "pcholakov");
        assert_eq!(Persona::slugify("Pavel Cholakov"), "pavel-cholakov");
        assert_eq!(Persona::slugify("  Ben  Howard! "), "ben-howard");
        // The one that matters: `split_versioned` splits on the first `@`, so a slug
        // containing one would hand the splitter half a persona name.
        assert_eq!(Persona::slugify("ben@restate.dev"), "ben-restate.dev");
        assert!(!Persona::slugify("a@b").contains('@'));
    }

    /// The actor field is whatever the watcher captured — a bare id, a Slack mention
    /// wrapper, or an `@login`. Being strict here harvests nothing and looks like a quiet
    /// colleague rather than a bug.
    #[test]
    fn identities_match_however_the_actor_was_captured() {
        let id = Identity::new(Source::Slack, "U04ABC", IdentityProvenance::Operator);
        assert!(id.matches("U04ABC"));
        assert!(id.matches("<@U04ABC>"));
        assert!(id.matches("u04abc"));
        assert!(!id.matches("U04ABD"));

        let gh = Identity::new(Source::GitHub, "pcholakov", IdentityProvenance::Exact);
        assert!(gh.matches("@pcholakov"));
        assert!(gh.matches("PCholakov"));
    }

    /// An unconfirmed guess contributes nothing. This is the whole safety property of the
    /// identity model: a wrong join builds a profile from two people and nothing about the
    /// output looks wrong.
    #[test]
    fn a_proposed_identity_harvests_nothing() {
        let p = Persona {
            slug: "p".into(),
            display_name: "P".into(),
            role: None,
            notes: None,
            identities: vec![
                Identity::new(Source::GitHub, "pcholakov", IdentityProvenance::Operator),
                Identity::new(Source::Slack, "U0GUESS", IdentityProvenance::Proposed),
            ],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            harvested_at: None,
            profiled_at: None,
            evidence_watermark: None,
        };
        assert_eq!(p.confirmed_identities().count(), 1);
        assert_eq!(p.handle_on(Source::GitHub), Some("pcholakov"));
        assert_eq!(p.handle_on(Source::Slack), None, "a guess is not a handle");
        assert!(p.is_actor(Source::GitHub, "pcholakov"));
        assert!(!p.is_actor(Source::Slack, "U0GUESS"));
    }

    /// The generic-virtue failure, which is the default output of a model asked to describe
    /// a person and the exact analogue of `prdiff`'s empty `approve`.
    #[test]
    fn generic_virtue_is_not_a_trait() {
        let harvested = vec![ev("e1", EvidenceKind::Review, "looks fine")];
        let (kept, removed) = verify(
            Facet::Style,
            vec![
                candidate(
                    Facet::Style,
                    "A great engineer who cares about quality",
                    &["e1"],
                    0.9,
                ),
                candidate(
                    Facet::Style,
                    "Writes two-line reviews with no preamble",
                    &["e1"],
                    0.4,
                ),
            ],
            &harvested,
        );
        assert_eq!(kept.len(), 1, "only the falsifiable claim survives");
        assert!(kept[0].claim.starts_with("Writes two-line"));
        assert_eq!(removed.len(), 1);
        assert!(removed[0].why.contains("not falsifiable"));
    }

    /// A model told to be candid about "biases" reaches for inferred personal
    /// characteristics immediately. None of them is evidence-bearing on how somebody reviews
    /// a pull request, and no excerpt can support one.
    #[test]
    fn inferred_personal_characteristics_are_dropped() {
        let harvested = vec![ev("e1", EvidenceKind::Slack, "I'm out next week")];
        for claim in [
            "Seems burned out, based on shorter replies",
            "Politically opposed to the vendor choice",
            "Probably has children, given the hours they post",
        ] {
            let (kept, removed) = verify(
                Facet::HobbyHorses,
                vec![candidate(Facet::HobbyHorses, claim, &["e1"], 0.8)],
                &harvested,
            );
            assert!(kept.is_empty(), "{claim} must not survive");
            assert_eq!(removed.len(), 1);
        }
        // A *stated* preference is behaviour, and survives — the filter is about inference,
        // not about topics being untouchable.
        let (kept, _) = verify(
            Facet::HobbyHorses,
            vec![candidate(
                Facet::HobbyHorses,
                "Says repeatedly that they do not want to own the alerting rotation",
                &["e1"],
                0.6,
            )],
            &harvested,
        );
        assert_eq!(kept.len(), 1, "a stated preference is observable behaviour");
    }

    /// A citation to evidence we do not hold is worse than no citation: it looks checkable.
    #[test]
    fn hallucinated_and_missing_citations_are_dropped() {
        let harvested = vec![ev("e1", EvidenceKind::Review, "needs a test")];
        let (kept, removed) = verify(
            Facet::ReviewsFor,
            vec![
                candidate(Facet::ReviewsFor, "Blocks on missing tests", &[], 0.9),
                candidate(
                    Facet::ReviewsFor,
                    "Blocks on missing docs",
                    &["e-nope"],
                    0.9,
                ),
                candidate(
                    Facet::ReviewsFor,
                    "Asks for a test on every change",
                    &["e1"],
                    0.9,
                ),
            ],
            &harvested,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(removed.len(), 2);
        assert!(removed[0].why.contains("cites no evidence"));
        assert!(removed[1].why.contains("we do not hold"));
    }

    /// A claim about Slack cited entirely to GitHub reviews is answering from the wrong
    /// material — the same class of error as a review finding the diff cannot support.
    #[test]
    fn a_facet_must_be_cited_to_material_it_can_be_judged_from() {
        let harvested = vec![
            ev("gh1", EvidenceKind::Review, "please add a test"),
            ev("sl1", EvidenceKind::Slack, "on it, give me ten minutes"),
        ];
        let (kept, removed) = verify(
            Facet::SlackRegister,
            vec![candidate(
                Facet::SlackRegister,
                "Replies within minutes in Slack",
                &["gh1"],
                0.8,
            )],
            &harvested,
        );
        assert!(kept.is_empty());
        assert!(removed[0].why.contains("not the kind"));

        let (kept, _) = verify(
            Facet::SlackRegister,
            vec![candidate(
                Facet::SlackRegister,
                "Replies within minutes in Slack",
                &["sl1"],
                0.8,
            )],
            &harvested,
        );
        assert_eq!(kept.len(), 1);
    }

    /// One excerpt is an anecdote. The model routinely returns 0.9 for a pattern it saw
    /// once, and the capped number is the honest one.
    #[test]
    fn one_excerpt_cannot_be_high_confidence() {
        let harvested = vec![
            ev("e1", EvidenceKind::Review, "needs a test"),
            ev("e2", EvidenceKind::Review, "needs a test here too"),
            ev("e3", EvidenceKind::Review, "and a test for this path"),
        ];
        let (kept, _) = verify(
            Facet::ReviewsFor,
            vec![candidate(
                Facet::ReviewsFor,
                "Asks for tests",
                &["e1"],
                0.95,
            )],
            &harvested,
        );
        assert!(kept[0].confidence <= SINGLE_EVIDENCE_CONFIDENCE + f32::EPSILON);

        let (kept, _) = verify(
            Facet::ReviewsFor,
            vec![candidate(
                Facet::ReviewsFor,
                "Asks for tests",
                &["e1", "e2", "e3"],
                0.9,
            )],
            &harvested,
        );
        assert!(
            kept[0].confidence > SINGLE_EVIDENCE_CONFIDENCE,
            "three excerpts can support more than one can"
        );
    }

    /// Counter-evidence pulls confidence down in proportion, and a claim contradicted a
    /// third of the time reads as contested rather than established.
    #[test]
    fn counter_evidence_contests_a_claim() {
        let harvested: Vec<Evidence> = (0..6)
            .map(|i| ev(&format!("e{i}"), EvidenceKind::Review, "x"))
            .collect();
        let mut t = candidate(
            Facet::ReviewsFor,
            "Blocks on missing tests",
            &["e0", "e1", "e2", "e3"],
            0.9,
        );
        t.counter_evidence = vec!["e4".into(), "e5".into()];
        let (kept, _) = verify(Facet::ReviewsFor, vec![t], &harvested);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].contested(), "2 of 6 is contested");
        assert!(kept[0].confidence < 0.9);

        let clean = candidate(
            Facet::ReviewsFor,
            "Blocks on missing tests",
            &["e0", "e1"],
            0.6,
        );
        let (kept, _) = verify(Facet::ReviewsFor, vec![clean], &harvested);
        assert!(!kept[0].contested());
    }

    /// The prediction-grounding rule: a point citing no trait is the base model reviewing
    /// the diff with somebody's name on it, which the operator already has from `pr_review`.
    #[test]
    fn a_prediction_point_must_cite_a_trait() {
        let traits = vec![candidate(Facet::ReviewsFor, "Asks for tests", &["e1"], 0.5)];
        let trait_id = traits[0].id.clone();
        let mut p = Prediction {
            persona: "p".into(),
            subject_key: "o/r!1".into(),
            kind: PredictionKind::CodeReview,
            watermark: "w".into(),
            would_engage: true,
            confidence: 0.7,
            recommendation: Some("request_changes".into()),
            summary: "Needs tests".into(),
            points: vec![
                PredictedPoint {
                    text: "No test for the retry path".into(),
                    path: Some("src/a.rs".into()),
                    line: None,
                    because: vec![trait_id],
                },
                PredictedPoint {
                    text: "This variable name is unclear".into(),
                    path: None,
                    line: None,
                    because: vec![],
                },
                PredictedPoint {
                    text: "Invented citation".into(),
                    path: None,
                    line: None,
                    because: vec!["tr-nope".into()],
                },
            ],
            caveats: vec![],
            produced_by: "local".into(),
            created_at: Utc::now(),
        };
        p.verify(&traits);
        assert_eq!(p.points.len(), 1);
        assert_eq!(p.points[0].text, "No test for the retry path");
        assert!(p.caveats.iter().any(|c| c.contains("2 predicted point(s)")));
    }

    /// An empty profile must predict nothing rather than fall back on the base model.
    #[test]
    fn an_empty_profile_predicts_nothing() {
        let mut p = Prediction {
            persona: "p".into(),
            subject_key: "o/r!1".into(),
            kind: PredictionKind::CodeReview,
            watermark: "w".into(),
            would_engage: true,
            confidence: 0.9,
            recommendation: Some("approve".into()),
            summary: "Looks good to me!".into(),
            points: vec![PredictedPoint {
                text: "nice".into(),
                path: None,
                line: None,
                because: vec!["tr-x".into()],
            }],
            caveats: vec![],
            produced_by: "local".into(),
            created_at: Utc::now(),
        };
        p.verify(&[]);
        assert!(!p.would_engage);
        assert_eq!(p.confidence, 0.0);
        assert!(p.points.is_empty());
        assert!(p.summary.contains("Nothing is established"));
    }

    /// The kind has to follow the subject: offering a code review on a Slack thread produces
    /// one — models are obliging — about a diff that does not exist.
    #[test]
    fn the_prediction_kind_follows_the_subject() {
        assert_eq!(
            PredictionKind::for_subject("restatedev/restate!412"),
            PredictionKind::CodeReview
        );
        assert_eq!(
            PredictionKind::for_subject("restatedev/restate#412"),
            PredictionKind::IssueResponse
        );
        assert_eq!(
            PredictionKind::for_subject("C02ABC/1721822400.001"),
            PredictionKind::SlackEngagement
        );
        // An incident reference contains `#`-free `INC-448` but is prefixed; it is not an
        // issue and must not be predicted as one.
        assert_eq!(
            PredictionKind::for_subject("incident:INC-448"),
            PredictionKind::SlackEngagement
        );
    }

    /// Stats are counted, and the two nothings are distinguishable: no decided reviews is
    /// `None`, not zero — which must never render as "never approves anything".
    #[test]
    fn stats_are_counted_and_distinguish_the_two_nothings() {
        let mut approved = ev("e1", EvidenceKind::Review, "lgtm");
        approved.state = Some("APPROVED".into());
        let mut blocked = ev("e2", EvidenceKind::Review, "needs a test?");
        blocked.state = Some("CHANGES_REQUESTED".into());
        let inline = ev("e3", EvidenceKind::ReviewComment, "this leaks");
        let chat = ev("e4", EvidenceKind::Slack, "looking now");

        let s = Stats::compute(&[approved, blocked, inline, chat]);
        assert_eq!(s.evidence, 4);
        assert_eq!(s.reviews, 3);
        assert_eq!(s.approvals, 1);
        assert_eq!(s.changes_requested, 1);
        assert_eq!(s.approval_rate(), Some(0.5));
        assert!((s.inline_ratio - 1.0 / 3.0).abs() < 0.01);
        assert!((s.question_ratio - 0.25).abs() < 0.01);
        assert_eq!(s.by_source.len(), 2);

        // Nothing decided is `None`, not 0.0.
        let undecided = Stats::compute(&[ev("e9", EvidenceKind::Slack, "hi")]);
        assert_eq!(undecided.approval_rate(), None);
        assert_eq!(undecided.reviews, 0);

        assert_eq!(Stats::compute(&[]).approval_rate(), None);
    }

    /// A thin profile has to say so. A prediction from four excerpts and one from four
    /// hundred look identical on screen, and the operator is about to act on which it is.
    #[test]
    fn a_thin_profile_declares_itself() {
        let persona = Persona {
            slug: "p".into(),
            display_name: "P".into(),
            role: None,
            notes: None,
            identities: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            harvested_at: None,
            profiled_at: None,
            evidence_watermark: None,
        };
        let thin = Profile {
            sme: vec![],
            context: vec![],
            persona: persona.clone(),
            traits: vec![],
            removed: vec![],
            stats: Stats::compute(&[ev("e1", EvidenceKind::Slack, "hi")]),
        };
        let caveats = thin.caveats();
        assert!(caveats.iter().any(|c| c.contains("excerpt(s)")));
        assert!(caveats.iter().any(|c| c.contains("No review activity")));
        assert!(caveats.iter().any(|c| c.contains("reviews_for")));

        // An empty profile renders as an explicit "nothing established" rather than a blank
        // block the model would fill in from its own priors.
        assert!(thin.render().contains("nothing established yet"));
    }
}
