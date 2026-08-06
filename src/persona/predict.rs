//! Predicting what one person would do about one subject.
//!
//! Three questions, one per [`PredictionKind`]: the review they would leave on this pull
//! request, the comment they would leave on this issue, whether and how they would engage in
//! this thread. All three are **rehearsals** — private, never posted, and labelled as
//! predictions wherever they are rendered.
//!
//! # This is not a second code review
//!
//! MuggleBot already reviews pull requests ([`crate::prdiff`]), and that review is the
//! operator's own tool for deciding whether a change should land. A prediction answers a
//! different question — *what will Pavel say about this* — and it is only worth anything if
//! it is grounded in Pavel rather than in the diff. Two rules keep it honest, and both are
//! enforced in code rather than asked for in the prompt:
//!
//! 1. **Every point must cite a trait** ([`crate::persona::Prediction::verify`]). A point
//!    citing nothing is the base model reviewing the diff with somebody's name on it, which
//!    is worse than useless — it is misattribution the operator might act on.
//! 2. **The verdict must be consistent with the counted base rate** ([`reconcile`]). Somebody
//!    who has requested changes twice in forty reviews does not get predicted to block your
//!    PR unless a trait explains why this one is different.
//!
//! # "Nothing" is a real prediction
//!
//! The most useful answer is often that they will not engage at all. A predictor that always
//! produces a review is a predictor that tells you nothing about who to ask, and the honest
//! answer for a docs change in front of a storage reviewer is silence. So `would_engage` is a
//! first-class field, the prompt says so, and [`reconcile`] refuses to leave a confident
//! engagement standing on nothing.

use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tracing::debug;

use super::{Facet, PredictedPoint, Prediction, PredictionKind, Profile, Trait};
use crate::reasoner::{CompletionRequest, Reasoner};
use crate::store::Store;

/// Characters of subject dossier shown to the model.
const MAX_DOSSIER_CHARS: usize = 14_000;

/// Characters of diff shown when predicting a code review.
///
/// Smaller than [`crate::prdiff`]'s budget on purpose: the question here is which of *their*
/// habits this change trips, not a line-by-line review, so the shape of the change matters
/// more than every hunk of it.
const MAX_DIFF_CHARS: usize = 12_000;

/// Predicted points accepted before verification.
const MAX_POINTS: usize = 6;

/// A `request_changes` prediction below this counted approval rate needs no explaining; above
/// it, blocking is the unusual outcome and has to be justified by a trait. See [`reconcile`].
const BLOCKS_FREELY_BELOW: f32 = 0.7;

/// What to predict, and everything needed to predict it.
///
/// The dossier and diff are assembled by the caller rather than read here, so this stays
/// callable — and testable — without Restate, a checkout, or a GitHub token. Same division as
/// the rest of the workflow layer: the handler gathers, the engine reasons.
pub struct Request<'a> {
    pub profile: &'a Profile,
    pub subject_key: &'a str,
    pub kind: PredictionKind,
    /// The subject's newest attributed signal id. Carried into the stored row so a stale
    /// prediction is visibly stale.
    pub watermark: &'a str,
    /// The subject, as [`crate::restate::workflows::explain::gather`] assembled it.
    pub dossier: String,
    /// The patches, for [`PredictionKind::CodeReview`]. `None` is not fatal — see
    /// [`Predictor::predict`], which says so in the caveats rather than inventing a review.
    pub diff: Option<String>,
    /// `local`, or the cloud model the operator named.
    pub produced_by: String,
}

pub struct Predictor {
    pub store: Arc<Store>,
}

impl Predictor {
    /// Predict, verify, store.
    pub async fn predict(&self, req: Request<'_>, reasoner: &dyn Reasoner) -> Result<Prediction> {
        let profile = req.profile;
        let mut caveats = profile.caveats();

        // An empty profile short-circuits before the model call. Asking a model to predict a
        // person it has been told nothing about produces a confident, fluent, entirely
        // invented answer — and `Prediction::verify` would strip it to an empty shell
        // afterwards anyway, having paid for it.
        if profile.traits.is_empty() {
            let mut p = empty(&req, caveats);
            p.verify(&profile.traits);
            self.store.put_persona_prediction(&p)?;
            return Ok(p);
        }

        if req.kind == PredictionKind::CodeReview && req.diff.as_deref().unwrap_or("").is_empty() {
            caveats.push(
                "No diff is stored for this pull request, so the prediction is from its \
                 description and conversation only. Read the diff first for a better one."
                    .into(),
            );
        }

        let system = system_prompt(req.kind, profile);
        let prompt = user_prompt(&req);
        let completion = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(1_600);
        let text = reasoner.complete(&completion).await?;

        let mut prediction = match parse(&req, &text, caveats.clone()) {
            Some(p) => p,
            None => {
                // Prose instead of JSON. Stored as a non-engagement with the reason rather
                // than failed: a stored "could not predict" stops the pane re-dispatching the
                // same failing pass every time it opens, which is what an absent row does.
                debug!(
                    "persona {}: prediction returned no JSON",
                    req.profile.persona.slug
                );
                let mut p = empty(&req, caveats);
                p.summary = "The model did not return a usable prediction. Try again, or pick a \
                             different model in the chat pane."
                    .into();
                p
            }
        };

        prediction.verify(&profile.traits);
        reconcile(&mut prediction, &profile.traits, profile);
        self.store.put_persona_prediction(&prediction)?;
        Ok(prediction)
    }
}

/// Hold the prediction to what the evidence supports, in both directions.
///
/// Modelled on [`crate::prdiff`]'s `reconcile`, which taught the same lesson the hard way: a
/// verdict that contradicts the findings underneath it is the findings' to win, and the
/// *demotion* direction matters as much as the promotion one.
///
/// 1. **Blocking needs a reason.** A predicted `request_changes` against somebody whose
///    counted approval rate is high, with no `reviews_for` or `bar` trait cited, is the model
///    reviewing the diff and attributing its own objections. Demoted to `comment`.
/// 2. **Engagement needs something to say.** `would_engage` with no surviving points is a
///    confident claim above an empty list, which reads as a rendering bug rather than as an
///    answer. Turned into a non-engagement.
/// 3. **Confidence is bounded by the traits behind it.** A prediction resting on one
///    low-confidence trait cannot be more certain than that trait.
pub fn reconcile(p: &mut Prediction, traits: &[Trait], profile: &Profile) {
    let cited: Vec<&Trait> = traits
        .iter()
        .filter(|t| p.points.iter().any(|pt| pt.because.contains(&t.id)))
        .collect();

    if p.recommendation.as_deref() == Some("request_changes") {
        let has_reason = cited
            .iter()
            .any(|t| matches!(t.facet, Facet::ReviewsFor | Facet::Bar));
        let blocks_rarely = profile
            .stats
            .approval_rate()
            .is_some_and(|r| r >= BLOCKS_FREELY_BELOW);
        if !has_reason && blocks_rarely {
            p.recommendation = Some("comment".into());
            p.caveats.push(format!(
                "Demoted from request_changes to comment: they approve {:.0}% of the reviews \
                 they decide, and nothing established about what they block on applies here.",
                profile.stats.approval_rate().unwrap_or(0.0) * 100.0
            ));
        }
    }

    if p.would_engage && p.points.is_empty() && p.summary.trim().is_empty() {
        p.would_engage = false;
        p.caveats.push(
            "Predicted as engaging, but with nothing to say — reported as no engagement.".into(),
        );
    }

    if let Some(best) = cited
        .iter()
        .map(|t| t.confidence)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    {
        // A prediction cannot be more confident than the strongest thing it rests on.
        p.confidence = p.confidence.min(best);
    }
    p.confidence = p.confidence.clamp(0.0, 1.0);
}

fn system_prompt(kind: PredictionKind, profile: &Profile) -> String {
    let task = match kind {
        PredictionKind::CodeReview => {
            "Predict the review THIS PERSON would leave on this pull request: the \
             recommendation they would pick, the note they would write above the button, and \
             the specific comments they would leave."
        }
        PredictionKind::IssueResponse => {
            "Predict the comment THIS PERSON would leave on this issue — or that they would \
             leave none."
        }
        PredictionKind::SlackEngagement => {
            "Predict whether THIS PERSON would engage in this thread, and if so what they \
             would say."
        }
    };
    let verdict_rule = match kind {
        PredictionKind::CodeReview => {
            "\n- `recommendation` must be one of approve, comment, request_changes, and must be \
             consistent with their counted approval rate unless a cited trait explains why \
             this change is different. A checker demotes an unexplained request_changes."
        }
        _ => "\n- Leave `recommendation` null; it applies only to a code review.",
    };
    format!(
        "You predict how ONE specific person will respond to a piece of work, from a profile \
         built out of things they actually wrote. A colleague uses this to rehearse before \
         asking them.\n\n\
         {task}\n\n\
         You are NOT reviewing the work yourself. Your own opinion of it is not wanted and \
         must not appear. Every prediction has to come from the profile:\n\
         - Every entry in `points` must cite the [tr:ID] traits it follows from. A point that \
           cites none is DISCARDED by a checker before anyone sees it, so do not pad.\n\
         - Never invent a quotation. Write what they would probably say, in their register — \
           never present it as something they did say.\n\
         - `would_engage` may be false, and often should be. If the profile shows they do not \
           engage with work like this, say so and return no points: \"they will not look at \
           this\" is a useful answer. Do not manufacture engagement.\n\
         - If the profile is too thin to answer, say that in `summary`, set `would_engage` \
           false, and return no points.\n\
         - Match the length they actually write ({median} characters is their median).{verdict_rule}\n\n\
         Reply with ONE JSON object and nothing else:\n\
         {{\"would_engage\":true,\"confidence\":0.0,\"recommendation\":null,\
         \"summary\":\"the note or reply they would write\",\
         \"points\":[{{\"text\":\"what they would say\",\"path\":\"file if applicable\",\
         \"line\":\"the exact line copied from the patch, if applicable\",\
         \"because\":[\"tr:...\"]}}]}}\n\
         At most {max} points.",
        median = profile.stats.median_excerpt_chars.max(1),
        max = MAX_POINTS,
    )
}

fn user_prompt(req: &Request<'_>) -> String {
    let mut out = req.profile.render();
    out.push_str("\n\n---\n\nTHE WORK\n");
    out.push_str(&truncate(&req.dossier, MAX_DOSSIER_CHARS));
    if let Some(diff) = req.diff.as_deref().filter(|d| !d.trim().is_empty()) {
        out.push_str("\n\nTHE DIFF\n");
        out.push_str(&truncate(diff, MAX_DIFF_CHARS));
    }
    out.push_str(&format!(
        "\n\n---\n\nWhat would {} do about {}?",
        req.profile.persona.display_name, req.subject_key
    ));
    out
}

/// Parse a reply into a prediction. `None` when there is no JSON in it at all.
fn parse(req: &Request<'_>, text: &str, caveats: Vec<String>) -> Option<Prediction> {
    let json = crate::reasoner::extract_json(text)?;
    let points: Vec<PredictedPoint> = json
        .get("points")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .take(MAX_POINTS)
                .filter_map(|p| {
                    let text = p.get("text").and_then(|t| t.as_str())?.trim();
                    (!text.is_empty()).then(|| PredictedPoint {
                        text: text.to_string(),
                        path: string_of(p.get("path")),
                        line: string_of(p.get("line")),
                        because: p
                            .get("because")
                            .and_then(|b| b.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(|s| s.trim().trim_start_matches("tr:").trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(Prediction {
        persona: req.profile.persona.slug.clone(),
        subject_key: req.subject_key.to_string(),
        kind: req.kind,
        watermark: req.watermark.to_string(),
        // Absent means engaging, since a model that returned points clearly thinks so; the
        // reconcile pass catches the case where nothing survived.
        would_engage: json
            .get("would_engage")
            .and_then(|w| w.as_bool())
            .unwrap_or(!points.is_empty()),
        confidence: json
            .get("confidence")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.4) as f32,
        recommendation: match req.kind {
            PredictionKind::CodeReview => recommendation(json.get("recommendation")),
            // Refused rather than carried for the other kinds: a model asked to leave it null
            // sometimes fills it in anyway, and an `approve` badge on a predicted Slack reply
            // is a category error the UI would render faithfully.
            _ => None,
        },
        summary: string_of(json.get("summary")).unwrap_or_default(),
        points,
        caveats,
        produced_by: req.produced_by.clone(),
        created_at: Utc::now(),
    })
}

/// Normalize a recommendation, refusing anything outside the three verdicts.
///
/// A model that answers `"needs work"` or `"REQUEST CHANGES"` means one of the three; a model
/// that answers `"lgtm"` means approve. Anything genuinely unrecognized becomes `None` rather
/// than a badge the UI has no styling for and the operator cannot interpret.
fn recommendation(node: Option<&serde_json::Value>) -> Option<String> {
    let raw = node?.as_str()?.trim().to_ascii_lowercase();
    let normalized = raw.replace([' ', '-'], "_");
    match normalized.as_str() {
        "approve" | "approved" | "lgtm" => Some("approve".into()),
        "comment" | "commented" | "comment_only" => Some("comment".into()),
        "request_changes" | "changes_requested" | "reject" | "needs_work" | "block" => {
            Some("request_changes".into())
        }
        _ => None,
    }
}

fn string_of(node: Option<&serde_json::Value>) -> Option<String> {
    node.and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "null")
        .map(str::to_string)
}

/// A prediction that predicts nothing, with the reason.
fn empty(req: &Request<'_>, caveats: Vec<String>) -> Prediction {
    Prediction {
        persona: req.profile.persona.slug.clone(),
        subject_key: req.subject_key.to_string(),
        kind: req.kind,
        watermark: req.watermark.to_string(),
        would_engage: false,
        confidence: 0.0,
        recommendation: None,
        summary: String::new(),
        points: Vec::new(),
        caveats,
        produced_by: req.produced_by.clone(),
        created_at: Utc::now(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n… (truncated)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::{Evidence, EvidenceKind, Persona, Stats};
    use crate::reasoner::MockReasoner;
    use crate::signal::Source;

    fn tr(id: &str, facet: Facet, confidence: f32) -> Trait {
        Trait {
            id: id.into(),
            persona: "pav".into(),
            facet,
            claim: format!("claim for {id}"),
            confidence,
            evidence: vec!["e1".into(), "e2".into()],
            counter_evidence: vec![],
            created_at: Utc::now(),
        }
    }

    fn review_ev(state: &str) -> Evidence {
        Evidence {
            id: format!("e-{state}"),
            persona: "pav".into(),
            source: Source::GitHub,
            kind: EvidenceKind::Review,
            subject_key: None,
            url: None,
            excerpt: "a comment long enough to keep".into(),
            context: None,
            state: Some(state.into()),
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
        }
    }

    fn profile(traits: Vec<Trait>, evidence: Vec<Evidence>) -> Profile {
        Profile {
            sme: vec![],
            context: vec![],
            persona: Persona {
                slug: "pav".into(),
                display_name: "Pavel".into(),
                role: None,
                notes: None,
                identities: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
                harvested_at: None,
                profiled_at: None,
                evidence_watermark: None,
            },
            traits,
            removed: vec![],
            stats: Stats::compute(&evidence),
        }
    }

    fn request<'a>(profile: &'a Profile, kind: PredictionKind) -> Request<'a> {
        Request {
            profile,
            subject_key: "o/r!412",
            kind,
            watermark: "w1",
            dossier: "A pull request that adds a retry to the store client.".into(),
            diff: Some("+ retry(3)".into()),
            produced_by: "local".into(),
        }
    }

    /// The recommendation vocabulary a real model actually returns, and the refusal of
    /// anything outside it — a badge the UI cannot style is worse than none.
    #[test]
    fn recommendations_are_normalized_or_refused() {
        for (raw, want) in [
            ("approve", "approve"),
            ("APPROVED", "approve"),
            ("lgtm", "approve"),
            ("comment", "comment"),
            ("request changes", "request_changes"),
            ("CHANGES_REQUESTED", "request_changes"),
            ("needs-work", "request_changes"),
        ] {
            assert_eq!(
                recommendation(Some(&serde_json::json!(raw))).as_deref(),
                Some(want),
                "{raw}"
            );
        }
        assert_eq!(recommendation(Some(&serde_json::json!("maybe?"))), None);
        assert_eq!(recommendation(Some(&serde_json::json!(null))), None);
        assert_eq!(recommendation(None), None);
    }

    /// An unexplained `request_changes` against somebody who approves nearly everything is
    /// the model's own objection wearing their name. Demoted, with the reason.
    #[test]
    fn blocking_without_a_reason_is_demoted_against_a_high_approval_rate() {
        let traits = vec![tr("t-style", Facet::Style, 0.8)];
        let evidence: Vec<Evidence> = (0..9)
            .map(|_| review_ev("APPROVED"))
            .chain(std::iter::once(review_ev("CHANGES_REQUESTED")))
            .collect();
        let p = profile(traits.clone(), evidence);

        let mut pred = Prediction {
            persona: "pav".into(),
            subject_key: "o/r!412".into(),
            kind: PredictionKind::CodeReview,
            watermark: "w1".into(),
            would_engage: true,
            confidence: 0.8,
            recommendation: Some("request_changes".into()),
            summary: "I don't like this".into(),
            points: vec![PredictedPoint {
                text: "terse note".into(),
                path: None,
                line: None,
                because: vec!["t-style".into()],
            }],
            caveats: vec![],
            produced_by: "local".into(),
            created_at: Utc::now(),
        };
        reconcile(&mut pred, &traits, &p);
        assert_eq!(pred.recommendation.as_deref(), Some("comment"));
        assert!(pred.caveats.iter().any(|c| c.contains("Demoted")));

        // With a `reviews_for` trait cited, blocking is explained and stands.
        let traits = vec![tr("t-blocks", Facet::ReviewsFor, 0.8)];
        let p = profile(
            traits.clone(),
            (0..9)
                .map(|_| review_ev("APPROVED"))
                .chain(std::iter::once(review_ev("CHANGES_REQUESTED")))
                .collect(),
        );
        let mut pred2 = pred.clone();
        pred2.recommendation = Some("request_changes".into());
        pred2.caveats.clear();
        pred2.points[0].because = vec!["t-blocks".into()];
        reconcile(&mut pred2, &traits, &p);
        assert_eq!(pred2.recommendation.as_deref(), Some("request_changes"));
    }

    /// Somebody who blocks often is not demoted — the rule is about an *unusual* outcome,
    /// not about being reluctant to predict a block.
    #[test]
    fn blocking_stands_for_somebody_who_blocks_often() {
        let traits = vec![tr("t-style", Facet::Style, 0.8)];
        let evidence: Vec<Evidence> = (0..6)
            .map(|_| review_ev("CHANGES_REQUESTED"))
            .chain((0..4).map(|_| review_ev("APPROVED")))
            .collect();
        let p = profile(traits.clone(), evidence);
        assert!(p.stats.approval_rate().unwrap() < BLOCKS_FREELY_BELOW);

        let mut pred = Prediction {
            persona: "pav".into(),
            subject_key: "o/r!412".into(),
            kind: PredictionKind::CodeReview,
            watermark: "w1".into(),
            would_engage: true,
            confidence: 0.5,
            recommendation: Some("request_changes".into()),
            summary: "no".into(),
            points: vec![PredictedPoint {
                text: "x".into(),
                path: None,
                line: None,
                because: vec!["t-style".into()],
            }],
            caveats: vec![],
            produced_by: "local".into(),
            created_at: Utc::now(),
        };
        reconcile(&mut pred, &traits, &p);
        assert_eq!(pred.recommendation.as_deref(), Some("request_changes"));
    }

    /// A prediction cannot be more certain than the strongest trait it rests on.
    #[test]
    fn confidence_is_bounded_by_the_traits_behind_it() {
        let traits = vec![tr("t1", Facet::ReviewsFor, 0.3)];
        let p = profile(traits.clone(), vec![review_ev("APPROVED")]);
        let mut pred = Prediction {
            persona: "pav".into(),
            subject_key: "o/r!412".into(),
            kind: PredictionKind::IssueResponse,
            watermark: "w".into(),
            would_engage: true,
            confidence: 0.95,
            recommendation: None,
            summary: "s".into(),
            points: vec![PredictedPoint {
                text: "x".into(),
                path: None,
                line: None,
                because: vec!["t1".into()],
            }],
            caveats: vec![],
            produced_by: "local".into(),
            created_at: Utc::now(),
        };
        reconcile(&mut pred, &traits, &p);
        assert!((pred.confidence - 0.3).abs() < 0.001);
    }

    /// The end-to-end pass, including the two things a real model gets wrong: an uncited
    /// point, and a recommendation on a prediction that is not a code review.
    #[tokio::test]
    async fn a_prediction_is_grounded_stored_and_scoped_to_its_kind() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let traits = vec![tr("t-blocks", Facet::ReviewsFor, 0.7)];
        store
            .put_persona(&Persona {
                slug: "pav".into(),
                display_name: "Pavel".into(),
                role: None,
                notes: None,
                identities: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
                harvested_at: None,
                profiled_at: None,
                evidence_watermark: None,
            })
            .unwrap();
        let p = profile(
            traits.clone(),
            (0..5).map(|_| review_ev("CHANGES_REQUESTED")).collect(),
        );

        let model = MockReasoner::new(
            r#"{"would_engage":true,"confidence":0.6,"recommendation":"request changes",
                "summary":"Needs a test on the retry path.",
                "points":[
                  {"text":"No test for the retry","path":"src/store.rs","because":["tr:t-blocks"]},
                  {"text":"I would rename this","because":[]}
                ]}"#,
        );
        let predictor = Predictor {
            store: store.clone(),
        };
        let got = predictor
            .predict(request(&p, PredictionKind::CodeReview), &model)
            .await
            .unwrap();

        assert!(got.would_engage);
        assert_eq!(got.recommendation.as_deref(), Some("request_changes"));
        assert_eq!(got.points.len(), 1, "the uncited point is dropped");
        assert_eq!(got.points[0].path.as_deref(), Some("src/store.rs"));
        // The `tr:` prefix the prompt asks for does not cost the citation.
        assert_eq!(got.points[0].because, vec!["t-blocks"]);
        assert!(got.caveats.iter().any(|c| c.contains("dropped")));

        // Stored and readable back.
        let stored = store
            .get_persona_prediction("pav", "o/r!412", PredictionKind::CodeReview)
            .unwrap()
            .expect("stored");
        assert_eq!(stored.points.len(), 1);

        // The same reply for a Slack engagement must not carry a review verdict.
        let slack = predictor
            .predict(request(&p, PredictionKind::SlackEngagement), &model)
            .await
            .unwrap();
        assert_eq!(slack.recommendation, None);
    }

    /// An empty profile costs no model call and predicts nothing — asking a model about a
    /// person it has been told nothing about produces a fluent invention.
    #[tokio::test]
    async fn an_empty_profile_short_circuits() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .put_persona(&Persona {
                slug: "pav".into(),
                display_name: "Pavel".into(),
                role: None,
                notes: None,
                identities: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
                harvested_at: None,
                profiled_at: None,
                evidence_watermark: None,
            })
            .unwrap();
        let p = profile(vec![], vec![]);
        let model = MockReasoner::new("this should never be called");
        let got = Predictor {
            store: store.clone(),
        }
        .predict(request(&p, PredictionKind::CodeReview), &model)
        .await
        .unwrap();
        assert!(!got.would_engage);
        assert_eq!(got.confidence, 0.0);
        assert!(got.summary.contains("Nothing is established"));
    }

    /// Prose instead of JSON is stored as an explicit failure, not left absent — an absent
    /// row makes the pane re-dispatch the same failing pass on every open.
    #[tokio::test]
    async fn an_unparseable_reply_is_stored_as_a_non_prediction() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .put_persona(&Persona {
                slug: "pav".into(),
                display_name: "Pavel".into(),
                role: None,
                notes: None,
                identities: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
                harvested_at: None,
                profiled_at: None,
                evidence_watermark: None,
            })
            .unwrap();
        let traits = vec![tr("t1", Facet::Style, 0.6)];
        let p = profile(traits, vec![review_ev("APPROVED")]);
        let model = MockReasoner::new("Pavel would probably be fine with this, I think.");
        let got = Predictor {
            store: store.clone(),
        }
        .predict(request(&p, PredictionKind::IssueResponse), &model)
        .await
        .unwrap();
        assert!(!got.would_engage);
        assert!(got.summary.contains("did not return a usable prediction"));
        assert!(store
            .get_persona_prediction("pav", "o/r!412", PredictionKind::IssueResponse)
            .unwrap()
            .is_some());
    }
}
