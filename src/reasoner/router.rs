//! The difficulty router — the front door for every task-shaped reasoning call.
//!
//! MuggleBot's default model is the **local** one (`deepseek-coder:33b` on Ollama).
//! Most of
//! what an ops agent asks a model to do is not hard: label a thread, extract search
//! terms, decide whether two alerts are the same thing, shortlist commits. Sending
//! all of that to a metered frontier model is how an always-on daemon gets
//! expensive without getting better.
//!
//! But some of it *is* hard, and a small local model quietly producing a worse
//! answer is the failure mode you never notice. So before running a task, the local
//! model **grades its own difficulty**, and the grade decides who answers:
//!
//! | Grade | Who answers |
//! |---|---|
//! | `easy`, `medium` | local (`deepseek-coder:33b`) alone |
//! | `hard` | local drafts, then **Sonnet cleans it up** — or Sonnet does it outright if local fails |
//! | `extra_hard` | **Opus**, directly; local doesn't attempt it |
//!
//! Grading is itself a local call, and a deliberately tiny one (~10 output tokens).
//! It's also cached per call-site shape, so a task type is graded roughly once per
//! process rather than once per invocation — see [`grade_key`].
//!
//! # Why "cleanup" rather than "escalate"
//!
//! For `hard`, the local model's draft is passed *to* Sonnet as material rather
//! than thrown away. Two reasons: a local draft that's 80% right makes the cloud
//! call shorter and better-anchored than a cold start, and the cleanup prompt can
//! insist on preserving the output contract. That last part matters — most callers
//! here parse strict JSON, so a "cleanup" that reformatted the answer would break
//! them. The cleanup instruction is explicit that the shape is not up for revision.
//!
//! # Failure and degradation
//!
//! Grading failure is treated as *local is unavailable* rather than *this is easy*:
//! if Ollama can't grade, it can't answer either, so the task goes to Sonnet
//! (subject to `cloud_fallback`). With `cloud_fallback = false` the router stays
//! on-device and a local outage surfaces as an error, which the callers' own
//! deterministic fallbacks already handle. Requests carrying images bypass grading
//! entirely and go to a vision-capable tier, since a text-only local model would
//! silently ignore the attachment.

use anyhow::{bail, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use super::{CompletionRequest, Message, Reasoner, Role};
use crate::config::Routing as RoutingCfg;

/// How hard a task is, as the local model judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
    ExtraHard,
}

impl Difficulty {
    pub fn as_str(self) -> &'static str {
        match self {
            Difficulty::Easy => "easy",
            Difficulty::Medium => "medium",
            Difficulty::Hard => "hard",
            Difficulty::ExtraHard => "extra_hard",
        }
    }

    /// Parse a grader verdict. Tolerant of case, spacing, and the hyphenated
    /// spelling, because that's what models actually emit.
    pub fn parse(s: &str) -> Option<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str()
        {
            "easy" | "trivial" => Some(Difficulty::Easy),
            "medium" | "moderate" => Some(Difficulty::Medium),
            "hard" | "difficult" => Some(Difficulty::Hard),
            "extra_hard" | "extrahard" | "very_hard" | "expert" => Some(Difficulty::ExtraHard),
            _ => None,
        }
    }
}

/// The grading rubric. Kept concrete and example-driven: an abstract "how hard is
/// this?" makes a small model grade almost everything `hard`, which would defeat
/// the point by escalating constantly.
const GRADER_SYSTEM: &str = "You grade how much reasoning capability a task needs. You do NOT do the \
     task. Reply with ONLY a JSON object: {\"difficulty\":\"easy|medium|hard|extra_hard\"}.\n\n\
     easy — mechanical: classify into given categories, extract named terms, pick items from a \
     list, reformat.\n\
     medium — summarize or compare concrete given material; make a judgment where the evidence \
     plainly supports one answer.\n\
     hard — weigh conflicting or incomplete evidence; infer cause from indirect signals; produce a \
     careful multi-part analysis where being wrong matters.\n\
     extra_hard — reason about an unfamiliar system from sparse evidence, hold many interacting \
     factors at once, or make a high-stakes judgment where a plausible-but-wrong answer would \
     mislead an engineer during an incident.\n\n\
     Grade the REASONING DIFFICULTY, not the length of the input. A long list of items to filter is \
     still easy. Default to the lower grade when uncertain.";

/// The cleanup contract for `hard`. The format clause is load-bearing: most callers
/// parse strict JSON out of this, so a cleanup that "improved" the shape would break
/// them.
const CLEANUP_SYSTEM: &str = "A smaller model drafted an answer to the task below. Produce the \
     corrected final answer.\n\
     - Fix anything wrong, unsupported, or missed. Cut anything the evidence doesn't support.\n\
     - If the draft is already right, return it essentially unchanged.\n\
     - PRESERVE THE OUTPUT FORMAT the task asks for exactly. If the task wants JSON, return only \
     that JSON — no commentary, no markdown fence, no explanation of your changes.\n\
     Output only the final answer.";

pub struct RoutingReasoner {
    /// The default: local, on-device.
    local: Arc<dyn Reasoner>,
    /// Mid tier — cleanup for `hard`, and the fallback when local can't answer.
    mid: Arc<dyn Reasoner>,
    /// Top tier — `extra_hard` only.
    heavy: Arc<dyn Reasoner>,
    cfg: RoutingCfg,
    /// Difficulty by call-site shape. A task type grades once, not once per call.
    grades: Mutex<HashMap<u64, Difficulty>>,
}

impl RoutingReasoner {
    pub fn new(
        local: Arc<dyn Reasoner>,
        mid: Arc<dyn Reasoner>,
        heavy: Arc<dyn Reasoner>,
        cfg: RoutingCfg,
    ) -> Self {
        Self {
            local,
            mid,
            heavy,
            cfg,
            grades: Mutex::new(HashMap::new()),
        }
    }

    /// Grade a task, consulting the cache first.
    ///
    /// `None` means grading itself failed — the caller treats that as "local is
    /// unavailable", not as a difficulty.
    async fn grade(&self, req: &CompletionRequest) -> Option<Difficulty> {
        let key = grade_key(req);
        if let Some(cached) = self.grades.lock().ok().and_then(|g| g.get(&key).copied()) {
            return Some(cached);
        }
        // The grader sees the task's shape, not its full payload: the system prompt
        // (which is the job description) plus a slice of the input. A whole 8k-token
        // incident doesn't grade differently from its first paragraph, and sending
        // it would make grading as expensive as the task.
        let brief = format!(
            "Task instructions:\n{}\n\nInput begins:\n{}",
            truncate(req.system.as_deref().unwrap_or("(none)"), 1_200),
            truncate(&joined_content(&req.messages), 800),
        );
        let probe = CompletionRequest::single(brief)
            .with_system(GRADER_SYSTEM)
            .max_tokens(64);
        let raw = match self.local.complete(&probe).await {
            Ok(raw) => raw,
            Err(e) => {
                debug!("router: grading failed: {e:#}");
                return None;
            }
        };
        let graded = super::extract_json(&raw)
            .and_then(|v| {
                v.get("difficulty")
                    .and_then(|d| d.as_str())
                    .and_then(Difficulty::parse)
            })
            // A grader that answered but not in the expected shape still told us
            // something; look for a bare grade word before giving up.
            .or_else(|| Difficulty::parse(&raw))
            .unwrap_or(Difficulty::Medium);
        if let Ok(mut cache) = self.grades.lock() {
            cache.insert(key, graded);
        }
        Some(graded)
    }

    /// Run the task on `mid`, if cloud fallback is permitted.
    async fn fall_back(&self, req: &CompletionRequest, why: &str) -> Result<String> {
        if !self.cfg.cloud_fallback {
            bail!("{why}, and cloud_fallback is disabled");
        }
        warn!("router: {why}; using the mid tier");
        self.mid.complete(req).await
    }

    /// `hard`: local drafts, the mid tier finalizes. A local failure hands the whole
    /// task to the mid tier instead.
    async fn draft_then_cleanup(&self, req: &CompletionRequest) -> Result<String> {
        let draft = match self.local.complete(req).await {
            Ok(draft) if !draft.trim().is_empty() => draft,
            Ok(_) => {
                return self
                    .fall_back(req, "local returned nothing on a hard task")
                    .await
            }
            Err(e) => {
                return self
                    .fall_back(req, &format!("local failed on a hard task ({})", brief(&e)))
                    .await
            }
        };
        if !self.cfg.cleanup {
            return Ok(draft);
        }
        // Carry the original task verbatim so the cleanup model is bound by the same
        // instructions (and the same output contract) the draft was.
        let prompt = format!(
            "=== TASK ===\n{}\n\n{}\n\n=== DRAFT TO CORRECT ===\n{draft}",
            req.system.as_deref().unwrap_or("(no instructions)"),
            joined_content(&req.messages),
        );
        let cleanup = CompletionRequest::single(prompt)
            .with_system(CLEANUP_SYSTEM)
            .max_tokens(req.max_tokens);
        match self.mid.complete(&cleanup).await {
            Ok(final_answer) if !final_answer.trim().is_empty() => Ok(final_answer),
            // Cleanup is an improvement pass, not a gate: if it's unavailable the
            // local draft is still a usable answer.
            Ok(_) => Ok(draft),
            Err(e) => {
                debug!(
                    "router: cleanup unavailable ({}); keeping local draft",
                    brief(&e)
                );
                Ok(draft)
            }
        }
    }

    /// The first tier that can actually see an image.
    fn vision_tier(&self) -> Option<&Arc<dyn Reasoner>> {
        [&self.heavy, &self.mid, &self.local]
            .into_iter()
            .find(|r| r.supports_vision())
    }
}

#[async_trait]
impl Reasoner for RoutingReasoner {
    async fn complete(&self, req: &CompletionRequest) -> Result<String> {
        if !self.cfg.enabled {
            return self.local.complete(req).await;
        }
        // Images can't be graded or downgraded — a text-only model would drop the
        // attachment and answer confidently about nothing.
        if req.messages.iter().any(|m| !m.images.is_empty()) {
            let Some(tier) = self.vision_tier() else {
                bail!("this request carries images but no configured model supports vision");
            };
            debug!("router: image request → vision tier");
            return tier.complete(req).await;
        }

        let Some(difficulty) = self.grade(req).await else {
            return self.fall_back(req, "the local grader is unreachable").await;
        };
        debug!("router: graded {}", difficulty.as_str());
        match difficulty {
            Difficulty::Easy | Difficulty::Medium => match self.local.complete(req).await {
                Ok(answer) if !answer.trim().is_empty() => Ok(answer),
                Ok(_) => self.fall_back(req, "local returned nothing").await,
                Err(e) => {
                    self.fall_back(req, &format!("local failed ({})", brief(&e)))
                        .await
                }
            },
            Difficulty::Hard => self.draft_then_cleanup(req).await,
            // Extra-hard skips the local attempt entirely: the whole point of the
            // grade is that a small model's answer here would be confidently wrong.
            Difficulty::ExtraHard => match self.heavy.complete(req).await {
                Ok(answer) if !answer.trim().is_empty() => Ok(answer),
                Ok(_) => self.mid.complete(req).await,
                Err(e) => {
                    warn!(
                        "router: top tier failed on an extra-hard task ({}); trying mid",
                        brief(&e)
                    );
                    self.mid.complete(req).await
                }
            },
        }
    }

    fn supports_vision(&self) -> bool {
        self.vision_tier().is_some()
    }
}

/// Cache key for a grade: the task's *shape*, not its content.
///
/// The system prompt identifies the call site ("summarize a correlated thread",
/// "judge whether these two threads are the same"), which is what actually
/// determines difficulty. A coarse size bucket rides along so that a two-signal
/// thread and a forty-signal incident can still grade differently, without every
/// distinct payload minting a new cache entry.
fn grade_key(req: &CompletionRequest) -> u64 {
    let mut h = fnv(req.system.as_deref().unwrap_or("").as_bytes());
    let size = joined_content(&req.messages).len();
    h ^= fnv(&size_bucket(size).to_le_bytes()).rotate_left(9);
    h
}

/// Order-of-magnitude bucket, so difficulty can scale with input size without the
/// cache key changing on every byte.
fn size_bucket(len: usize) -> u32 {
    match len {
        0..=500 => 0,
        501..=2_000 => 1,
        2_001..=8_000 => 2,
        8_001..=32_000 => 3,
        _ => 4,
    }
}

fn joined_content(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role != Role::Assistant)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// One short line of an error, for a log message.
fn brief(e: &anyhow::Error) -> String {
    truncate(format!("{e:#}").lines().next().unwrap_or("").trim(), 120)
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records what it was asked, so a test can assert which tier answered.
    struct Spy {
        name: &'static str,
        reply: String,
        calls: AtomicUsize,
        fail: bool,
        vision: bool,
    }

    impl Spy {
        fn new(name: &'static str, reply: &str) -> Arc<Self> {
            Arc::new(Self {
                name,
                reply: reply.into(),
                calls: AtomicUsize::new(0),
                fail: false,
                vision: false,
            })
        }
        fn failing(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                reply: String::new(),
                calls: AtomicUsize::new(0),
                fail: true,
                vision: false,
            })
        }
        fn seeing(name: &'static str, reply: &str) -> Arc<Self> {
            Arc::new(Self {
                name,
                reply: reply.into(),
                calls: AtomicUsize::new(0),
                fail: false,
                vision: true,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl Reasoner for Spy {
        async fn complete(&self, _req: &CompletionRequest) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                bail!("{} is down", self.name);
            }
            Ok(self.reply.clone())
        }
        fn supports_vision(&self) -> bool {
            self.vision
        }
    }

    /// A local model that grades, then answers — the two calls return different
    /// things, which is how a test distinguishes grading from answering.
    struct Local {
        grade: &'static str,
        answer: String,
        grades: AtomicUsize,
        answers: AtomicUsize,
    }

    impl Local {
        fn new(grade: &'static str, answer: &str) -> Arc<Self> {
            Arc::new(Self {
                grade,
                answer: answer.into(),
                grades: AtomicUsize::new(0),
                answers: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Reasoner for Local {
        async fn complete(&self, req: &CompletionRequest) -> Result<String> {
            if req.system.as_deref() == Some(GRADER_SYSTEM) {
                self.grades.fetch_add(1, Ordering::Relaxed);
                return Ok(format!(r#"{{"difficulty":"{}"}}"#, self.grade));
            }
            self.answers.fetch_add(1, Ordering::Relaxed);
            Ok(self.answer.clone())
        }
    }

    fn task(system: &str, prompt: &str) -> CompletionRequest {
        CompletionRequest::single(prompt).with_system(system)
    }

    fn router(
        local: Arc<dyn Reasoner>,
        mid: Arc<dyn Reasoner>,
        heavy: Arc<dyn Reasoner>,
    ) -> RoutingReasoner {
        RoutingReasoner::new(local, mid, heavy, RoutingCfg::default())
    }

    #[tokio::test]
    async fn easy_and_medium_stay_local() {
        for grade in ["easy", "medium"] {
            let local = Local::new(grade, "local answer");
            let mid = Spy::new("mid", "mid answer");
            let heavy = Spy::new("heavy", "heavy answer");
            let r = router(local.clone(), mid.clone(), heavy.clone());
            let out = r.complete(&task("classify this", "input")).await.unwrap();
            assert_eq!(out, "local answer", "{grade} must not leave the machine");
            assert_eq!(mid.calls(), 0);
            assert_eq!(heavy.calls(), 0);
        }
    }

    #[tokio::test]
    async fn hard_drafts_locally_then_cleans_up_on_mid() {
        let local = Local::new("hard", "rough draft");
        let mid = Spy::new("mid", "cleaned up");
        let heavy = Spy::new("heavy", "heavy answer");
        let r = router(local.clone(), mid.clone(), heavy.clone());
        let out = r.complete(&task("analyze this", "input")).await.unwrap();
        assert_eq!(out, "cleaned up");
        assert_eq!(local.answers.load(Ordering::Relaxed), 1, "local drafted");
        assert_eq!(mid.calls(), 1, "mid cleaned up");
        assert_eq!(heavy.calls(), 0, "hard never reaches the top tier");
    }

    /// "or if it fails, use sonnet" — a local failure on a hard task hands the whole
    /// task over rather than returning nothing.
    #[tokio::test]
    async fn hard_falls_back_to_mid_when_local_fails() {
        // Grading succeeds (own model), answering fails.
        struct GradeThenDie;
        #[async_trait]
        impl Reasoner for GradeThenDie {
            async fn complete(&self, req: &CompletionRequest) -> Result<String> {
                if req.system.as_deref() == Some(GRADER_SYSTEM) {
                    return Ok(r#"{"difficulty":"hard"}"#.into());
                }
                bail!("ollama died mid-task")
            }
        }
        let mid = Spy::new("mid", "mid did it all");
        let heavy = Spy::new("heavy", "heavy answer");
        let r = router(Arc::new(GradeThenDie), mid.clone(), heavy.clone());
        let out = r.complete(&task("analyze this", "input")).await.unwrap();
        assert_eq!(out, "mid did it all");
        assert_eq!(mid.calls(), 1);
        assert_eq!(heavy.calls(), 0);
    }

    #[tokio::test]
    async fn extra_hard_goes_straight_to_the_top_tier() {
        let local = Local::new("extra_hard", "local answer");
        let mid = Spy::new("mid", "mid answer");
        let heavy = Spy::new("heavy", "heavy answer");
        let r = router(local.clone(), mid.clone(), heavy.clone());
        let out = r
            .complete(&task("reason about this", "input"))
            .await
            .unwrap();
        assert_eq!(out, "heavy answer");
        assert_eq!(
            local.answers.load(Ordering::Relaxed),
            0,
            "the local model must not attempt an extra-hard task"
        );
        assert_eq!(mid.calls(), 0);
    }

    /// If Ollama can't grade, it can't answer either — so this is an outage, not a
    /// difficulty of "easy".
    #[tokio::test]
    async fn ungradable_task_goes_to_mid_not_local() {
        let mid = Spy::new("mid", "mid answer");
        let heavy = Spy::new("heavy", "heavy answer");
        let r = router(Spy::failing("local"), mid.clone(), heavy.clone());
        let out = r.complete(&task("do this", "input")).await.unwrap();
        assert_eq!(out, "mid answer");
    }

    #[tokio::test]
    async fn cloud_fallback_off_keeps_everything_on_device() {
        let mid = Spy::new("mid", "mid answer");
        let r = RoutingReasoner::new(
            Spy::failing("local"),
            mid.clone(),
            Spy::new("heavy", "heavy"),
            RoutingCfg {
                cloud_fallback: false,
                ..Default::default()
            },
        );
        let err = r
            .complete(&task("do this", "input"))
            .await
            .expect_err("no cloud fallback means the failure surfaces");
        assert!(format!("{err:#}").contains("cloud_fallback is disabled"));
        assert_eq!(mid.calls(), 0, "nothing may leave the machine");
    }

    #[tokio::test]
    async fn cleanup_can_be_disabled_to_keep_hard_tasks_local() {
        let local = Local::new("hard", "rough draft");
        let mid = Spy::new("mid", "cleaned up");
        let r = RoutingReasoner::new(
            local,
            mid.clone(),
            Spy::new("heavy", "heavy"),
            RoutingCfg {
                cleanup: false,
                ..Default::default()
            },
        );
        let out = r.complete(&task("analyze", "input")).await.unwrap();
        assert_eq!(out, "rough draft");
        assert_eq!(mid.calls(), 0);
    }

    /// Cleanup improves an answer; it must not be able to destroy one.
    #[tokio::test]
    async fn failed_cleanup_keeps_the_local_draft() {
        let local = Local::new("hard", "rough draft");
        let r = router(
            local,
            Spy::failing("mid"),
            Spy::new("heavy", "heavy answer"),
        );
        let out = r.complete(&task("analyze", "input")).await.unwrap();
        assert_eq!(out, "rough draft");
    }

    #[tokio::test]
    async fn images_skip_grading_and_reach_a_vision_model() {
        let local = Local::new("easy", "local answer");
        let heavy = Spy::seeing("heavy", "i can see it");
        let r = router(local.clone(), Spy::new("mid", "mid"), heavy.clone());
        let mut req = task("describe the screenshot", "what is this?");
        req.messages[0].images.push(super::super::Image {
            media_type: "image/png".into(),
            base64: "iVBORw0KGgo=".into(),
        });
        let out = r.complete(&req).await.unwrap();
        assert_eq!(out, "i can see it");
        assert_eq!(
            local.grades.load(Ordering::Relaxed),
            0,
            "an image request must not be graded by a text model"
        );
    }

    /// Grading is a per-call-site decision, not a per-call one.
    #[tokio::test]
    async fn grades_are_cached_by_task_shape() {
        let local = Local::new("easy", "local answer");
        let r = router(
            local.clone(),
            Spy::new("mid", "mid"),
            Spy::new("heavy", "heavy"),
        );
        for i in 0..5 {
            r.complete(&task("classify this", &format!("input {i}")))
                .await
                .unwrap();
        }
        assert_eq!(
            local.grades.load(Ordering::Relaxed),
            1,
            "the same call site should grade once"
        );
        assert_eq!(local.answers.load(Ordering::Relaxed), 5);

        // A different call site grades separately.
        r.complete(&task("a completely different job", "x"))
            .await
            .unwrap();
        assert_eq!(local.grades.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn grade_words_parse_the_way_models_write_them() {
        assert_eq!(Difficulty::parse("Easy"), Some(Difficulty::Easy));
        assert_eq!(Difficulty::parse("extra-hard"), Some(Difficulty::ExtraHard));
        assert_eq!(Difficulty::parse("EXTRA HARD"), Some(Difficulty::ExtraHard));
        assert_eq!(Difficulty::parse("moderate"), Some(Difficulty::Medium));
        assert_eq!(Difficulty::parse("banana"), None);
    }

    /// An unparseable grade must not silently escalate — defaulting to medium keeps
    /// a confused grader from routing everything to Opus.
    #[tokio::test]
    async fn unparseable_grade_defaults_to_medium() {
        let local = Local::new("banana", "local answer");
        let heavy = Spy::new("heavy", "heavy answer");
        let r = router(local, Spy::new("mid", "mid"), heavy.clone());
        let out = r.complete(&task("do this", "input")).await.unwrap();
        assert_eq!(out, "local answer");
        assert_eq!(heavy.calls(), 0);
    }

    #[test]
    fn size_buckets_separate_small_from_large_inputs() {
        let small = task("same job", "tiny");
        let large = task("same job", &"x".repeat(20_000));
        assert_ne!(
            grade_key(&small),
            grade_key(&large),
            "a much larger input should be allowed to grade differently"
        );
        // …but two similar-sized inputs share a key.
        let a = task("same job", "one short input");
        let b = task("same job", "another short input");
        assert_eq!(grade_key(&a), grade_key(&b));
    }

    #[tokio::test]
    async fn routing_disabled_is_pure_local() {
        let local = Local::new("extra_hard", "local answer");
        let heavy = Spy::new("heavy", "heavy answer");
        let r = RoutingReasoner::new(
            local.clone(),
            Spy::new("mid", "mid"),
            heavy.clone(),
            RoutingCfg {
                enabled: false,
                ..Default::default()
            },
        );
        let out = r.complete(&task("anything", "input")).await.unwrap();
        assert_eq!(out, "local answer");
        assert_eq!(local.grades.load(Ordering::Relaxed), 0, "no grading call");
        assert_eq!(heavy.calls(), 0);
    }
}
