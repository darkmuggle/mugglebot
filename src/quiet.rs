//! Snoozing a whole source — "not Slack, not now".
//!
//! Snooze already exists per subject: one thread goes quiet until you re-enter it. This is
//! the other axis. When 204 of 291 ingested Slack signals are Grafana alerts, the thing you
//! want to silence is not a thread, it is *Slack*.
//!
//! # Why it has to cover what arrives next, not just what is there
//!
//! Snoozing the Slack subjects currently on the board would be useless on its own: the Slack
//! watcher polls every 30 seconds and alert channels refill it immediately. So a source
//! snooze does two things — it snoozes what is already there, and it keeps arriving work from
//! that source off the board until it expires. Only the first half is visible when you press
//! the button, and only the second half is what makes it worth pressing.
//!
//! # Critical still gets through
//!
//! This is the guard the whole feature turns on. Silencing Slack for two hours must not be
//! able to hide a firing incident, and Slack is exactly where a firing incident arrives. So a
//! snooze carries a **floor**: severities below it are suppressed and everything at or above
//! it lands anyway.
//!
//! The default floor is `Critical`, which is not a new rule — it is the one
//! [`crate::notify`] already applies to `general.quiet_hours`, where a Critical notification
//! fires regardless of the hour. A snooze that swallowed everything would be a mute, and a
//! mute you set at 4pm is a mute you have forgotten by 6pm.
//!
//! # Expiry is read, never scheduled
//!
//! A snooze is a row with an `until`, and everything asks "is `until` still in the future?".
//! There is no timer to arm, nothing to re-arm after a restart, and no way for a snooze to
//! outlive its expiry because the thing meant to cancel it did not run. The same reason
//! `snoozed_until` on a subject works that way.
//!
//! # Purely-that-source subjects only
//!
//! A pull request with Slack chatter on it is not a Slack notification. Auto-snoozing is
//! restricted to subjects whose signals *all* come from the snoozed source, so silencing
//! Slack never silences a PR someone happened to link.

use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::signal::{Severity, Source};
use crate::store::Store;
use crate::subject::Handled;

/// The longest a source may be snoozed for.
///
/// A cap rather than a preference: past a day this stops being "not now" and becomes a
/// disabled source, and `[sources.slack].enabled = false` is the honest way to say that.
const MAX: Duration = Duration::hours(24);

/// One source, quiet until a time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snooze {
    pub source: Source,
    pub until: DateTime<Utc>,
    /// Severities **below** this are suppressed; this and above land anyway.
    ///
    /// `Some(Critical)` by default, so an outage is never what a snooze hides. `None` means
    /// *everything* is suppressed, criticals included — which is what "snooze all Slack
    /// notifications" literally asks for, and it has to be askable: a safe default that
    /// cannot be overridden is a tool arguing with its operator. It is never the default, and
    /// the rail marks it, because silencing criticals is a different kind of decision.
    pub floor: Option<Severity>,
    pub set_at: DateTime<Utc>,
}

impl Snooze {
    pub fn active_at(&self, now: DateTime<Utc>) -> bool {
        self.until > now
    }

    /// Does this snooze suppress a signal of this severity?
    ///
    /// Pure, and the one function every surface routes through — the board's auto-snooze,
    /// the notifier, and the UI's explanation of what is being held back. Three
    /// reimplementations of "is this suppressed" is three chances to disagree about whether
    /// an incident was silenced.
    pub fn suppresses(&self, severity: Severity, now: DateTime<Utc>) -> bool {
        self.active_at(now) && self.floor.is_none_or(|floor| severity < floor)
    }

    /// Is this snooze silencing everything, criticals included?
    pub fn total(&self) -> bool {
        self.floor.is_none()
    }

    /// How the floor reads on a surface: `critical and above still land`, or the warning.
    pub fn floor_label(&self) -> String {
        match self.floor {
            Some(f) => format!("{f:?}").to_lowercase(),
            None => "everything".into(),
        }
    }

    /// How much longer, as `1h47m` / `12m` / `40s`.
    ///
    /// Rendered from the row rather than stored, and shown wherever the snooze is: a snooze
    /// with no visible remaining time is indistinguishable from a source that has gone
    /// silently broken, which is the failure this feature could most easily be mistaken for.
    pub fn remaining(&self, now: DateTime<Utc>) -> String {
        let secs = (self.until - now).num_seconds().max(0);
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        if h > 0 {
            format!("{h}h{m:02}m")
        } else if m > 0 {
            format!("{m}m")
        } else {
            format!("{s}s")
        }
    }
}

/// Reads and writes source snoozes.
pub struct Quiet {
    store: std::sync::Arc<Store>,
}

/// What snoozing a source did to the board on the way in.
#[derive(Debug, Clone, Serialize)]
pub struct Snoozed {
    pub snooze: Snooze,
    /// Subjects taken off the board, all of whose signals came from this source.
    pub subjects: usize,
}

impl Quiet {
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }

    /// Snooze a source for `how_long`, and take its purely-own subjects off the board.
    ///
    /// The existing subjects are snoozed **with the same expiry**, so they come back on their
    /// own when the source does. Snoozing them open-endedly would leave a pile of muted work
    /// behind after the source snooze lapsed — the operator said "not now", not "never".
    pub fn snooze(
        &self,
        source: Source,
        how_long: Duration,
        floor: Option<Severity>,
    ) -> Result<Snoozed> {
        if how_long <= Duration::zero() {
            bail!("a snooze needs a positive duration");
        }
        if how_long > MAX {
            bail!(
                "{} is longer than the {}h maximum — to stop a source entirely, disable it in \
                 config rather than snoozing it indefinitely",
                humanize(how_long),
                MAX.num_hours()
            );
        }
        let now = Utc::now();
        let snooze = Snooze {
            source,
            until: now + how_long,
            floor,
            set_at: now,
        };
        self.store.set_source_snooze(&snooze)?;
        let subjects = self
            .store
            .snooze_subjects_only_from(source, snooze.until, floor)?;
        if snooze.total() {
            tracing::warn!(
                "quiet: {} snoozed for {} — EVERYTHING held back, criticals included ({} \
                 subject(s) off the board)",
                source.as_str(),
                snooze.remaining(now),
                subjects
            );
        } else {
            tracing::info!(
                "quiet: {} snoozed for {} ({} subject(s) off the board; {} and above still land)",
                source.as_str(),
                snooze.remaining(now),
                subjects,
                snooze.floor_label()
            );
        }
        Ok(Snoozed { snooze, subjects })
    }

    /// End a snooze now.
    ///
    /// Deliberately does **not** un-snooze the subjects it took off the board. Their
    /// `snoozed_until` is already set to the same expiry, so they surface at the moment they
    /// would have anyway — and reopening them here would dump the whole backlog back onto the
    /// board at once, which is the opposite of what someone pressing "wake" wants: they want
    /// *new* Slack to reach them again.
    pub fn wake(&self, source: Source) -> Result<bool> {
        let cleared = self.store.clear_source_snooze(source)?;
        if cleared {
            tracing::info!("quiet: {} woken", source.as_str());
        }
        Ok(cleared)
    }

    /// The live snooze for a source, if any. Expired rows read as `None`.
    pub fn active(&self, source: Source) -> Option<Snooze> {
        self.all_active().into_iter().find(|s| s.source == source)
    }

    /// Every live snooze — what the rail renders.
    pub fn all_active(&self) -> Vec<Snooze> {
        let now = Utc::now();
        self.store
            .list_source_snoozes()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.active_at(now))
            .collect()
    }

    /// Would a signal from this source at this severity be held back?
    pub fn suppresses(&self, source: Source, severity: Severity) -> bool {
        let now = Utc::now();
        self.active(source)
            .is_some_and(|s| s.suppresses(severity, now))
    }

    /// The expiry to snooze a newly-attached subject until, if its source is quiet.
    ///
    /// `None` means "let it through": no snooze, expired, or a severity at or above the floor.
    pub fn until_for(&self, source: Source, severity: Severity) -> Option<DateTime<Utc>> {
        let now = Utc::now();
        self.active(source)
            .filter(|s| s.suppresses(severity, now))
            .map(|s| s.until)
    }
}

/// `2h`, `45m`, `90s` — for error messages about a duration the caller supplied.
fn humanize(d: Duration) -> String {
    let secs = d.num_seconds();
    if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// The state a subject is put into by a source snooze.
///
/// [`Handled::Snoozed`] rather than a new state: the board, the notifier, the correlation
/// engine and the root-cause guard all already refuse to act on a snoozed subject, and
/// inventing a fifth handled state would mean teaching every one of them about it — with the
/// ones that were missed becoming places the snooze silently did not apply.
pub const AS: Handled = Handled::Snoozed;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mins: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + mins * 60, 0).expect("valid timestamp")
    }

    fn snooze(floor: Option<Severity>) -> Snooze {
        Snooze {
            source: Source::Slack,
            until: at(120),
            floor,
            set_at: at(0),
        }
    }

    // ---- the guard ----

    /// The guard the whole feature turns on. Slack is where a firing incident arrives, so a
    /// two-hour Slack snooze that could hide one would be worse than the noise.
    #[test]
    fn a_critical_signal_is_never_suppressed_by_default() {
        let s = snooze(Some(Severity::Critical));
        assert!(!s.suppresses(Severity::Critical, at(1)));
        for quieter in [Severity::Info, Severity::Notice, Severity::Warning] {
            assert!(s.suppresses(quieter, at(1)), "{quieter:?} should be held");
        }
    }

    /// The literal reading of "snooze **all** slack notifications", and it has to be
    /// expressible: a safe default that cannot be overridden is a tool arguing with its
    /// operator. `None` means no floor at all.
    ///
    /// Nothing sorts above `Critical`, so this could not be said with a `Severity` alone —
    /// which is why the floor is an `Option` rather than a level.
    #[test]
    fn total_silence_is_askable_and_is_never_the_default() {
        let total = snooze(None);
        assert!(total.total());
        assert_eq!(total.floor_label(), "everything");
        for sev in [
            Severity::Info,
            Severity::Notice,
            Severity::Warning,
            Severity::Critical,
        ] {
            assert!(total.suppresses(sev, at(1)), "{sev:?} must be held");
        }
        let safe = snooze(Some(Severity::Critical));
        assert!(!safe.total());
        assert!(!safe.suppresses(Severity::Critical, at(1)));
        assert_eq!(safe.floor_label(), "critical");
    }

    /// Total silence still expires. That is the whole reason it is safe to offer: the
    /// difference between a snooze and a mute is that one of them ends.
    #[test]
    fn even_total_silence_expires() {
        let total = snooze(None);
        assert!(total.suppresses(Severity::Critical, at(119)));
        assert!(!total.suppresses(Severity::Critical, at(121)));
    }

    #[test]
    fn an_intermediate_floor_holds_back_only_what_is_below_it() {
        let s = snooze(Some(Severity::Warning));
        assert!(s.suppresses(Severity::Notice, at(1)));
        assert!(
            !s.suppresses(Severity::Warning, at(1)),
            "at the floor, so it lands"
        );
        assert!(!s.suppresses(Severity::Critical, at(1)));
    }

    #[test]
    fn a_lower_floor_lets_more_through() {
        let s = snooze(Some(Severity::Notice));
        assert!(s.suppresses(Severity::Info, at(1)));
        assert!(!s.suppresses(Severity::Notice, at(1)));
        assert!(!s.suppresses(Severity::Warning, at(1)));
    }

    // ---- expiry ----

    /// Expiry is read, not scheduled — so there is no timer that can fail to fire and leave a
    /// source silenced past its snooze.
    #[test]
    fn an_expired_snooze_suppresses_nothing() {
        let s = snooze(Some(Severity::Critical));
        assert!(s.suppresses(Severity::Warning, at(119)));
        assert!(
            !s.suppresses(Severity::Warning, at(120)),
            "exactly at expiry"
        );
        assert!(!s.suppresses(Severity::Warning, at(121)));
        assert!(!s.active_at(at(500)));
    }

    #[test]
    fn remaining_reads_as_a_countdown() {
        let s = snooze(Some(Severity::Critical));
        assert_eq!(s.remaining(at(0)), "2h00m");
        assert_eq!(s.remaining(at(13)), "1h47m");
        assert_eq!(s.remaining(at(108)), "12m");
        assert_eq!(s.remaining(at(119)), "1m");
        // Never negative: a snooze read a moment after expiry says "0s", not "-3s".
        assert_eq!(s.remaining(at(200)), "0s");
    }

    // ---- the store round trip ----

    fn quiet() -> (Quiet, std::sync::Arc<Store>) {
        let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
        (Quiet::new(store.clone()), store)
    }

    #[test]
    fn a_snooze_round_trips_and_wakes() {
        let (q, _) = quiet();
        assert!(q.active(Source::Slack).is_none());

        let out = q
            .snooze(Source::Slack, Duration::hours(2), Some(Severity::Critical))
            .unwrap();
        assert_eq!(out.snooze.source, Source::Slack);
        let live = q.active(Source::Slack).expect("snoozed");
        assert_eq!(live.floor, Some(Severity::Critical));
        assert!(q.suppresses(Source::Slack, Severity::Warning));
        assert!(!q.suppresses(Source::Slack, Severity::Critical));
        // One source at a time: snoozing Slack says nothing about GitHub.
        assert!(!q.suppresses(Source::GitHub, Severity::Warning));

        assert!(q.wake(Source::Slack).unwrap());
        assert!(q.active(Source::Slack).is_none());
        assert!(!q.suppresses(Source::Slack, Severity::Warning));
        // Waking something that is not snoozed is not an error, just nothing.
        assert!(!q.wake(Source::Slack).unwrap());
    }

    #[test]
    fn re_snoozing_replaces_rather_than_stacking() {
        let (q, _) = quiet();
        q.snooze(
            Source::Slack,
            Duration::minutes(30),
            Some(Severity::Critical),
        )
        .unwrap();
        let first = q.active(Source::Slack).unwrap().until;
        q.snooze(Source::Slack, Duration::hours(4), Some(Severity::Notice))
            .unwrap();
        let live = q.active(Source::Slack).unwrap();
        assert!(live.until > first, "the later snooze wins");
        assert_eq!(live.floor, Some(Severity::Notice));
        assert_eq!(q.all_active().len(), 1);
    }

    /// A cap rather than a preference: past a day this is a disabled source, and config is
    /// the honest way to say that.
    #[test]
    fn a_snooze_longer_than_a_day_is_refused_and_says_what_to_do_instead() {
        let (q, _) = quiet();
        let e = q
            .snooze(Source::Slack, Duration::hours(48), Some(Severity::Critical))
            .unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("24h maximum"), "{msg}");
        assert!(msg.contains("disable it in config"), "{msg}");
        assert!(q.active(Source::Slack).is_none(), "and nothing was written");
    }

    #[test]
    fn a_zero_or_negative_snooze_is_refused() {
        let (q, _) = quiet();
        assert!(q
            .snooze(Source::Slack, Duration::zero(), Some(Severity::Critical))
            .is_err());
        assert!(q
            .snooze(
                Source::Slack,
                Duration::minutes(-5),
                Some(Severity::Critical)
            )
            .is_err());
    }

    #[test]
    fn several_sources_can_be_quiet_at_once() {
        let (q, _) = quiet();
        q.snooze(Source::Slack, Duration::hours(1), Some(Severity::Critical))
            .unwrap();
        q.snooze(Source::GitHub, Duration::hours(2), Some(Severity::Warning))
            .unwrap();
        let live = q.all_active();
        assert_eq!(live.len(), 2);
        assert!(q.suppresses(Source::Slack, Severity::Warning));
        assert!(q.suppresses(Source::GitHub, Severity::Notice));
        assert!(
            !q.suppresses(Source::GitHub, Severity::Warning),
            "at its floor"
        );
    }

    /// Snooze Slack on a **copy** of the operator's real store and report exactly which of
    /// their subjects it takes. The four-shape unit test above proves the SQL; this proves it
    /// against real subjects, whose signal mixes are messier than any fixture.
    ///
    /// ```text
    /// cp data/mugglebot.sqlite /tmp/c.sqlite
    /// MUGGLEBOT_DB=/tmp/c.sqlite cargo test snoozes_the_real_board -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn snoozes_the_real_board() {
        let db = std::env::var("MUGGLEBOT_DB")
            .expect("point MUGGLEBOT_DB at a COPY, not the live store");
        assert!(
            !db.contains("data/mugglebot.sqlite"),
            "refusing to run against the live store — copy it first"
        );
        let store = std::sync::Arc::new(
            Store::open(std::path::Path::new(&db)).expect("open the store copy"),
        );
        let q = Quiet::new(store.clone());

        // Read with a plain connection: `Store::lock` is private and should stay that way —
        // a test is not a reason to open an encapsulation boundary.
        let read = rusqlite::Connection::open(&db).expect("second reader");
        let before: Vec<(String, String)> = {
            let mut stmt = read
                .prepare(
                    "SELECT key, handled FROM subjects WHERE handled IN ('open','seen') \
                     ORDER BY key",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(Result::ok)
                .collect()
        };
        println!("{} subject(s) on the board before\n", before.len());

        // Both floors, on the same real board, so the difference is visible rather than
        // argued about. `MUGGLEBOT_SNOOZE_ALL=1` to see the literal "all notifications".
        let floor = if std::env::var("MUGGLEBOT_SNOOZE_ALL").is_ok() {
            None
        } else {
            Some(Severity::Critical)
        };
        let out = q.snooze(Source::Slack, Duration::hours(2), floor).unwrap();
        println!(
            "snoozed slack for {} (floor: {}) — {} subject(s) taken off the board\n",
            out.snooze.remaining(Utc::now()),
            out.snooze.floor_label(),
            out.subjects
        );

        let mut stmt = read
            .prepare(
                "SELECT s.key, s.handled, substr(s.title,1,52), \
                        (SELECT group_concat(DISTINCT g.source) FROM signals g \
                         WHERE g.subject = s.key), \
                        (SELECT group_concat(DISTINCT g.severity) FROM signals g \
                         WHERE g.subject = s.key) \
                 FROM subjects s WHERE s.key IN (SELECT key FROM subjects) \
                   AND EXISTS (SELECT 1 FROM signals g WHERE g.subject = s.key) \
                 ORDER BY s.handled, s.key",
            )
            .unwrap();
        /// One board row as the diagnostic prints it: key, handled, title, sources, severities.
        type Row = (String, String, String, Option<String>, Option<String>);
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        let was_open: std::collections::BTreeSet<&str> =
            before.iter().map(|(k, _)| k.as_str()).collect();
        for (key, handled, title, sources, sevs) in &rows {
            if !was_open.contains(key.as_str()) {
                continue;
            }
            let mark = if handled == "snoozed" {
                "SNOOZED"
            } else {
                "kept   "
            };
            println!(
                "{mark} [{}] {}\n         severities: {}",
                sources.as_deref().unwrap_or("-"),
                title,
                sevs.as_deref().unwrap_or("-").replace('"', "")
            );
        }
    }

    #[test]
    fn until_for_gives_the_expiry_only_when_it_would_suppress() {
        let (q, _) = quiet();
        let out = q
            .snooze(Source::Slack, Duration::hours(2), Some(Severity::Critical))
            .unwrap();
        assert_eq!(
            q.until_for(Source::Slack, Severity::Warning),
            Some(out.snooze.until)
        );
        assert_eq!(q.until_for(Source::Slack, Severity::Critical), None);
        assert_eq!(q.until_for(Source::GitHub, Severity::Warning), None);
    }
}
