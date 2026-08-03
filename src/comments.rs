//! Judging issue and PR comments **on their merits**.
//!
//! The discussion on an issue is usually where its real content is. A title says
//! what broke; the comments say what was tried, what was ruled out, what a
//! maintainer decided, and — on a pull request — what a reviewer is blocking on.
//! Reasoning from the body alone reliably misses an answer someone already wrote
//! down.
//!
//! But a long subject cannot go into a bounded context window whole, and the obvious
//! shortcuts are both wrong:
//!
//! - **Keeping the most recent N** throws away the framing. The opening comments
//!   carry the reproduction and the initial diagnosis; the tail carries the
//!   resolution. Truncating to the tail keeps the conclusion and discards what it
//!   was a conclusion *about*.
//! - **Keeping the first N and last M** is better, but still decides by position.
//!   A decisive comment in the middle of a fifty-comment subject — "this is a
//!   duplicate of the connection-pool bug, see #204" — is exactly the one that
//!   changes what you do, and position tells you nothing about that.
//!
//! So **every comment is considered on its own merits**: each is scored for whether
//! it carries decision-relevant information, and what survives is chosen by that
//! score rather than by where it sits in the subject. Scoring runs on the local model
//! in a single batched pass (all comments, by index, one call), with a deterministic
//! heuristic underneath so the pass still works with nothing reachable.
//!
//! Order is always restored before rendering: a model that ranks comment 40 above
//! comment 2 has said which matters more, not that the conversation happened
//! backwards.

use std::sync::Arc;
use tracing::debug;

use crate::github::Comment;
use crate::reasoner::{self, CompletionRequest, Reasoner};

/// A comment with the merit judgment attached.
#[derive(Debug, Clone)]
pub struct Judged {
    pub comment: Comment,
    /// 0–100. Not a probability — a ranking key.
    pub merit: u32,
    /// Why it scored that way, for the audit trail.
    pub reason: &'static str,
}

/// Signals that a comment carries real information. Deliberately about *substance*
/// rather than length: a two-line comment naming the root cause outranks three
/// paragraphs of speculation.
const SUBSTANCE_MARKERS: &[&str] = &[
    "root cause",
    "because",
    "reproduce",
    "repro",
    "the problem is",
    "the issue is",
    "turns out",
    "i tried",
    "we tried",
    "doesn't work",
    "does not work",
    "fails",
    "failing",
    "error",
    "exception",
    "panic",
    "traceback",
    "stack trace",
    "workaround",
    "fixed by",
    "fixed in",
    "duplicate of",
    "caused by",
    "regression",
    "should instead",
    "needs to",
    "blocking",
    "won't work",
    "edge case",
];

/// Comments that are social rather than informational. These are the ones a merit
/// pass exists to drop.
const NOISE_PATTERNS: &[&str] = &[
    "+1",
    "same here",
    "me too",
    "any update",
    "any updates",
    "bump",
    "following",
    "thanks",
    "thank you",
    "lgtm",
    "ping",
    "friendly ping",
    "still happening",
];

/// Bot authors whose comments are automation, not discussion.
const BOT_SUFFIXES: &[&str] = &["[bot]", "-bot", "bot"];

pub struct CommentJudge {
    /// Local model — scoring a subject's comments is high-volume mechanical work.
    local: Arc<dyn Reasoner>,
}

impl CommentJudge {
    pub fn new(local: Arc<dyn Reasoner>) -> Self {
        Self { local }
    }

    /// Judge every comment, then return the meritorious ones in conversation order,
    /// bounded by `max_chars`.
    ///
    /// `context` is what merit is judged *relative to* — the issue title, or the
    /// question being asked of a PR. A comment is not meritorious in the abstract;
    /// it's meritorious about something.
    pub async fn select(
        &self,
        comments: &[Comment],
        context: &str,
        max_chars: usize,
        fresh: bool,
    ) -> Vec<Judged> {
        if comments.is_empty() {
            return Vec::new();
        }
        // Every comment starts with a deterministic score, so nothing depends on a
        // model being reachable.
        let mut judged: Vec<Judged> = comments.iter().map(judge_locally).collect();

        // Then the model reconsiders all of them together, which is the only way to
        // spot "this one is decisive" — merit is comparative. This runs for a single
        // comment too: "is this substance or is it 'any update?'" is exactly the
        // judgment being asked for, and skipping it would mean one comment escapes
        // being considered.
        if let Some(scores) = self.score_with_model(comments, context, fresh).await {
            for (i, score) in scores {
                if let Some(j) = judged.get_mut(i) {
                    // Take the higher of the two. A blocking review the model
                    // happens to overlook must not be demoted by it.
                    if score > j.merit {
                        j.merit = score;
                        j.reason = "model judged substantive";
                    }
                }
            }
        }

        // Rank by merit, keep what fits, then restore conversation order.
        let mut ranked: Vec<(usize, Judged)> = judged.into_iter().enumerate().collect();
        ranked.sort_by(|a, b| b.1.merit.cmp(&a.1.merit).then(a.0.cmp(&b.0)));

        let mut kept: Vec<(usize, Judged)> = Vec::new();
        let mut used = 0usize;
        for (idx, j) in ranked {
            // Zero merit is noise; including it would defeat the point.
            if j.merit == 0 {
                continue;
            }
            let cost = j.comment.body.len();
            if used + cost > max_chars && !kept.is_empty() {
                continue;
            }
            used += cost;
            kept.push((idx, j));
        }
        kept.sort_by_key(|(idx, _)| *idx);
        kept.into_iter().map(|(_, j)| j).collect()
    }

    /// One batched call: every comment, by index, scored 0–100.
    async fn score_with_model(
        &self,
        comments: &[Comment],
        context: &str,
        fresh: bool,
    ) -> Option<Vec<(usize, u32)>> {
        let mut catalog = String::new();
        for (i, c) in comments.iter().enumerate() {
            catalog.push_str(&format!(
                "{i}. [{}] {}{}: {}\n",
                c.kind,
                c.author.as_deref().unwrap_or("unknown"),
                c.state
                    .as_deref()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default(),
                truncate(&c.body.replace('\n', " "), 400),
            ));
        }
        let system =
            "You score how much each comment on an issue or pull request would change what \
             an engineer does next. Reply with ONLY a JSON object mapping the comment index to a \
             score 0-100: {\"0\": 80, \"1\": 0, …}. Score EVERY index you are given.\n\
             90-100 — names the cause, the fix, a duplicate, or blocks the change.\n\
             60-89  — concrete evidence: a reproduction, an error, something tried and its result, \
             a constraint or decision.\n\
             30-59  — relevant opinion or a partial clue.\n\
             0-9    — social or contentless: \"+1\", \"same here\", \"any update\", thanks, a bare \
             approval, bot automation.\n\
             Judge substance, not length: two lines naming the root cause outrank three paragraphs \
             of speculation. Position is irrelevant — a decisive comment in the middle of a long \
             subject scores high.";
        let prompt = format!("Subject: {context}\n\nComments:\n{catalog}");
        let mut req = CompletionRequest::single(prompt)
            .with_system(system)
            .max_tokens(600);
        req.no_cache = fresh;

        let raw = match self.local.complete(&req).await {
            Ok(raw) => raw,
            Err(e) => {
                debug!("comments: merit scoring unavailable ({e:#}); using heuristics");
                return None;
            }
        };
        let value = reasoner::extract_json(&raw)?;
        let map = value.as_object()?;
        let mut out = Vec::new();
        for (key, score) in map {
            if let (Ok(idx), Some(score)) = (key.trim().parse::<usize>(), score.as_u64()) {
                if idx < comments.len() {
                    out.push((idx, score.min(100) as u32));
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// The deterministic merit judgment, used as the floor under the model's.
fn judge_locally(c: &Comment) -> Judged {
    let body = c.body.trim();
    let lower = body.to_ascii_lowercase();

    // A reviewer blocking the change is the highest-merit comment there is, and its
    // score must not depend on a model call succeeding.
    if c.is_blocking() {
        return judged(c, 100, "blocking review");
    }
    // Bot automation is not discussion.
    if c.author.as_deref().is_some_and(is_bot) {
        return judged(c, 0, "bot");
    }
    // Contentless social replies, judged on the whole comment so that a long comment
    // that merely opens with "thanks" isn't discarded.
    let compact: String = lower
        .chars()
        .filter(|ch| ch.is_alphanumeric() || ch.is_whitespace() || *ch == '+')
        .collect();
    let compact = compact.trim();
    if compact.is_empty() {
        return judged(c, 0, "empty");
    }
    if compact.len() <= 40 && NOISE_PATTERNS.iter().any(|p| compact.contains(p)) {
        return judged(c, 0, "social");
    }

    let mut score = 30u32;
    // Evidence markers.
    let markers = SUBSTANCE_MARKERS
        .iter()
        .filter(|m| lower.contains(**m))
        .count();
    score += (markers as u32 * 15).min(50);
    // Pasted code, logs, or stack traces are evidence by construction.
    if body.contains("```") || body.lines().filter(|l| l.starts_with("    ")).count() >= 2 {
        score += 20;
    }
    // A cross-reference to another issue or PR is a real link in the graph.
    if lower.contains('#') && body.chars().any(|ch| ch.is_ascii_digit()) {
        score += 10;
    }
    // An inline review comment is anchored to a specific line, so it is about the
    // change rather than about the discussion.
    if c.kind == "review_comment" {
        score += 10;
    }
    // Very short and marker-free: probably chatter.
    if markers == 0 && body.len() < 80 {
        score = score.saturating_sub(25);
    }
    judged(c, score.min(99), "heuristic")
}

fn judged(c: &Comment, merit: u32, reason: &'static str) -> Judged {
    Judged {
        comment: c.clone(),
        merit,
        reason,
    }
}

fn is_bot(author: &str) -> bool {
    let lower = author.to_ascii_lowercase();
    BOT_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// Render judged comments for a prompt, stating what was set aside so the model
/// knows it is seeing a selection.
pub fn render(judged: &[Judged], total: usize) -> String {
    if judged.is_empty() {
        return if total == 0 {
            "(no comments)".into()
        } else {
            format!("({total} comments, none carrying decision-relevant content)")
        };
    }
    let mut out = String::new();
    if total > judged.len() {
        out.push_str(&format!(
            "({} of {total} comments — every comment was scored for relevance; \
             these are the substantive ones, in conversation order)\n\n",
            judged.len()
        ));
    }
    for j in judged {
        out.push_str(&j.comment.render());
        out.push('\n');
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reasoner::MockReasoner;

    fn comment(author: &str, body: &str) -> Comment {
        Comment {
            author: Some(author.into()),
            created_at: Some("2026-07-01T00:00:00Z".into()),
            body: body.into(),
            kind: "discussion".into(),
            path: None,
            state: None,
            url: Some(format!("https://github.com/o/r/issues/1#issuecomment-{author}")),
        }
    }

    fn blocking(body: &str) -> Comment {
        Comment {
            state: Some("CHANGES_REQUESTED".into()),
            kind: "review".into(),
            ..comment("reviewer", body)
        }
    }

    #[test]
    fn social_noise_scores_zero() {
        for body in [
            "+1",
            "same here",
            "any update?",
            "bump",
            "Thanks!",
            "  👍  ",
        ] {
            let j = judge_locally(&comment("someone", body));
            assert_eq!(j.merit, 0, "{body:?} should be noise, got {}", j.merit);
        }
    }

    /// A long comment that merely opens with "thanks" is not social noise.
    #[test]
    fn a_substantive_comment_that_starts_politely_is_kept() {
        let j = judge_locally(&comment(
            "dev",
            "Thanks for the report. The root cause is that the pool is never drained \
             on a terminal error, so it grows without bound under retries.",
        ));
        assert!(j.merit >= 60, "got {}", j.merit);
    }

    #[test]
    fn evidence_outranks_speculation() {
        let evidence = judge_locally(&comment(
            "dev",
            "I tried it and it fails with:\n```\nthread panicked: pool exhausted\n```",
        ));
        let speculation = judge_locally(&comment("dev", "Maybe it is a caching thing?"));
        assert!(
            evidence.merit > speculation.merit,
            "{} vs {}",
            evidence.merit,
            speculation.merit
        );
    }

    /// A blocking review is the highest-merit comment there is, and must not depend
    /// on the model to be scored that way.
    #[test]
    fn a_blocking_review_always_wins() {
        let j = judge_locally(&blocking("This doesn't handle the retry case."));
        assert_eq!(j.merit, 100);
        assert_eq!(j.reason, "blocking review");
    }

    #[test]
    fn bot_comments_are_not_discussion() {
        assert_eq!(
            judge_locally(&comment("dependabot[bot]", "Bumps serde")).merit,
            0
        );
        assert_eq!(
            judge_locally(&comment("codecov-bot", "Coverage 91%")).merit,
            0
        );
    }

    /// The core requirement: merit, not position. A decisive comment in the middle
    /// of a long subject must survive.
    #[tokio::test]
    async fn a_decisive_middle_comment_survives() {
        let judge = CommentJudge::new(Arc::new(MockReasoner::new("not json")));
        let mut comments: Vec<Comment> = (0..40)
            .map(|i| comment("someone", &format!("+1 (me too number {i})")))
            .collect();
        comments[20] = comment(
            "maintainer",
            "This is a duplicate of #204 — the root cause is the unbounded pool.",
        );

        let kept = judge.select(&comments, "pool leak", 10_000, false).await;
        assert_eq!(kept.len(), 1, "only the substantive comment should survive");
        assert!(kept[0].comment.body.contains("duplicate of #204"));
    }

    /// Output must be in conversation order, whatever order merit ranked it in.
    #[tokio::test]
    async fn selection_is_returned_in_conversation_order() {
        let judge = CommentJudge::new(Arc::new(MockReasoner::new("not json")));
        let comments = vec![
            comment("a", "The root cause is the pool never draining on error."),
            comment("b", "+1"),
            comment("c", "I tried the workaround and it fails with a panic."),
        ];
        let kept = judge.select(&comments, "pool", 10_000, false).await;
        assert_eq!(kept.len(), 2);
        assert!(kept[0].comment.body.starts_with("The root cause"));
        assert!(kept[1].comment.body.starts_with("I tried"));
    }

    /// The model's score raises merit but must never demote a blocking review.
    #[tokio::test]
    async fn the_model_cannot_demote_a_blocking_review() {
        let judge = CommentJudge::new(Arc::new(MockReasoner::new(r#"{"0": 0, "1": 95}"#)));
        let comments = vec![
            blocking("This misses the retry path."),
            comment("dev", "Some ordinary remark about the change."),
        ];
        let kept = judge.select(&comments, "pr", 10_000, false).await;
        assert_eq!(kept.len(), 2);
        assert!(
            kept[0].comment.is_blocking(),
            "the blocking review must still be present"
        );
    }

    #[tokio::test]
    async fn the_model_can_rescue_a_comment_heuristics_would_drop() {
        // Heuristics would score this low (short, no markers); the model says 90.
        let judge = CommentJudge::new(Arc::new(MockReasoner::new(r#"{"0": 90}"#)));
        let kept = judge
            .select(
                &[comment("dev", "It's the mutex ordering.")],
                "deadlock",
                10_000,
                false,
            )
            .await;
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].reason, "model judged substantive");
    }

    #[tokio::test]
    async fn the_character_budget_is_respected() {
        let judge = CommentJudge::new(Arc::new(MockReasoner::new("not json")));
        let comments: Vec<Comment> = (0..10)
            .map(|i| {
                comment(
                    "dev",
                    &format!("The root cause is issue number {i}: {}", "x".repeat(500)),
                )
            })
            .collect();
        let kept = judge.select(&comments, "topic", 1_200, false).await;
        assert!(!kept.is_empty(), "at least one comment must survive");
        let total: usize = kept.iter().map(|j| j.comment.body.len()).sum();
        assert!(total <= 1_200 + 600, "budget wildly exceeded: {total}");
    }

    #[tokio::test]
    async fn no_comments_is_not_an_error() {
        let judge = CommentJudge::new(Arc::new(MockReasoner::new("{}")));
        assert!(judge.select(&[], "topic", 1_000, false).await.is_empty());
        assert_eq!(render(&[], 0), "(no comments)");
    }

    #[test]
    fn render_states_that_it_is_a_selection() {
        let j = vec![judge_locally(&comment(
            "dev",
            "The root cause is X because Y.",
        ))];
        let out = render(&j, 30);
        assert!(out.contains("1 of 30 comments"));
        assert!(out.contains("scored for relevance"));
    }
}
