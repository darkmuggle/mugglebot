//! A persistent completion cache — don't buy the same answer twice.
//!
//! MuggleBot re-reasons constantly. A thread is re-analyzed on every new signal;
//! the same candidate pairs get re-judged; the router grades the same call sites;
//! a restart replays work the daemon already did. Most of those requests are
//! *byte-identical* to one already answered, and each costs either a metered cloud
//! call or half a minute of local inference.
//!
//! So this decorator sits in front of any [`Reasoner`] and keys completions on the
//! full request — tier label, system prompt, every message, and the sampling
//! limits. Identical request in, stored answer out, no model involved.
//!
//! It's stored in SQLite rather than a process map on purpose: restarts are
//! exactly when you most want the answers back, and an in-memory cache throws away
//! its value precisely then.
//!
//! # What is deliberately *not* cached
//!
//! - **Requests marked [`CompletionRequest::no_cache`]** — "reconsider this on
//!   model X" and "re-triage this issue" are the user asking for the work to be
//!   *redone*. Serving those from cache would make the button look broken.
//! - **Empty responses.** A model returning nothing is a transient failure, not an
//!   answer; caching it would make one bad minute stick for the whole TTL.
//! - **Errors**, for the same reason.
//!
//! The cache key includes the tier label, so switching models doesn't serve
//! answers from the model you switched away from.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

use super::{CompletionRequest, Reasoner};
use crate::store::Store;

pub struct CachingReasoner {
    inner: Arc<dyn Reasoner>,
    store: Arc<Store>,
    /// Identifies the tier + model, so a model swap can't serve stale answers.
    label: String,
    ttl: Duration,
}

impl CachingReasoner {
    pub fn new(inner: Arc<dyn Reasoner>, store: Arc<Store>, label: String, ttl: Duration) -> Self {
        Self {
            inner,
            store,
            label,
            ttl,
        }
    }
}

#[async_trait]
impl Reasoner for CachingReasoner {
    async fn complete(&self, req: &CompletionRequest) -> Result<String> {
        if req.no_cache {
            debug!("cache: bypassed for a deliberate redo ({})", self.label);
            return self.inner.complete(req).await;
        }
        let key = cache_key(&self.label, req);
        match self.store.get_completion(&key, self.ttl) {
            Ok(Some(hit)) => {
                debug!("cache: hit ({})", self.label);
                return Ok(hit);
            }
            Ok(None) => {}
            // A cache that can't be read must never break reasoning.
            Err(e) => debug!("cache: read failed ({e:#})"),
        }
        let answer = self.inner.complete(req).await?;
        if !answer.trim().is_empty() {
            if let Err(e) = self.store.put_completion(&key, &self.label, &answer) {
                debug!("cache: write failed ({e:#})");
            }
        }
        Ok(answer)
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }
}

/// Hash the whole request into a cache key.
///
/// Everything that could change the answer goes in: the tier, the system prompt,
/// every message's role and content, any attached images, and the sampling limits.
/// The `session` key is deliberately excluded — it's conversation bookkeeping for
/// the CLI bridge, not part of the question being asked, and including it would
/// mint a fresh key for every session and make the cache useless.
pub fn cache_key(label: &str, req: &CompletionRequest) -> String {
    let mut h = Hasher::new();
    h.write(label.as_bytes());
    h.write(req.system.as_deref().unwrap_or("").as_bytes());
    for m in &req.messages {
        h.write(m.role.as_str().as_bytes());
        h.write(m.content.as_bytes());
        for img in &m.images {
            h.write(img.media_type.as_bytes());
            h.write(img.base64.as_bytes());
        }
    }
    h.write(&req.max_tokens.to_le_bytes());
    h.write(&req.temperature.to_le_bytes());
    format!("{:016x}{:016x}", h.a, h.b)
}

/// Two independent FNV-1a lanes, giving a 128-bit key. Collisions here would serve
/// the wrong answer, so one 64-bit lane is not enough margin for a cache that
/// accumulates for days.
struct Hasher {
    a: u64,
    b: u64,
}

impl Hasher {
    fn new() -> Self {
        Self {
            a: 0xcbf29ce484222325,
            b: 0x9e3779b97f4a7c15,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.a ^= byte as u64;
            self.a = self.a.wrapping_mul(0x100000001b3);
            self.b = self.b.rotate_left(7) ^ (byte as u64).wrapping_mul(0xff51afd7ed558ccd);
        }
        // Length-mix, so appending to one field can't collide with prefixing the
        // next one.
        self.a ^= bytes.len() as u64;
        self.b = self.b.wrapping_add(bytes.len() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reasoner::{Image, Message, Role};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counter {
        calls: AtomicUsize,
        reply: String,
    }

    impl Counter {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                reply: reply.into(),
            })
        }
    }

    #[async_trait]
    impl Reasoner for Counter {
        async fn complete(&self, _req: &CompletionRequest) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.reply.clone())
        }
    }

    fn cached(inner: Arc<dyn Reasoner>) -> (CachingReasoner, Arc<Store>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        (
            CachingReasoner::new(
                inner,
                store.clone(),
                "test/model".into(),
                Duration::from_secs(3600),
            ),
            store,
        )
    }

    fn task(prompt: &str) -> CompletionRequest {
        CompletionRequest::single(prompt).with_system("do the thing")
    }

    #[tokio::test]
    async fn identical_requests_hit_the_cache() {
        let inner = Counter::new("the answer");
        let (r, store) = cached(inner.clone());
        for _ in 0..5 {
            assert_eq!(r.complete(&task("same input")).await.unwrap(), "the answer");
        }
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            1,
            "only the first call"
        );
        let (entries, hits) = store.completion_cache_stats().unwrap();
        assert_eq!(entries, 1);
        assert_eq!(hits, 4);
    }

    #[tokio::test]
    async fn a_different_request_is_a_miss() {
        let inner = Counter::new("answer");
        let (r, _) = cached(inner.clone());
        r.complete(&task("input one")).await.unwrap();
        r.complete(&task("input two")).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::Relaxed), 2);
    }

    /// "Reconsider on model X" and "re-triage" are the user asking for the work to
    /// be redone; a cache hit would make the button look broken.
    #[tokio::test]
    async fn no_cache_forces_a_fresh_call_and_refreshes_the_entry() {
        let inner = Counter::new("answer");
        let (r, _) = cached(inner.clone());
        r.complete(&task("input")).await.unwrap();
        r.complete(&task("input")).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::Relaxed), 1);

        let mut redo = task("input");
        redo.no_cache = true;
        r.complete(&redo).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::Relaxed), 2, "bypassed the cache");
    }

    /// An empty response is a transient failure. Caching it would make one bad
    /// minute stick for the whole TTL.
    #[tokio::test]
    async fn empty_responses_are_not_cached() {
        let inner = Counter::new("   ");
        let (r, store) = cached(inner.clone());
        r.complete(&task("input")).await.unwrap();
        r.complete(&task("input")).await.unwrap();
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "retried, not cached"
        );
        assert_eq!(store.completion_cache_stats().unwrap().0, 0);
    }

    #[tokio::test]
    async fn an_expired_entry_is_a_miss() {
        let inner = Counter::new("answer");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let r = CachingReasoner::new(
            inner.clone(),
            store.clone(),
            "test/model".into(),
            Duration::ZERO,
        );
        r.complete(&task("input")).await.unwrap();
        r.complete(&task("input")).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::Relaxed), 2);
    }

    /// Switching models must not serve answers from the model you switched away
    /// from.
    #[test]
    fn the_tier_label_is_part_of_the_key() {
        let req = task("input");
        assert_ne!(
            cache_key("ollama_local/deepseek-coder:33b", &req),
            cache_key("claude/claude-opus-4-8", &req)
        );
    }

    #[test]
    fn every_field_that_changes_the_answer_changes_the_key() {
        let base = task("input");
        let baseline = cache_key("m", &base);

        let mut other_system = base.clone();
        other_system.system = Some("a different job".into());
        assert_ne!(cache_key("m", &other_system), baseline);

        let mut other_limit = base.clone();
        other_limit.max_tokens = base.max_tokens + 1;
        assert_ne!(cache_key("m", &other_limit), baseline);

        let mut other_temp = base.clone();
        other_temp.temperature = 0.9;
        assert_ne!(cache_key("m", &other_temp), baseline);

        let mut with_image = base.clone();
        with_image.messages[0].images.push(Image {
            media_type: "image/png".into(),
            base64: "AAAA".into(),
        });
        assert_ne!(cache_key("m", &with_image), baseline);

        let mut other_role = base.clone();
        other_role
            .messages
            .push(Message::text(Role::Assistant, "x"));
        assert_ne!(cache_key("m", &other_role), baseline);
    }

    /// The session key is bookkeeping, not part of the question — including it
    /// would mint a fresh key per session and make the cache useless.
    #[test]
    fn the_session_key_does_not_affect_the_key() {
        let plain = task("input");
        let sessioned = task("input").session("thread:abc");
        assert_eq!(cache_key("m", &plain), cache_key("m", &sessioned));
    }

    /// Field boundaries must be real: "ab"+"c" must not key the same as "a"+"bc",
    /// or two different conversations could collide onto one answer.
    #[test]
    fn concatenation_across_fields_does_not_collide() {
        let mut a = CompletionRequest::single("bc");
        a.system = Some("a".into());
        let mut b = CompletionRequest::single("c");
        b.system = Some("ab".into());
        assert_ne!(cache_key("m", &a), cache_key("m", &b));
    }

    #[test]
    fn pruning_expires_then_evicts_least_recently_used() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..10 {
            store
                .put_completion(&format!("k{i}"), "test/model", "answer")
                .unwrap();
        }
        assert_eq!(store.completion_cache_stats().unwrap().0, 10);

        // Keep the newest 4 by last use.
        let removed = store
            .prune_completions(Duration::from_secs(3600), 4)
            .unwrap();
        assert_eq!(removed, 6);
        assert_eq!(store.completion_cache_stats().unwrap().0, 4);

        // A zero TTL expires everything.
        store.prune_completions(Duration::ZERO, 100).unwrap();
        assert_eq!(store.completion_cache_stats().unwrap().0, 0);
    }
}
