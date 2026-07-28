//! Following "the fix is over there" out of a pull request's discussion.
//!
//! The most consequential thing a reviewer can write on a PR is that the change is not where
//! the change should be: *"this was already fixed in a1b2c3d"*, *"the real fix is in
//! restatedev/sdk-typescript#218"*, *"superseded by #4412"*. Judging the PR without reading
//! what those point at produces the confident wrong answer — a diff that looks like it doesn't
//! resolve the issue, judged as if nothing else exists, when the work has already landed
//! somewhere the judge never looked.
//!
//! So references are followed **out of the discussion and into the code index**, and what comes
//! back — the commit message, and for a merge the pull request title it carries — is handed to
//! the judge alongside the diff.
//!
//! # Why the cue gate
//!
//! Comments are full of `#`-numbers and hex strings that are not claims about where the fix
//! lives: a stack frame, a container digest, an issue mentioned in passing. Extracting every
//! one of them would fill the prompt with commits nobody meant. So a reference is only followed
//! when the segment it appears in *says* it is the fix — "fixed in", "landed in", "superseded
//! by", "duplicate of". That is precisely the case this exists for, and everything else is
//! left alone.
//!
//! # Registry-only
//!
//! Resolution reads the local code index and nothing else. A reference into an unindexed repo
//! comes back unresolved and is reported as such rather than fetched: an unresolved reference
//! is a fact the judge needs ("someone says the fix is elsewhere and we can't see it"), and
//! quietly dropping it reads as "there is nothing there".

use std::sync::Arc;

use crate::store::{RegistryCommit, Store};

/// Phrases that turn a bare reference into a claim about where the fix is.
///
/// Lowercase, matched as substrings against a segment of a comment. Deliberately includes the
/// negative-for-this-PR forms ("superseded by", "duplicate of") as well as the positive ones:
/// both say the work is somewhere other than the diff being judged.
const FIX_CUES: &[&str] = &[
    "fixed in",
    "fixed by",
    "fix is in",
    "fix landed",
    "fix for this",
    "landed in",
    "landed as",
    "merged in",
    "merged as",
    "already in",
    "already fixed",
    "already merged",
    "addressed in",
    "addressed by",
    "handled in",
    "handled by",
    "resolved in",
    "resolved by",
    "implemented in",
    "implemented by",
    "shipped in",
    "closed by",
    "done in",
    "covered by",
    "superseded by",
    "supersedes",
    "replaced by",
    "replaces",
    "duplicate of",
    "moved to",
    "ported to",
    "reverted in",
    "reverted by",
    "follow-up in",
    "follow up in",
    "backported to",
];

/// What a comment pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// A commit, by full sha or an abbreviation of one. `repo` is set only when the reference
    /// named one (`owner/repo@sha`, or a commit URL).
    Commit { repo: Option<String>, sha: String },
    /// A pull request or issue. The repo is always known: a bare `#N` means the PR's own repo.
    Pull { repo: String, number: u64 },
}

/// A reference plus what the index could say about it.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// How the discussion wrote it, for the judge to match against the comment it came from.
    pub cited_as: String,
    pub commit: Option<RegistryCommit>,
}

/// Characters of a commit message kept. Long enough for a merge commit's PR body, short enough
/// that a squashed thirty-commit branch doesn't crowd out the diff.
const MAX_MESSAGE_CHARS: usize = 1_200;
/// References followed per PR. A discussion naming more than a handful of "real fixes" is not
/// pointing at a fix, and each one costs prompt room the diff needs.
const MAX_REFERENCES: usize = 4;

/// Pull the fix-is-elsewhere references out of some discussion text.
///
/// `home_repo` is the repo the discussion lives in, which is what a bare `#N` means. `skip`
/// are the numbers that are *this* conversation — the PR itself and the issue it is judged
/// against — because "fixes #412" pointing at the issue under judgment is the ordinary case,
/// not a claim that the work happened somewhere else.
pub fn extract(text: &str, home_repo: &str, skip: &[u64]) -> Vec<Reference> {
    let mut out: Vec<Reference> = Vec::new();
    for segment in segments(text) {
        let lower = segment.to_ascii_lowercase();
        if !FIX_CUES.iter().any(|cue| lower.contains(cue)) {
            continue;
        }
        for r in references_in(segment, home_repo, skip) {
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// Resolve references against the code index, keeping the unresolved ones.
///
/// An unresolved reference is kept on purpose: "a reviewer says the fix is in
/// `otherorg/thing#7` and that repo is not indexed" is information, and it is the opposite of
/// the information "there is nothing there".
pub fn resolve(store: &Arc<Store>, refs: &[Reference]) -> Vec<Resolved> {
    let mut out = Vec::new();
    for r in refs.iter().take(MAX_REFERENCES) {
        let (cited_as, commit) = match r {
            Reference::Commit { repo, sha } => {
                let cited = match repo {
                    Some(repo) => format!("{repo}@{sha}"),
                    None => sha.clone(),
                };
                (
                    cited,
                    store
                        .commit_by_sha(repo.as_deref(), sha)
                        .unwrap_or_default(),
                )
            }
            Reference::Pull { repo, number } => (
                format!("{repo}#{number}"),
                store.commit_for_pull(repo, *number).unwrap_or_default(),
            ),
        };
        out.push(Resolved { cited_as, commit });
    }
    out
}

/// Lay the followed references out for the judge.
///
/// Returns `None` when the discussion pointed nowhere, so the caller can leave the section out
/// of the prompt entirely rather than print a header over "(none)".
pub fn render(resolved: &[Resolved]) -> Option<String> {
    if resolved.is_empty() {
        return None;
    }
    let mut out = String::new();
    for r in resolved {
        match &r.commit {
            Some(c) => {
                out.push_str(&format!(
                    "\n--- cited as `{}` → {}@{} ---\n{} by {}\n",
                    r.cited_as,
                    c.full_name,
                    &c.sha[..c.sha.len().min(8)],
                    &c.committed_at[..c.committed_at.len().min(10)],
                    c.author.as_deref().unwrap_or("unknown"),
                ));
                out.push_str(&format!(
                    "COMMIT MESSAGE:\n{}\n",
                    truncate(c.message.trim(), MAX_MESSAGE_CHARS)
                ));
                if let Some(summary) = &c.summary {
                    out.push_str(&format!("WHAT IT CHANGED: {summary}\n"));
                }
            }
            // Said plainly, because the judge must not read silence as absence.
            None => out.push_str(&format!(
                "\n--- cited as `{}` → not in the code index (that repository is not indexed, \
                 or the commit is older than the indexed history). Treat it as work that may \
                 exist and that you cannot read. ---\n",
                r.cited_as
            )),
        }
    }
    Some(out)
}

// ---- extraction ----------------------------------------------------------------

/// Split text into the units a cue is judged over: lines, then sentences.
///
/// Sentence-level rather than whole-comment, because a long comment that mentions a fix landing
/// elsewhere in one line and cites four unrelated issues in others should follow one reference,
/// not five.
fn segments(text: &str) -> Vec<&str> {
    text.lines()
        .flat_map(|line| line.split(". "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

fn references_in(segment: &str, home_repo: &str, skip: &[u64]) -> Vec<Reference> {
    let mut out = Vec::new();
    // Tokenized on whitespace and the punctuation that surrounds a reference in prose, so
    // `(#4412),` and `<https://…/commit/abc>` reduce to the reference itself.
    for token in segment.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '(' | ')' | '[' | ']' | '<' | '>' | ',' | '"' | '\'' | '`'
            )
    }) {
        let token = token.trim_end_matches(['.', ':', ';', '!', '?']);
        if token.is_empty() {
            continue;
        }
        if let Some(r) = parse_url(token).or_else(|| parse_shorthand(token, home_repo)) {
            // The conversation referring to itself is not a claim that the work is elsewhere.
            if let Reference::Pull { repo, number } = &r {
                if repo == home_repo && skip.contains(number) {
                    continue;
                }
            }
            out.push(r);
        }
    }
    out
}

/// `https://github.com/owner/repo/commit/<sha>` and `…/pull/<n>` (or `/issues/<n>`).
fn parse_url(token: &str) -> Option<Reference> {
    let rest = token
        .strip_prefix("https://github.com/")
        .or_else(|| token.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let kind = parts.next()?;
    let tail = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let full = format!("{owner}/{repo}");
    match kind {
        "commit" | "commits" => {
            let sha = tail.split('#').next().unwrap_or(tail);
            is_sha(sha).then(|| Reference::Commit {
                repo: Some(full),
                sha: sha.to_ascii_lowercase(),
            })
        }
        "pull" | "issues" => tail
            .split('#')
            .next()
            .unwrap_or(tail)
            .parse::<u64>()
            .ok()
            .map(|number| Reference::Pull { repo: full, number }),
        _ => None,
    }
}

/// `owner/repo@sha`, `owner/repo#123`, `#123`, and a bare sha.
fn parse_shorthand(token: &str, home_repo: &str) -> Option<Reference> {
    if let Some((repo, sha)) = token.split_once('@') {
        if is_repo(repo) && is_sha(sha) {
            return Some(Reference::Commit {
                repo: Some(repo.to_string()),
                sha: sha.to_ascii_lowercase(),
            });
        }
    }
    if let Some((repo, number)) = token.split_once('#') {
        let number = number.parse::<u64>().ok()?;
        if repo.is_empty() {
            return Some(Reference::Pull {
                repo: home_repo.to_string(),
                number,
            });
        }
        return is_repo(repo).then(|| Reference::Pull {
            repo: repo.to_string(),
            number,
        });
    }
    is_sha(token).then(|| Reference::Commit {
        repo: None,
        sha: token.to_ascii_lowercase(),
    })
}

fn is_repo(s: &str) -> bool {
    let mut parts = s.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !owner.is_empty()
        && !repo.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// A plausible abbreviated sha.
///
/// Seven is git's own default abbreviation and the shortest anyone writes. The all-digits
/// rejection is what keeps a version number or a timestamp out: `20260127` is hex-clean and is
/// never a commit anyone is citing.
fn is_sha(s: &str) -> bool {
    let len = s.chars().count();
    (7..=40).contains(&len)
        && s.chars().all(|c| c.is_ascii_hexdigit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
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

    const HOME: &str = "restatedev/restate";

    #[test]
    fn a_fix_in_another_repo_is_followed() {
        let refs = extract(
            "This is the wrong layer — the real fix landed in restatedev/sdk-typescript#218.",
            HOME,
            &[],
        );
        assert_eq!(
            refs,
            vec![Reference::Pull {
                repo: "restatedev/sdk-typescript".into(),
                number: 218
            }]
        );
    }

    #[test]
    fn a_fix_in_another_commit_is_followed() {
        for body in [
            "already fixed in a1b2c3d4",
            "This was fixed by restatedev/restate@a1b2c3d4",
            "superseded by https://github.com/restatedev/restate/commit/a1b2c3d4",
        ] {
            let refs = extract(body, HOME, &[]);
            assert_eq!(refs.len(), 1, "{body:?} → {refs:?}");
            let Reference::Commit { sha, .. } = &refs[0] else {
                panic!("{body:?} did not yield a commit: {refs:?}");
            };
            assert_eq!(sha, "a1b2c3d4");
        }
    }

    /// The whole reason for the cue gate: comments are full of references that are not claims
    /// about where the fix is.
    #[test]
    fn references_without_a_fix_cue_are_left_alone() {
        for body in [
            "See #4412 for background on why the pool is bounded this way.",
            "Stack trace mentions deadbeef1 somewhere in the frame table.",
            "Related to restatedev/restate#900, but not the same thing.",
        ] {
            assert!(
                extract(body, HOME, &[]).is_empty(),
                "{body:?} should not be followed"
            );
        }
    }

    /// "Fixes #412" pointing at the issue under judgment is the ordinary case, not a claim
    /// that the work is somewhere else.
    #[test]
    fn the_conversations_own_numbers_are_skipped() {
        let refs = extract("Fixed by #412, which is this issue.", HOME, &[412]);
        assert!(refs.is_empty(), "{refs:?}");
        // …but another number in the same repo still counts.
        let refs = extract("Actually fixed by #999.", HOME, &[412]);
        assert_eq!(
            refs,
            vec![Reference::Pull {
                repo: HOME.into(),
                number: 999
            }]
        );
    }

    #[test]
    fn one_cue_line_does_not_drag_in_the_rest_of_the_comment() {
        let body = "I looked at #100 and #200 first.\n\
                    The actual fix landed in #300.\n\
                    Compare with #400 for the older approach.";
        assert_eq!(
            extract(body, HOME, &[]),
            vec![Reference::Pull {
                repo: HOME.into(),
                number: 300
            }]
        );
    }

    #[test]
    fn version_numbers_and_dates_are_not_shas() {
        assert!(!is_sha("20260127"));
        assert!(!is_sha("1234567"));
        assert!(!is_sha("abc"));
        assert!(is_sha("a1b2c3d"));
        assert!(is_sha("deadbeef"));
    }

    #[test]
    fn punctuation_around_a_reference_is_stripped() {
        assert_eq!(
            extract("Superseded by (#4412).", HOME, &[]),
            vec![Reference::Pull {
                repo: HOME.into(),
                number: 4412
            }]
        );
    }

    #[test]
    fn duplicates_across_comments_are_followed_once() {
        let body = "Fixed in #300.\nAs I said, fixed in #300.";
        assert_eq!(extract(body, HOME, &[]).len(), 1);
    }

    /// An unresolvable reference must reach the judge as an unresolvable reference.
    #[test]
    fn an_unresolved_reference_is_reported_not_dropped() {
        let rendered = render(&[Resolved {
            cited_as: "otherorg/thing#7".into(),
            commit: None,
        }])
        .expect("a reference was passed in");
        assert!(rendered.contains("otherorg/thing#7"));
        assert!(rendered.contains("not in the code index"));
    }

    #[test]
    fn a_resolved_reference_carries_its_commit_message() {
        let rendered = render(&[Resolved {
            cited_as: "#300".into(),
            commit: Some(RegistryCommit {
                full_name: HOME.into(),
                sha: "a1b2c3d4e5f6".into(),
                author: Some("alice".into()),
                committed_at: "2026-06-02T10:00:00Z".into(),
                message: "Drain the pool on terminal errors (#300)\n\nThe pool was never \
                          drained when a connection failed terminally."
                    .into(),
                url: None,
                summary: Some("Bounds pool growth under retries.".into()),
            }),
        }])
        .expect("a reference was passed in");
        assert!(rendered.contains("Drain the pool on terminal errors (#300)"));
        assert!(rendered.contains("never drained"), "the body is kept too");
        assert!(rendered.contains("WHAT IT CHANGED"));
        assert!(rendered.contains("a1b2c3d4"), "cited by sha: {rendered}");
    }

    #[test]
    fn nothing_referenced_renders_nothing() {
        assert!(render(&[]).is_none());
    }
}
