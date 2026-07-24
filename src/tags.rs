//! Tags — the categorical routing layer over the context library.
//!
//! Context-library entries carry **tags** (runbook → `database`, `postgres`).
//! When an issue lands on the board it's **classified** into the tags that apply,
//! and reasoning is grounded with the contexts for those tags first, before the
//! vector-similarity fill (see [`crate::correlation::llm`]). Two producers of
//! tags share this module:
//!
//!   - **context auto-tag** (open vocabulary): a cheap pass proposes initial tags
//!     on ingest, a heavy pass refines — new tags allowed. Human-pinned tags win.
//!   - **thread classify** (closed vocabulary): pick which of the existing tags
//!     apply to a thread; deterministic substring matching is the fallback when
//!     no reasoner is reachable.
//!
//! Tags are normalized to lowercase kebab-case so `Postgres`, `postgres`, and
//! `post gres` collapse to one label.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::reasoner::{self, CompletionRequest, Reasoner};

/// A tag in the vocabulary: a routing label plus a short summary of what it
/// covers, so the classifier has context when deciding which tags apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub summary: String,
    pub created_at: DateTime<Utc>,
}

/// A tag proposed by the auto-tagger: the normalized name and a one-line gloss.
#[derive(Debug, Clone)]
pub struct TagSuggestion {
    pub name: String,
    pub summary: String,
}

/// Cap on how many tags we keep per entry/thread and how long each may be —
/// keeps the vocabulary legible and one document from sprouting a hundred labels.
const MAX_TAGS: usize = 8;
const MAX_TAG_LEN: usize = 40;

/// Normalize one label to lowercase kebab-case, or `None` if nothing usable
/// remains. `Postgres DB!` → `postgres-db`.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        return None;
    }
    out.truncate(MAX_TAG_LEN);
    while out.ends_with('-') {
        out.pop();
    }
    Some(out)
}

/// Normalize a batch, de-duplicating while preserving first-seen order and
/// capping the count.
pub fn normalize_tags<I, S>(raw: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        if let Some(t) = normalize_tag(r.as_ref()) {
            if !out.contains(&t) {
                out.push(t);
                if out.len() >= MAX_TAGS {
                    break;
                }
            }
        }
    }
    out
}

/// Deterministic classification fallback: keep every vocabulary tag whose words
/// all appear in `text`. Explainable and reasoner-free — `kubernetes` matches
/// "pod crashloop on kubernetes", `db-replica` matches "the db replica lag".
pub fn deterministic_match(vocab: &[String], text: &str) -> Vec<String> {
    let hay = text.to_ascii_lowercase();
    vocab
        .iter()
        .filter(|tag| {
            tag.split('-')
                .filter(|w| !w.is_empty())
                .all(|w| hay.contains(w))
        })
        .cloned()
        .collect()
}

/// Parse a model's tag response — a bare JSON array `["a","b"]` or an object
/// `{"tags": [...]}` — into normalized tags. Anything unparseable → empty.
pub fn parse_tags_response(text: &str) -> Vec<String> {
    let Some(v) = reasoner::extract_json(text) else {
        return Vec::new();
    };
    let arr = if v.is_array() {
        v.as_array().cloned()
    } else {
        v.get("tags").and_then(|t| t.as_array()).cloned()
    };
    match arr {
        Some(items) => normalize_tags(items.iter().filter_map(|i| i.as_str())),
        None => Vec::new(),
    }
}

/// Render a vocabulary as `- name — summary` lines for a prompt.
fn vocab_lines(vocab: &[Tag]) -> String {
    vocab
        .iter()
        .map(|t| {
            if t.summary.trim().is_empty() {
                format!("- {}", t.name)
            } else {
                format!("- {} — {}", t.name, t.summary)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ask a reasoner to **classify** `text` against a fixed vocabulary (each tag
/// carrying a summary for context), returning only tags drawn from it. Empty
/// vocabulary or an unreachable reasoner → `None` (the caller falls back to
/// [`deterministic_match`]).
pub async fn classify(reasoner: &dyn Reasoner, vocab: &[Tag], text: &str) -> Option<Vec<String>> {
    if vocab.is_empty() {
        return None;
    }
    let system = "You are a tagging classifier for an ops-awareness assistant. Given an incident \
        thread and a fixed list of allowed tags (each with a short description of what it covers), \
        return ONLY the tags that clearly apply. Choose exclusively from the allowed list — never \
        invent tags. Prefer precision: omit a tag if unsure. Output ONLY a JSON array of tag \
        strings, e.g. [\"database\",\"kubernetes\"].";
    let prompt = format!(
        "Allowed tags:\n{}\n\n---\nThread:\n{}",
        vocab_lines(vocab),
        truncate(text, 4_000)
    );
    let req = CompletionRequest::single(prompt)
        .with_system(system)
        .max_tokens(256);
    match reasoner.complete(&req).await {
        Ok(resp) => {
            let names: Vec<String> = vocab.iter().map(|t| t.name.clone()).collect();
            // Keep only tags that are actually in the vocabulary.
            Some(
                parse_tags_response(&resp)
                    .into_iter()
                    .filter(|t| names.contains(t))
                    .collect(),
            )
        }
        Err(_) => None,
    }
}

/// Ask a reasoner to **suggest** tags for a context document (open vocabulary):
/// it may reuse existing tags or coin new ones, and returns a short summary for
/// each so the vocabulary self-documents. `None` on an unreachable reasoner.
pub async fn suggest(
    reasoner: &dyn Reasoner,
    existing_vocab: &[Tag],
    text: &str,
) -> Option<Vec<TagSuggestion>> {
    let system = "You are tagging reference documents (runbooks, architecture docs, status pages) \
        for an ops-awareness assistant so incidents can be routed to the right background. Propose \
        1-6 short topical tags describing what this document is about. Each tag: a lowercase single \
        word or kebab-case phrase (e.g. \"database\", \"kubernetes\", \"payments\") plus a one-line \
        summary of what the tag covers. Reuse an existing tag when it fits rather than coining a \
        near-duplicate. Output ONLY a JSON array of objects: [{\"tag\":\"database\",\"summary\":\"…\"}].";
    let vocab_hint = if existing_vocab.is_empty() {
        String::new()
    } else {
        format!(
            "Existing tags you should prefer when apt:\n{}\n\n",
            vocab_lines(existing_vocab)
        )
    };
    let prompt = format!("{vocab_hint}---\nDocument:\n{}", truncate(text, 6_000));
    let req = CompletionRequest::single(prompt)
        .with_system(system)
        .max_tokens(400);
    match reasoner.complete(&req).await {
        Ok(resp) => Some(parse_suggestions_response(&resp)),
        Err(_) => None,
    }
}

/// Parse a suggestion response — `[{"tag":"..","summary":".."}]`, or a bare
/// array of strings (summaries default empty) — into normalized suggestions.
pub fn parse_suggestions_response(text: &str) -> Vec<TagSuggestion> {
    let Some(v) = reasoner::extract_json(text) else {
        return Vec::new();
    };
    let Some(items) = v
        .as_array()
        .or_else(|| v.get("tags").and_then(|t| t.as_array()))
    else {
        return Vec::new();
    };
    let mut out: Vec<TagSuggestion> = Vec::new();
    for item in items {
        let (name, summary) = if let Some(s) = item.as_str() {
            (normalize_tag(s), String::new())
        } else {
            let name = item
                .get("tag")
                .or_else(|| item.get("name"))
                .and_then(|n| n.as_str())
                .and_then(normalize_tag);
            let summary = item
                .get("summary")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            (name, summary)
        };
        if let Some(name) = name {
            if !out.iter().any(|s| s.name == name) {
                out.push(TagSuggestion { name, summary });
                if out.len() >= MAX_TAGS {
                    break;
                }
            }
        }
    }
    out
}

/// Generate a one-line description of what a tag covers, from example content
/// tagged with it — the classifier reads this. Used to backfill a summary once
/// for automatically-created tags (folder tags, manual tags); thereafter the
/// summary is edited by hand. `None` on an unreachable reasoner.
pub async fn summarize_tag(
    reasoner: &dyn Reasoner,
    name: &str,
    samples: &[String],
) -> Option<String> {
    let system = "You are documenting a routing tag for an ops-awareness assistant. Given a tag \
        name and examples of content filed under it, write ONE concise sentence describing what \
        the tag covers, so a classifier can decide when it applies to an incident. Output ONLY \
        the sentence.";
    let body = if samples.is_empty() {
        format!("Tag: {name}\n(no example content yet — infer from the name)")
    } else {
        format!(
            "Tag: {name}\n\nExamples of content tagged '{name}':\n- {}",
            samples.join("\n- ")
        )
    };
    let req = CompletionRequest::single(body)
        .with_system(system)
        .max_tokens(120);
    match reasoner.complete(&req).await {
        Ok(s) if !s.trim().is_empty() => Some(truncate(s.trim(), 200)),
        _ => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_kebab() {
        assert_eq!(
            normalize_tag("Postgres DB!").as_deref(),
            Some("postgres-db")
        );
        assert_eq!(
            normalize_tag("  kubernetes  ").as_deref(),
            Some("kubernetes")
        );
        assert_eq!(normalize_tag("--__--").as_deref(), None);
        assert_eq!(normalize_tag("").as_deref(), None);
    }

    #[test]
    fn dedups_and_caps() {
        let tags = normalize_tags(["db", "DB", "db ", "cache"]);
        assert_eq!(tags, vec!["db", "cache"]);
    }

    #[test]
    fn deterministic_matches_word_parts() {
        let vocab = vec![
            "database".to_string(),
            "kubernetes".to_string(),
            "db-replica".to_string(),
        ];
        let m = deterministic_match(&vocab, "Pod crashloop on Kubernetes; the db replica lags");
        assert!(m.contains(&"kubernetes".to_string()));
        assert!(m.contains(&"db-replica".to_string()));
        assert!(!m.contains(&"database".to_string()));
    }

    #[test]
    fn parses_array_and_object() {
        assert_eq!(
            parse_tags_response(r#"["database","Kubernetes"]"#),
            vec!["database", "kubernetes"]
        );
        assert_eq!(
            parse_tags_response(r#"{"tags":["payments"]}"#),
            vec!["payments"]
        );
        assert!(parse_tags_response("no json").is_empty());
    }

    #[test]
    fn parses_suggestions_with_summaries() {
        let s = parse_suggestions_response(
            r#"[{"tag":"Database","summary":"db runbooks"},{"tag":"k8s"}]"#,
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "database");
        assert_eq!(s[0].summary, "db runbooks");
        assert_eq!(s[1].name, "k8s");
        assert_eq!(s[1].summary, "");
        // Bare string array also works, with empty summaries.
        let s = parse_suggestions_response(r#"["payments"]"#);
        assert_eq!(s[0].name, "payments");
    }
}
