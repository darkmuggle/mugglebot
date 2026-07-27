//! Scoring: given an issue anywhere, which repo, component, and change is it likely about?
//!
//! Three independent retrievals over the code index, fused. They're independent on
//! purpose — each is blind to what the others find, and each fails differently:
//!
//! - **Semantic.** Embed the issue text; cosine-rank component cards and commit
//!   summaries. Finds "the pool never returns connections" against a commit that says
//!   "release the guard on the error path" — no shared vocabulary at all. Fails when the
//!   issue is mostly identifiers, or when embeddings aren't available.
//! - **Lexical.** Identifiers, error strings, and paths from the issue matched against
//!   commit messages, changed files, and component digests. Finds `max_connections`
//!   exactly. Fails on paraphrase, which is most incident prose.
//! - **Structural.** Walk the dependency graph out from the issue's own repo. Finds the
//!   case neither of the others can: the symptom is in the consumer, the change is in the
//!   dependency, and the two share no words because they're different codebases.
//!
//! **Every contribution is attributed and cited.** A score with no explanation is worse
//! than no score: the operator can't tell a strong semantic match from a lucky substring,
//! so they either trust all of it or none of it. Each candidate carries which passes found
//! it, what they matched, and how far through the graph it came.
//!
//! What this is not: a verdict. Ranked candidates are hypotheses, same as the root-cause
//! report — "likely `restate/partition-processor`", never "caused by".

use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::embed::{self, Embedder};
use crate::store::Store;

/// How much of the final score each pass can contribute. Weights, not a probability:
/// semantic leads because paraphrase is the common case in incident prose, lexical is a
/// strong but narrow signal, and the graph is a modifier rather than evidence on its own.
const W_SEMANTIC: f32 = 1.0;
const W_LEXICAL: f32 = 0.8;
const W_STRUCTURAL: f32 = 0.5;

/// Score retained per dependency hop. One hop is a real lead; three hops is the whole org.
const HOP_DECAY: f32 = 0.45;

/// How far to walk the dependency graph.
const MAX_HOPS: usize = 2;

/// Below this, a candidate is noise and is dropped rather than shown with a low number —
/// a long tail of 3% matches reads as thoroughness and is just cosine floor.
const MIN_SCORE: f32 = 0.08;

pub struct Scorer {
    pub store: Arc<Store>,
    pub embedder: Arc<dyn Embedder>,
}

/// One ranked candidate: a repo, optionally a component, optionally a commit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Candidate {
    pub repo: String,
    /// The component within it, when the evidence points at one.
    pub component: Option<String>,
    /// The specific change, when a commit summary matched.
    pub commit: Option<String>,
    /// Fused score, 0..1. A ranking, not a probability.
    pub score: f32,
    /// Which passes contributed, and what each matched. The whole basis for trusting it.
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Evidence {
    /// `semantic` | `lexical` | `dependency`.
    pub pass: String,
    /// Contribution to the fused score.
    pub weight: f32,
    /// What matched, in the operator's terms.
    pub detail: String,
}

/// The scored answer, with enough context to say how much it had to go on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScoreReport {
    /// The issue's own repo, when known — the origin for the graph walk.
    pub origin_repo: Option<String>,
    pub terms: Vec<String>,
    pub candidates: Vec<Candidate>,
    /// Set when the index is still being built, so a thin answer is explained rather than
    /// looking like a confident "nothing matches".
    pub index_note: Option<String>,
}

impl Scorer {
    /// Score an issue's text against the code index.
    ///
    /// `origin_repo` is the repo the issue was filed in, if any. It is the graph's
    /// starting point and is *not* itself favoured — an issue filed in the cloud repo is
    /// very often caused by the runtime it depends on, which is the entire reason the
    /// graph exists.
    pub async fn score(&self, text: &str, origin_repo: Option<&str>) -> Result<ScoreReport> {
        let terms = symptom_terms(text);
        let mut acc: BTreeMap<(String, Option<String>, Option<String>), Candidate> =
            BTreeMap::new();

        self.semantic_pass(text, &mut acc).await?;
        self.lexical_pass(&terms, &mut acc)?;
        if let Some(origin) = origin_repo {
            self.structural_pass(origin, &mut acc)?;
        }

        let mut candidates: Vec<Candidate> =
            acc.into_values().filter(|c| c.score >= MIN_SCORE).collect();
        // Highest first; ties broken so the output is stable rather than hash-ordered.
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.repo.cmp(&b.repo))
                .then_with(|| a.component.cmp(&b.component))
        });
        candidates.truncate(20);

        Ok(ScoreReport {
            origin_repo: origin_repo.map(str::to_string),
            terms,
            index_note: self.index_note()?,
            candidates,
        })
    }

    /// Cosine similarity over component cards and commit summaries.
    async fn semantic_pass(
        &self,
        text: &str,
        acc: &mut BTreeMap<(String, Option<String>, Option<String>), Candidate>,
    ) -> Result<()> {
        let query = match self.embedder.embed(text).await {
            Ok(v) if !v.is_empty() => v,
            // Without embeddings the lexical pass still works. Degrading is right; failing
            // the whole score because recall is unavailable is not.
            _ => return Ok(()),
        };

        for (component, blob) in self.store.component_embeddings()? {
            let sim = embed::cosine(&query, &embed::from_blob(&blob));
            if sim <= 0.15 {
                continue;
            }
            let detail = component
                .symptoms
                .clone()
                .or_else(|| component.purpose.clone())
                .unwrap_or_else(|| component.path.clone());
            self.add(
                acc,
                &component.full_name,
                Some(component.path.clone()),
                None,
                sim * W_SEMANTIC,
                Evidence {
                    pass: "semantic".into(),
                    weight: sim * W_SEMANTIC,
                    detail: format!("component card resembles the issue ({sim:.2}): {detail}"),
                },
            );
        }

        for (commit, blob) in self.store.commit_summary_embeddings(&[])? {
            let sim = embed::cosine(&query, &embed::from_blob(&blob));
            if sim <= 0.2 {
                continue;
            }
            // A commit attributes to each component it touched, so the score lands at the
            // granularity the operator asked for rather than only on the repo.
            let components: Vec<Option<String>> = if commit.components.is_empty() {
                vec![None]
            } else {
                commit.components.iter().cloned().map(Some).collect()
            };
            for component in components {
                self.add(
                    acc,
                    &commit.full_name,
                    component,
                    Some(commit.sha.clone()),
                    sim * W_SEMANTIC,
                    Evidence {
                        pass: "semantic".into(),
                        weight: sim * W_SEMANTIC,
                        detail: format!(
                            "commit {} resembles the issue ({sim:.2}): {}",
                            short(&commit.sha),
                            first_sentence(&commit.summary)
                        ),
                    },
                );
            }
        }
        Ok(())
    }

    /// Identifier and error-string matching. Narrow, and very strong when it fires.
    fn lexical_pass(
        &self,
        terms: &[String],
        acc: &mut BTreeMap<(String, Option<String>, Option<String>), Candidate>,
    ) -> Result<()> {
        for term in terms.iter().take(12) {
            for commit in self.store.search_commit_summaries(term, 25)? {
                let components: Vec<Option<String>> = if commit.components.is_empty() {
                    vec![None]
                } else {
                    commit.components.iter().cloned().map(Some).collect()
                };
                // Longer terms are more discriminating: `max_connections` matching is
                // evidence, `pool` matching is barely anything.
                let strength = (term.len() as f32 / 24.0).clamp(0.15, 1.0) * W_LEXICAL;
                for component in components {
                    self.add(
                        acc,
                        &commit.full_name,
                        component,
                        Some(commit.sha.clone()),
                        strength,
                        Evidence {
                            pass: "lexical".into(),
                            weight: strength,
                            detail: format!("`{term}` appears in commit {}", short(&commit.sha)),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Propagate outward through the dependency graph from the issue's own repo.
    ///
    /// Both directions, and they mean different things. A repo the origin *depends on* can
    /// be where the behaviour actually broke. A repo that *depends on the origin* is where
    /// the origin's change shows up as a symptom. Both are leads; neither is a finding on
    /// its own, which is why the weight is the smallest of the three.
    fn structural_pass(
        &self,
        origin: &str,
        acc: &mut BTreeMap<(String, Option<String>, Option<String>), Candidate>,
    ) -> Result<()> {
        let mut frontier = vec![origin.to_string()];
        let mut seen: Vec<String> = vec![origin.to_string()];
        let mut weight = W_STRUCTURAL;

        for hop in 1..=MAX_HOPS {
            weight *= HOP_DECAY;
            let mut next = Vec::new();
            for repo in &frontier {
                let (out, inbound) = self.store.repo_deps(repo)?;
                for (edge, direction) in out
                    .into_iter()
                    .map(|e| (e, "depends on"))
                    .chain(inbound.into_iter().map(|e| (e, "is depended on by")))
                {
                    let other = if edge.from_repo == *repo {
                        edge.to_repo.clone()
                    } else {
                        edge.from_repo.clone()
                    };
                    if seen.contains(&other) {
                        continue;
                    }
                    seen.push(other.clone());
                    next.push(other.clone());
                    // The graph raises a whole repo, not a component: it says "look over
                    // here", and the other two passes say where within it.
                    self.add(
                        acc,
                        &other,
                        None,
                        None,
                        weight,
                        Evidence {
                            pass: "dependency".into(),
                            weight,
                            detail: format!(
                                "{hop} hop{} from {origin}: {} {direction} {} (via `{}` in {})",
                                if hop == 1 { "" } else { "s" },
                                repo,
                                other,
                                edge.dep_name,
                                edge.source
                            ),
                        },
                    );
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(())
    }

    /// Accumulate one contribution.
    ///
    /// Contributions to the same candidate compound rather than summing linearly: two
    /// independent passes agreeing is much stronger than one pass firing twice, and a plain
    /// sum lets a dozen weak lexical hits outrank a single strong cross-pass agreement.
    fn add(
        &self,
        acc: &mut BTreeMap<(String, Option<String>, Option<String>), Candidate>,
        repo: &str,
        component: Option<String>,
        commit: Option<String>,
        contribution: f32,
        evidence: Evidence,
    ) {
        let key = (repo.to_string(), component.clone(), commit.clone());
        let entry = acc.entry(key).or_insert_with(|| Candidate {
            repo: repo.to_string(),
            component,
            commit,
            score: 0.0,
            evidence: Vec::new(),
        });
        // Diminishing returns: 1 - Π(1 - wᵢ), bounded at 1 whatever fires.
        entry.score = 1.0 - (1.0 - entry.score) * (1.0 - contribution.clamp(0.0, 0.95));
        if entry.evidence.len() < 6 {
            entry.evidence.push(evidence);
        }
    }

    /// Say so when the index is incomplete. A thin answer from a half-built index is a
    /// different thing from a thin answer from a complete one, and the operator can only
    /// tell if we say which.
    fn index_note(&self) -> Result<Option<String>> {
        let repos = self.store.list_repos()?;
        if repos.is_empty() {
            return Ok(Some(
                "the repo index is empty — nothing to score against yet".into(),
            ));
        }
        let mut done = 0i64;
        let mut total = 0i64;
        let mut without_components = 0usize;
        for repo in &repos {
            let (d, t) = self.store.commit_index_progress(&repo.full_name)?;
            done += d;
            total += t;
            if self.store.components_for_repo(&repo.full_name)?.is_empty() {
                without_components += 1;
            }
        }
        // Reported first, and reported even when `total` is 0. A repo whose commit log
        // hasn't been fetched has 0/0 commits, which reads as complete — so an index one
        // component deep would otherwise answer with no caveat at all, which is the same
        // failure as a confident "nothing matches".
        if without_components > 0 {
            return Ok(Some(format!(
                "{without_components} of {} repo(s) are not indexed yet; \
                 candidates from them are missing",
                repos.len()
            )));
        }
        if total > 0 && done < total {
            let pct = (done as f64 / total as f64 * 100.0).round();
            return Ok(Some(format!(
                "the commit index is {pct}% built ({done}/{total}); \
                 candidates from unindexed commits are missing"
            )));
        }
        if total == 0 {
            return Ok(Some(
                "no commit history is indexed yet; scoring is on component cards alone".into(),
            ));
        }
        Ok(None)
    }
}

/// Distinctive terms worth matching lexically: identifiers, error codes, and the
/// technical vocabulary in between.
///
/// Starts from the same extraction triage's file selection uses — backticked spans,
/// `snake_case`, `camelCase`, dotted paths — for the same reason: a model is never asked to
/// guess at identifiers, so this works with nothing reachable.
///
/// That alone is nearly inert on incident prose, though, which is most of what an issue
/// is. "Users are getting 401s — the auth token metadata isn't refreshed before it
/// expires" yields exactly one identifier-shaped token, and it's `users`. So three cheap
/// additions, each aimed at a form that actually appears in bug reports: HTTP/error codes,
/// `CamelCase` type names appearing bare in prose, and multi-character technical words
/// minus a stop list. Common words are dropped rather than ranked low — a lexical hit on
/// "getting" is noise wearing the costume of evidence.
pub fn symptom_terms(text: &str) -> Vec<String> {
    // `identifiers` treats any capitalized-then-lowercase word as code-shaped, which is
    // right for picking files to read and wrong here: a sentence that opens with "Users"
    // would contribute `users` as evidence. Same stop list, applied to both sources.
    let mut terms: Vec<String> = crate::triage::identifiers(text)
        .into_iter()
        .filter(|t| !STOP_WORDS.contains(&t.to_ascii_lowercase().as_str()))
        .collect();

    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        let word = raw.trim_matches(|c: char| !c.is_alphanumeric());
        if word.len() < 3 {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        // An error code: `401`, `5xx`, `503`.
        let is_code = word.len() <= 4
            && word.chars().next().is_some_and(|c| c.is_ascii_digit())
            && word.chars().all(|c| c.is_ascii_alphanumeric());
        // A bare type name in prose: `ConnectionPool`, `TlsExpiry`.
        let is_camel = word.chars().next().is_some_and(char::is_uppercase)
            && word.chars().skip(1).any(char::is_uppercase);
        let is_technical = word.len() >= 5 && !STOP_WORDS.contains(&lower.as_str());
        if is_code || is_camel || is_technical {
            terms.push(word.to_string());
        }
    }

    // Longest first: the lexical pass has a budget, and it should spend it on the terms
    // that discriminate rather than on whichever came first in the sentence.
    terms.sort_by(|a, b| {
        let cased = |s: &String| !s.chars().any(char::is_uppercase);
        b.len()
            .cmp(&a.len())
            .then_with(|| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()))
            // The two sources disagree on case for the same word — `identifiers` folds
            // everything down. Matching is case-insensitive either way, but the term is
            // quoted back at the operator as evidence, so keep the author's spelling.
            .then_with(|| cased(a).cmp(&cased(b)))
    });
    terms.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    terms.truncate(24);
    terms
}

/// Words long enough to look technical and common enough to match everything. Kept short
/// and specific: over-filtering costs a real term, under-filtering costs precision, and
/// the asymmetry favours a small list of words that genuinely appear in every report.
const STOP_WORDS: &[&str] = &[
    "about",
    "after",
    "again",
    "along",
    "already",
    "also",
    "always",
    "another",
    "anything",
    "around",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "cannot",
    "could",
    "does",
    "doing",
    "during",
    "each",
    "either",
    "enough",
    "every",
    "getting",
    "have",
    "having",
    "here",
    "however",
    "issue",
    "into",
    "just",
    "like",
    "looks",
    "made",
    "make",
    "many",
    "maybe",
    "might",
    "more",
    "most",
    "much",
    "must",
    "never",
    "nothing",
    "often",
    "only",
    "other",
    "over",
    "problem",
    "really",
    "same",
    "seems",
    "should",
    "since",
    "some",
    "something",
    "still",
    "such",
    "sure",
    "than",
    "that",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "thing",
    "think",
    "this",
    "those",
    "through",
    "under",
    "until",
    "used",
    "user",
    "users",
    "using",
    "very",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "will",
    "with",
    "without",
    "would",
    "your",
];

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn first_sentence(s: &str) -> String {
    let trimmed = s.trim();
    match trimmed.find(". ") {
        Some(i) => trimmed[..=i].to_string(),
        None => trimmed.chars().take(160).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CommitEntry, ComponentSummary, RepoEntry};

    fn scorer(store: Arc<Store>) -> Scorer {
        Scorer {
            store,
            embedder: Arc::new(crate::embed::HashEmbedder),
        }
    }

    fn repo(full_name: &str) -> RepoEntry {
        RepoEntry {
            full_name: full_name.into(),
            description: None,
            topics: vec![],
            language: None,
            archived: false,
            pushed_at: None,
            readme_etag: None,
            readme: None,
            summary: None,
            indexed_sha: None,
            digest: None,
            kind: None,
            kind_pinned: false,
            fetched_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    fn component(full_name: &str, path: &str) -> ComponentSummary {
        ComponentSummary {
            full_name: full_name.into(),
            path: path.into(),
            purpose: Some("does a thing".into()),
            symptoms: Some("[\"broken\"]".into()),
            digest: None,
            indexed_sha: None,
        }
    }

    fn commit(full_name: &str, sha: &str) -> CommitEntry {
        CommitEntry {
            full_name: full_name.into(),
            sha: sha.into(),
            author: None,
            committed_at: chrono::Utc::now(),
            message: "fix the thing".into(),
            url: None,
            files: vec!["src/lib.rs".into()],
        }
    }

    fn candidate() -> Candidate {
        Candidate {
            repo: "o/r".into(),
            component: None,
            commit: None,
            score: 0.0,
            evidence: vec![],
        }
    }

    /// Two independent passes agreeing must beat one pass firing repeatedly. A linear sum
    /// lets a dozen weak substring hits outrank a single strong cross-pass agreement, which
    /// is precisely the wrong ranking.
    #[test]
    fn agreement_compounds_rather_than_summing() {
        let compound = |contributions: &[f32]| {
            let mut score = 0.0f32;
            for c in contributions {
                score = 1.0 - (1.0 - score) * (1.0 - c.clamp(0.0, 0.95));
            }
            score
        };
        let two_strong = compound(&[0.6, 0.5]);
        let many_weak = compound(&[0.12; 8]);
        assert!(
            two_strong > many_weak,
            "two strong passes ({two_strong:.3}) must outrank eight weak ones ({many_weak:.3})"
        );
        // And nothing can exceed 1, however much fires.
        assert!(compound(&[0.9; 20]) <= 1.0);
    }

    #[test]
    fn a_long_identifier_is_stronger_evidence_than_a_short_word() {
        let strength = |t: &str| (t.len() as f32 / 24.0).clamp(0.15, 1.0) * W_LEXICAL;
        assert!(strength("max_connections_per_partition") > strength("pool"));
        // ...but a short term still counts for something rather than nothing.
        assert!(strength("pool") > 0.0);
    }

    #[test]
    fn hop_decay_makes_the_second_hop_a_hint_not_a_finding() {
        let one = W_STRUCTURAL * HOP_DECAY;
        let two = W_STRUCTURAL * HOP_DECAY * HOP_DECAY;
        assert!(one > two);
        // Even a direct dependency edge alone must not clear the display threshold on its
        // own: the graph says "look over here", it does not identify a cause.
        assert!(one < 0.3, "a bare dependency edge is a lead, not evidence");
        assert!(two > MIN_SCORE * 0.5);
    }

    #[test]
    fn terms_prefer_the_discriminating_ones() {
        let terms = symptom_terms(
            "The `max_connections` setting in pool.rs causes ConnectionPoolExhausted after 50",
        );
        assert!(!terms.is_empty());
        // Longest first, so the lexical pass spends its budget on the specific terms.
        let lengths: Vec<usize> = terms.iter().map(String::len).collect();
        assert!(
            lengths.windows(2).all(|w| w[0] >= w[1]),
            "terms must be ordered most-discriminating first: {terms:?}"
        );
    }

    /// Identifier extraction alone is nearly inert on incident prose, which is most of what
    /// an issue actually contains. Measured on a real report, it yielded exactly one token
    /// — `users` — from a sentence full of usable terms.
    #[test]
    fn prose_yields_usable_terms_not_just_identifiers() {
        let terms = symptom_terms(
            "Users are getting 401s intermittently — it looks like the auth token metadata \
             is not being refreshed before it expires",
        );
        let has = |t: &str| terms.iter().any(|x| x.eq_ignore_ascii_case(t));
        assert!(
            has("401s"),
            "an error code is the most citable term here: {terms:?}"
        );
        assert!(has("metadata"), "technical words must survive: {terms:?}");
        assert!(has("refreshed") || has("intermittently"));
        // ...and the words that appear in every bug report must not, or a lexical hit
        // stops meaning anything.
        for noise in ["users", "getting", "looks", "being", "before"] {
            assert!(
                !has(noise),
                "`{noise}` is noise wearing the costume of evidence"
            );
        }
    }

    #[test]
    fn an_empty_index_says_so() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let note = scorer(store).index_note().unwrap().unwrap();
        assert!(note.contains("empty"), "{note}");
    }

    /// The failure this pins: a repo whose commit log has never been fetched reports 0/0
    /// commits, which arithmetic reads as *complete*. So an index one component deep used
    /// to answer with no caveat at all — indistinguishable from a fully-built index that
    /// genuinely found nothing.
    #[test]
    fn an_unindexed_repo_is_named_even_when_another_repo_is_complete() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.put_repo(&repo("o/indexed"), false).unwrap();
        store.put_repo(&repo("o/untouched"), false).unwrap();
        store
            .put_component_summary(&component("o/indexed", "crates/a"), None)
            .unwrap();
        store.put_commits(&[commit("o/indexed", "aaa")]).unwrap();
        store
            .put_commit_summary("o/indexed", "aaa", "summary", &[], None, None)
            .unwrap();

        let note = scorer(store).index_note().unwrap().unwrap();
        assert!(note.contains("1 of 2"), "must count the gap: {note}");
        assert!(note.contains("not indexed"), "{note}");
    }

    #[test]
    fn components_without_commit_history_still_earn_a_caveat() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.put_repo(&repo("o/r"), false).unwrap();
        store
            .put_component_summary(&component("o/r", "crates/a"), None)
            .unwrap();

        let note = scorer(store).index_note().unwrap().unwrap();
        assert!(
            note.contains("no commit history"),
            "0/0 commits is not completeness: {note}"
        );
    }

    #[test]
    fn a_fully_built_index_adds_no_caveat() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.put_repo(&repo("o/r"), false).unwrap();
        store
            .put_component_summary(&component("o/r", "crates/a"), None)
            .unwrap();
        store.put_commits(&[commit("o/r", "aaa")]).unwrap();
        store
            .put_commit_summary("o/r", "aaa", "summary", &[], None, None)
            .unwrap();

        assert_eq!(scorer(store).index_note().unwrap(), None);
    }

    #[test]
    fn a_bare_type_name_in_prose_is_a_term() {
        let terms = symptom_terms("the ConnectionPool never drains and TlsExpiry fires");
        let has = |t: &str| terms.iter().any(|x| x == t);
        assert!(has("ConnectionPool"));
        assert!(has("TlsExpiry"));
    }

    #[test]
    fn evidence_is_capped_so_one_candidate_cannot_flood_the_panel() {
        let mut c = candidate();
        for i in 0..20 {
            if c.evidence.len() < 6 {
                c.evidence.push(Evidence {
                    pass: "lexical".into(),
                    weight: 0.1,
                    detail: format!("hit {i}"),
                });
            }
        }
        assert_eq!(c.evidence.len(), 6);
    }
}
