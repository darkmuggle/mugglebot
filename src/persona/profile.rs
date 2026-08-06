//! Distilling harvested evidence into a profile — one facet at a time.
//!
//! # Why one facet per pass
//!
//! The first version asked the model, once, to "describe how this person works" over
//! everything harvested. It returned a fluent paragraph about a thoughtful, detail-oriented
//! engineer who values clear communication. Every word of it was true of everybody, and none
//! of it predicted anything.
//!
//! That is the same failure [`crate::prdiff`] hit when it asked one small model to read a
//! large diff, decide, and justify in a single pass — and the fix is the same shape. Each
//! [`Facet`] is asked on its own, over a bounded sample of only the evidence that facet can
//! legitimately be judged from, with a schema that has nowhere to put prose. A model with one
//! narrow question and forty excerpts in front of it produces "asks for a test on anything
//! touching the store, in 4 of 6 reviews"; the same model with ten questions produces an
//! adjective.
//!
//! # Why the sample is spread, not the newest N
//!
//! Newest-N is the obvious sample and it is biased: somebody who spent last week reviewing
//! one large migration comes out looking like a person who only ever talks about migrations.
//! A profile is meant to describe a habit, so the sample is spread across the whole harvested
//! range — see [`spread`].
//!
//! # Trait ids are derived from the claim
//!
//! A profile pass replaces the previous traits wholesale, and predictions cite trait ids in
//! [`crate::persona::PredictedPoint::because`]. If ids were freshly minted each pass, every
//! stored prediction would lose its citations the next time the profile refreshed and
//! [`crate::persona::Prediction::verify`] would strip it to nothing on re-read. Deriving the
//! id from the claim text means a re-run that reaches the same conclusion keeps the same id,
//! and a genuinely new claim gets a new one.

use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, warn};

use super::{verify, Evidence, Facet, Persona, Profile, Removed, Stats, Trait};
use crate::reasoner::{CompletionRequest, Reasoner};
use crate::store::Store;

/// Excerpts shown to the model per facet.
///
/// Forty is enough to establish a habit and few enough that a 33B local model keeps them all
/// in view. Beyond about this the model starts summarizing the sample rather than reading it,
/// and the citations get vaguer — which [`verify`] then throws away, so the extra excerpts
/// cost time and lose information.
const MAX_EXCERPTS_PER_FACET: usize = 40;

/// Total characters of evidence per facet prompt.
const MAX_EVIDENCE_CHARS: usize = 24_000;

/// Traits accepted per facet before verification. More than this and the model has stopped
/// finding patterns and started paraphrasing individual comments.
const MAX_TRAITS_PER_FACET: usize = 5;

/// Traits kept in one profile, across every facet.
///
/// Ten facets at [`MAX_TRAITS_PER_FACET`] each is fifty, and a capable model fills that — a real
/// profile came back with 39, of which the last dozen were hedged restatements of the first
/// dozen. Capped on the profile rather than the facet because which facets have something to say
/// varies by person: a per-facet cap would silence a rich `reviews_for` to make room for a thin
/// `escalation`.
const MAX_TRAITS_PER_PROFILE: usize = 18;

/// Facets with fewer than this many admissible excerpts are skipped entirely.
///
/// Two comments cannot establish a habit, and the model will happily claim one from them.
/// Skipping is better than distilling and then dropping on confidence: the caveat "nothing
/// established for reviews_for" is a true and useful statement, where a 0.2-confidence trait
/// is noise the operator has to evaluate.
const MIN_EXCERPTS_PER_FACET: usize = 3;

pub struct Distiller {
    pub store: Arc<Store>,
    /// The tier that forms the opinion, from `[personas] profile_tier` — Claude by default.
    ///
    /// This is the one place in MuggleBot whose default is not on-device, and the reversal was
    /// earned rather than chosen: the local 33B model produced sensible claims and then mangled
    /// the citation ids, so verification correctly dropped every one and the profile came back
    /// empty. Ten facet passes behind the single local permit is also tens of minutes.
    ///
    /// Every safeguard applies whichever tier answers — falsifiability, citations,
    /// counter-evidence, the removal report. A stronger model just clears the bar more often.
    /// The cost is that harvested excerpts of a colleague's writing leave the machine; see the
    /// config docs, which say so plainly.
    pub reasoner: Arc<dyn Reasoner>,
    /// The tier's name, recorded so a profile says who formed it.
    pub tier: String,
}

impl Distiller {
    /// Rebuild a persona's profile from everything harvested, and store it.
    ///
    /// Returns what was kept *and* what was refused: on a first run against real review
    /// history the removed list is routinely longer than the profile, and hiding it would
    /// make the filter undebuggable.
    pub async fn distil(&self, persona: &Persona) -> Result<Profile> {
        let evidence = self.store.persona_evidence(&persona.slug, None)?;
        let stats = Stats::compute(&evidence);
        // Read up front and fed into every facet prompt: a fact the operator asserted is
        // context the model should reason *with*, not something to rediscover. "Owns the
        // release process" changes what their review comments mean.
        let context = self.store.persona_context(&persona.slug)?;
        let mut traits = Vec::new();
        let mut removed = Vec::new();

        for facet in Facet::ALL {
            let sample = spread(
                &evidence
                    .iter()
                    .filter(|e| facet.admits(e.kind))
                    .cloned()
                    .collect::<Vec<_>>(),
                MAX_EXCERPTS_PER_FACET,
            );
            if sample.len() < MIN_EXCERPTS_PER_FACET {
                debug!(
                    "persona {}: {} skipped, only {} admissible excerpt(s)",
                    persona.slug,
                    facet.as_str(),
                    sample.len()
                );
                continue;
            }
            match self.facet_pass(persona, *facet, &sample).await {
                Ok(candidates) => {
                    // A pass that produced nothing at all is reported, not passed over.
                    //
                    // `verify` only records what it *refused*, so a facet whose reply held no
                    // usable traits left no trace anywhere: the profile came back with zero
                    // traits and zero refusals, which reads as "nothing established about this
                    // person" when the truth was "the model answered in a shape we could not
                    // use, eight times". Two states that must never render identically.
                    if candidates.is_empty() {
                        removed.push(Removed {
                            facet: facet.as_str().into(),
                            claim: String::new(),
                            why: format!(
                                "the pass returned no usable traits from {} excerpt(s) — either \
                                 nothing is established here, or the reply was not in the \
                                 requested shape",
                                sample.len()
                            ),
                        });
                        continue;
                    }
                    // Verified against the *whole* harvested set, not the sample: a model
                    // shown forty excerpts sometimes cites a forty-first it saw in an earlier
                    // facet's prompt, and that citation is real.
                    let (kept, dropped) = verify(*facet, candidates, &evidence);
                    traits.extend(kept);
                    removed.extend(dropped);
                }
                // One facet failing is not the profile failing. A model that returns prose
                // for `escalation` must not cost the nine facets that parsed.
                Err(e) => {
                    warn!(
                        "persona {}: facet {} failed: {e:#}",
                        persona.slug,
                        facet.as_str()
                    );
                    removed.push(Removed {
                        facet: facet.as_str().into(),
                        claim: String::new(),
                        why: format!("the pass failed: {e:#}"),
                    });
                }
            }
        }

        traits.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // One claim, one place. Each facet is asked in isolation, so the same observation
        // legitimately occurs to the model twice — the first live profile carried "comments only
        // on the first file of a large diff" under both `reviews_for` and `ignores`, which reads
        // as two independent findings and is one. Kept under the facet where it scored highest,
        // which the sort above has already put first, and the loser is reported rather than
        // vanishing.
        let mut seen: Vec<String> = Vec::new();
        let mut deduped = Vec::with_capacity(traits.len());
        for t in traits {
            let key = t.claim.to_ascii_lowercase();
            if seen.contains(&key) {
                removed.push(Removed {
                    facet: t.facet.as_str().into(),
                    claim: t.claim,
                    why: "the same claim scored higher under another facet".into(),
                });
                continue;
            }
            seen.push(key);
            deduped.push(t);
        }
        // A profile is read top to bottom before a conversation, so its length is a cost.
        //
        // Ten facets at five traits each is fifty, and a capable model fills that: a real
        // profile came back with 39, of which the last dozen were hedged restatements of the
        // first dozen. The cap is on the *profile*, not the facet, because which facets have
        // something to say varies by person — capping per facet would silence a rich
        // `reviews_for` to make room for a thin `escalation`.
        let mut traits = deduped;
        if traits.len() > MAX_TRAITS_PER_PROFILE {
            for t in traits.split_off(MAX_TRAITS_PER_PROFILE) {
                removed.push(Removed {
                    facet: t.facet.as_str().into(),
                    claim: t.claim,
                    why: format!(
                        "outside the {MAX_TRAITS_PER_PROFILE} strongest claims in this profile"
                    ),
                });
            }
        }
        let traits = traits;
        // Not the newest excerpt's id — see [`Store::persona_evidence_watermark`] for why
        // that token does not move when the backward walk adds older material, and why a
        // `PersonaProfile` keyed on it would freeze the profile for the whole backfill.
        let watermark = self.store.persona_evidence_watermark(&persona.slug)?;
        self.store
            .replace_persona_traits(&persona.slug, &traits, &removed)?;
        self.store
            .touch_persona_profiled(&persona.slug, watermark.as_deref())?;

        let mut persona = persona.clone();
        persona.profiled_at = Some(Utc::now());
        persona.evidence_watermark = watermark;
        // Annotated with the depth the `expertise` facet just established, so the profile the
        // caller gets back is the same one a later read assembles.
        let sme = super::sme::with_depth(super::sme::areas(&evidence), &traits);
        Ok(Profile {
            persona,
            traits,
            removed,
            stats,
            sme,
            context,
        })
    }

    /// One facet, one model call.
    async fn facet_pass(
        &self,
        persona: &Persona,
        facet: Facet,
        sample: &[Evidence],
    ) -> Result<Vec<Trait>> {
        // Short ordinal tokens — `e1`, `e2` — not the real evidence ids.
        //
        // The real id is `{persona}/{source}/{kind}/{url}`: about a hundred characters, ending
        // in a GitHub permalink. Asking a 33B model to transcribe that exactly is asking for the
        // failure it produced on the first live run — it cited
        // `pavel/github/review_comment/…#issuecomment-5168030937` for an excerpt stored as
        // `pavel/github/issue_comment/…#issuecomment-5168030937`, having "corrected" the one
        // segment that looks guessable. Every citation missed, `verify` correctly dropped every
        // trait, and the profile came back empty from a model that had answered well.
        //
        // Same lesson as `prdiff`'s line anchoring: anchor on something the model can actually
        // reproduce and resolve it yourself. `e7` has no structure to fix and is one token to
        // copy.
        let mut index: Vec<&Evidence> = Vec::new();
        let mut evidence_block = String::new();
        for e in sample {
            let token = format!("e{}", index.len() + 1);
            let line = e.render_as(&token);
            if evidence_block.len() + line.len() > MAX_EVIDENCE_CHARS {
                break;
            }
            evidence_block.push_str(&line);
            evidence_block.push('\n');
            index.push(e);
        }

        let system = format!(
            "You build a behavioural profile of one person from things they actually wrote, so \
             that a colleague can predict how they will respond before asking them.\n\n\
             You are answering ONE question about {name}: {question}\n\n\
             Rules, and a claim breaking any of them is discarded by a checker before anyone \
             reads it:\n\
             1. Every claim must be about OBSERVABLE BEHAVIOUR and must be falsifiable against \
                their next message. \"Asks for a test on anything touching the store\" is a \
                claim. \"Cares about quality\" is not, and is discarded.\n\
             2. Every claim must cite the [eN] excerpts it comes from, copied EXACTLY as \
                written — [e3], not a paraphrase and not a URL. A claim with no citation is \
                discarded. Never cite a token that is not in the list below.\n\
             3. Say the unflattering thing when the excerpts show it — \"stops replying when \
                pushed back on\", \"comments only on the first file of a large diff\" are \
                useful and citable. Do NOT infer anything about their health, politics, \
                religion, personal life, or worth as a person: none of it is in the excerpts \
                and all of it is discarded.\n\
             4. If excerpts contradict a claim, list them in counter_evidence rather than \
                omitting them. A contested pattern is a more useful answer than a clean one.\n\
             5. If the excerpts establish nothing for this question, return an empty list. That \
                is a correct and useful answer — do not manufacture a pattern from one comment.\n\n\
             Reply with ONE JSON object and nothing else:\n\
             {{\"traits\":[{{\"claim\":\"one sentence about observable behaviour\",\
             \"confidence\":0.0,\"evidence\":[\"e3\",\"e7\"],\"counter_evidence\":[\"e5\"]}}]}}\n\
             At most {max} traits.",
            name = persona.display_name,
            question = facet.question(),
            max = MAX_TRAITS_PER_FACET,
        );

        let prompt = format!(
            "Excerpts written by {} ({} of them, spread across the whole period observed):\n\n{}",
            persona.display_name,
            sample.len(),
            evidence_block
        );

        let req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(1_200);
        let text = self.reasoner.complete(&req).await?;
        Ok(parse_traits(
            &persona.slug,
            facet,
            &text,
            MAX_TRAITS_PER_FACET,
            &index,
        ))
    }
}

/// Parse the model's reply into candidate traits.
///
/// Tolerant on the way in and strict on the way out: an unparseable reply yields no traits
/// rather than an error, because a facet that produced prose is a facet with nothing
/// established — which is a legitimate outcome — and failing the whole profile over it would
/// throw away the nine that parsed.
fn parse_traits(
    persona: &str,
    facet: Facet,
    text: &str,
    max: usize,
    index: &[&Evidence],
) -> Vec<Trait> {
    let Some(json) = crate::reasoner::extract_json(text) else {
        return Vec::new();
    };
    let items = json
        .get("traits")
        .and_then(|t| t.as_array())
        .cloned()
        // A model that returns a bare array instead of the wrapper is answering correctly in
        // the wrong shape, and refusing it would discard a good pass over punctuation.
        .or_else(|| json.as_array().cloned())
        .unwrap_or_default();
    let mut out = Vec::new();
    for item in items.into_iter().take(max) {
        let claim = item
            .get("claim")
            .or_else(|| item.get("trait"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if claim.is_empty() {
            continue;
        }
        out.push(Trait {
            id: trait_id(persona, facet, &claim),
            persona: persona.to_string(),
            facet,
            claim,
            confidence: item
                .get("confidence")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.4) as f32,
            evidence: ids(item.get("evidence"), index),
            counter_evidence: ids(item.get("counter_evidence"), index),
            created_at: Utc::now(),
        });
    }
    out
}

/// Resolve the model's citation tokens back to real evidence ids.
///
/// `e3` → the third excerpt this facet was shown. Tolerant on the way in — `[e3]`, `e3`, `E3`
/// and a bare `3` all resolve — because the shape of the token is our convention, not the
/// model's, and discarding a good citation over a bracket would be self-inflicted.
///
/// A token outside the range is dropped here rather than passed on as a fake id. `verify` would
/// drop it anyway, but dropping it here means the *reason* recorded is "cites no evidence"
/// rather than "cites an excerpt we do not hold", and only one of those is true.
///
/// A full real id is also accepted, for the case where the model echoes one it saw elsewhere.
fn ids(node: Option<&serde_json::Value>, index: &[&Evidence]) -> Vec<String> {
    let resolve = |raw: &str| -> Option<String> {
        let token = raw
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim()
            .trim_start_matches("ev:")
            .trim();
        if token.is_empty() {
            return None;
        }
        // The ordinal form, which is what the prompt asks for.
        let digits = token.trim_start_matches(['e', 'E']);
        if let Ok(n) = digits.parse::<usize>() {
            return index.get(n.checked_sub(1)?).map(|e| e.id.clone());
        }
        // A real id echoed verbatim.
        index.iter().find(|e| e.id == token).map(|e| e.id.clone())
    };
    node.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter_map(resolve)
                .collect()
        })
        .unwrap_or_default()
}

/// A stable id for a claim. See the module note on why this is derived rather than minted.
fn trait_id(persona: &str, facet: Facet, claim: &str) -> String {
    // FNV-1a over the normalized claim. Not cryptographic and does not need to be: it keys
    // one persona's traits, and a collision would merge two claims that are the same
    // sentence in different case.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in claim.to_ascii_lowercase().bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{persona}/{}/{hash:x}", facet.as_str())
}

/// A sample spread evenly across `items`, oldest to newest.
///
/// Every k-th element rather than the last N. See the module note: newest-N describes last
/// week, and a profile is supposed to describe a habit. The first and last are always
/// included, so the range the sample covers is the range the evidence covers.
fn spread(items: &[Evidence], max: usize) -> Vec<Evidence> {
    // A budget of one has no stride, and the arithmetic below would divide by zero. Not
    // reachable from the one caller (`MAX_EXCERPTS_PER_FACET` is 40), guarded anyway because
    // "the constant was lowered for a small model" is a plausible future edit and a panic
    // inside a facet pass would take the whole profile with it.
    if max <= 1 {
        return items
            .iter()
            .max_by_key(|e| e.occurred_at)
            .cloned()
            .into_iter()
            .collect();
    }
    if items.len() <= max {
        let mut out = items.to_vec();
        out.sort_by_key(|e| e.occurred_at);
        return out;
    }
    let mut sorted = items.to_vec();
    sorted.sort_by_key(|e| e.occurred_at);
    let mut out = Vec::with_capacity(max);
    // Fixed-point stride over the index space, so the picks are evenly spaced and the last
    // element is always the last pick.
    for i in 0..max {
        let idx = i * (sorted.len() - 1) / (max - 1);
        out.push(sorted[idx].clone());
    }
    out.dedup_by(|a, b| a.id == b.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persona::{EvidenceKind, Identity, IdentityProvenance};
    use crate::reasoner::MockReasoner;
    use crate::signal::Source;
    use chrono::Duration;

    fn persona() -> Persona {
        Persona {
            slug: "pcholakov".into(),
            display_name: "Pavel".into(),
            role: Some("storage".into()),
            notes: None,
            identities: vec![Identity::new(
                Source::GitHub,
                "pcholakov",
                IdentityProvenance::Operator,
            )],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            harvested_at: None,
            profiled_at: None,
            evidence_watermark: None,
        }
    }

    fn ev(id: &str, days_ago: i64, kind: EvidenceKind) -> Evidence {
        Evidence {
            id: id.into(),
            persona: "pcholakov".into(),
            source: Source::GitHub,
            kind,
            subject_key: None,
            url: None,
            excerpt: format!("comment {id}, long enough to be kept as evidence"),
            context: None,
            state: None,
            occurred_at: Utc::now() - Duration::days(days_ago),
            ingested_at: Utc::now(),
        }
    }

    /// The id has to be stable across passes, or every stored prediction loses its citations
    /// the next time the profile refreshes.
    #[test]
    fn trait_ids_are_stable_for_the_same_claim() {
        let a = trait_id("p", Facet::ReviewsFor, "Asks for a test on storage changes");
        let b = trait_id("p", Facet::ReviewsFor, "asks for a test on STORAGE changes");
        assert_eq!(a, b, "case and nothing else must not change the id");

        let c = trait_id("p", Facet::ReviewsFor, "Asks for docs");
        assert_ne!(a, c);
        // Scoped per persona and per facet, so two people reaching the same conclusion do
        // not share a row.
        assert_ne!(
            a,
            trait_id("q", Facet::ReviewsFor, "Asks for a test on storage changes")
        );
        assert_ne!(
            a,
            trait_id("p", Facet::Style, "Asks for a test on storage changes")
        );
    }

    /// Newest-N would describe last week. The spread covers the whole observed range, and
    /// always includes the oldest and newest excerpt.
    #[test]
    fn the_sample_is_spread_across_the_whole_range() {
        let items: Vec<Evidence> = (0..100)
            .map(|i| ev(&format!("e{i}"), 100 - i, EvidenceKind::Review))
            .collect();
        let sample = spread(&items, 10);
        assert_eq!(sample.len(), 10);
        assert_eq!(
            sample.first().unwrap().id,
            "e0",
            "the oldest is always included"
        );
        assert_eq!(sample.last().unwrap().id, "e99", "and so is the newest");
        // Evenly spaced, so no single week dominates.
        assert!(sample
            .windows(2)
            .all(|w| w[0].occurred_at <= w[1].occurred_at));

        // Fewer items than the budget: everything, in order.
        let few = spread(&items[..3], 10);
        assert_eq!(few.len(), 3);
        assert_eq!(few[0].id, "e0");

        // Degenerate budgets return the newest rather than panicking on the stride.
        assert_eq!(spread(&items, 1).len(), 1);
        assert_eq!(spread(&items, 1)[0].id, "e99");
        assert!(spread(&items, 0).is_empty() || spread(&items, 0).len() == 1);
        assert!(spread(&[], 10).is_empty());
    }

    /// A reply the model got wrong in shape must not cost the facets that parsed.
    #[test]
    fn unparseable_and_oddly_shaped_replies_are_survivable() {
        let held = [
            ev("real-id-one", 10, EvidenceKind::Review),
            ev("real-id-two", 8, EvidenceKind::Review),
        ];
        let index: Vec<&Evidence> = held.iter().collect();

        // Prose: nothing established, no error.
        assert!(
            parse_traits("p", Facet::Style, "Pavel is a careful reviewer.", 5, &index).is_empty()
        );

        // The wrapper shape, with the citation tokens the prompt asks for.
        let wrapped = r#"{"traits":[{"claim":"Writes two-line reviews","confidence":0.6,
                          "evidence":["e1","[e2]"],"counter_evidence":[]}]}"#;
        let got = parse_traits("p", Facet::Style, wrapped, 5, &index);
        assert_eq!(got.len(), 1);
        // Resolved to the *real* ids, so `verify` can check them — and tolerant of brackets,
        // because the token shape is our convention and discarding a good citation over
        // punctuation would be self-inflicted.
        assert_eq!(got[0].evidence, vec!["real-id-one", "real-id-two"]);

        // A bare array is the right answer in the wrong shape.
        let bare = r#"[{"claim":"Answers within the hour","confidence":0.5,"evidence":["e1"]}]"#;
        assert_eq!(
            parse_traits("p", Facet::SlackRegister, bare, 5, &index).len(),
            1
        );

        // The cap holds.
        let many = format!(
            r#"{{"traits":[{}]}}"#,
            (0..20)
                .map(|i| format!(r#"{{"claim":"c{i}","confidence":0.5,"evidence":["e1"]}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(parse_traits("p", Facet::Style, &many, 5, &index).len(), 5);
    }

    /// Citations are ordinals because the model cannot transcribe the real ids.
    ///
    /// The failure this replaces, observed on a live workspace: asked to cite
    /// `pavel/github/issue_comment/https://github.com/…#issuecomment-5168030937`, the model
    /// returned the same string with `issue_comment` changed to `review_comment` — having
    /// "corrected" the one segment that looks guessable. Every citation missed, `verify`
    /// correctly dropped every trait, and a model that had answered *well* produced an empty
    /// profile.
    #[test]
    fn citation_tokens_resolve_and_bad_ones_are_dropped_not_faked() {
        let held = [
            ev(
                "pavel/github/issue_comment/https://github.com/o/r/pull/1#issuecomment-5168030937",
                10,
                EvidenceKind::IssueComment,
            ),
            ev(
                "pavel/github/review/https://github.com/o/r/pull/2",
                8,
                EvidenceKind::Review,
            ),
        ];
        let index: Vec<&Evidence> = held.iter().collect();

        let reply = |cites: &str| {
            format!(
                r#"{{"traits":[{{"claim":"Asks for tests","confidence":0.7,"evidence":{cites}}}]}}"#
            )
        };

        // The ordinal form, in the several spellings a model actually emits.
        for cite in [r#"["e1"]"#, r#"["E1"]"#, r#"["[e1]"]"#, r#"["1"]"#] {
            let got = parse_traits("p", Facet::ReviewsFor, &reply(cite), 5, &index);
            assert_eq!(got[0].evidence, vec![held[0].id.clone()], "{cite}");
        }

        // A real id echoed verbatim still works.
        let echoed = reply(&format!("[{:?}]", held[1].id));
        assert_eq!(
            parse_traits("p", Facet::ReviewsFor, &echoed, 5, &index)[0].evidence,
            vec![held[1].id.clone()]
        );

        // The mangled id — the actual live failure. Dropped, and *not* passed on as a fake id:
        // the reason recorded then reads "cites no evidence", which is true, rather than
        // "cites an excerpt we do not hold", which blames the wrong thing.
        let mangled = reply(
            r#"["pavel/github/review_comment/https://github.com/o/r/pull/1#issuecomment-5168030937"]"#,
        );
        let got = parse_traits("p", Facet::ReviewsFor, &mangled, 5, &index);
        assert!(got[0].evidence.is_empty(), "a mangled id must not resolve");

        // Out of range is dropped rather than indexing past the end.
        let over = reply(r#"["e99"]"#);
        assert!(parse_traits("p", Facet::ReviewsFor, &over, 5, &index)[0]
            .evidence
            .is_empty());
    }

    /// A facet with two excerpts is skipped rather than distilled and then dropped on
    /// confidence: "nothing established" is a true statement, a 0.2 trait is noise.
    #[tokio::test]
    async fn thin_facets_are_skipped_and_the_profile_still_builds() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let p = persona();
        store.put_persona(&p).unwrap();
        // Two review excerpts — below the floor for every facet that admits them.
        store
            .put_persona_evidence(&[
                ev("e1", 10, EvidenceKind::Review),
                ev("e2", 5, EvidenceKind::Review),
            ])
            .unwrap();

        let model = Arc::new(MockReasoner::new(
            r#"{"traits":[{"claim":"Asks for tests","confidence":0.9,"evidence":["e1","e2"]}]}"#,
        ));
        let d = Distiller {
            store: store.clone(),
            reasoner: model,
            tier: "local".into(),
        };
        let profile = d.distil(&p).await.unwrap();
        assert!(
            profile.traits.is_empty(),
            "two excerpts cannot establish a habit"
        );
        assert_eq!(profile.stats.evidence, 2);
        // And the profile says so, rather than presenting an empty block.
        assert!(profile.caveats().iter().any(|c| c.contains("excerpt(s)")));
    }

    /// The end-to-end pass: enough evidence, a well-formed reply, a stored profile whose
    /// traits survived verification.
    #[tokio::test]
    async fn a_real_pass_stores_verified_traits() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let p = persona();
        store.put_persona(&p).unwrap();
        let evidence: Vec<Evidence> = (0..6)
            .map(|i| ev(&format!("e{i}"), 30 - i * 4, EvidenceKind::Review))
            .collect();
        store.put_persona_evidence(&evidence).unwrap();

        // One good claim and one generic-virtue claim, which is what a real model returns.
        let model = Arc::new(MockReasoner::new(
            r#"{"traits":[
                 {"claim":"Asks for a test on anything touching the store","confidence":0.8,
                  "evidence":["e0","e1","e2"],"counter_evidence":["e3"]},
                 {"claim":"A great engineer who cares about quality","confidence":0.9,
                  "evidence":["e0"]}
               ]}"#,
        ));
        let d = Distiller {
            store: store.clone(),
            reasoner: model,
            tier: "local".into(),
        };
        let profile = d.distil(&p).await.unwrap();

        assert!(!profile.traits.is_empty());
        assert!(
            profile
                .traits
                .iter()
                .all(|t| t.claim.starts_with("Asks for a test")),
            "generic virtue must not survive: {:?}",
            profile.traits.iter().map(|t| &t.claim).collect::<Vec<_>>()
        );
        assert!(
            profile
                .removed
                .iter()
                .any(|r| r.why.contains("not falsifiable")),
            "and the removal is reported rather than silent"
        );
        // Counter-evidence is kept, so the claim reads as contested where it is.
        assert!(profile
            .traits
            .iter()
            .any(|t| !t.counter_evidence.is_empty()));

        // Stored, and readable back.
        let stored = store.persona_traits("pcholakov").unwrap();
        assert_eq!(stored.len(), profile.traits.len());
        let refreshed = store.get_persona("pcholakov").unwrap().unwrap();
        assert!(refreshed.profiled_at.is_some());
        // `{count}@{newest ingested_at}`, so it moves when the backward walk adds *older*
        // evidence — which the newest excerpt's id would not.
        assert!(
            refreshed
                .evidence_watermark
                .as_deref()
                .is_some_and(|w| w.starts_with("6@")),
            "watermark was {:?}",
            refreshed.evidence_watermark
        );
    }
}
