//! Text embeddings for semantic recall over memory and the context library.
//!
//! Two implementations behind one trait:
//!   - [`HashEmbedder`] — a fully local, deterministic hashing embedder. No model,
//!     no network; always available (default, and used in tests). It captures
//!     lexical overlap rather than deep semantics, which is enough to rank a
//!     curated store of runbooks and lessons.
//!   - [`OllamaEmbedder`] — real embeddings from a local Ollama model, for when
//!     on-device semantic quality matters.
//!
//! Vectors are stored in SQLite as little-endian `f32` BLOBs and ranked in-process
//! by cosine similarity. At the scale of a curated grounding store (tens to a few
//! hundred entries) a brute-force scan is exact and trivially fast — no ANN index
//! or native vector extension to keep in sync.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;

/// Dimensionality of [`HashEmbedder`] vectors. Ollama vectors keep their model's
/// native dimension; a store must not mix embedders across entries.
pub const HASH_DIM: usize = 256;

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

/// Deterministic, dependency-free embedder: hashed bag-of-tokens with sublinear
/// term weighting, L2-normalized.
pub struct HashEmbedder;

impl HashEmbedder {
    pub fn embed_sync(text: &str) -> Vec<f32> {
        let mut v = vec![0f32; HASH_DIM];
        for tok in tokenize(text) {
            let h = fnv1a(tok.as_bytes());
            let idx = (h as usize) % HASH_DIM;
            // Sign bit spreads tokens across +/- to reduce collisions cancelling
            // meaning; magnitude is a flat term weight.
            let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        l2_normalize(&mut v);
        v
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(Self::embed_sync(text))
    }
}

/// Embeddings from an Ollama model via `/api/embeddings`.
///
/// **Deliberately outside the completion gate** in [`crate::reasoner::ollama`], for two
/// reasons. It would deadlock the obvious caller: anything that generates text and then
/// embeds it would be holding the permit while asking for the embedding. And it would be the
/// wrong trade even if it were safe — an embedding is milliseconds against a small model,
/// while a completion is tens of seconds against a 33B one, so queueing recall behind a
/// generation would make search feel broken to save contention that barely exists.
pub struct OllamaEmbedder {
    client: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    /// Set once the fallback warning has been logged.
    warned: std::sync::atomic::AtomicBool,
}

impl OllamaEmbedder {
    pub fn new(url: impl Into<String>, model: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            // Short by comparison with a generation: an embedding is milliseconds against a
            // small model, so a minute means the server is wedged, not busy.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            url: url.into(),
            model: model.into(),
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            warned: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    /// Embed `text`, degrading to the hashing embedder rather than failing.
    ///
    /// Not every Ollama model has an embedding head — a *coder* or chat model
    /// answers `/api/embeddings` with a 500. Propagating that would break every
    /// caller that stores or recalls an embedding (saving a memory, searching
    /// context) over what is really a configuration mismatch. Recall quality
    /// degrades to lexical; nothing stops working.
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match self.try_embed(text).await {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) => {
                self.warn_once("returned an empty vector");
                Ok(HashEmbedder::embed_sync(text))
            }
            Err(e) => {
                self.warn_once(&format!("{e:#}"));
                Ok(HashEmbedder::embed_sync(text))
            }
        }
    }
}

impl OllamaEmbedder {
    async fn try_embed(&self, text: &str) -> Result<Vec<f32>> {
        #[derive(serde::Deserialize)]
        struct Resp {
            #[serde(default)]
            embedding: Vec<f32>,
        }
        let mut req_b = self
            .client
            .post(format!("{}/api/embeddings", self.url.trim_end_matches('/')))
            .json(&serde_json::json!({ "model": self.model, "prompt": text }));
        if let Some(key) = &self.api_key {
            req_b = req_b.bearer_auth(key);
        }
        let resp: Resp = req_b
            .send()
            .await
            .context("ollama embeddings request")?
            .error_for_status()
            .context("ollama embeddings status")?
            .json()
            .await
            .context("parsing ollama embeddings")?;
        let mut v = resp.embedding;
        l2_normalize(&mut v);
        Ok(v)
    }

    /// Complain once, not once per embedded string. A misconfigured embedding model
    /// fails on *every* call, and the fallback is silent by design — so the log
    /// needs to say it exactly once, with the fix.
    fn warn_once(&self, detail: &str) {
        if self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            "embeddings via '{}' failed ({detail}); falling back to the local hashing embedder. \
             Set [reasoner].embed_model to a real embedding model (e.g. `ollama pull \
             nomic-embed-text`) — chat and coder models have no embedding head.",
            self.model
        );
    }
}

/// Build the configured embedder. Falls back to the local hashing embedder for
/// any provider we don't recognize, so recall always works.
pub fn build(
    provider: &str,
    ollama_url: &str,
    embed_model: &str,
    ollama_key: Option<String>,
) -> Arc<dyn Embedder> {
    match provider {
        "ollama" => Arc::new(OllamaEmbedder::new(ollama_url, embed_model, ollama_key)),
        _ => Arc::new(HashEmbedder),
    }
}

/// Cosine similarity of two equal-length vectors. Returns `0.0` on a length
/// mismatch rather than panicking — a mismatched store entry simply ranks last.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Serialize a vector to a little-endian `f32` BLOB for SQLite storage.
pub fn to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode a BLOB written by [`to_blob`]. A length not divisible by 4 yields an
/// empty vector (treated as "no embedding").
pub fn from_blob(bytes: &[u8]) -> Vec<f32> {
    if !bytes.len().is_multiple_of(4) {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_ascii_lowercase())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_text_ranks_above_unrelated() {
        let q = HashEmbedder::embed_sync("database connection pool exhausted on service foo");
        let near = HashEmbedder::embed_sync("service foo connection pool is exhausted again");
        let far = HashEmbedder::embed_sync("the quarterly marketing report is due friday");
        assert!(
            cosine(&q, &near) > cosine(&q, &far),
            "related text must score higher than unrelated"
        );
    }

    #[test]
    fn blob_roundtrips() {
        let v = HashEmbedder::embed_sync("hello world");
        let back = from_blob(&to_blob(&v));
        assert_eq!(v.len(), back.len());
        for (a, b) in v.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn cosine_length_mismatch_is_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
    }
}
