//! Memory (Phase 3) — editable institutional memory.
//!
//! Lessons, corrections, and confirmed approaches, written by MuggleBot
//! (postmortem-assist and live-assist false-positive feedback) and by the user.
//! One entry = one fact with a one-line summary; entries link back to the signals
//! or threads they came from. Stored in SQLite and embedded (see [`crate::embed`])
//! for semantic recall. Full CRUD via the WebUI editor and the MCP memory tools.
//!
//! The store owns persistence; [`MemoryManager`] adds the embed-on-write and
//! semantic-recall behavior on top.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::embed::{self, Embedder};
use crate::reasoner::Reasoner;
use crate::store::Store;
use crate::tags::{self, TagSuggestion};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    /// The fact, free text.
    pub text: String,
    /// One-line summary used in listings and recall ranking.
    pub summary: String,
    /// Signal / thread ids this fact came from.
    pub links: Vec<String>,
    /// Topical tags for categorical routing (auto-suggested on write, editable).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tags were set by a human and must not be overwritten by auto-tagging.
    #[serde(default)]
    pub tags_pinned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryHit {
    #[serde(flatten)]
    pub memory: Memory,
    pub score: f32,
}

/// Wraps the store with embedding-on-write and vector recall.
pub struct MemoryManager {
    store: Arc<Store>,
    embedder: Arc<dyn Embedder>,
    /// Cheap/ambient reasoner: the fast initial tag pass.
    reasoner: Arc<dyn Reasoner>,
    /// Heavy reasoner: the refining tag pass that runs after the cheap one.
    heavy: Arc<dyn Reasoner>,
}

impl MemoryManager {
    pub fn new(
        store: Arc<Store>,
        embedder: Arc<dyn Embedder>,
        reasoner: Arc<dyn Reasoner>,
        heavy: Arc<dyn Reasoner>,
    ) -> Self {
        Self {
            store,
            embedder,
            reasoner,
            heavy,
        }
    }

    /// Create a memory. A blank one-line `summary` is derived from `text`.
    /// Human-supplied `tags` are pinned; otherwise the two-tier auto-tagger fills
    /// them from the fact's content (same mechanics as the context library).
    pub async fn put(
        &self,
        text: &str,
        summary: Option<String>,
        links: Vec<String>,
        tags: Option<Vec<String>>,
    ) -> Result<Memory> {
        let now = Utc::now();
        let summary = summary
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| first_line(text));
        let pinned_tags = tags.map(tags::normalize_tags).filter(|t| !t.is_empty());
        let mut mem = Memory {
            id: format!("mem/{}", crate::store::new_id()),
            text: text.to_string(),
            summary,
            links,
            tags: pinned_tags.clone().unwrap_or_default(),
            tags_pinned: pinned_tags.is_some(),
            created_at: now,
            updated_at: now,
        };
        let vec = self.embedder.embed(&mem.embed_text()).await?;
        self.store.put_memory(&mem, &embed::to_blob(&vec))?;
        if let Some(tags) = &pinned_tags {
            for t in tags {
                self.store.ensure_tag(t, "", now)?;
            }
        } else {
            self.autotag(&mem).await;
            mem = self.store.get_memory(&mem.id)?.unwrap_or(mem);
        }
        Ok(mem)
    }

    pub async fn edit(
        &self,
        id: &str,
        text: &str,
        summary: Option<String>,
    ) -> Result<Option<Memory>> {
        let Some(mut mem) = self.store.get_memory(id)? else {
            return Ok(None);
        };
        mem.text = text.to_string();
        if let Some(s) = summary.filter(|s| !s.trim().is_empty()) {
            mem.summary = s;
        }
        mem.updated_at = Utc::now();
        let vec = self.embedder.embed(&mem.embed_text()).await?;
        self.store.put_memory(&mem, &embed::to_blob(&vec))?;
        // Re-tag on edit unless a human pinned the tags.
        if !mem.tags_pinned {
            self.autotag(&mem).await;
            mem = self.store.get_memory(&mem.id)?.unwrap_or(mem);
        }
        Ok(Some(mem))
    }

    /// Set (pin) a memory's tags from a human edit, registering any new tags.
    pub fn set_tags(&self, id: &str, tags: Vec<String>) -> Result<Option<Memory>> {
        let names = tags::normalize_tags(tags);
        let now = Utc::now();
        for n in &names {
            self.store.ensure_tag(n, "", now)?;
        }
        self.store.set_memory_tags(id, &names, true)?;
        self.store.get_memory(id)
    }

    /// Two-tier auto-tagging, mirroring the context library: cheap pass then heavy
    /// refine, each persisting and registering new tags. Best-effort.
    async fn autotag(&self, mem: &Memory) {
        let body = mem.embed_text();
        let vocab = self.store.list_tags().unwrap_or_default();
        if let Some(sugg) = tags::suggest(self.reasoner.as_ref(), &vocab, &body).await {
            if let Err(e) = self.apply_suggestions(&mem.id, &sugg) {
                tracing::warn!("memory {}: initial autotag store failed: {e:#}", mem.id);
            }
        }
        let vocab = self.store.list_tags().unwrap_or_default();
        if let Some(sugg) = tags::suggest(self.heavy.as_ref(), &vocab, &body).await {
            if let Err(e) = self.apply_suggestions(&mem.id, &sugg) {
                tracing::warn!("memory {}: refine autotag store failed: {e:#}", mem.id);
            }
        }
    }

    fn apply_suggestions(&self, id: &str, sugg: &[TagSuggestion]) -> Result<()> {
        if sugg.is_empty() {
            return Ok(());
        }
        let now = Utc::now();
        let names: Vec<String> = sugg.iter().map(|s| s.name.clone()).collect();
        for s in sugg {
            self.store.ensure_tag(&s.name, &s.summary, now)?;
        }
        self.store.set_memory_tags(id, &names, false)?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.store.delete_memory(id)
    }

    pub fn list(&self) -> Result<Vec<Memory>> {
        self.store.list_memories()
    }

    pub fn get(&self, id: &str) -> Result<Option<Memory>> {
        self.store.get_memory(id)
    }

    /// Semantic recall — the top-`k` memories by cosine similarity to `query`.
    pub async fn search(&self, query: &str, k: usize) -> Result<Vec<MemoryHit>> {
        let q = self.embedder.embed(query).await?;
        let rows = self.store.all_memory_embeddings()?;
        let mut scored: Vec<MemoryHit> = rows
            .into_iter()
            .map(|(mem, blob)| MemoryHit {
                score: embed::cosine(&q, &embed::from_blob(&blob)),
                memory: mem,
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }
}

impl Memory {
    /// Text fed to the embedder — summary weighted by leading it, plus the body.
    fn embed_text(&self) -> String {
        format!("{}\n{}", self.summary, self.text)
    }
}

fn first_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        let truncated: String = line.chars().take(117).collect();
        format!("{truncated}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::reasoner::MockReasoner;

    fn manager(resp: &str) -> (Arc<Store>, MemoryManager) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new(resp));
        let mgr = MemoryManager::new(store.clone(), embedder, reasoner.clone(), reasoner);
        (store, mgr)
    }

    #[tokio::test]
    async fn autotags_on_put_and_registers_vocab() {
        let (store, mgr) = manager(r#"[{"tag":"Database","summary":"db lessons"}]"#);
        let mem = mgr
            .put(
                "a spike in pool waits usually means a slow query",
                None,
                vec![],
                None,
            )
            .await
            .unwrap();
        assert_eq!(mem.tags, vec!["database".to_string()], "auto-tagged");
        assert!(!mem.tags_pinned);
        assert_eq!(
            store.get_tag("database").unwrap().unwrap().summary,
            "db lessons"
        );
    }

    #[tokio::test]
    async fn pinned_tags_skip_autotag() {
        let (_store, mgr) = manager(r#"[{"tag":"autotag","summary":"x"}]"#);
        let mem = mgr
            .put("some fact", None, vec![], Some(vec!["Payments".into()]))
            .await
            .unwrap();
        assert_eq!(mem.tags, vec!["payments".to_string()]);
        assert!(mem.tags_pinned);
    }
}
