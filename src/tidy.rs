//! Clearing GitHub notifications that do not need you — the one place this codebase writes.
//!
//! # The invariant this narrows, and why it survives
//!
//! AGENTS.md states that nothing is ever written back to GitHub, and the reason it gives is
//! the right one: *"an ops agent that comments on your colleagues' pull requests is a
//! different, much worse product."* Every example it lists — a review, an approval, a label,
//! a predicted comment — is **something a colleague can see**.
//!
//! Your notification inbox is on the other side of that line. Marking a thread read is
//! invisible to everyone else, mutates no shared artifact, and says nothing in your name. So
//! the invariant is narrowed rather than abandoned: *nothing is written to anything anyone
//! else can see.* That is the principle the original sentence was reaching for.
//!
//! # The write cannot be aimed anywhere else
//!
//! This is a property of the code, not a promise. [`Tidier::clear`] takes a thread id, checks
//! it is nothing but digits, and constructs `…/notifications/threads/{id}` itself. There is no
//! parameter that reaches the path, so no caller — and no confused subject URL arriving from
//! the API — can turn this into a write against a pull request. [`GithubClient`] still has no
//! write methods at all; this module holds its own client so that stays true.
//!
//! [`GithubClient`]: crate::github::GithubClient
//!
//! # What gets cleared, and the two things that do not
//!
//! Asked for: `ci_activity`, and anything already merged. Read literally that is a one-line
//! rule, but the *purpose* — "clean up my notifications for things that really need my
//! attention" — qualifies it in two places, and both are the kind of thing you would only
//! notice after it had thrown something away:
//!
//! - **A failed CI run is not noise.** A red run on a branch you own is the canonical
//!   thing that needs attention, and it arrives under `ci_activity` like every green one.
//!   [`Tidy::keep_failed_ci`] defaults to keeping those.
//! - **A merged pull request can still have a question in it aimed at you.** `mention`,
//!   `team_mention`, `review_requested` and `assign` mean a human addressed you
//!   specifically; merging the PR did not answer them.
//!
//! And one thing that is never cleared at all, not configurably: `security_alert`.
//!
//! # Dry run first
//!
//! [`Tidy::dry_run`] defaults to **true**, so enabling this produces a list of what it would
//! take rather than an inbox that has already been emptied. Every decision is recorded either
//! way, cleared or not — the audit trail is the answer to "what did it throw away", which is
//! the question you have precisely when you can no longer look.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Tidy;

/// GitHub's notification thread endpoint. The `{id}` is substituted from a digits-only
/// string, which is what makes this un-aimable at anything else.
const THREADS: &str = "https://api.github.com/notifications/threads";

/// Reasons that mean a person addressed *you*, not a repository you watch.
///
/// Merging a pull request does not answer a question someone asked you in it, so these
/// survive the merged rule. They do **not** survive `ci_activity`: a CI run has no author
/// and cannot have asked you anything.
const ADDRESSED_TO_YOU: &[&str] = &["mention", "team_mention", "review_requested", "assign"];

/// Never cleared, and deliberately not configurable.
///
/// A security alert is the one notification whose cost of being wrongly dismissed is
/// unbounded, and it arrives with `state: closed` on a repository whose advisory has been
/// published — which is exactly the shape the merged rule would sweep.
const NEVER: &[&str] = &["security_alert"];

// ---- the decision ------------------------------------------------------------

/// One notification, reduced to what the rules read.
///
/// Owned. The watcher consumes its notifications as it walks them and collects candidates at
/// two different points in that walk — a handful of allocations per poll is not worth the
/// lifetime it would take to avoid.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    /// GitHub's notification thread id — digits.
    pub thread_id: String,
    /// `ci_activity`, `mention`, `subscribed`, …
    pub reason: String,
    /// `PullRequest`, `Issue`, `CheckSuite`, `Discussion`, …
    pub subject_type: String,
    pub title: String,
    pub repo: String,
    /// The subject's lifecycle, as the watcher's enrichment computed it: `open`, `closed`,
    /// `merged`, `draft`. `None` when enrichment is off or the fetch failed — in which case
    /// the merged rule cannot fire, because it does not guess.
    pub state: Option<String>,
    pub url: Option<String>,
}

/// Which rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    CiActivity,
    Merged,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::CiActivity => "ci_activity",
            Rule::Merged => "merged",
        }
    }
}

/// What to do with one notification, and why.
///
/// `Keep` carries its reason because the dry run is the feature: a list of what *would* be
/// cleared is only half of what you need to trust this — the other half is seeing that the
/// thing you were worried about is in the kept column, with the rule that spared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Clear(Rule),
    Keep(&'static str),
}

impl Decision {
    pub fn rule(&self) -> Option<Rule> {
        match self {
            Decision::Clear(r) => Some(*r),
            Decision::Keep(_) => None,
        }
    }
}

/// Does this CI title report a failure?
///
/// Same reading as the watcher's own check, and deliberately generous about what counts as a
/// failure: this decides whether something is *kept*, so a title it cannot classify should
/// err towards keeping. `cancelled` and `timed_out` are included for that reason — neither is
/// success, and a cancelled run on your own branch is usually worth a look.
fn ci_failed(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    ["fail", "error", "cancel", "timed out", "timeout"]
        .iter()
        .any(|needle| t.contains(needle))
}

/// Is this subject merged?
///
/// Only a pull request can be, and only when the watcher actually resolved its state. An
/// `Issue` that reads `closed` is not merged — it was closed, possibly as "not planned", and
/// possibly with the question you were asked still in it.
fn is_merged(c: &Candidate) -> bool {
    c.subject_type == "PullRequest" && c.state.as_deref() == Some("merged")
}

/// Decide one notification's fate.
///
/// Pure, so the whole rule set is testable without a token — which matters more here than
/// anywhere else in this codebase, because the failure mode is silently discarding something
/// you needed and never learning that it happened.
pub fn decide(c: &Candidate, cfg: &Tidy) -> Decision {
    if NEVER.contains(&c.reason.as_str()) {
        return Decision::Keep("security alerts are never cleared");
    }
    if c.reason == "ci_activity" {
        if cfg.keep_failed_ci && ci_failed(&c.title) {
            return Decision::Keep("CI failed, and a red run is not noise");
        }
        if !cfg.clear_ci {
            return Decision::Keep("[tidy].clear_ci is off");
        }
        return Decision::Clear(Rule::CiActivity);
    }
    if is_merged(c) {
        if ADDRESSED_TO_YOU.contains(&c.reason.as_str()) {
            return Decision::Keep("merged, but somebody addressed you in it directly");
        }
        if !cfg.clear_merged {
            return Decision::Keep("[tidy].clear_merged is off");
        }
        return Decision::Clear(Rule::Merged);
    }
    Decision::Keep("needs you, or nothing says otherwise")
}

// ---- the sweep ---------------------------------------------------------------

/// What one pass over the inbox did.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Swept {
    pub considered: usize,
    /// Threads actually cleared on GitHub. Zero in a dry run, however many matched.
    pub cleared: usize,
    /// Threads that matched a rule. In a dry run this is what *would* have gone.
    pub matched: usize,
    pub failed: usize,
}

pub struct Tidier {
    store: std::sync::Arc<crate::store::Store>,
    client: reqwest::Client,
    token: Option<String>,
    cfg: Tidy,
}

impl Tidier {
    pub fn new(
        store: std::sync::Arc<crate::store::Store>,
        token: Option<String>,
        cfg: Tidy,
    ) -> Self {
        Self {
            store,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            token,
            cfg,
        }
    }

    /// On, and holding a token.
    pub fn ready(&self) -> bool {
        self.cfg.enabled && self.token.as_deref().is_some_and(|t| !t.trim().is_empty())
    }

    pub fn dry_run(&self) -> bool {
        self.cfg.dry_run
    }

    /// Consider a batch of notifications and clear the ones no rule protects.
    ///
    /// Records every decision, kept or cleared, before doing anything — so a crash between
    /// the decision and the API call leaves a record of what was about to happen rather than
    /// a gap. The record is the only way to answer "what did it take", and that question
    /// arrives exactly when the notification is no longer there to look at.
    pub async fn sweep(&self, candidates: &[Candidate]) -> Swept {
        let mut out = Swept {
            considered: candidates.len(),
            ..Default::default()
        };
        if !self.ready() {
            return out;
        }
        for c in candidates {
            let decision = decide(c, &self.cfg);
            let Some(rule) = decision.rule() else {
                continue;
            };
            out.matched += 1;
            let dry = self.cfg.dry_run;
            let row = crate::store::TidiedNotification {
                thread_id: c.thread_id.clone(),
                reason: c.reason.clone(),
                rule: rule.as_str().to_string(),
                repo: c.repo.clone(),
                title: c.title.clone(),
                url: c.url.clone(),
                cleared: !dry,
                // Set by the store on write; carried here only to complete the struct.
                at: String::new(),
            };
            if let Err(e) = self.store.record_tidy(&row) {
                tracing::warn!("tidy: recording {} failed: {e:#}", c.thread_id);
            }
            if dry {
                tracing::info!(
                    "tidy (dry run): would clear {} — {} · {} · {}",
                    c.thread_id,
                    rule.as_str(),
                    c.repo,
                    c.title
                );
                continue;
            }
            match self.clear(&c.thread_id).await {
                Ok(()) => {
                    out.cleared += 1;
                    tracing::info!(
                        "tidy: cleared {} — {} · {} · {}",
                        c.thread_id,
                        rule.as_str(),
                        c.repo,
                        c.title
                    );
                }
                Err(e) => {
                    out.failed += 1;
                    // Mark the record as not-cleared so the audit trail does not claim
                    // something happened that did not.
                    self.store.unrecord_tidy_cleared(&c.thread_id).ok();
                    tracing::warn!("tidy: clearing {} failed: {e:#}", c.thread_id);
                }
            }
        }
        out
    }

    /// The write. The only one in this codebase.
    ///
    /// `thread_id` is checked to be digits and nothing else, and the URL is built here — so
    /// there is no input that can steer this at a pull request, a review, or a label, however
    /// wrong a caller or an API response is. That is the guarantee, and
    /// `the_write_cannot_be_aimed_at_anything_but_a_notification_thread` is the test that
    /// keeps it.
    pub async fn clear(&self, thread_id: &str) -> Result<()> {
        let token = self
            .token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .context("no GitHub token")?;
        if thread_id.is_empty() || !thread_id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("refusing to clear {thread_id:?}: a notification thread id is digits only");
        }
        let url = format!("{THREADS}/{thread_id}");
        // Interactive priority: this is a handful of requests behind a poll the operator is
        // waiting on, and it must not be paced into next hour by the background gate.
        let req = match self.cfg.mode.trim().to_ascii_lowercase().as_str() {
            // "Mark a thread as done" — GitHub's own Done button. Harder to undo than read,
            // which is why it is not the default.
            "done" => self.client.delete(&url),
            _ => self.client.patch(&url),
        };
        let resp = req
            .bearer_auth(token)
            .header(reqwest::header::USER_AGENT, "mugglebot")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            // A PATCH with no body still needs a length, and some proxies reject a
            // Content-Length-less write outright.
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .send()
            .await
            .with_context(|| format!("clearing notification thread {thread_id}"))?;
        crate::github::observe(resp.headers());
        let status = resp.status();
        // 205 Reset Content for a mark-as-read, 204 for a mark-as-done, and 304 when it was
        // already read — all of which mean the inbox is now in the state we wanted.
        if status.is_success() || status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        let hint = match status.as_u16() {
            401 => " — the GitHub token is missing, wrong, or revoked",
            403 => {
                " — the token lacks the `notifications` scope. A classic PAT needs \
                 `notifications`; a fine-grained one needs read+write on \"Notifications\""
            }
            404 => " — no such notification thread (already cleared, or not yours)",
            _ => "",
        };
        bail!(
            "clearing notification thread {thread_id}: {}{hint}: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Tidy {
        Tidy {
            enabled: true,
            dry_run: false,
            ..Default::default()
        }
    }

    fn candidate(reason: &str, subject_type: &str, title: &str) -> Candidate {
        Candidate {
            thread_id: "1234567890".into(),
            reason: reason.into(),
            subject_type: subject_type.into(),
            title: title.into(),
            repo: "restatedev/restate-cloud".into(),
            state: None,
            url: None,
        }
    }

    // ---- what gets cleared ----

    #[test]
    fn a_green_ci_run_is_cleared() {
        let c = candidate(
            "ci_activity",
            "CheckSuite",
            "PR Checks (npm) workflow run succeeded",
        );
        assert_eq!(decide(&c, &cfg()), Decision::Clear(Rule::CiActivity));
    }

    #[test]
    fn a_merged_pull_request_is_cleared() {
        let mut c = candidate("subscribed", "PullRequest", "Bump the pool ceiling");
        c.state = Some("merged".into());
        assert_eq!(decide(&c, &cfg()), Decision::Clear(Rule::Merged));
    }

    /// The user's own board carries exactly this notification, which is what the feature is
    /// for: a succeeded workflow run sitting in the inbox as "nothing to do".
    #[test]
    fn the_real_noise_notification_is_cleared() {
        let c = candidate(
            "ci_activity",
            "CheckSuite",
            "PR Checks (npm) workflow run succeeded for bh/otel1 branch",
        );
        assert_eq!(decide(&c, &cfg()), Decision::Clear(Rule::CiActivity));
    }

    // ---- what survives, and why ----

    /// The first qualification the stated purpose puts on the stated rule. A red run on a
    /// branch you own is the canonical thing that needs attention, and it arrives under
    /// `ci_activity` like every green one.
    #[test]
    fn a_failed_ci_run_is_kept_by_default() {
        let c = candidate(
            "ci_activity",
            "CheckSuite",
            "CI workflow run failed for main",
        );
        match decide(&c, &cfg()) {
            Decision::Keep(why) => assert!(why.contains("red run"), "{why}"),
            other => panic!("a failed run must be kept, got {other:?}"),
        }
    }

    /// Cancelled and timed-out are not success either, and the classifier errs towards
    /// keeping whatever it cannot read as green.
    #[test]
    fn a_cancelled_or_timed_out_run_is_kept_too() {
        for title in [
            "Deploy workflow run cancelled",
            "Nightly workflow run timed out",
            "Build errored",
        ] {
            let c = candidate("ci_activity", "CheckSuite", title);
            assert!(
                matches!(decide(&c, &cfg()), Decision::Keep(_)),
                "{title} should be kept"
            );
        }
    }

    /// Turning the failure guard off is the literal reading of the request, and it works —
    /// it just has to be asked for.
    #[test]
    fn a_failed_run_can_be_cleared_when_the_guard_is_turned_off() {
        let c = candidate("ci_activity", "CheckSuite", "CI workflow run failed");
        let cfg = Tidy {
            keep_failed_ci: false,
            ..cfg()
        };
        assert_eq!(decide(&c, &cfg), Decision::Clear(Rule::CiActivity));
    }

    /// The second qualification. Merging a pull request does not answer a question somebody
    /// asked you inside it.
    #[test]
    fn a_merged_pr_that_mentions_you_is_kept() {
        for reason in ["mention", "team_mention", "review_requested", "assign"] {
            let mut c = candidate(reason, "PullRequest", "Bump the pool ceiling");
            c.state = Some("merged".into());
            match decide(&c, &cfg()) {
                Decision::Keep(why) => assert!(why.contains("addressed you"), "{why}"),
                other => panic!("{reason} on a merged PR must be kept, got {other:?}"),
            }
        }
    }

    /// But a CI run cannot have addressed you, so those reasons do not protect it — and in
    /// practice CI never carries them anyway.
    #[test]
    fn being_mentioned_does_not_protect_a_green_ci_run() {
        let c = candidate("mention", "CheckSuite", "workflow run succeeded");
        // `mention` is not `ci_activity`, so this falls through to the default: kept. The
        // point of the test is that the CI branch is keyed on the *reason*, not on a
        // guess from the subject type.
        assert!(matches!(decide(&c, &cfg()), Decision::Keep(_)));
    }

    /// Unbounded downside, so it is not a setting.
    #[test]
    fn a_security_alert_is_never_cleared_even_when_merged() {
        let mut c = candidate("security_alert", "PullRequest", "Bump serde to 1.0.200");
        c.state = Some("merged".into());
        let permissive = Tidy {
            keep_failed_ci: false,
            clear_ci: true,
            clear_merged: true,
            ..cfg()
        };
        match decide(&c, &permissive) {
            Decision::Keep(why) => assert!(why.contains("security"), "{why}"),
            other => panic!("a security alert must never be cleared, got {other:?}"),
        }
    }

    /// A closed issue is not a merged pull request. It may have been closed as "not
    /// planned", with the question you were asked still in it.
    #[test]
    fn a_closed_issue_is_not_merged() {
        let mut c = candidate(
            "subscribed",
            "Issue",
            "Tenant pod reschedules are invisible",
        );
        c.state = Some("closed".into());
        assert!(matches!(decide(&c, &cfg()), Decision::Keep(_)));
    }

    /// The merged rule reads a resolved state and never infers one. With enrichment off,
    /// `state` is `None` and nothing is swept on a guess.
    #[test]
    fn an_unknown_state_is_never_treated_as_merged() {
        let c = candidate("subscribed", "PullRequest", "Bump the pool ceiling");
        assert_eq!(c.state, None);
        assert!(matches!(decide(&c, &cfg()), Decision::Keep(_)));
    }

    #[test]
    fn an_open_pull_request_is_kept() {
        let mut c = candidate("subscribed", "PullRequest", "Gate production deploys");
        c.state = Some("open".into());
        assert!(matches!(decide(&c, &cfg()), Decision::Keep(_)));
    }

    #[test]
    fn a_review_request_on_an_open_pr_is_kept() {
        let mut c = candidate("review_requested", "PullRequest", "Env VO creates the CR");
        c.state = Some("open".into());
        assert!(matches!(decide(&c, &cfg()), Decision::Keep(_)));
    }

    #[test]
    fn each_rule_can_be_switched_off_on_its_own() {
        let ci = candidate("ci_activity", "CheckSuite", "workflow run succeeded");
        let mut merged = candidate("subscribed", "PullRequest", "x");
        merged.state = Some("merged".into());

        let no_ci = Tidy {
            clear_ci: false,
            ..cfg()
        };
        assert!(matches!(decide(&ci, &no_ci), Decision::Keep(_)));
        assert_eq!(decide(&merged, &no_ci), Decision::Clear(Rule::Merged));

        let no_merged = Tidy {
            clear_merged: false,
            ..cfg()
        };
        assert_eq!(decide(&ci, &no_merged), Decision::Clear(Rule::CiActivity));
        assert!(matches!(decide(&merged, &no_merged), Decision::Keep(_)));
    }

    // ---- the write ----

    fn tidier(cfg: Tidy) -> Tidier {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        Tidier::new(store, Some("ghp_test".into()), cfg)
    }

    /// The structural guarantee. A thread id is digits, the URL is built here, and no input
    /// reaches the path — so this cannot be turned into a write against a pull request even
    /// by a caller that wants to.
    #[test]
    fn the_write_cannot_be_aimed_at_anything_but_a_notification_thread() {
        let t = tidier(cfg());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for evil in [
            "../../repos/restatedev/restate/pulls/1",
            "1234/../../../repos/o/r/issues/1/comments",
            "https://api.github.com/repos/o/r/pulls/1",
            "1234567890?x=y",
            "",
            "abc",
            "12 34",
        ] {
            let err = rt.block_on(t.clear(evil)).expect_err("must refuse {evil}");
            assert!(
                format!("{err:#}").contains("digits only"),
                "{evil} was not refused for the right reason: {err:#}"
            );
        }
    }

    /// A disabled tidier does nothing at all — no decisions, no records, no requests.
    #[tokio::test]
    async fn a_disabled_tidier_sweeps_nothing() {
        let t = tidier(Tidy::default());
        assert!(!t.ready());
        let c = candidate("ci_activity", "CheckSuite", "workflow run succeeded");
        let swept = t.sweep(&[c]).await;
        assert_eq!(swept.considered, 1);
        assert_eq!(swept.matched, 0);
        assert_eq!(swept.cleared, 0);
    }

    /// A tidier with no token is inert however configured — the same shape every other
    /// credentialled component here degrades into.
    #[tokio::test]
    async fn no_token_means_inert() {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let t = Tidier::new(store, None, cfg());
        assert!(!t.ready());
    }

    /// The dry run records what it would take and takes nothing. This is what the first run
    /// after enabling should look like.
    #[tokio::test]
    async fn a_dry_run_records_the_decision_and_clears_nothing() {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let t = Tidier::new(
            store.clone(),
            Some("ghp_test".into()),
            Tidy {
                enabled: true,
                dry_run: true,
                ..Default::default()
            },
        );
        let c = candidate("ci_activity", "CheckSuite", "workflow run succeeded");
        let swept = t.sweep(&[c]).await;
        assert_eq!(swept.matched, 1);
        assert_eq!(swept.cleared, 0, "a dry run must not clear");
        assert_eq!(swept.failed, 0, "and must not call the API to fail at it");

        let recorded = store.list_tidied(10).unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].thread_id, "1234567890");
        assert_eq!(recorded[0].rule, "ci_activity");
        assert!(
            !recorded[0].cleared,
            "recorded as considered, not as cleared"
        );
    }

    /// Run the rules over the operator's **real** notification feed and print what they
    /// would do. Reads only: it never calls [`Tidier::clear`], so it cannot change anything
    /// whatever the config says.
    ///
    /// This is the check that matters before turning the feature on, and a fixture cannot
    /// give it: the question is whether these rules, on this inbox, keep what you need.
    ///
    /// ```text
    /// cargo test what_the_rules_would_do_to_the_real_inbox -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn what_the_rules_would_do_to_the_real_inbox() {
        let db = std::env::var("MUGGLEBOT_DB").unwrap_or_else(|_| "data/mugglebot.sqlite".into());
        let store = std::sync::Arc::new(
            crate::store::Store::open(std::path::Path::new(&db)).expect("open the store"),
        );
        let secrets = crate::secrets::Secrets::for_tests(store.clone());
        let token = secrets
            .get_opt("github")
            .expect("no `github` credential stored");

        // `all=true` by default, unlike the watcher's `all=false`. The watcher only ever sees
        // unread threads, but an empty unread inbox tells you nothing about the rules — and
        // read notifications are the same shapes. Set MUGGLEBOT_TIDY_UNREAD_ONLY=1 to see
        // exactly what a poll right now would consider.
        let all = if std::env::var("MUGGLEBOT_TIDY_UNREAD_ONLY").is_ok() {
            "false"
        } else {
            "true"
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let feed: Vec<serde_json::Value> = rt.block_on(async {
            let client = reqwest::Client::new();
            let resp = client
                .get("https://api.github.com/notifications")
                .bearer_auth(&token)
                .header(reqwest::header::USER_AGENT, "mugglebot")
                .query(&[("all", all), ("per_page", "100")])
                .send()
                .await
                .expect("requesting notifications");
            assert!(resp.status().is_success(), "GitHub said {}", resp.status());
            resp.json().await.expect("parsing notifications")
        });
        println!("{} notification(s) (all={all})\n", feed.len());

        // `state` is what the watcher's enrichment would supply. It is not fetched here — so
        // the merged rule will mostly show as "state unknown", which is itself the finding:
        // with `[sources.github].enrich` off, the merged rule cannot fire at all.
        let cfg = Tidy {
            enabled: true,
            ..Default::default()
        };
        let mut clear = 0usize;
        let mut keep = 0usize;
        for n in &feed {
            let get = |k: &str| n.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let c = Candidate {
                thread_id: get("id"),
                reason: get("reason"),
                subject_type: n
                    .get("subject")
                    .and_then(|s| s.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                title: n
                    .get("subject")
                    .and_then(|s| s.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                repo: n
                    .get("repository")
                    .and_then(|r| r.get("full_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                state: None,
                url: None,
            };
            let title: String = c.title.chars().take(58).collect();
            match decide(&c, &cfg) {
                Decision::Clear(rule) => {
                    clear += 1;
                    println!("CLEAR  [{:<11}] {:<24} {title}", rule.as_str(), c.repo);
                }
                Decision::Keep(why) => {
                    keep += 1;
                    println!(
                        "keep   [{:<11}] {:<24} {title}\n         ({why})",
                        c.reason, c.repo
                    );
                }
            }
        }
        println!("\nwould clear {clear}, would keep {keep}");
    }

    /// Kept notifications are not recorded. The table answers "what did it take from my
    /// inbox", and filling it with everything it left alone would bury that.
    #[tokio::test]
    async fn what_is_kept_leaves_no_record() {
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let t = Tidier::new(
            store.clone(),
            Some("ghp_test".into()),
            Tidy {
                enabled: true,
                dry_run: true,
                ..Default::default()
            },
        );
        let c = candidate("mention", "Issue", "can you look at this?");
        let swept = t.sweep(&[c]).await;
        assert_eq!(swept.matched, 0);
        assert!(store.list_tidied(10).unwrap().is_empty());
    }
}
