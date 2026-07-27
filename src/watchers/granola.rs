//! Granola watcher (Phase 1).
//!
//! Polls the Granola API for recent meeting documents and turns each into a
//! `MeetingNote` signal, extracting action items, decisions, and owners from the
//! notes. The Granola API surface varies, so [`poll`] deserializes leniently into
//! JSON and the pure [`normalize_document`] pulls fields defensively — which
//! keeps the watcher working across shape drift and makes the extraction
//! unit-testable.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::Mutex;
use std::time::Duration;
use tracing::warn;

use super::{PollBatch, Watcher};
use crate::config::{self, GranolaSource};
use crate::signal::{ResolutionKey, Severity, Signal, SignalKind, Source};

pub struct GranolaWatcher {
    client: reqwest::Client,
    token: String,
    api_base: String,
    interval: Duration,
    /// Newest document timestamp seen, to skip already-ingested meetings.
    cursor: Mutex<Option<String>>,
}

impl GranolaWatcher {
    pub fn new(cfg: &GranolaSource, token: String) -> Result<Self> {
        let interval =
            config::parse_duration(&cfg.poll_interval).unwrap_or(Duration::from_secs(120));
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent("mugglebot")
                .build()
                .context("building HTTP client")?,
            token,
            api_base: cfg.api_base.trim_end_matches('/').to_string(),
            interval,
            cursor: Mutex::new(None),
        })
    }
}

#[async_trait]
impl Watcher for GranolaWatcher {
    fn name(&self) -> &'static str {
        "granola"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn cursor(&self) -> Option<String> {
        self.cursor.lock().ok()?.clone()
    }

    fn restore_cursor(&self, cursor: &str) {
        if let Ok(mut c) = self.cursor.lock() {
            *c = Some(cursor.to_string());
        }
    }

    async fn poll(&self) -> Result<PollBatch> {
        // Granola's list endpoint returns recent documents; the payload is read
        // leniently so shape changes don't break ingest.
        let resp = self
            .client
            .post(format!("{}/v2/get-documents", self.api_base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "limit": 50 }))
            .send()
            .await
            .context("granola get-documents")?
            .error_for_status()
            .context("granola get-documents status")?;
        let body: serde_json::Value = resp.json().await.context("parsing granola documents")?;

        let docs = body
            .get("docs")
            .or_else(|| body.get("documents"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let last = self.cursor.lock().unwrap().clone();
        let mut newest = last.clone();
        let mut out = Vec::new();
        for doc in &docs {
            let updated = doc_timestamp(doc);
            if let Some(u) = &updated {
                if last.as_deref().map(|l| u.as_str() <= l).unwrap_or(false) {
                    continue;
                }
                if newest.as_deref().map(|n| u.as_str() > n).unwrap_or(true) {
                    newest = Some(u.clone());
                }
            }
            if let Some(sig) = normalize_document(doc) {
                out.push(sig);
            }
        }
        if let Some(n) = newest {
            *self.cursor.lock().unwrap() = Some(n);
        }
        if docs.is_empty() {
            warn!("granola: no documents in response (check API base / token)");
        }
        Ok(PollBatch::incremental(out))
    }
}

/// Turn one Granola document (as JSON) into a `MeetingNote` signal, or `None` if
/// it has no usable content.
pub fn normalize_document(doc: &serde_json::Value) -> Option<Signal> {
    let id = doc
        .get("id")
        .or_else(|| doc.get("document_id"))
        .and_then(|v| v.as_str())?
        .to_string();
    let title = doc
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Untitled meeting")
        .to_string();
    let notes = extract_notes(doc);
    let updated = doc_timestamp(doc);
    let occurred_at = updated
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let actions = extract_actions(&notes);
    let decisions = extract_lines(&notes, &["decision:", "decided:", "agreed:"]);

    // A meeting with neither notes nor action items isn't worth a signal.
    if notes.trim().is_empty() && actions.is_empty() {
        return None;
    }

    let mut body = String::new();
    if !actions.is_empty() {
        body.push_str("Action items:\n");
        for a in &actions {
            body.push_str(&format!("• {a}\n"));
        }
    }
    if !decisions.is_empty() {
        body.push_str("Decisions:\n");
        for d in &decisions {
            body.push_str(&format!("• {d}\n"));
        }
    }
    if body.is_empty() {
        body = notes.chars().take(400).collect();
    }

    let severity = if actions.is_empty() {
        Severity::Info
    } else {
        Severity::Notice
    };

    let mut keys = vec![ResolutionKey::new("meeting", &title)];
    for owner in extract_owners(&actions) {
        keys.push(ResolutionKey::new("person", owner));
    }

    // The updated timestamp is the version, so an edited meeting re-notifies.
    let external_id = id.to_string();
    let version = Some(updated.clone().unwrap_or_else(|| "0".into()));
    Some(Signal {
        id: Signal::make_id(Source::Granola, &external_id, version.as_deref()),
        source: Source::Granola,
        external_id,
        version,
        kind: SignalKind::MeetingNote,
        title,
        body: Some(body.trim_end().to_string()),
        url: doc.get("url").and_then(|v| v.as_str()).map(str::to_string),
        actor: None,
        keys,
        severity,
        upstream_gone: false,
        occurred_at,
        ingested_at: Utc::now(),
        subject: None,
        raw: doc.clone(),
        tags: Vec::new(),
    })
}

fn doc_timestamp(doc: &serde_json::Value) -> Option<String> {
    for key in ["updated_at", "created_at", "date", "timestamp"] {
        if let Some(s) = doc.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn extract_notes(doc: &serde_json::Value) -> String {
    for key in [
        "notes_markdown",
        "notes_plain",
        "notes",
        "content",
        "summary",
        "transcript",
    ] {
        if let Some(s) = doc.get(key).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Action items: markdown checkboxes and `TODO`/`Action:`-prefixed lines.
fn extract_actions(notes: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in notes.lines() {
        let line = raw.trim();
        let lower = line.to_ascii_lowercase();
        let stripped = if let Some(rest) = line
            .strip_prefix("- [ ]")
            .or_else(|| line.strip_prefix("* [ ]"))
        {
            Some(rest.trim())
        } else if lower.starts_with("todo:")
            || lower.starts_with("action:")
            || lower.starts_with("action item:")
        {
            line.split_once(':').map(|(_, r)| r.trim())
        } else {
            None
        };
        if let Some(item) = stripped {
            if !item.is_empty() {
                out.push(item.to_string());
            }
        }
    }
    out
}

fn extract_lines(notes: &str, prefixes: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in notes.lines() {
        let line = raw.trim().trim_start_matches(['-', '*', ' ']);
        let lower = line.to_ascii_lowercase();
        for p in prefixes {
            if lower.starts_with(p) {
                if let Some((_, rest)) = line.split_once(':') {
                    let r = rest.trim();
                    if !r.is_empty() {
                        out.push(r.to_string());
                    }
                }
                break;
            }
        }
    }
    out
}

/// `@Name` owners referenced in action items.
fn extract_owners(actions: &[String]) -> Vec<String> {
    let mut owners = Vec::new();
    for a in actions {
        for tok in a.split_whitespace() {
            if let Some(name) = tok.strip_prefix('@') {
                let clean: String = name
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
                    .collect();
                if !clean.is_empty() && !owners.contains(&clean) {
                    owners.push(clean);
                }
            }
        }
    }
    owners
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_actions_decisions_owners() {
        let doc = serde_json::json!({
            "id": "doc1",
            "title": "Weekly sync",
            "updated_at": "2026-07-20T10:00:00Z",
            "notes_markdown": "Notes\n- [ ] @ben ship the fix\n- [ ] follow up with infra\nDecision: roll back the deploy\nrandom chatter",
        });
        let s = normalize_document(&doc).unwrap();
        assert!(matches!(s.kind, SignalKind::MeetingNote));
        assert_eq!(s.severity, Severity::Notice);
        let body = s.body.unwrap();
        assert!(body.contains("ship the fix"));
        assert!(body.contains("roll back the deploy"));
        assert!(s
            .keys
            .iter()
            .any(|e| e.kind == "person" && e.value == "ben"));
        assert!(s
            .keys
            .iter()
            .any(|e| e.kind == "meeting" && e.value == "Weekly sync"));
    }

    #[test]
    fn empty_meeting_yields_nothing() {
        let doc = serde_json::json!({ "id": "d", "title": "Empty", "notes": "   " });
        assert!(normalize_document(&doc).is_none());
    }

    #[test]
    fn dedup_key_separates_the_id_from_the_version() {
        let doc = serde_json::json!({
            "id": "doc1", "title": "T", "updated_at": "2026-07-20T10:00:00Z",
            "notes": "- [ ] do a thing",
        });
        let s = normalize_document(&doc).unwrap();
        // The id is the meeting; the timestamp is the version. Keeping them in
        // separate fields is what lets the ingress idempotency key and the store's
        // unique index agree without either re-parsing a composite string.
        assert_eq!(s.external_id, "doc1");
        assert_eq!(s.version.as_deref(), Some("2026-07-20T10:00:00Z"));
        assert_eq!(s.dedup_key(), "granola:doc1:2026-07-20T10:00:00Z");
    }
}
