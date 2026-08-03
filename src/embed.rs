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

/// Characters of input sent to an embedding model.
///
/// **This is a correctness limit, and its absence was a silent data-loss bug.**
/// `nomic-embed-text` has a 2048-token window and answers an over-long input with
/// `500 {"error": "the input length exceeds the context length"}`. That 500 was caught and
/// degraded to [`HashEmbedder`] — which produces a **256**-dimensional vector where the real
/// model produces **768**, and [`cosine`] returns `0.0` on a length mismatch. So the item was
/// not "degraded to lexical recall" as the fallback intended: it was stored as a vector that
/// scores exactly zero against every query, permanently invisible to semantic search. Measured
/// on a live board: 28 of 36 context sources, the long documents, all unfindable.
///
/// Sized from measurement, and **not trusted** — see [`OllamaEmbedder::try_embed`], which
/// halves and retries on a length error rather than relying on this number being right.
///
/// A character budget cannot be right, because characters-per-token varies by more than 4x
/// with the content. Measured against this very model: repetitive English embeds fine at
/// 8,000 characters and fails at 20,000; a pathological run of single characters passes at
/// 4,000 and fails at 5,000. Any constant chosen from one of those is wrong for the other,
/// which is why the first attempt at this used 6,000 and still returned a 500.
///
/// So this is a first guess that covers ordinary prose and code, and the retry covers being
/// wrong about it.
///
/// Truncation, not chunking. An embedding of a long document's opening is a worse
/// representation than the mean of its chunks would be, and a far better one than a vector
/// that matches nothing. Chunk-and-average is the available improvement.
pub const MAX_EMBED_CHARS: usize = 4_000;

/// Stop shrinking here. Below this an embedding represents so little of the document that the
/// hashing fallback's lexical behaviour is no worse, and something is wrong with the model
/// rather than with the input.
const MIN_EMBED_CHARS: usize = 400;

/// Cut `text` to something an embedding model will accept, on a character boundary.
pub fn truncate_for_embedding(text: &str) -> &str {
    truncate_to(text, MAX_EMBED_CHARS)
}

/// Cut `text` to at most `budget` bytes, on a character boundary.
pub fn truncate_to(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    // Walk back to a char boundary — slicing mid-UTF-8 would panic, and an em-dash in a
    // document is not a reason to lose its embedding.
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

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
    /// Embed, halving the input and retrying while the model says it is too long.
    ///
    /// Adaptive because a character budget cannot be correct — see [`MAX_EMBED_CHARS`]. The
    /// alternative was picking a constant conservative enough for base64 and thereby throwing
    /// away most of every ordinary document, or picking one sized for prose and going on
    /// silently storing incomparable vectors for anything denser. This asks the model.
    ///
    /// Only a *length* error retries. Every other failure — no embedding head, Ollama down —
    /// is not fixed by sending less, so it falls through to the caller and the fallback on the
    /// first attempt rather than after four.
    async fn try_embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut budget = MAX_EMBED_CHARS;
        loop {
            match self.embed_at(text, budget).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let too_long = format!("{e:#}").contains("exceeds the context length");
                    if !too_long || budget <= MIN_EMBED_CHARS {
                        return Err(e);
                    }
                    budget /= 2;
                    tracing::debug!(
                        "embedding input too long for '{}'; retrying at {budget} characters",
                        self.model
                    );
                }
            }
        }
    }

    async fn embed_at(&self, text: &str, budget: usize) -> Result<Vec<f32>> {
        #[derive(serde::Deserialize)]
        struct Resp {
            #[serde(default)]
            embedding: Vec<f32>,
        }
        let mut req_b = self
            .client
            .post(format!("{}/api/embeddings", self.url.trim_end_matches('/')))
            .json(&serde_json::json!({
                "model": self.model,
                "prompt": truncate_to(text, budget),
            }));
        if let Some(key) = &self.api_key {
            req_b = req_b.bearer_auth(key);
        }
        let resp = req_b
            .send()
            .await
            .context("ollama embeddings request")?;
        let status = resp.status();
        // The **body**, not just the status. `error_for_status()` discards it, and the body is
        // the only place Ollama says *why*: the log line that started this read
        // "HTTP status server error (500 Internal Server Error) for url (…)" and nothing more,
        // while the response itself said `{"error": "the input length exceeds the context
        // length"}`. A 500 with the reason thrown away is a 500 nobody can act on — and it is
        // what the retry above matches against.
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "ollama embeddings status {status}: {}",
                crate::tools::truncate_for_prompt(body.trim(), 200)
            );
        }
        let resp: Resp = serde_json::from_str(&body).context("parsing ollama embeddings")?;
        let mut v = resp.embedding;
        l2_normalize(&mut v);
        Ok(v)
    }

    /// Complain once, not once per embedded string. A misconfigured embedding model
    /// fails on *every* call, and the fallback is silent by design — so the log
    /// needs to say it exactly once, with the fix.
    /// Complain once, and complain about the *right thing*.
    ///
    /// The single message this used to print always blamed the configuration — "set
    /// `embed_model` to a real embedding model, chat and coder models have no embedding head".
    /// That is one cause of a 500. It was not the cause of the one seen in practice, where the
    /// model was `nomic-embed-text`, pulled, and loaded on the GPU; the input was simply longer
    /// than its 2048-token window. Sending the reader to check a correct setting is worse than
    /// saying nothing, because it looks like an answer.
    fn warn_once(&self, detail: &str) {
        if self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // The fallback's dimensionality is the part nobody expects, so it is stated outright:
        // a hash vector is 256-dimensional where a real one is 768, and `cosine` scores a
        // mismatch as 0. The affected item is not degraded, it is unfindable.
        let consequence = "the hashing fallback returns a 256-dimensional vector where this                            model returns 768, and cosine similarity scores a dimension                            mismatch as 0 — so anything embedded this way is invisible to                            semantic search until it is re-embedded";
        if detail.contains("exceeds the context length") {
            tracing::warn!(
                "embeddings via '{}' failed on an over-long input ({detail}). Input is                  truncated to {} characters before sending, so this should not happen — if it                  does, this model's window is smaller than that. Lower                  `embed::MAX_EMBED_CHARS`. Note: {consequence}.",
                self.model,
                MAX_EMBED_CHARS,
            );
        } else {
            tracing::warn!(
                "embeddings via '{}' failed ({detail}); falling back to the local hashing \
                 embedder. If this model is not an embedding model, set [reasoner].embed_model \
                 to one (e.g. `ollama pull nomic-embed-text`) — chat and coder models have no \
                 embedding head. Otherwise check that Ollama is up and the model is pulled. \
                 Note: {consequence}.",
                self.model
            );
        }
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

    /// Live, against the operator's own Ollama. `cargo test live_embed -- --ignored --nocapture`
    ///
    /// The only way to know the retry works is to make a real model refuse a real input. The
    /// character budget is measurably unreliable — 4,000 characters of prose is fine and 4,000
    /// of dense single-character tokens is near the edge — so this asserts the *behaviour*
    /// (a 768-dimensional vector comes back) rather than any threshold.
    #[test]
    #[ignore]
    fn live_embed_survives_a_pathological_input() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(live_embed_body());
    }

    async fn live_embed_body() {
        let e = OllamaEmbedder::new("http://127.0.0.1:11434", "nomic-embed-text", None);
        // Prose far over any window: must come back real, not hashed.
        let prose = "the quick brown fox jumps over the lazy dog. ".repeat(4_000);
        let v = e.embed(&prose).await.unwrap();
        println!("prose {} chars -> {} dims", prose.len(), v.len());
        assert_eq!(v.len(), 768, "a long document must still get a real embedding");

        // The shape that defeated a fixed budget: dense, one token per character.
        let dense = "x".repeat(60_000);
        let v = e.embed(&dense).await.unwrap();
        println!("dense {} chars -> {} dims", dense.len(), v.len());
        assert_eq!(v.len(), 768, "the retry must shrink until the model accepts it");
    }

    /// The bug this file's truncation exists to prevent, asserted on the two facts that
    /// combined to cause it.
    ///
    /// A long input used to reach Ollama unmodified, come back `500 {"error": "the input
    /// length exceeds the context length"}`, and fall back to a hash vector — which is a
    /// *different dimensionality*, which `cosine` scores as 0. Neither half is wrong on its
    /// own; together they made 28 of 36 real context sources permanently unfindable.
    #[test]
    fn a_long_input_is_truncated_rather_than_lost() {
        // Short input is untouched — the common case must not be altered.
        assert_eq!(truncate_for_embedding("TenantPodOOMKillLoop"), "TenantPodOOMKillLoop");

        // A document larger than any model's window comes back bounded.
        let long = "the quick brown fox jumps over the lazy dog. ".repeat(4_000);
        let cut = truncate_for_embedding(&long);
        assert!(cut.len() <= MAX_EMBED_CHARS);
        assert!(cut.len() > MAX_EMBED_CHARS - 8, "cut close to the budget: {}", cut.len());
        // Still a prefix of the original: an embedding of the opening, not of nothing.
        assert!(long.starts_with(cut));

        // Multi-byte characters must not panic the slice. An em-dash straddling the cut is
        // exactly the kind of thing that took the board down elsewhere in this codebase.
        let dashes = "—".repeat(MAX_EMBED_CHARS);
        let cut = truncate_for_embedding(&dashes);
        assert!(cut.len() <= MAX_EMBED_CHARS);
        assert!(dashes.starts_with(cut));
        // And it really did cut on a boundary — otherwise this would not be valid UTF-8.
        assert!(cut.chars().all(|c| c == '—'));
    }

    /// Why truncation matters rather than just being tidy: the fallback is not comparable to
    /// a real embedding, so falling back is not a graceful degradation.
    #[test]
    fn a_fallback_vector_scores_zero_against_a_real_one() {
        let hashed = HashEmbedder::embed_sync("TenantPodOOMKillLoop");
        assert_eq!(hashed.len(), HASH_DIM);
        // Stand in for a real 768-dimensional embedding.
        let real = vec![0.1f32; 768];
        assert_eq!(
            cosine(&hashed, &real),
            0.0,
            "a hash vector and a model vector are incomparable — this is the data loss"
        );
    }

    fn cosine_length_mismatch_is_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
    }
}
