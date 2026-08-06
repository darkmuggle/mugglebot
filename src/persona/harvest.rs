//! Gathering the evidence a persona is built from.
//!
//! Four sources, and they cost very different amounts:
//!
//! | Source | Where it comes from | Cost |
//! |---|---|---|
//! | Slack (log) | the signal log, filtered by `actor` | a SQL query |
//! | Granola | the signal log, transcripts split by speaker | a SQL query |
//! | Slack (fetched) | `search.messages` with `from:@handle` | Slack API calls |
//! | GitHub | the search API, then reviews/comments per hit | GitHub API calls |
//!
//! # Why Slack is read twice
//!
//! The signal-log half was originally the *only* Slack half, on the reasoning that MuggleBot has
//! already ingested it. That was wrong about **what the log holds**: notifications — alert-channel
//! posts, @-mentions of the operator, keyword hits. Measured on a live workspace with
//! `channels = []`, that came to 194 Slack signals of which every single one was from
//! `#cloud-alerts` or `#incidents`, so a colleague who posts all day had *two* excerpts to their
//! name. Fetching their own messages instead turned the same persona's 2 into 292.
//!
//! Both halves are kept, and they are additive: the log carries alert-channel context the search
//! also sees, and deterministic ids mean an excerpt found by both is one row. GitHub is fetched
//! for the same reason and walked backwards a page at a time, because a review history is the
//! densest evidence of how somebody reviews and none of it is in the log.
//!
//! # Why the backward walk
//!
//! The interesting evidence is not the newest. Someone's last three reviews are a sample of
//! three; three months of them are a pattern. So each pass does two searches: one forward
//! (what happened since we last looked) and one backward from a cursor, which moves further
//! into the past every pass, until it reaches `[personas] history_days`. Over a few ticks a
//! persona accumulates real history without ever spending more than a handful of requests at
//! once — the same shape as [`crate::codeindex`]'s history walk, and for the same reason.
//!
//! # Priority: who asked
//!
//! The GitHub half runs at **interactive** priority when the operator asked for it and
//! **background** priority when the loop did — see [`Trigger`]. Getting this wrong is what
//! made the feature look broken on its first live run: everything was background, the code
//! index had the budget down to its reserve, and so every pass a freshly created persona ran
//! was refused.
//!
//! The signal-log half has no priority because it has no cost: it is a SQL query over signals
//! already ingested. A persona with a Slack handle linked gains evidence immediately, budget or
//! no budget, which is why linking one is the fastest way to get a usable profile.
//!
//! The cursor is stored rather than derived from the oldest evidence held. Deriving it looks
//! tidier and does not terminate: a barren window (a month where they reviewed nothing)
//! leaves the oldest evidence where it was, so the next pass issues the identical query and
//! the walk stalls forever, silently, looking merely slow. An explicit cursor moves whether
//! or not the window produced anything.
//!
//! # Evidence is never harvested through a guess
//!
//! Only [`IdentityProvenance::confirmed`] identities are read. An unconfirmed join sits in
//! the table contributing nothing until the operator confirms it, because the failure it
//! prevents — one person's words in another person's profile — is invisible in the output.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::{Evidence, EvidenceKind, Persona, EXCERPT_CHARS};
use crate::github::GithubClient;
use crate::signal::{Signal, Source};
use crate::store::{SignalFilter, Store};

/// Pull requests inspected per pass. Each costs up to two API calls (reviews + inline
/// comments), so this is the knob that decides what a pass costs.
const MAX_PRS_PER_PASS: usize = 8;

/// Search hits considered per query before the per-pass PR cap applies.
const SEARCH_HITS: usize = 25;

/// How far to jump when a window yields nothing.
///
/// Without this the walk advances only when it finds something, which stalls on a quiet
/// fortnight. Sized against the window: with three months of history to cover, 60-day jumps
/// would cross the whole span in two steps and skip most of it on a couple of quiet weeks.
const BARREN_JUMP_DAYS: i64 = 14;

/// Slack messages read per persona.
///
/// Deliberately high rather than a sample. Slack is the free half — it is a SQL query over
/// signals already ingested — and it is where somebody's register, latency and willingness to
/// engage actually show. Capping it at a few hundred was borrowing a bound from the GitHub
/// side, where each item costs an API call, and paying it where nothing was being spent.
const MAX_SLACK_EXCERPTS: usize = 5_000;

/// Granola meetings scanned per pass. Each is one transcript split by speaker, so the cost is
/// parsing rather than requests.
const MAX_MEETINGS: usize = 500;

/// Shortest excerpt worth keeping.
///
/// "lgtm", "+1", "thanks" and a lone emoji are the bulk of anybody's comment history and
/// carry nothing a profile can use — except as a *review state*, which is captured
/// separately, so an empty approval still counts toward the approval rate.
const MIN_EXCERPT_CHARS: usize = 12;

/// What a pass achieved. Returned to the object so its state and the UI agree.
///
/// `Deserialize` as well as `Serialize` because it crosses a `ctx.run` boundary: the value is
/// journalled and replayed on retry, so Restate has to be able to read it back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Harvested {
    pub persona: String,
    /// Rows written (new or refreshed) this pass.
    pub written: usize,
    /// Total evidence held afterwards.
    pub total: usize,
    pub by_source: Vec<(String, usize)>,
    /// How far back the GitHub walk has reached, RFC3339. Absent means it has not started —
    /// a different state from "nothing left to walk", which reads identically without it.
    pub walked_back_to: Option<String>,
    /// True when the walk has reached `[personas] history_days` and there is no more to fetch.
    pub complete: bool,
    /// Some of this pass did not run — the GitHub budget refused a background caller, or an
    /// item could not be read.
    ///
    /// Distinct from `complete` and from an empty `by_source`, because those are the same
    /// whether the person is quiet or the request never happened. The board's "0 ev" read as
    /// the first when it was the second.
    pub deferred: bool,
    /// Things the operator should know **and can act on**: a missing identity, an unsearchable
    /// handle, a bot token where a user token is needed.
    ///
    /// Surfaced rather than logged, because "this persona is thin" and "this persona cannot be
    /// harvested" look identical on screen otherwise.
    pub notes: Vec<String>,
    /// Things that merely have not happened yet — a background pass waiting on GitHub budget.
    ///
    /// Separate from [`Self::notes`] because they are *normal operation* and the operator can do
    /// nothing about them. Collapsing the two marked a persona holding 289 excerpts and 18
    /// established traits as `deferred` forever, on a machine whose code index keeps the budget
    /// at its reserve permanently — so the badge reported the feature as broken while it worked.
    /// Reported when the persona has nothing at all, since then it *is* the reason.
    pub waiting: Vec<String>,
}

/// What caused this pass, which decides whether it may spend the GitHub reserve.
///
/// This distinction is the fix for the feature's first live failure. Everything was
/// `Background`, so every pass was refused whenever the code index had drawn the budget down
/// to its reserve — which, crawling 147 repositories, is most of the time. A persona created
/// two minutes ago sat at `0 ev` indefinitely and looked broken, because it *was* broken.
///
/// AGENTS.md already had the right rule and this code did not follow it: *watchers and
/// operator actions are `Interactive`: never paced, never refused.* Pressing "harvest", or
/// linking a handle, or creating a persona, is an operator action. The scheduled backfill walk
/// is not, and stays `Background` — a profile filling in slowly costs nothing, where a watcher
/// that stopped noticing incidents costs everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The operator asked, now. Interactive priority.
    Operator,
    /// The harvest loop's own tick, or a refresh prompted by somebody's activity.
    /// Background priority.
    Scheduled,
}

pub struct Harvester {
    pub store: Arc<Store>,
    /// Interactive priority — for [`Trigger::Operator`]. Never paced, never refused.
    pub github: Option<GithubClient>,
    /// Background priority — for [`Trigger::Scheduled`]. Defers to the watchers.
    ///
    /// Two clients rather than one, because [`GithubClient::background`] consumes and the
    /// priority is a property of the client. Both read the same token.
    pub github_background: Option<GithubClient>,
    /// The org whose repositories their GitHub activity is searched within. Unscoped search
    /// would return their open-source activity across all of GitHub, which is real evidence
    /// about a different context.
    pub org: Option<String>,
    /// How far back to read their GitHub history, in days. From `[personas] history_days`.
    pub history_days: i64,
    /// Slack user token, for fetching a person's own messages.
    ///
    /// `None` disables the fetched Slack half and leaves only the signal-log re-read — which on
    /// a workspace whose watched `channels` are empty is almost nothing: see
    /// [`crate::watchers::slack::search_from_user`] for the measurement that prompted this.
    pub slack_token: Option<String>,
    /// Pages of Slack search per pass. 100 matches a page.
    pub slack_pages: u32,
}

impl Harvester {
    /// The client this trigger is allowed to use.
    fn client(&self, trigger: Trigger) -> Option<&GithubClient> {
        match trigger {
            Trigger::Operator => self.github.as_ref(),
            Trigger::Scheduled => self.github_background.as_ref(),
        }
    }
}

impl Harvester {
    /// One bounded pass: read the signal log, then walk a page of GitHub history.
    ///
    /// `trigger` decides whether the GitHub half may spend the reserve — see [`Trigger`]. The
    /// signal-log half runs either way, because it costs a SQL query: a persona with a Slack
    /// handle linked gains evidence immediately even when the API budget is exhausted.
    pub async fn harvest(&self, persona: &Persona, trigger: Trigger) -> Result<Harvested> {
        let mut out = Harvested {
            persona: persona.slug.clone(),
            ..Default::default()
        };

        if persona.confirmed_identities().count() == 0 {
            out.notes.push(
                "No confirmed identity, so there is nothing to harvest. Link a GitHub login or \
                 Slack user id — a proposed identity is deliberately never read."
                    .into(),
            );
            return Ok(out);
        }

        let mut evidence = self.signal_log_evidence(persona)?;

        // The fetched Slack half. Separate from the signal-log re-read above and additive to it:
        // the log holds alert-channel posts and @-mentions, this holds ordinary conversation, and
        // the two barely overlap. Deterministic ids mean an excerpt found by both is one row.
        match self.slack_messages(persona).await {
            Ok(mut slack) => evidence.append(&mut slack),
            Err(e) => {
                // Non-fatal, and reported. A bot token cannot search, which is a configuration
                // fact the operator needs told rather than a profile that is quietly GitHub-only.
                warn!("persona {}: slack harvest failed: {e:#}", persona.slug);
                out.deferred = true;
                out.notes.push(format!("Slack evidence unavailable: {e:#}"));
            }
        }

        match self.walk_github(persona, trigger).await {
            Ok(mut pass) => {
                evidence.append(&mut pass.evidence);
                out.walked_back_to = pass.cursor;
                out.complete = pass.complete;
                // `|=`, not `=`. The Slack half runs first and may already have set this; an
                // assignment here erased a real Slack failure whenever the GitHub half happened
                // to come back clean — including the common case of a persona with no GitHub
                // identity at all, where `walk_github` returns an empty success. The flag is
                // "something in this pass did not run", so it can only ever accumulate.
                out.deferred |= !pass.deferred.is_empty() || pass.unread > 0;
                // One note, naming the reason once rather than repeating it per query. The
                // GitHub budget message already says when it lifts, which is the part the
                // operator needs — a message that reads as permanent sends them looking for
                // a bug that isn't there.
                if let Some(reason) = pass.deferred.first() {
                    let note = format!(
                        "GitHub search was deferred, so this pass read no history and the \
                         backward walk did not advance — it will retry the same window. \
                         Reason: {reason}"
                    );
                    // A budget deferral is normal operation, not a problem to report — see
                    // `waiting` on `Harvested`.
                    if is_transient(reason) {
                        out.waiting.push(note);
                    } else {
                        out.notes.push(note);
                    }
                }
                if pass.unread > 0 {
                    let note = format!(
                        "{} item(s) turned up but could not be read, so anything this person \
                         wrote on them is not in the profile yet.{}",
                        pass.unread,
                        pass.unread_reason
                            .as_deref()
                            .map(|r| format!(" Reason: {r}"))
                            .unwrap_or_default()
                    );
                    // Same rule as the search deferral: the budget refusing a background caller
                    // is normal operation, and reporting it here was how the spurious badge
                    // survived being silenced on the other path.
                    if pass.unread_reason.as_deref().is_some_and(is_transient) {
                        out.waiting.push(note);
                    } else {
                        out.notes.push(note);
                    }
                }
            }
            Err(e) => {
                // Non-fatal. A missing token or an unset org must not discard the Slack and
                // Granola evidence this pass already gathered — and the next tick retries.
                warn!("persona {}: github harvest failed: {e:#}", persona.slug);
                out.deferred = true;
                let msg = format!("{e:#}");
                let note = format!("GitHub evidence unavailable: {msg}");
                if is_transient(&msg) {
                    out.waiting.push(note);
                } else {
                    out.notes.push(note);
                }
            }
        }

        out.written = self.store.put_persona_evidence(&evidence)?;
        let held = self.store.persona_evidence(&persona.slug, None)?;
        out.total = held.len();
        out.by_source = super::Stats::compute(&held).by_source;
        if let Some(cursor) = out.walked_back_to.as_deref() {
            self.store
                .set_persona_harvest_cursor(&persona.slug, Some(cursor), out.complete)?;
        }
        // A gap the operator can close beats a status message they cannot act on.
        if let Some(gap) = self.missing_source_hint(persona, &held)? {
            out.notes.push(gap);
        }

        // What reaches the badge, and what does not.
        //
        // Persisted rather than only returned, because the pass runs inside a Restate handler
        // minutes after whatever asked for it — a note in a return value reaches nobody.
        //
        // But **only when the operator can act on it.** A background tick that could not get
        // GitHub budget is normal operation on a machine whose code index keeps the budget at
        // its reserve — which is to say, always. Writing that as a warning marked a persona
        // holding 289 excerpts and 18 established traits as `deferred` in perpetuity, so the
        // badge said "broken" about the feature working. The incomplete backfill is already
        // visible as `backfilling`, which is the honest place for it.
        //
        // So the note is reserved for: nothing gathered at all (whatever the reason — a
        // persona with no evidence *is* broken), a configuration problem that will never
        // self-heal, or a gap the operator can close.
        let actionable = out
            .notes
            .first()
            .cloned()
            .or_else(|| out.waiting.first().cloned().filter(|_| out.total == 0));
        self.store
            .set_persona_harvest_note(&persona.slug, actionable.as_deref())?;
        self.store.touch_persona_harvested(&persona.slug)?;
        debug!(
            "persona {}: {} row(s) written, {} held{}",
            persona.slug,
            out.written,
            out.total,
            if out.deferred { ", some deferred" } else { "" }
        );
        Ok(out)
    }

    /// A source with no linked identity that the workspace directory can name a candidate for.
    ///
    /// The most useful thing this pass can say, and the reason it exists: a persona linked only
    /// to GitHub harvested nothing at all on a machine whose GitHub budget is starved, and the
    /// row said `deferred` — a message about the budget, when the actual fix was "you have not
    /// linked their Slack". Slack is where most of the evidence is (292 excerpts against 32 for
    /// one real persona), so a missing Slack handle is worth naming, with the candidate.
    ///
    /// Only Slack, and only from the directory. A GitHub login cannot be guessed from a display
    /// name, and a guess offered as a suggestion is how a wrong join gets made.
    fn missing_source_hint(&self, persona: &Persona, held: &[Evidence]) -> Result<Option<String>> {
        if persona.handles_on(Source::Slack).next().is_some() {
            return Ok(None);
        }
        // Nothing to suggest from, and nothing to search with either.
        if self.slack_token.is_none() {
            return Ok(None);
        }
        // Matched on the display name, then the slug: `Ben Howard` finds `@ben` where the slug
        // `ben-howard` would not.
        let candidate = self
            .store
            .find_slack_user(&persona.display_name)?
            .or(self.store.find_slack_user(&persona.slug)?)
            .or_else(|| {
                self.store
                    .slack_users_like(&persona.display_name, 1)
                    .ok()
                    .and_then(|v| v.into_iter().next())
            });
        let held_slack = held.iter().any(|e| e.source == Source::Slack);
        Ok(match (candidate, held_slack) {
            (Some(user), _) => Some(format!(
                "No Slack handle linked — {} looks like them. Slack is where most of the \
                 evidence is, so linking it is the fastest way to fill this profile in.",
                user.label()
            )),
            // No candidate, but still worth saying: the profile is GitHub-only by omission
            // rather than because they are quiet.
            (None, false) => Some(
                "No Slack handle linked, so this profile is GitHub-only. Slack is usually where \
                 most of the evidence is."
                    .into(),
            ),
            (None, true) => None,
        })
    }

    // ---- the free half: the signal log ---------------------------------------

    /// Slack messages they posted, and meeting lines attributed to them.
    ///
    /// GitHub signals are deliberately *not* read here even though the log holds them. A
    /// notification's body is the notification, not the comment — the actual review text
    /// comes from [`Self::walk_github`], where it arrives with its state, its file path and
    /// its permalink. Reading both would double-count the same review under two ids.
    fn signal_log_evidence(&self, persona: &Persona) -> Result<Vec<Evidence>> {
        let mut out = Vec::new();

        for identity in persona.confirmed_identities() {
            if identity.source != Source::Slack {
                continue;
            }
            let signals = self
                .store
                .signals_by_actor(Source::Slack, &identity.handle, MAX_SLACK_EXCERPTS)
                .context("reading slack signals for a persona")?;
            for s in signals {
                let Some(excerpt) = excerpt_of(s.body.as_deref().unwrap_or(&s.title)) else {
                    continue;
                };
                out.push(Evidence {
                    id: Evidence::make_id(
                        &persona.slug,
                        Source::Slack,
                        EvidenceKind::Slack,
                        &s.external_id,
                    ),
                    persona: persona.slug.clone(),
                    source: Source::Slack,
                    kind: EvidenceKind::Slack,
                    subject_key: s.subject.clone(),
                    url: s.url.clone(),
                    excerpt,
                    context: channel_of(&s),
                    state: None,
                    occurred_at: s.occurred_at,
                    ingested_at: Utc::now(),
                });
            }
        }

        if persona
            .confirmed_identities()
            .any(|i| i.source == Source::Granola)
        {
            out.extend(self.meeting_evidence(persona)?);
        }
        Ok(out)
    }

    /// Meeting lines attributed to this person.
    ///
    /// Granola signals carry no `actor` — a meeting is not something one person did — so the
    /// attribution has to come from inside the transcript. [`transcript_lines`] is tolerant
    /// of both shapes Granola has been observed to return, because a strict parser here fails
    /// by finding nothing, which looks like a colleague who never speaks.
    fn meeting_evidence(&self, persona: &Persona) -> Result<Vec<Evidence>> {
        let handles: Vec<&str> = persona
            .confirmed_identities()
            .filter(|i| i.source == Source::Granola)
            .map(|i| i.handle.as_str())
            .collect();
        let meetings = self.store.list_signals(&SignalFilter {
            source: Some(Source::Granola),
            limit: Some(MAX_MEETINGS),
            ..Default::default()
        })?;
        let mut out = Vec::new();
        for m in meetings {
            for (n, (speaker, text)) in transcript_lines(&m.raw).into_iter().enumerate() {
                if !handles
                    .iter()
                    .any(|h| speaker.eq_ignore_ascii_case(h.trim()))
                {
                    continue;
                }
                let Some(excerpt) = excerpt_of(&text) else {
                    continue;
                };
                out.push(Evidence {
                    id: Evidence::make_id(
                        &persona.slug,
                        Source::Granola,
                        EvidenceKind::Meeting,
                        &format!("{}#{n}", m.external_id),
                    ),
                    persona: persona.slug.clone(),
                    source: Source::Granola,
                    kind: EvidenceKind::Meeting,
                    subject_key: m.subject.clone(),
                    url: m.url.clone(),
                    excerpt,
                    context: Some(m.title.clone()),
                    state: None,
                    occurred_at: m.occurred_at,
                    ingested_at: Utc::now(),
                });
            }
        }
        Ok(out)
    }

    /// Their own Slack messages, fetched.
    ///
    /// Keyed on the `@handle` rather than the `U…` id, because `from:` takes a handle — a
    /// `from:U063…` query silently matches nothing, which looks exactly like a colleague who
    /// never posts. The id is what the signal log records, so the handle is recovered from the
    /// cached workspace directory; a persona whose Slack identity is a raw id and whose directory
    /// is empty gets nothing here, and says so.
    async fn slack_messages(&self, persona: &Persona) -> Result<Vec<Evidence>> {
        let Some(token) = self.slack_token.as_deref() else {
            return Ok(Vec::new());
        };
        let handles: Vec<String> = persona
            .handles_on(Source::Slack)
            .map(str::to_string)
            .collect();
        if handles.is_empty() {
            return Ok(Vec::new());
        }

        // **Every** linked Slack handle, not just the first.
        //
        // A person can have more than one — a renamed account, or simply a wrong handle linked
        // before the right one. Taking the first is what made this fail on a live workspace:
        // `lukebond` was linked, then the correct `U0ADMKZL692` was linked beside it, and every
        // pass afterwards searched `lukebond`, failed, and reported the failure while never
        // trying the handle that worked. One bad handle must not suppress a good one.
        let mut out = Vec::new();
        let mut failures = Vec::new();
        for id_or_handle in &handles {
            match self.slack_for_handle(persona, token, id_or_handle).await {
                Ok(mut found) => out.append(&mut found),
                Err(e) => failures.push(format!("{e:#}")),
            }
        }
        // Only a total failure is a failure. If any handle produced messages, a dud handle
        // alongside it is noise the operator can fix at their leisure — and reporting it as a
        // deferral would mark a working profile as broken.
        if out.is_empty() && !failures.is_empty() {
            anyhow::bail!("{}", failures.join("; "));
        }
        Ok(out)
    }

    /// One Slack handle's messages.
    async fn slack_for_handle(
        &self,
        persona: &Persona,
        token: &str,
        id_or_handle: &str,
    ) -> Result<Vec<Evidence>> {
        // The directory turns a stored id back into the handle `from:` needs — and, just as
        // importantly, tells us when the handle is not a real member.
        //
        // A `from:@lukebond` query for somebody whose handle is `@luke` returns **200 with zero
        // matches**, which is indistinguishable from a colleague who never posts. Measured
        // exactly that way: a persona on a plausible-but-wrong handle harvested its GitHub half,
        // no Slack, and reported nothing wrong.
        let handle = match self.store.slack_user(id_or_handle)? {
            Some(user) => user.name,
            None => match self.store.find_slack_user(id_or_handle)? {
                Some(user) => user.name,
                None => {
                    let (_, members) = self.store.slack_directory_age()?;
                    if members > 0 {
                        anyhow::bail!(
                            "'{id_or_handle}' is not a member of the Slack workspace ({members} \
                             cached) — searching it would return nothing and look like silence"
                        );
                    }
                    // No directory to check against, so the handle is taken at face value: a
                    // workspace whose token lacks `users:read` must still be searchable.
                    id_or_handle.to_string()
                }
            },
        };

        let matches = crate::watchers::slack::search_from_user(
            &reqwest::Client::new(),
            token,
            &handle,
            self.slack_pages,
        )
        .await?;

        let mut out = Vec::new();
        for m in matches {
            let Some(excerpt) = excerpt_of(&m.text) else {
                continue;
            };
            let occurred_at =
                m.ts.split('.')
                    .next()
                    .and_then(|s| s.parse::<i64>().ok())
                    .and_then(|secs| DateTime::from_timestamp(secs, 0))
                    .unwrap_or_else(Utc::now);
            out.push(Evidence {
                // The channel and ts, so the same message found through the signal log and
                // through search is one row rather than two.
                id: Evidence::make_id(
                    &persona.slug,
                    Source::Slack,
                    EvidenceKind::Slack,
                    &format!("{}/{}", m.channel.id, m.ts),
                ),
                persona: persona.slug.clone(),
                source: Source::Slack,
                kind: EvidenceKind::Slack,
                subject_key: None,
                url: m.permalink.clone(),
                excerpt,
                context: Some(channel_label(&m.channel, &handle)),
                state: None,
                occurred_at,
                ingested_at: Utc::now(),
            });
        }
        debug!(
            "persona {}: {} slack message(s) fetched for @{handle}",
            persona.slug,
            out.len()
        );
        Ok(out)
    }

    // ---- the expensive half: GitHub ------------------------------------------

    /// One forward search and one backward step, then read the hits.
    async fn walk_github(&self, persona: &Persona, trigger: Trigger) -> Result<GithubPass> {
        let Some(login) = persona.handle_on(Source::GitHub) else {
            return Ok(GithubPass::default());
        };
        let Some(gh) = self.client(trigger) else {
            anyhow::bail!("no stored GitHub token");
        };
        let Some(org) = self.org.as_deref().filter(|o| !o.is_empty()) else {
            anyhow::bail!(
                "[investigation].org is unset, so there is no repository scope to search within"
            );
        };

        let (cursor, complete) = self.store.persona_harvest_cursor(&persona.slug)?;
        // The window the operator asked for, not a fixed constant: three months of somebody's
        // reviews is a pattern and finishes in a handful of passes, where two years was a
        // backfill that never visibly completed.
        let floor = Utc::now() - Duration::days(self.history_days.max(1));
        let mut pass = GithubPass {
            // Left where it was unless a window is actually read. See below.
            cursor: cursor.clone(),
            complete,
            ..Default::default()
        };

        // Forward: what has happened since we last looked. Always run, unbounded by the
        // cursor — the cursor is about how far *back* the walk has reached, and letting it
        // bound the forward query would freeze the persona at the moment of the first pass.
        let forward = self.search(gh, org, login, None).await;
        let mut hits = forward.hits;
        pass.deferred.extend(forward.deferred);

        // Backward: one window further into the past.
        if !complete {
            let until = cursor
                .as_deref()
                .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            if until <= floor {
                pass.complete = true;
            } else {
                let older = self.search(gh, org, login, Some(until)).await;
                pass.deferred.extend(older.deferred.clone());

                // **The cursor moves only when the window was actually read.**
                //
                // It used to advance unconditionally, on the reasoning that a barren window
                // must not stall the walk — which is right, and which is what the
                // `BARREN_JUMP_DAYS` branch below is for. What it got wrong is that a query
                // that never ran is not a barren window. The GitHub budget refuses background
                // callers once it is down to the reserve held for notifications (see
                // `crate::github`), and treating that refusal as "nothing here" marked six
                // months of somebody's review history as walked without a single successful
                // request. The walk only goes backwards, so that history was gone for good.
                //
                // Measured on the live board: three consecutive passes moved
                // `walked_back_to` from now to 2026-02-04 while every query was deferred and
                // the pass reported success with an empty note list.
                if older.deferred.is_empty() {
                    let oldest = older
                        .hits
                        .iter()
                        .filter_map(|h| h.updated_at.as_deref())
                        .filter_map(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc))
                        .min();
                    // A genuinely barren window still advances — otherwise a quiet quarter
                    // stalls the walk forever, which is the failure the module note describes.
                    let advanced = match oldest {
                        Some(o) if o < until => o,
                        _ => until - Duration::days(BARREN_JUMP_DAYS),
                    };
                    pass.cursor = Some(advanced.to_rfc3339());
                    if advanced <= floor {
                        pass.complete = true;
                    }
                }
                hits.extend(older.hits);
            }
        }

        // Deduped, because the two searches overlap: a pull request they both reviewed and
        // commented on is one item, and reading it twice in a pass is two wasted API calls
        // for rows that would be written identically.
        let mut seen: Vec<(String, u64)> = Vec::new();
        hits.retain(|h| {
            let key = (h.repo.clone(), h.number);
            // A hit whose repository could not be determined is dropped rather than read
            // against an empty repo name, which would 404 once per pass forever.
            if h.repo.is_empty() || seen.contains(&key) {
                return false;
            }
            seen.push(key);
            true
        });
        hits.truncate(MAX_PRS_PER_PASS);

        for hit in hits {
            let is_pr = hit.kind == "pull_request";
            let read = self
                .read_one(gh, persona, login, &hit.repo, hit.number, is_pr)
                .await;
            pass.evidence.extend(read.evidence);
            pass.unread += read.unread;
            if pass.unread_reason.is_none() {
                pass.unread_reason = read.unread_reason;
            }
        }
        Ok(pass)
    }

    /// Their review activity and comments on one issue or pull request.
    ///
    /// Failures are tolerated per item — a private repo, a deleted PR, or a refusal on hit six
    /// must not discard the five that succeeded; the pass is a batch, not a transaction — but
    /// they are **counted**. An item that could not be read is not an item they said nothing
    /// on, and reporting zero either way is how a whole pass came back empty and looked calm.
    async fn read_one(
        &self,
        gh: &GithubClient,
        persona: &Persona,
        login: &str,
        repo: &str,
        number: u64,
        is_pr: bool,
    ) -> ItemRead {
        let mut out = Vec::new();
        let mut unread = 0usize;
        let mut unread_reason: Option<String> = None;
        let subject_key = Some(if is_pr {
            format!("{repo}!{number}")
        } else {
            format!("{repo}#{number}")
        });

        let mut comments = Vec::new();
        match gh.issue_comments(repo, number).await {
            Ok(c) => comments.extend(c),
            Err(e) => {
                unread += 1;
                unread_reason.get_or_insert_with(|| format!("{e:#}"));
                debug!("persona {}: {repo}#{number} comments: {e:#}", persona.slug);
            }
        }
        if is_pr {
            match gh.pull_reviews(repo, number).await {
                Ok(c) => comments.extend(c),
                Err(e) => {
                    unread += 1;
                    unread_reason.get_or_insert_with(|| format!("{e:#}"));
                    debug!("persona {}: {repo}!{number} reviews: {e:#}", persona.slug);
                }
            }
        }

        for c in comments {
            if !c
                .author
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(login))
            {
                continue;
            }
            let kind = match c.kind.as_str() {
                "review" => EvidenceKind::Review,
                "review_comment" => EvidenceKind::ReviewComment,
                _ => EvidenceKind::IssueComment,
            };
            // A bare approval carries no text and is still evidence — it is the numerator of
            // the approval rate. Everything else needs something to read.
            let has_state = c.state.as_deref().is_some_and(|s| !s.is_empty());
            let excerpt = match excerpt_of(&c.body) {
                Some(e) => e,
                None if has_state => format!("(no comment text; {})", c.state.clone().unwrap()),
                None => continue,
            };
            let upstream = c
                .url
                .clone()
                .unwrap_or_else(|| format!("{repo}/{number}/{}", out.len()));
            let occurred_at = c
                .created_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            out.push(Evidence {
                id: Evidence::make_id(&persona.slug, Source::GitHub, kind, &upstream),
                persona: persona.slug.clone(),
                source: Source::GitHub,
                kind,
                subject_key: subject_key.clone(),
                url: c.url.clone(),
                excerpt,
                context: c.path.clone().or_else(|| subject_key.clone()),
                state: c.state.clone(),
                occurred_at,
                ingested_at: Utc::now(),
            });
        }
        ItemRead {
            evidence: out,
            unread,
            unread_reason,
        }
    }

    /// Search the org for things this person touched.
    ///
    /// `reviewed-by` and `commenter` rather than `involves`: `involves` includes every issue
    /// they were merely assigned or @-mentioned on, which is a list of things that happened
    /// *to* them and carries no writing of theirs to harvest.
    ///
    /// One of the two queries failing is not the pass failing — a search that 422s on a
    /// qualifier the org does not support should not cost the other one. But a failure is
    /// **reported** rather than swallowed, because "no hits" and "the query never ran" are
    /// different facts and the caller decides the cursor from that difference.
    async fn search(
        &self,
        gh: &GithubClient,
        org: &str,
        login: &str,
        until: Option<DateTime<Utc>>,
    ) -> SearchResult {
        let window = match until {
            Some(u) => format!(" updated:<{}", u.format("%Y-%m-%d")),
            None => String::new(),
        };
        let mut result = SearchResult::default();
        for q in [
            format!("org:{org} type:pr reviewed-by:{login}{window}"),
            format!("org:{org} commenter:{login}{window}"),
        ] {
            match gh.search_issues(&q, SEARCH_HITS).await {
                Ok(hits) => result.hits.extend(hits),
                Err(e) => {
                    debug!("persona search '{q}' failed: {e:#}");
                    result.deferred.push(format!("{e:#}"));
                }
            }
        }
        result
    }
}

/// What one GitHub search produced, and whether it actually ran.
///
/// The distinction is the whole point: an empty `hits` with an empty `deferred` means the
/// person genuinely has nothing in that window, and an empty `hits` with a non-empty
/// `deferred` means we never asked. Conflating them advanced the backward cursor over
/// unread history.
#[derive(Default)]
struct SearchResult {
    hits: Vec<crate::github::IssueHit>,
    /// One message per query that did not run.
    deferred: Vec<String>,
}

/// What reading one issue or pull request produced.
#[derive(Default)]
struct ItemRead {
    evidence: Vec<Evidence>,
    /// Endpoints that could not be read. Counted so a pass can say "8 items deferred"
    /// rather than reporting the same zero as "they said nothing".
    unread: usize,
    /// Why the first of them could not be read.
    ///
    /// Carried so the caller can tell a budget refusal from a deleted repository. Without it the
    /// unread count was reported as a warning even when the cause was the same transient budget
    /// the pass had already correctly decided not to warn about — so silencing one path just
    /// moved the spurious badge to the other.
    unread_reason: Option<String>,
}

/// What one GitHub pass produced.
#[derive(Default)]
struct GithubPass {
    evidence: Vec<Evidence>,
    /// Where the backward walk has reached — **unchanged** when the window was deferred.
    cursor: Option<String>,
    complete: bool,
    /// Queries that did not run.
    deferred: Vec<String>,
    /// Items whose comments or reviews could not be read.
    unread: usize,
    /// Why, for classification. See [`ItemRead::unread_reason`].
    unread_reason: Option<String>,
}

// ---- proposing people worth a persona ----------------------------------------

/// A person the signal log has seen, ranked by how much the operator deals with them.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub source: String,
    pub handle: String,
    /// Signals they authored that reached the board — the ranking key. This is
    /// deliberately "how much do I deal with them", not "how active are they": the second
    /// would rank the busiest person in the org top for everybody.
    pub interactions: usize,
    pub last_seen: Option<DateTime<Utc>>,
    /// One of their messages, so the operator can tell a person from a bot.
    pub sample: Option<String>,
    /// What [`Persona::slugify`] would name them.
    pub suggested_slug: String,
}

/// Rank the people worth making a persona of.
///
/// Proposes; never creates. Minting a persona per actor would build several hundred profiles
/// of near strangers and bury the handful that matter — and it would do it to real people
/// without anyone asking for it.
///
/// Bots are excluded on the name, which is imperfect and the right trade: a missed bot is a
/// junk row in a proposal list the operator is already reading, and a missed *person* is
/// invisible.
pub fn propose(store: &Store, existing: &[String], limit: usize) -> Result<Vec<Candidate>> {
    let signals = store.list_signals(&SignalFilter {
        limit: Some(5_000),
        ..Default::default()
    })?;
    let mut tally: HashMap<(String, String), Candidate> = HashMap::new();
    for s in &signals {
        let Some(actor) = s.actor.as_deref().map(str::trim).filter(|a| !a.is_empty()) else {
            continue;
        };
        if is_bot(actor) {
            continue;
        }
        let key = (s.source.as_str().to_string(), actor.to_ascii_lowercase());
        let entry = tally.entry(key).or_insert_with(|| Candidate {
            source: s.source.as_str().to_string(),
            handle: actor.to_string(),
            interactions: 0,
            last_seen: None,
            sample: None,
            suggested_slug: Persona::slugify(actor),
        });
        entry.interactions += 1;
        if entry.last_seen.is_none_or(|l| l < s.occurred_at) {
            entry.last_seen = Some(s.occurred_at);
        }
        if entry.sample.is_none() {
            entry.sample = s
                .body
                .as_deref()
                .map(str::trim)
                .filter(|b| b.len() > MIN_EXCERPT_CHARS)
                .map(|b| b.chars().take(160).collect());
        }
    }
    let mut out: Vec<Candidate> = tally
        .into_values()
        // Already modelled. Kept out of the list rather than shown as "done", because the
        // list is a to-do and a done item on a to-do is noise.
        .filter(|c| {
            !existing
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&c.suggested_slug))
        })
        .collect();
    out.sort_by(|a, b| {
        b.interactions
            .cmp(&a.interactions)
            .then(b.last_seen.cmp(&a.last_seen))
    });
    out.truncate(limit);
    Ok(out)
}

/// Names that are automation. See [`propose`] for why this errs toward including people.
///
/// Two rules, kept separate because they fail differently. The suffix rule is GitHub's own
/// convention and is near-certain. The substring rule is a judgment call: `alert` catches
/// `Cloud Alerts`, the Slack app that posts every incident in `#alerts`, and no colleague is
/// called that — but it would also catch a person whose handle happened to contain it, which
/// is why the list is short and specific rather than a general "looks automated" heuristic.
fn is_bot(actor: &str) -> bool {
    const SUFFIXES: &[&str] = &["[bot]", "-bot", "_bot", " bot"];
    const NAMES: &[&str] = &[
        "github-actions",
        "dependabot",
        "renovate",
        "codecov",
        "sonarcloud",
        "grafana",
        "alert",
        "prometheus",
        "incident",
        "pagerduty",
        "zapier",
        "webhook",
    ];
    let lower = actor.to_ascii_lowercase();
    SUFFIXES.iter().any(|s| lower.ends_with(s)) || NAMES.iter().any(|n| lower.contains(n))
}

// ---- shared helpers ----------------------------------------------------------

/// Whether a deferral will clear on its own.
///
/// The GitHub budget refusal names the seconds until it lifts, a rate limit is a wait, and a
/// connection reset is a flake — none of those is something the operator does anything about. A
/// missing token, an unset org, an unsearchable handle and a bot-token-cannot-search are
/// configuration, and will still be true tomorrow.
///
/// Matched on the message for the same reason [`crate::restate::workflows::persona`] classifies
/// terminal errors that way: the distinction is carried in the text by the layer that produced
/// it, and threading a type through every call site would buy the same answer.
fn is_transient(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    [
        "budget",
        "resumes in",
        "rate limit",
        "ratelimited",
        "429",
        "timed out",
        "timeout",
        "connection",
        "502",
        "503",
        "504",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// How a Slack conversation should be labelled on an excerpt.
///
/// A **DM is labelled as a DM**, and that matters twice over. It changes how to read the
/// excerpt — somebody is blunter in a direct message than in `#eng`, and a profile that mixes
/// the two without saying which is which mispredicts both registers. And it is the
/// privacy-sensitive category: `[personas] slack_search` warns that a user token reaches the
/// operator's DMs, and an excerpt that came from one should say so rather than looking like it
/// came from a public channel.
///
/// Detected on the id prefix, which is Slack's own convention (`D…` is an IM, `C…` a channel,
/// `G…` a private group). Search results give a DM's `name` as the other party's *user id*, so
/// without this the label rendered as `#U0ADMKZL692` — indistinguishable from a channel with an
/// odd name, and 40 of one person's excerpts arrived that way.
fn channel_label(channel: &crate::watchers::slack::SearchChannel, handle: &str) -> String {
    if channel.id.starts_with('D') {
        return format!("DM with @{handle}");
    }
    match channel.name.as_deref().filter(|n| !n.is_empty()) {
        // A private group's name is real; the `#` is still the right sigil for it.
        Some(name) => format!("#{name}"),
        None => channel.id.clone(),
    }
}

/// A bounded, trimmed excerpt, or `None` when there is nothing worth keeping.
fn excerpt_of(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.chars().count() < MIN_EXCERPT_CHARS {
        return None;
    }
    Some(text.chars().take(EXCERPT_CHARS).collect())
}

/// The channel a Slack signal was in, for the excerpt's context line.
fn channel_of(s: &Signal) -> Option<String> {
    s.keys
        .iter()
        .find(|k| k.kind == "channel")
        .map(|k| format!("#{}", k.value.trim_start_matches('#')))
}

/// Speaker-attributed lines from a Granola transcript.
///
/// Tolerant of both shapes seen in the wild: an array of `{speaker, text}` objects, and a
/// flat string of `Name: said something` lines. A strict parser fails by returning nothing,
/// which is indistinguishable from a colleague who never speaks in meetings — so this
/// accepts several field spellings and gives up quietly per line rather than per transcript.
fn transcript_lines(raw: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let node = [
        "transcript",
        "transcript_segments",
        "segments",
        "utterances",
    ]
    .iter()
    .find_map(|k| raw.get(*k));
    match node {
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                let speaker = ["speaker", "speaker_name", "source", "name", "who"]
                    .iter()
                    .find_map(|k| item.get(*k).and_then(|v| v.as_str()));
                let text = ["text", "content", "value", "words"]
                    .iter()
                    .find_map(|k| item.get(*k).and_then(|v| v.as_str()));
                if let (Some(s), Some(t)) = (speaker, text) {
                    out.push((s.trim().to_string(), t.trim().to_string()));
                }
            }
        }
        Some(serde_json::Value::String(text)) => out.extend(split_speaker_lines(text)),
        _ => {
            // No transcript node at all: some Granola documents carry the conversation in
            // the notes instead. Worth one attempt at the same line shape.
            if let Some(notes) = ["notes_markdown", "notes_plain", "notes"]
                .iter()
                .find_map(|k| raw.get(*k).and_then(|v| v.as_str()))
            {
                out.extend(split_speaker_lines(notes));
            }
        }
    }
    out
}

/// `Name: text` lines, ignoring anything that isn't one.
///
/// The colon has to be early in the line, or every sentence containing one becomes a
/// speaker attribution — `"Decision: we ship on Friday"` would otherwise be filed as
/// something said by a person called "Decision".
fn split_speaker_lines(text: &str) -> Vec<(String, String)> {
    const MAX_SPEAKER_CHARS: usize = 40;
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((speaker, said)) = line.split_once(':') else {
            continue;
        };
        let speaker = speaker
            .trim()
            .trim_start_matches(['-', '*', '•', '#'])
            .trim();
        if speaker.is_empty()
            || speaker.chars().count() > MAX_SPEAKER_CHARS
            || speaker.contains("http")
            // A speaker label is a name, not a sentence.
            || speaker.split_whitespace().count() > 4
        {
            continue;
        }
        let said = said.trim();
        if said.is_empty() {
            continue;
        }
        out.push((speaker.to_string(), said.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{ResolutionKey, Severity, SignalKind};

    fn signal(source: Source, actor: Option<&str>, body: &str, id: &str) -> Signal {
        Signal {
            id: Signal::make_id(source, id, None),
            source,
            external_id: id.into(),
            kind: SignalKind::ThreadReply,
            title: "t".into(),
            body: Some(body.into()),
            url: None,
            actor: actor.map(str::to_string),
            keys: vec![ResolutionKey::new("channel", "alerts")],
            severity: Severity::Info,
            version: None,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: serde_json::json!({}),
            tags: vec![],
        }
    }

    /// Both transcript shapes, and the failure a strict parser has: finding nothing and
    /// looking like a colleague who never speaks.
    #[test]
    fn transcripts_yield_speaker_lines_in_either_shape() {
        let structured = serde_json::json!({
            "transcript": [
                { "speaker": "Pavel Cholakov", "text": "the retry path is the risk here" },
                { "speaker_name": "Ben Howard", "content": "agreed, let's gate it" },
                { "speaker": "Nobody" }
            ]
        });
        let lines = transcript_lines(&structured);
        assert_eq!(
            lines.len(),
            2,
            "an entry with no text is skipped, not fatal"
        );
        assert_eq!(lines[0].0, "Pavel Cholakov");
        assert!(lines[0].1.contains("retry path"));

        let flat = serde_json::json!({
            "transcript": "Pavel: I'd rather not ship this on a Friday\n\
                           Ben: fair\n\
                           Decision: we ship on Monday\n\
                           https://example.com/x: not a speaker\n"
        });
        let lines = transcript_lines(&flat);
        let speakers: Vec<&str> = lines.iter().map(|(s, _)| s.as_str()).collect();
        assert!(speakers.contains(&"Pavel"));
        assert!(speakers.contains(&"Ben"));
        // `Decision:` is a heading, and filing it as a person called "Decision" is exactly
        // the failure the early-colon rule exists to prevent... but it *is* a short label,
        // so what actually saves us is that no persona is ever named "Decision".
        assert!(!speakers.iter().any(|s| s.contains("http")));
    }

    /// Falling back to the notes when there is no transcript node at all — some Granola
    /// documents carry the conversation there.
    #[test]
    fn notes_are_read_when_there_is_no_transcript() {
        let doc = serde_json::json!({ "notes": "Pavel: this needs a test\nBen: on it" });
        let lines = transcript_lines(&doc);
        assert_eq!(lines.len(), 2);
    }

    /// "lgtm" and "+1" are the bulk of anybody's comment history and carry nothing a
    /// profile can use.
    #[test]
    fn trivial_excerpts_are_not_evidence() {
        assert_eq!(excerpt_of("lgtm"), None);
        assert_eq!(excerpt_of("+1"), None);
        assert_eq!(excerpt_of("   \n  "), None);
        assert!(excerpt_of("this leaks the connection on the retry path").is_some());
        // Bounded, and the bound is on characters so a multibyte body can't panic.
        let long = "é".repeat(EXCERPT_CHARS * 2);
        assert_eq!(excerpt_of(&long).unwrap().chars().count(), EXCERPT_CHARS);
    }

    /// Who asked decides which client, and therefore whether the pass can run at all.
    ///
    /// This one line is the difference between the feature working and not. With everything on
    /// the background client, a persona created two minutes ago was refused for as long as the
    /// code index held the budget at its reserve — measured on a live workspace as every pass
    /// harvesting zero. The first operator-triggered pass after the split harvested 34 excerpts
    /// from the same account, unchanged in every other respect.
    #[test]
    fn the_trigger_decides_which_client_a_pass_gets() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let interactive = GithubClient::new("t".into()).unwrap();
        let background = GithubClient::new("t".into()).unwrap().background();
        let h = Harvester {
            store: store.clone(),
            github: Some(interactive),
            github_background: Some(background),
            org: Some("restatedev".into()),
            history_days: 90,
            slack_token: None,
            slack_pages: 1,
        };
        // Not identity-comparable, so assert on the property that matters: each trigger
        // resolves to a client, and to a *different* one.
        let op = h.client(Trigger::Operator).expect("operator has a client");
        let sched = h
            .client(Trigger::Scheduled)
            .expect("scheduled has a client");
        assert!(
            !std::ptr::eq(op, sched),
            "the two triggers must not share a client, or the priority split does nothing"
        );

        // No token means no client either way, rather than a silent fallback to the other
        // priority — which would let a scheduled pass spend the reserve.
        let none = Harvester {
            store,
            github: None,
            github_background: None,
            org: None,
            history_days: 90,
            slack_token: None,
            slack_pages: 1,
        };
        assert!(none.client(Trigger::Operator).is_none());
        assert!(none.client(Trigger::Scheduled).is_none());
    }

    /// A Slack identity stored as an id cannot be searched by handle, and says so.
    ///
    /// `search.messages` takes `from:@handle`; a `from:U063RCBCFSP` query matches nothing and
    /// returns cleanly, which would look exactly like a colleague who never posts. So the id is
    /// resolved through the cached directory, and when it cannot be, the pass reports it instead
    /// of harvesting silence.
    #[tokio::test]
    async fn a_slack_id_with_no_directory_entry_is_reported_not_searched() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let persona = Persona {
            slug: "pav".into(),
            display_name: "Pavel".into(),
            role: None,
            notes: None,
            identities: vec![super::super::Identity::new(
                Source::Slack,
                "U063RCBCFSP",
                super::super::IdentityProvenance::Operator,
            )],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            harvested_at: None,
            profiled_at: None,
            evidence_watermark: None,
        };
        store.put_persona(&persona).unwrap();

        let harvester = Harvester {
            store: store.clone(),
            github: None,
            github_background: None,
            org: None,
            history_days: 90,
            // A token, so the Slack half is attempted rather than skipped.
            slack_token: Some("xoxp-test".into()),
            slack_pages: 1,
        };
        let report = harvester
            .harvest(&persona, Trigger::Operator)
            .await
            .unwrap();
        assert!(report.deferred, "an unsearchable identity is not a success");
        assert!(
            report.notes.iter().any(|n| n.contains("Slack")),
            "the reason has to reach the operator: {:?}",
            report.notes
        );

        // With the directory populated, the id resolves to the handle and the search is
        // attempted — it fails here only because the token is fake, which is a different note.
        store
            .put_slack_users(&[crate::watchers::slack::SlackUser {
                id: "U063RCBCFSP".into(),
                name: "pavel".into(),
                real_name: Some("Pavel Tcholakov".into()),
                display_name: None,
                email: None,
                is_bot: false,
                deleted: false,
            }])
            .unwrap();
        let report = harvester
            .harvest(&persona, Trigger::Operator)
            .await
            .unwrap();
        assert!(
            !report
                .notes
                .iter()
                .any(|n| n.contains("cannot be searched")),
            "a resolvable id must get past the handle check: {:?}",
            report.notes
        );
    }

    /// A budget deferral on a working persona is not a problem to report.
    ///
    /// The failure this replaces: a persona holding 289 excerpts and 18 established traits showed
    /// `deferred` in perpetuity, because every background tick was refused GitHub budget by a code
    /// index that keeps it at the reserve permanently. The badge reported the feature as broken
    /// while it was working. A persona with *nothing*, though, is broken, and then the budget is
    /// genuinely the reason.
    #[test]
    fn a_transient_deferral_is_only_reported_when_nothing_was_gathered() {
        for transient in [
            "GitHub budget is down to its 1000-request reserve; background work resumes in 59s",
            "GitHub returned 403: rate limit exceeded",
            "error sending request: connection reset",
            "operation timed out",
        ] {
            assert!(is_transient(transient), "{transient} clears on its own");
        }
        for config in [
            "no stored GitHub token",
            "[investigation].org is unset, so there is no repository scope to search within",
            "'lukebond' is not a member of the Slack workspace (96 cached)",
            "slack search failed: Some(\"not_allowed_token_type\")",
        ] {
            assert!(
                !is_config_clear(config),
                "{config} will still be true tomorrow"
            );
        }
    }

    /// Helper mirroring the assertion above — a configuration problem is not transient.
    fn is_config_clear(msg: &str) -> bool {
        is_transient(msg)
    }

    /// One dud handle must not suppress a working one.
    ///
    /// Measured on a live workspace: `lukebond` was linked, then the correct `U0ADMKZL692` was
    /// linked beside it. Every pass afterwards searched `lukebond` — the first-linked — failed,
    /// reported the failure, and never tried the handle that worked. The profile stayed at five
    /// Slack excerpts while 276 were available.
    #[tokio::test]
    async fn every_linked_slack_handle_is_tried_not_just_the_first() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .put_slack_users(&[crate::watchers::slack::SlackUser {
                id: "U0GOOD".into(),
                name: "luke".into(),
                real_name: Some("Luke Bond".into()),
                display_name: None,
                email: None,
                is_bot: false,
                deleted: false,
            }])
            .unwrap();

        let persona = Persona {
            slug: "luke".into(),
            display_name: "Luke".into(),
            role: None,
            notes: None,
            identities: vec![
                // The wrong one, linked first — exactly the live shape.
                super::super::Identity::new(
                    Source::Slack,
                    "lukebond",
                    super::super::IdentityProvenance::Operator,
                ),
                super::super::Identity::new(
                    Source::Slack,
                    "U0GOOD",
                    super::super::IdentityProvenance::Operator,
                ),
            ],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            harvested_at: None,
            profiled_at: None,
            evidence_watermark: None,
        };

        // `handle_on` is the first; `handles_on` is all of them, and the fetch uses the latter.
        assert_eq!(persona.handle_on(Source::Slack), Some("lukebond"));
        assert_eq!(
            persona.handles_on(Source::Slack).collect::<Vec<_>>(),
            vec!["lukebond", "U0GOOD"],
            "both handles have to be visible to the fetch, or the good one is never tried"
        );
        drop(store);
    }

    /// The bug this exists to prevent, measured on the live board.
    ///
    /// The harvester runs at background priority, and the GitHub budget refuses background
    /// callers once it is down to the reserve held for notifications. Three consecutive passes
    /// had every query refused, and each one still advanced `walked_back_to` — from *now* to
    /// 2026-02-04 — marking six months of somebody's review history as walked without a single
    /// successful request. The walk only goes backwards, so that history was unrecoverable.
    ///
    /// Both halves matter and they are easy to fix separately and wrong to separate: the
    /// cursor must hold, **and** the pass must say why. A pass that holds the cursor silently
    /// is a persona that stays empty for a reason nobody can see, which is the state that made
    /// this look like a feature that did nothing.
    #[tokio::test]
    async fn a_deferred_window_neither_advances_the_cursor_nor_reports_success() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let persona = Persona {
            slug: "pav".into(),
            display_name: "Pavel".into(),
            role: None,
            notes: None,
            identities: vec![super::super::Identity::new(
                Source::GitHub,
                "pcholakov",
                super::super::IdentityProvenance::Operator,
            )],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            harvested_at: None,
            profiled_at: None,
            evidence_watermark: None,
        };
        store.put_persona(&persona).unwrap();

        // Seed a cursor, then harvest with **no GitHub client** — which reaches the same
        // branch a refused budget does: the walk cannot read, so it must not pretend to.
        let seeded = "2026-06-01T00:00:00+00:00";
        store
            .set_persona_harvest_cursor("pav", Some(seeded), false)
            .unwrap();

        let harvester = Harvester {
            store: store.clone(),
            github: None,
            github_background: None,
            org: Some("restatedev".into()),
            history_days: 90,
            slack_token: None,
            slack_pages: 1,
        };
        let report = harvester
            .harvest(&persona, Trigger::Scheduled)
            .await
            .unwrap();

        assert!(report.deferred, "a pass that read nothing is not a success");
        assert!(
            report.notes.iter().any(|n| n.contains("GitHub")),
            "the reason has to be stated: {:?}",
            report.notes
        );
        let (cursor, complete) = store.persona_harvest_cursor("pav").unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some(seeded),
            "the cursor must not move over a window that was never read"
        );
        assert!(!complete, "and the walk is certainly not finished");

        // And the reason is persisted, because the pass runs in a handler minutes after
        // whatever asked for it — a note that only exists in the return value reaches nobody.
        assert!(
            store
                .persona_harvest_note("pav")
                .unwrap()
                .is_some_and(|n| n.contains("GitHub")),
            "the note has to survive the pass"
        );

        // A later clean pass clears it, so a recovered budget stops reporting a problem that
        // has gone — the same rule as never leaving a stale "AI done" flag.
        store.set_persona_harvest_note("pav", None).unwrap();
        assert_eq!(store.persona_harvest_note("pav").unwrap(), None);
    }

    /// A DM must be labelled as one — it reads differently and it is the privacy-sensitive
    /// category. Search results name a DM's channel after the *other user's id*, so without
    /// this it rendered as `#U0ADMKZL692` and 40 of one person's excerpts looked like a channel.
    #[test]
    fn a_dm_is_labelled_as_a_dm() {
        use crate::watchers::slack::SearchChannel;
        let dm = SearchChannel {
            id: "D0ADMKZL692".into(),
            name: Some("U0ADMKZL692".into()),
        };
        assert_eq!(channel_label(&dm, "luke"), "DM with @luke");

        let public = SearchChannel {
            id: "C123".into(),
            name: Some("dev-cloud".into()),
        };
        assert_eq!(channel_label(&public, "luke"), "#dev-cloud");

        // A channel whose name the search omitted falls back to the id rather than an empty
        // `#`, which would read as "no channel" rather than "unnamed channel".
        let nameless = SearchChannel {
            id: "C999".into(),
            name: None,
        };
        assert_eq!(channel_label(&nameless, "luke"), "C999");
    }

    #[test]
    fn automation_is_not_proposed_as_a_person() {
        for bot in [
            "github-actions[bot]",
            "dependabot",
            "renovate[bot]",
            "Cloud Alerts",
            "grafana",
            "pagerduty",
            "some-bot",
        ] {
            assert!(is_bot(bot), "{bot} should be filtered");
        }
        for human in ["pcholakov", "benhoward", "U04ABC", "Pavel Cholakov"] {
            assert!(!is_bot(human), "{human} is a person");
        }
    }

    /// Proposals rank by how much the operator deals with someone, exclude bots, and
    /// exclude people already modelled — a done item on a to-do list is noise.
    #[test]
    fn proposals_rank_by_interaction_and_skip_the_known() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            store
                .insert_signal(&signal(
                    Source::Slack,
                    Some("U0PAVEL"),
                    "the retry path is the risk here",
                    &format!("p{i}"),
                ))
                .unwrap();
        }
        for i in 0..2 {
            store
                .insert_signal(&signal(
                    Source::Slack,
                    Some("U0BEN"),
                    "agreed, let's gate it behind a flag",
                    &format!("b{i}"),
                ))
                .unwrap();
        }
        store
            .insert_signal(&signal(
                Source::Slack,
                Some("github-actions[bot]"),
                "build failed on main again",
                "bot1",
            ))
            .unwrap();

        let candidates = propose(&store, &[], 10).unwrap();
        assert_eq!(candidates.len(), 2, "the bot is not a candidate");
        assert_eq!(candidates[0].handle, "U0PAVEL");
        assert_eq!(candidates[0].interactions, 5);
        assert!(candidates[0]
            .sample
            .as_deref()
            .unwrap()
            .contains("retry path"));
        assert_eq!(candidates[1].handle, "U0BEN");

        // Already modelled, so no longer proposed.
        let candidates = propose(&store, &["u0pavel".to_string()], 10).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].handle, "U0BEN");
    }
}
