//! Subject-matter expertise — who to ask about what.
//!
//! The question this answers is the one you ask before you ask anybody anything: *who knows
//! about the storage layer?* A profile already says how somebody reviews; this says what they
//! review, and it is a different axis — the most useful colleague for a given change is often
//! not the one whose review style you were curious about.
//!
//! # Counted first, judged second
//!
//! Where somebody's activity concentrates is a **fact**, and it is derivable without a model:
//! every GitHub excerpt carries the subject it was written on (`owner/repo!123`) and, for an
//! inline review comment, the file path it was attached to. Group by those and the shape of
//! somebody's attention falls out.
//!
//! So [`areas`] counts, and the model is only asked for the part counting cannot supply:
//! whether their comments in an area are *specific* — which is the difference between somebody
//! who works there and somebody who knows it. That judgment arrives as an
//! [`Facet::Expertise`] trait and is folded in by [`with_depth`]; an area with no such trait is
//! reported as concentration alone, labelled as such.
//!
//! # Volume is not expertise, and saying so matters
//!
//! The tempting shortcut is "most comments in a repo ⇒ SME in that repo". It is wrong in a way
//! that would actively mislead: the person with the most comments on a repository is frequently
//! the one *learning* it, or the one who happens to own its release process, or a reviewer added
//! by a CODEOWNERS rule to everything. So an area has to clear three bars before it is offered
//! as expertise, and each one removes a specific false positive:
//!
//! 1. **Sustained** — at least [`MIN_EXCERPTS`] excerpts. Two comments is a visit.
//! 2. **Reviewing, not just talking** — at least [`MIN_REVIEWS`] review actions. Reviewing is
//!    somebody trusting you to judge the change; commenting is not. This is the bar that drops
//!    the person who asked three questions in an unfamiliar repo.
//! 3. **Concentrated** — at least [`MIN_SHARE`] of their own activity. Somebody who touched
//!    forty repos evenly is not an expert in the fortieth, and without this bar every repo they
//!    ever glanced at appears.
//!
//! What survives is described as *where their review activity concentrates*, with the counts
//! visible, and only called expertise when the model has also found their comments there to be
//! specific. That distinction is in the rendering, not just in this comment: an operator reading
//! "SME" needs to know which of the two they are looking at.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{Evidence, EvidenceKind, Facet, Trait};
use crate::signal::Source;

/// Excerpts an area needs before it is worth reporting.
const MIN_EXCERPTS: usize = 4;

/// Review actions an area needs. Reviewing is being trusted to judge; commenting is not.
const MIN_REVIEWS: usize = 2;

/// Share of the person's whole harvested activity an area needs.
const MIN_SHARE: f32 = 0.08;

/// Areas reported per persona, strongest first.
const MAX_AREAS: usize = 8;

/// What kind of thing an area is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaKind {
    /// `restatedev/restate-cloud` — a whole repository.
    Repo,
    /// `src/storage` — a directory root inside a repository, from the file paths their inline
    /// review comments were attached to. The sharper of the two, and only available for people
    /// who review inline.
    Component,
}

impl AreaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AreaKind::Repo => "repo",
            AreaKind::Component => "component",
        }
    }
}

/// One area somebody's activity concentrates in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmeArea {
    pub area: String,
    pub kind: AreaKind,
    /// Excerpts of theirs in this area.
    pub excerpts: usize,
    /// Of which review actions — the bar that separates judging from talking.
    pub reviews: usize,
    /// Share of their whole harvested activity.
    pub share: f32,
    /// The `expertise` trait whose claim covers this area, if the model found one.
    ///
    /// `None` means **concentration only**: they are demonstrably active here and nothing has
    /// established that their comments are specific. The two must not render alike — one is
    /// "ask them", the other is "they are around".
    pub depth: Option<String>,
    /// Trait id backing `depth`, so the claim is checkable.
    pub depth_trait: Option<String>,
    /// A few excerpt ids, so the operator can read what they actually said here.
    pub evidence: Vec<String>,
}

impl SmeArea {
    /// Whether this is expertise rather than mere presence — i.e. the model found their
    /// comments here specific, on top of the counted concentration.
    pub fn is_expert(&self) -> bool {
        self.depth.is_some()
    }

    /// Ranking weight. Reviews count double: being asked to judge a change is a stronger
    /// signal than having commented on one, and a component is sharper than a whole repo.
    fn weight(&self) -> f32 {
        let base = self.excerpts as f32 + self.reviews as f32;
        let kind_bonus = match self.kind {
            AreaKind::Component => 1.15,
            AreaKind::Repo => 1.0,
        };
        let depth_bonus = if self.is_expert() { 1.5 } else { 1.0 };
        base * kind_bonus * depth_bonus
    }
}

/// Where this person's activity concentrates, strongest first.
///
/// Counted from the evidence alone — no model. See the module docs for the three bars an area
/// has to clear and the false positive each one removes.
pub fn areas(evidence: &[Evidence]) -> Vec<SmeArea> {
    // Only GitHub evidence carries a subject and a path. Slack tells you how somebody talks,
    // not what code they know: a channel is a room, not a subject area, and treating
    // `#cloud-alerts` as an area of expertise would make everybody an SRE.
    let github: Vec<&Evidence> = evidence
        .iter()
        .filter(|e| e.source == Source::GitHub)
        .collect();
    if github.is_empty() {
        return Vec::new();
    }
    let total = github.len() as f32;

    let mut buckets: HashMap<(AreaKind, String), Vec<&Evidence>> = HashMap::new();
    for e in &github {
        if let Some(repo) = repo_of(e) {
            buckets.entry((AreaKind::Repo, repo)).or_default().push(e);
        }
        if let Some(component) = component_of(e) {
            buckets
                .entry((AreaKind::Component, component))
                .or_default()
                .push(e);
        }
    }

    let mut out: Vec<SmeArea> = buckets
        .into_iter()
        .filter_map(|((kind, area), items)| {
            let reviews = items.iter().filter(|e| e.kind.is_review()).count();
            let share = items.len() as f32 / total;
            // All three bars, or it is not offered. See the module docs.
            if items.len() < MIN_EXCERPTS || reviews < MIN_REVIEWS || share < MIN_SHARE {
                return None;
            }
            Some(SmeArea {
                area,
                kind,
                excerpts: items.len(),
                reviews,
                share,
                depth: None,
                depth_trait: None,
                // A handful, newest first — enough to read, not a dump.
                evidence: {
                    let mut ids: Vec<&Evidence> = items.clone();
                    ids.sort_by_key(|e| std::cmp::Reverse(e.occurred_at));
                    ids.into_iter().take(5).map(|e| e.id.clone()).collect()
                },
            })
        })
        .collect();

    out.sort_by(|a, b| {
        b.weight()
            .partial_cmp(&a.weight())
            .unwrap_or(std::cmp::Ordering::Equal)
            // Stable on ties, so the page does not reshuffle between reads.
            .then(a.area.cmp(&b.area))
    });
    out.truncate(MAX_AREAS);
    out
}

/// Fold the model's `expertise` findings onto the counted areas.
///
/// An area is matched to a trait by the trait's claim *mentioning* the area — the repo name, or
/// the component path, or its last segment. Deliberately literal: inferring which claim is
/// "about" which area would be a guess layered on a judgment, and a wrong pairing would attach
/// somebody's real expertise to the wrong part of the codebase.
pub fn with_depth(mut areas: Vec<SmeArea>, traits: &[Trait]) -> Vec<SmeArea> {
    let expertise: Vec<&Trait> = traits
        .iter()
        .filter(|t| t.facet == Facet::Expertise)
        .collect();
    for area in &mut areas {
        let needles = needles_for(&area.area);
        // The **longest** matching needle wins, and matching is on whole tokens.
        //
        // Substring matching credited the wrong repo on a real profile: `restatedev/restate`
        // yields the needle `restate`, which is a substring of a claim about
        // `restate-cloud` — so an expertise finding about the control plane was attached to
        // the runtime repo as well, and all three of somebody's areas showed the same claim.
        // Tokenizing fixes the direction that matters (`restate` no longer matches
        // `restate-cloud`), and preferring the longest needle stops a short one winning where
        // a specific one also matches.
        let best = expertise
            .iter()
            .filter_map(|t| {
                let tokens = tokenize(&t.claim);
                needles
                    .iter()
                    .filter(|n| tokens.iter().any(|tok| tok == *n))
                    .map(|n| n.len())
                    .max()
                    .map(|len| (len, *t))
            })
            .max_by_key(|(len, _)| *len);
        if let Some((_, t)) = best {
            area.depth = Some(t.claim.clone());
            area.depth_trait = Some(t.id.clone());
        }
    }
    // Re-sort: depth changes the weight, and an area the model called out should lead.
    areas.sort_by(|a, b| {
        b.weight()
            .partial_cmp(&a.weight())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.area.cmp(&b.area))
    });
    areas
}

/// The strings a claim might name this area by.
///
/// `restatedev/restate-cloud` → also `restate-cloud`; `src/storage` → also `storage`. Bare
/// segments shorter than four characters are dropped: matching a claim on `src` or `api` would
/// pair almost any claim with almost any area.
/// Split a claim into the tokens a needle can match against.
///
/// Splits on whitespace and punctuation but **keeps `-`, `/`, `.` and `_`**, because those are
/// what make an identifier an identifier: `restate-cloud`, `src/storage` and `Cargo.toml` have
/// to survive as single tokens or the whole point of matching on tokens is lost.
fn tokenize(claim: &str) -> Vec<String> {
    claim
        .to_ascii_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '/' | '.' | '_')))
        .filter(|t| !t.is_empty())
        // Trailing punctuation that survived the split — `restate-cloud,` keeps its comma out
        // but `restate-cloud.` would keep the dot, which is legal in an identifier.
        .map(|t| t.trim_matches('.').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn needles_for(area: &str) -> Vec<String> {
    let lower = area.to_ascii_lowercase();
    let mut out = vec![lower.clone()];
    // A component area is `repo:path` — see `component_of`. A claim names the *path*
    // (`src/storage`), never the internal prefixed form, so the path has to be a needle in its
    // own right or a component can never be matched at all.
    let bare = lower
        .split_once(':')
        .map(|(_, path)| path)
        .unwrap_or(&lower);
    if bare != lower {
        out.push(bare.to_string());
    }
    if let Some(last) = bare.rsplit('/').next() {
        // Four characters, so a needle of `src` or `api` cannot pair almost any claim with
        // almost any area.
        if last.len() >= 4 && last != bare {
            out.push(last.to_string());
        }
    }
    out.sort_by_key(|n| std::cmp::Reverse(n.len()));
    out.dedup();
    out
}

/// `owner/repo` from an evidence row's subject key.
fn repo_of(e: &Evidence) -> Option<String> {
    let key = e.subject_key.as_deref()?;
    let repo = key.split(['#', '!']).next()?;
    (repo.contains('/') && !repo.is_empty()).then(|| repo.to_string())
}

/// The directory root of the file an inline review comment was attached to, prefixed by the
/// repo so two repositories' `src/` are different areas.
///
/// Two path segments, because one is usually `src` — an area called `src` is not an area. A
/// path with a single segment (a top-level file like `Cargo.toml`) yields nothing: a persona
/// whose expertise is "the root directory" is not a useful answer.
fn component_of(e: &Evidence) -> Option<String> {
    let context = e.context.as_deref()?;
    // `context` holds a file path only for inline review comments; for everything else it is
    // the subject key or a channel, neither of which is a path into the code.
    if e.kind != EvidenceKind::ReviewComment || !context.contains('/') {
        return None;
    }
    let repo = repo_of(e)?;
    let mut parts = context.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    // `second` being the file itself means the path was `dir/file.rs`, which is a fine area.
    (!first.is_empty() && !second.is_empty())
        .then(|| format!("{repo}:{first}/{}", strip_file(second)))
}

/// Drop a trailing filename so `src/storage/mod.rs` and `src/storage/wal.rs` are one area.
fn strip_file(segment: &str) -> String {
    match segment.rsplit_once('.') {
        // Looks like a file — the area is the directory above it, which the caller already has.
        Some((stem, ext)) if !ext.contains('/') && ext.len() <= 5 => stem.to_string(),
        _ => segment.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn ev(id: &str, subject: &str, path: Option<&str>, kind: EvidenceKind) -> Evidence {
        Evidence {
            id: id.into(),
            persona: "p".into(),
            source: Source::GitHub,
            kind,
            subject_key: Some(subject.into()),
            url: None,
            excerpt: "a comment long enough to be kept".into(),
            context: path
                .map(str::to_string)
                .or_else(|| Some(subject.to_string())),
            state: None,
            occurred_at: Utc::now() - Duration::days(1),
            ingested_at: Utc::now(),
        }
    }

    fn reviews(repo: &str, n: usize, path: Option<&str>) -> Vec<Evidence> {
        (0..n)
            .map(|i| {
                ev(
                    &format!("{repo}-{i}"),
                    &format!("{repo}!{i}"),
                    path,
                    EvidenceKind::ReviewComment,
                )
            })
            .collect()
    }

    /// Sustained review activity in one repo is an area; a visit is not.
    #[test]
    fn concentration_needs_to_be_sustained_reviewed_and_concentrated() {
        // Six inline reviews in one repo: clears all three bars.
        let strong = reviews("o/storage", 6, Some("src/storage/wal.rs"));
        let got = areas(&strong);
        assert!(
            got.iter()
                .any(|a| a.area == "o/storage" && a.kind == AreaKind::Repo),
            "{got:?}"
        );
        // And the sharper component area, derived from the file path.
        assert!(
            got.iter()
                .any(|a| a.kind == AreaKind::Component && a.area.contains("src/storage")),
            "{got:?}"
        );

        // Two comments in a repo is a visit — dropped by MIN_EXCERPTS even though it is 100%
        // of that (tiny) sample.
        let visit = reviews("o/elsewhere", 2, None);
        assert!(areas(&visit).is_empty());
    }

    /// Talking is not judging. The person who asked three questions in an unfamiliar repo is
    /// not an SME in it, and this is the bar that drops them.
    #[test]
    fn commenting_without_reviewing_is_not_expertise() {
        let chatter: Vec<Evidence> = (0..10)
            .map(|i| {
                ev(
                    &format!("c{i}"),
                    &format!("o/unfamiliar#{i}"),
                    None,
                    EvidenceKind::IssueComment,
                )
            })
            .collect();
        assert!(
            areas(&chatter).is_empty(),
            "ten comments and no reviews is not an area of expertise"
        );

        // The same volume with reviews in it does qualify.
        let mut mixed = chatter.clone();
        mixed.extend(reviews("o/unfamiliar", 3, None));
        assert!(areas(&mixed).iter().any(|a| a.area == "o/unfamiliar"));
    }

    /// Somebody spread evenly over many repos is not an expert in any of them, and without the
    /// share bar every repo they ever glanced at appears.
    #[test]
    fn breadth_is_not_depth() {
        let mut wide = Vec::new();
        for r in 0..20 {
            wide.extend(reviews(&format!("o/r{r}"), 4, None));
        }
        // 4/80 = 5%, under MIN_SHARE — so none of the twenty is offered.
        let got = areas(&wide);
        assert!(
            got.is_empty(),
            "evenly spread activity must not yield twenty areas: {got:?}"
        );
    }

    /// Slack is how somebody talks, not what code they know. A channel is a room, and treating
    /// `#cloud-alerts` as expertise would make everybody an SRE.
    #[test]
    fn slack_is_not_an_area_of_expertise() {
        let mut chat: Vec<Evidence> = (0..10)
            .map(|i| {
                let mut e = ev(
                    &format!("s{i}"),
                    "C1/1721822400.001",
                    None,
                    EvidenceKind::Slack,
                );
                e.source = Source::Slack;
                e.context = Some("#cloud-alerts".into());
                e
            })
            .collect();
        assert!(areas(&chat).is_empty());

        // GitHub evidence alongside it is still counted, and the share is over the GitHub
        // subset — otherwise a chatty person's real expertise would be diluted below the bar
        // by their own Slack volume.
        chat.extend(reviews("o/storage", 5, Some("src/storage/wal.rs")));
        assert!(areas(&chat).iter().any(|a| a.area == "o/storage"));
    }

    /// Concentration and expertise must not render alike: one says "ask them", the other says
    /// "they are around".
    #[test]
    fn depth_comes_from_a_cited_trait_and_is_optional() {
        let evidence = reviews("o/restate-cloud", 6, Some("src/storage/wal.rs"));
        let counted = areas(&evidence);
        assert!(
            counted.iter().all(|a| !a.is_expert()),
            "counting alone never claims expertise"
        );

        let t = Trait {
            id: "tr-1".into(),
            persona: "p".into(),
            facet: Facet::Expertise,
            claim: "Comments in restate-cloud name specific failure modes in the WAL".into(),
            confidence: 0.7,
            evidence: vec!["o/restate-cloud-0".into()],
            counter_evidence: vec![],
            created_at: Utc::now(),
        };
        let judged = with_depth(counted, &[t]);
        let repo = judged
            .iter()
            .find(|a| a.area == "o/restate-cloud")
            .expect("the repo area");
        assert!(repo.is_expert());
        assert_eq!(repo.depth_trait.as_deref(), Some("tr-1"));
        // The expert area leads, because depth outweighs raw volume.
        assert!(judged[0].is_expert());

        // A trait about something else does not attach. A wrong pairing would credit somebody's
        // real expertise to the wrong part of the codebase.
        let unrelated = Trait {
            claim: "Comments in the billing service are specific".into(),
            ..judged
                .iter()
                .find(|a| a.is_expert())
                .map(|_| Trait {
                    id: "tr-2".into(),
                    persona: "p".into(),
                    facet: Facet::Expertise,
                    claim: String::new(),
                    confidence: 0.5,
                    evidence: vec![],
                    counter_evidence: vec![],
                    created_at: Utc::now(),
                })
                .unwrap()
        };
        let fresh = with_depth(areas(&evidence), &[unrelated]);
        assert!(fresh.iter().all(|a| !a.is_expert()));
    }

    /// The false match from a real profile: `restatedev/restate` credited with a claim about
    /// `restate-cloud`, because `restate` is a substring of `restate-cloud`.
    ///
    /// All three of somebody's areas showed the identical depth claim, which reads as "expert
    /// everywhere" and was really "expert in one place, matched three times".
    #[test]
    fn a_shorter_repo_name_does_not_absorb_a_longer_one() {
        let cloud = reviews("restatedev/restate-cloud", 8, Some("src/kube/reconcile.rs"));
        let runtime = reviews("restatedev/restate", 8, None);
        let evidence: Vec<Evidence> = cloud.into_iter().chain(runtime).collect();

        let about_cloud = Trait {
            id: "tr-cloud".into(),
            persona: "p".into(),
            facet: Facet::Expertise,
            claim: "His detailed comments cluster on control-plane code in restate-cloud, naming \
                    exact symbols"
                .into(),
            confidence: 0.6,
            evidence: vec![],
            counter_evidence: vec![],
            created_at: Utc::now(),
        };
        let judged = with_depth(areas(&evidence), &[about_cloud]);

        let cloud_area = judged
            .iter()
            .find(|a| a.area == "restatedev/restate-cloud")
            .expect("the cloud repo is an area");
        assert!(cloud_area.is_expert(), "the claim names this repo");

        let runtime_area = judged
            .iter()
            .find(|a| a.area == "restatedev/restate")
            .expect("the runtime repo is an area");
        assert!(
            !runtime_area.is_expert(),
            "a claim about restate-cloud must not credit restate"
        );
    }

    /// Tokens keep the characters that make an identifier one.
    #[test]
    fn tokenizing_preserves_identifiers() {
        let t = tokenize("Names src/storage and Cargo.toml, plus restate-cloud's reconcile.");
        assert!(t.contains(&"src/storage".to_string()));
        assert!(t.contains(&"cargo.toml".to_string()));
        // Possessives split on the apostrophe, leaving the identifier intact.
        assert!(t.contains(&"restate-cloud".to_string()));
        // And a bare word does not match a hyphenated one.
        assert!(!t.contains(&"restate".to_string()));
    }

    /// Where several claims match, the most specific wins — otherwise a claim about the whole
    /// repo would beat one about the exact component the operator asked about.
    #[test]
    fn the_longest_matching_needle_wins() {
        let evidence = reviews("o/r", 8, Some("src/storage/wal.rs"));
        let broad = Trait {
            id: "tr-broad".into(),
            persona: "p".into(),
            facet: Facet::Expertise,
            claim: "Specific about o/r generally".into(),
            confidence: 0.5,
            evidence: vec![],
            counter_evidence: vec![],
            created_at: Utc::now(),
        };
        let narrow = Trait {
            id: "tr-narrow".into(),
            claim: "Names exact failure modes in src/storage".into(),
            ..broad.clone()
        };
        let judged = with_depth(areas(&evidence), &[broad, narrow]);
        let component = judged
            .iter()
            .find(|a| a.kind == AreaKind::Component)
            .expect("the component area");
        assert_eq!(
            component.depth_trait.as_deref(),
            Some("tr-narrow"),
            "the claim naming the component beats the one naming the repo"
        );
    }

    /// A one-segment path is not an area: "the root directory" is not a useful answer, and
    /// neither is `src`.
    #[test]
    fn paths_become_areas_only_when_they_name_something() {
        let e = ev(
            "x",
            "o/r!1",
            Some("Cargo.toml"),
            EvidenceKind::ReviewComment,
        );
        assert_eq!(component_of(&e), None, "a top-level file is not an area");

        let e = ev(
            "x",
            "o/r!1",
            Some("src/storage/wal.rs"),
            EvidenceKind::ReviewComment,
        );
        // Repo-prefixed, so two repositories' `src/` are different areas.
        assert_eq!(component_of(&e).as_deref(), Some("o/r:src/storage"));

        // Only inline review comments carry a path; for everything else `context` is a subject
        // key or a channel, and reading one as a path would invent areas called `o/r!1`.
        let e = ev(
            "x",
            "o/r!1",
            Some("src/storage/wal.rs"),
            EvidenceKind::IssueComment,
        );
        assert_eq!(component_of(&e), None);
    }
}
