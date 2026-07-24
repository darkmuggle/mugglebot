//! Native macOS notifications, rule-gated by severity and quiet hours. Best-effort:
//! a failure to post a notification is logged, never fatal.

use chrono::{Local, NaiveTime};
use mac_notification_sys::Notification;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{debug, warn};

use crate::config::{self, Notifications};
use crate::signal::{Severity, Signal};

pub struct Notifier {
    min_severity: Severity,
    quiet_hours: Option<(NaiveTime, NaiveTime)>,
    critical_sound: bool,
    /// Highest severity already notified per thread — so we notify once per
    /// thread state change (a new thread or an escalation), not once per signal.
    notified: Mutex<HashMap<String, Severity>>,
}

impl Notifier {
    pub fn new(cfg: &Notifications, quiet_hours: Option<&str>) -> Self {
        Self {
            min_severity: config::severity_from_str(&cfg.min_severity),
            quiet_hours: quiet_hours.and_then(parse_quiet_hours),
            critical_sound: cfg.critical_sound,
            notified: Mutex::new(HashMap::new()),
        }
    }

    /// Notify for a thread-level change: fires only when a thread is first seen or
    /// escalates above the severity we last notified it at — deduped against the
    /// board, per the design ("once per thread state change, not per signal").
    pub fn maybe_notify_thread(&self, thread_id: &str, s: &Signal) {
        if s.severity < self.min_severity {
            return;
        }
        if s.severity < Severity::Critical && self.in_quiet_hours() {
            debug!(
                "suppressing {:?} notification during quiet hours",
                s.severity
            );
            return;
        }
        {
            let mut map = self.notified.lock().expect("notifier mutex poisoned");
            if let Some(prev) = map.get(thread_id) {
                if s.severity <= *prev {
                    return; // already notified this thread at an equal/higher severity
                }
            }
            map.insert(thread_id.to_string(), s.severity);
        }
        self.fire(s);
    }

    /// Forget a thread's "already notified" mark — called when you triage it
    /// (ack/snooze/resolve), so genuinely new activity on it can notify again
    /// rather than being deduped away.
    pub fn clear_notified(&self, thread_id: &str) {
        self.notified
            .lock()
            .expect("notifier mutex poisoned")
            .remove(thread_id);
    }

    /// Post a notification for a single signal unconditionally-of-thread (used when
    /// a signal couldn't be correlated into a thread).
    pub fn maybe_notify(&self, s: &Signal) {
        if s.severity < self.min_severity {
            return;
        }
        if s.severity < Severity::Critical && self.in_quiet_hours() {
            return;
        }
        self.fire(s);
    }

    fn fire(&self, s: &Signal) {
        let subtitle = s.source.as_str().to_ascii_uppercase();
        let body = s.body.as_deref().unwrap_or("");
        let mut notif = Notification::new();
        notif.title(&s.title).subtitle(&subtitle).message(body);
        if self.critical_sound && s.severity == Severity::Critical {
            notif.default_sound();
        }
        if let Err(e) = notif.send() {
            warn!("notification failed: {e}");
        }
    }

    /// Fire a Critical notification unconditionally (bypasses the severity floor
    /// and quiet hours) — for live-assist red-alert, the one case worth
    /// interrupting for. Best-effort.
    pub fn notify_critical(&self, title: &str, body: &str) {
        let mut notif = Notification::new();
        notif.title(title).subtitle("RED ALERT").message(body);
        if self.critical_sound {
            notif.default_sound();
        }
        if let Err(e) = notif.send() {
            warn!("critical notification failed: {e}");
        }
    }

    fn in_quiet_hours(&self) -> bool {
        let Some((start, end)) = self.quiet_hours else {
            return false;
        };
        let now = Local::now().time();
        if start <= end {
            now >= start && now < end
        } else {
            // Window wraps past midnight (e.g. 22:00-08:00).
            now >= start || now < end
        }
    }
}

/// Initialise the notification subsystem. On macOS, notifications need an owning
/// application bundle; we borrow the terminal's (or a sensible default). Failure
/// is non-fatal — notifications simply won't appear.
pub fn init() {
    let bundle = mac_notification_sys::get_bundle_identifier_or_default("com.apple.Terminal");
    if let Err(e) = mac_notification_sys::set_application(&bundle) {
        warn!(
            "could not set notification application ({bundle}): {e} — notifications may not appear"
        );
    }
}

fn parse_quiet_hours(s: &str) -> Option<(NaiveTime, NaiveTime)> {
    let (a, b) = s.split_once('-')?;
    let start = NaiveTime::parse_from_str(a.trim(), "%H:%M").ok()?;
    let end = NaiveTime::parse_from_str(b.trim(), "%H:%M").ok()?;
    Some((start, end))
}
