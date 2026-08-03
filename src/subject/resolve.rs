//! Attribution: which subject is this signal about?
//!
//! Deterministic, and **pure** — no I/O, no network, no store. That constraint is
//! load-bearing rather than stylistic: from Phase 2 this runs inside a Restate
//! ingest handler, where anything non-deterministic has to be wrapped in
//! `ctx.run` and journalled. It also means the whole attribution model is testable
//! from a list of keys.
//!
//! The consequence for callers: **branch → PR and commit → PR resolution happens
//! in the watcher**, which has the GitHub client, and arrives here as a `pr` key.
//! By the time a signal reaches the resolver, everything that could be looked up
//! already has been.

use super::{SubjectKey, SubjectRank};
use crate::signal::ResolutionKey;

/// Resolution-key kinds that can never own a signal, however specific they look.
///
/// All for the same reason: they're long-lived and shared. `repo` spans a
/// repository's entire history, `channel` spans everything that ever fired in it,
/// `person` spans a career. Grouping on one collapses unrelated work into a single
/// card — and unlike a bad LLM verdict, it does so silently.
const CONTEXT_ONLY: &[&str] = &[
    "repo", "channel", "person", "label", "ci", "meeting", "service",
];

/// Default branch names. A branch is a topic only when it's a feature branch:
/// `main` is shared by every CI run in a repository forever. CI on a default
/// branch is attributed to the PR that merged the commit — resolved upstream in
/// the watcher — and if that lookup failed, the signal is unattributed rather than
/// grouped under `main`.
const DEFAULT_BRANCHES: &[&str] = &["main", "master", "trunk", "develop", "development"];

/// The outcome of attributing one signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// The subject that owns the signal. `None` → the unattributed lane.
    ///
    /// Minting a subject per unresolvable event is deliberately *not* done: that is
    /// exactly how you get a board of near-identical one-signal cards.
    pub subject: Option<SubjectKey>,
    /// Lower-ranked subjects the signal also names, kept as links so a later
    /// notification about the PR lands on the same place. Ordered high rank first.
    pub secondary: Vec<SubjectKey>,
    /// A Slack-rank merge key (currently only `environment`) — see
    /// [`slack_merge_key`].
    pub merge_key: Option<String>,
}

/// Attribute a signal from its resolution keys.
///
/// Climbs as far up **issue > pull request > Slack thread** as the keys allow, and
/// keeps every lower rank as a secondary link.
pub fn attribute(keys: &[ResolutionKey]) -> Attribution {
    let mut candidates: Vec<SubjectKey> = Vec::new();
    for k in keys {
        if let Some(subject) = as_subject(k) {
            if !candidates.contains(&subject) {
                candidates.push(subject);
            }
        }
    }
    // Highest rank wins. Stable within a rank so two issues on one signal resolve
    // to the same subject on every replay — which matters once this runs inside a
    // journalled handler.
    candidates.sort_by_key(|k| std::cmp::Reverse(k.rank()));

    let mut iter = candidates.into_iter();
    let subject = iter.next();
    Attribution {
        merge_key: subject
            .as_ref()
            .filter(|s| s.rank() == SubjectRank::SlackThread)
            .and_then(|_| slack_merge_key(keys)),
        secondary: iter.collect(),
        subject,
    }
}

/// The subject this key names, if it names one at all.
fn as_subject(k: &ResolutionKey) -> Option<SubjectKey> {
    let kind = k.kind.to_ascii_lowercase();
    if CONTEXT_ONLY.contains(&kind.as_str()) {
        return None;
    }
    match kind.as_str() {
        // `incident:INC-448` → the incident subject. Highest rank, so a signal naming both an
        // incident and an issue lands on the incident — the outage is the work, and the issue
        // it turns out to be about is attached to it as an edge rather than owning it.
        "incident" => (!k.value.trim().is_empty()).then(|| SubjectKey::incident(&k.value)),
        // `issue:owner/repo#412` → `owner/repo#412`
        "issue" => split_repo_number(&k.value).map(|(repo, n)| SubjectKey::issue(repo, n)),
        "discussion" => {
            split_repo_number(&k.value).map(|(repo, n)| SubjectKey::discussion(repo, n))
        }
        // Watchers emit PRs as `repo#N` too; the subject form uses `!` so an issue
        // and a PR with the same number stay distinct.
        "pr" => split_repo_number(&k.value).map(|(repo, n)| SubjectKey::pull_request(repo, n)),
        "slack_thread" => {
            // `channel_id/thread_ts`; anything else isn't addressable.
            (k.value.contains('/')).then(|| SubjectKey::slack_thread(&k.value))
        }
        // A feature branch or a commit is *how you find* a PR, never a subject
        // itself. If the watcher couldn't resolve it, the signal is unattributed.
        "branch" | "commit" | "environment" => None,
        _ => None,
    }
}

/// Two Slack-rank subjects sharing this key are the same underlying thing, and
/// merge without asking the LLM.
///
/// Only `environment` qualifies today. A Restate Cloud environment id names one
/// customer's environment — specific in the way `main`, `repo`, and `#alerts` are
/// not, which is why it outranked everything in the pre-Restate model. It still
/// isn't a durable piece of work, so it can't own a subject; but demoting it to
/// plain context would stop two alerts about `env-2abc` in two different Slack
/// threads from ever collapsing, which is a real loss. So it groups, and never owns.
pub fn slack_merge_key(keys: &[ResolutionKey]) -> Option<String> {
    keys.iter()
        .find(|k| k.kind.eq_ignore_ascii_case("environment"))
        .map(|k| format!("environment:{}", k.value.to_ascii_lowercase()))
}

/// Is this resolution key a repository's default branch?
pub fn is_default_branch(kind: &str, value: &str) -> bool {
    if !kind.eq_ignore_ascii_case("branch") {
        return false;
    }
    let branch = value.rsplit_once('@').map(|(_, b)| b).unwrap_or(value);
    DEFAULT_BRANCHES.contains(&branch.to_ascii_lowercase().as_str())
}

/// `owner/repo#412` → `("owner/repo", 412)`.
fn split_repo_number(value: &str) -> Option<(&str, u64)> {
    let (repo, number) = value.rsplit_once('#')?;
    if repo.is_empty() {
        return None;
    }
    Some((repo, number.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(pairs: &[(&str, &str)]) -> Vec<ResolutionKey> {
        pairs
            .iter()
            .map(|(k, v)| ResolutionKey::new(*k, *v))
            .collect()
    }

    #[test]
    fn the_highest_rank_present_owns_the_signal() {
        // A CI run the watcher resolved branch → PR → issue: the issue owns it, and
        // the PR rides along so a later PR notification lands in the same place.
        let a = attribute(&keys(&[
            ("repo", "o/r"),
            ("branch", "o/r@fix-pool"),
            ("pr", "o/r#987"),
            ("issue", "o/r#412"),
        ]));
        assert_eq!(a.subject, Some(SubjectKey::issue("o/r", 412)));
        assert_eq!(a.secondary, vec![SubjectKey::pull_request("o/r", 987)]);
    }

    #[test]
    fn a_pr_owns_when_no_issue_resolved() {
        let a = attribute(&keys(&[("pr", "o/r#987"), ("branch", "o/r@fix")]));
        assert_eq!(a.subject, Some(SubjectKey::pull_request("o/r", 987)));
        assert!(a.secondary.is_empty());
    }

    #[test]
    fn slack_owns_only_when_nothing_github_resolved() {
        let alone = attribute(&keys(&[
            ("channel", "#alerts"),
            ("slack_thread", "C02/1721822400.001"),
        ]));
        assert_eq!(
            alone.subject,
            Some(SubjectKey::slack_thread("C02/1721822400.001"))
        );

        // The same thread, once it names an issue: the issue owns it and the
        // conversation becomes context rather than a second card.
        let resolved = attribute(&keys(&[
            ("slack_thread", "C02/1721822400.001"),
            ("issue", "o/r#412"),
        ]));
        assert_eq!(resolved.subject, Some(SubjectKey::issue("o/r", 412)));
        assert_eq!(
            resolved.secondary,
            vec![SubjectKey::slack_thread("C02/1721822400.001")]
        );
    }

    #[test]
    fn context_only_keys_never_own_anything() {
        // Every one of these is shared across unrelated work; a subject keyed on one
        // would collapse a repo's or a channel's whole history into a single card.
        for (kind, value) in [
            ("repo", "o/r"),
            ("channel", "#alerts"),
            ("person", "ben"),
            ("label", "bug"),
            ("ci", "o/r:12345"),
            ("meeting", "Weekly sync"),
            ("branch", "o/r@main"),
            ("branch", "o/r@fix-pool"),
            ("commit", "o/r@deadbee"),
            ("environment", "env-2abc"),
        ] {
            let a = attribute(&keys(&[(kind, value)]));
            assert_eq!(a.subject, None, "{kind}:{value} became a subject");
        }
    }

    #[test]
    fn environment_groups_slack_subjects_without_owning_them() {
        let a = attribute(&keys(&[
            ("environment", "env-2ABC"),
            ("slack_thread", "C02/1721822400.001"),
        ]));
        // Slack owns it...
        assert_eq!(
            a.subject,
            Some(SubjectKey::slack_thread("C02/1721822400.001"))
        );
        // ...and the environment is what lets a second alert thread about the same
        // customer environment merge into it deterministically.
        assert_eq!(a.merge_key.as_deref(), Some("environment:env-2abc"));
    }

    #[test]
    fn a_github_subject_gets_no_merge_key() {
        // Merging by environment is a Slack-rank affordance. An issue is already the
        // strongest identity there is; grouping two issues because they mention one
        // environment would be exactly the over-collapse the ranking exists to stop.
        let a = attribute(&keys(&[("environment", "env-2abc"), ("issue", "o/r#412")]));
        assert_eq!(a.subject, Some(SubjectKey::issue("o/r", 412)));
        assert_eq!(a.merge_key, None);
    }

    #[test]
    fn ci_on_a_default_branch_with_no_pr_is_unattributed() {
        // Deliberate: it still shows in the unattributed lane, but it does not mint
        // a subject keyed on `main`.
        let a = attribute(&keys(&[("repo", "o/r"), ("branch", "o/r@main")]));
        assert_eq!(a.subject, None);
        assert!(is_default_branch("branch", "o/r@main"));
        assert!(!is_default_branch("branch", "o/r@fix-pool"));
    }

    #[test]
    fn attribution_is_stable_across_key_order() {
        // Replay determinism: the same keys in any order must attribute identically,
        // or a journalled handler would diverge on retry.
        let forward = attribute(&keys(&[("issue", "o/r#412"), ("pr", "o/r#987")]));
        let reverse = attribute(&keys(&[("pr", "o/r#987"), ("issue", "o/r#412")]));
        assert_eq!(forward, reverse);
    }

    #[test]
    fn malformed_values_do_not_become_subjects() {
        for (kind, value) in [
            ("issue", "o/r#"),
            ("issue", "#412"),
            ("issue", "o/r"),
            ("pr", "not-a-number"),
            ("slack_thread", "no-slash"),
        ] {
            assert_eq!(
                attribute(&keys(&[(kind, value)])).subject,
                None,
                "{kind}:{value}"
            );
        }
    }
}
