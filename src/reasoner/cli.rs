//! The subscription CLI bridge: shell out to `claude -p` (Claude Code headless)
//! or `codex exec` (Codex CLI). This is the genuine "local connection" — a local
//! process reasoning over your existing Max/Pro / ChatGPT login, no API key and
//! no metering. Text-only: the prompt (system + turns, role-tagged) goes in on
//! stdin, the model's reply comes out on stdout.
//!
//! **Session chat per topic.** When a request carries a `session` key, the Claude
//! bridge keeps a persistent conversation for it: the first call for a key opens
//! a session (`--session-id <uuid>`), later calls continue it (`--resume <uuid>`).
//! If the installed CLI doesn't support those flags, a call that fails with a
//! session flag is retried once as a plain `-p` — so reasoning never breaks, it
//! just loses continuity.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{CompletionRequest, Reasoner, Role};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Claude,
    Codex,
}

pub struct CliReasoner {
    kind: Kind,
    model: String,
    /// Session key → (uuid, opened-this-process). Only used for Claude.
    sessions: Mutex<HashMap<String, Session>>,
}

#[derive(Clone)]
struct Session {
    uuid: String,
    opened: bool,
}

impl CliReasoner {
    pub fn claude(model: String) -> Self {
        Self {
            kind: Kind::Claude,
            model,
            sessions: Mutex::new(HashMap::new()),
        }
    }
    pub fn codex(model: String) -> Self {
        Self {
            kind: Kind::Codex,
            model,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn bin(&self) -> &'static str {
        match self.kind {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }

    /// Reserve/resume the session for `key`, returning `(uuid, resume)`. Marks the
    /// session opened optimistically so a concurrent caller resumes rather than
    /// re-creating.
    fn reserve_session(&self, key: &str) -> (String, bool) {
        let mut map = self.sessions.lock().expect("cli sessions mutex poisoned");
        let entry = map.entry(key.to_string()).or_insert_with(|| Session {
            uuid: gen_uuid(key),
            opened: false,
        });
        let resume = entry.opened;
        entry.opened = true;
        (entry.uuid.clone(), resume)
    }

    /// Forget a session (after a failure) so the next call re-opens it.
    fn drop_session(&self, key: &str) {
        self.sessions
            .lock()
            .expect("cli sessions mutex poisoned")
            .remove(key);
    }

    fn base_args(&self) -> Vec<String> {
        match self.kind {
            Kind::Claude => vec!["-p".into(), "--model".into(), self.model.clone()],
            Kind::Codex => vec![
                "exec".into(),
                "--ephemeral".into(),
                "--color".into(),
                "never".into(),
                "--model".into(),
                self.model.clone(),
                "-".into(),
            ],
        }
    }

    async fn run(&self, args: &[String], prompt: &str) -> Result<String> {
        let mut child = Command::new(self.bin())
            .args(args)
            // Tilt sets RUST_LOG for MuggleBot. Codex also consumes that variable,
            // so inheriting it floods stderr with internal, ANSI-colored traces.
            .env_remove("RUST_LOG")
            .env_remove("RUST_LOG_STYLE")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("FORCE_COLOR")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning `{}` bridge", self.bin()))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("writing prompt to CLI bridge")?;
            stdin.shutdown().await.ok();
        }

        let out = child
            .wait_with_output()
            .await
            .with_context(|| format!("running `{}` bridge", self.bin()))?;
        if !out.status.success() {
            bail!(
                "`{}` bridge exited with {}: {}",
                self.bin(),
                out.status,
                concise_cli_error(self.bin(), &String::from_utf8_lossy(&out.stderr))
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

fn concise_cli_error(bin: &str, stderr: &str) -> String {
    let clean = strip_ansi(stderr);
    if bin == "codex"
        && (clean.contains("attempt to write a readonly database")
            || clean.contains("failed to open state db")
            || clean.contains("failed to initialize in-process app-server client")
                && clean.contains("Operation not permitted"))
    {
        return "Codex cannot access its login/state directory (~/.codex) from this process. \
                Restart MuggleBot from a regular terminal outside the Codex sandbox, then retry."
            .into();
    }

    let meaningful = clean
        .lines()
        .rev()
        .find(|line| {
            let line = line.trim();
            !line.is_empty()
                && line != "Reading prompt from stdin..."
                && line != "Reading additional input from stdin..."
        })
        .unwrap_or("no error details")
        .trim();
    meaningful.chars().take(600).collect()
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Flatten a request into a single prompt the CLI can consume on stdin.
fn flatten(req: &CompletionRequest) -> String {
    let mut out = String::new();
    if let Some(sys) = &req.system {
        out.push_str(sys);
        out.push_str("\n\n");
    }
    for m in &req.messages {
        match m.role {
            Role::User => out.push_str(&m.content),
            Role::Assistant => {
                out.push_str("[assistant] ");
                out.push_str(&m.content);
            }
            Role::System => {
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
        }
        out.push('\n');
    }
    out
}

#[async_trait]
impl Reasoner for CliReasoner {
    async fn complete(&self, req: &CompletionRequest) -> Result<String> {
        let prompt = flatten(req);

        // Sessions are a Claude feature here; Codex runs plain.
        let session_key = match self.kind {
            Kind::Claude => req.session.clone(),
            Kind::Codex => None,
        };

        let Some(key) = session_key else {
            return self.run(&self.base_args(), &prompt).await;
        };

        let (uuid, resume) = self.reserve_session(&key);
        let mut args = self.base_args();
        if resume {
            args.push("--resume".into());
        } else {
            args.push("--session-id".into());
        }
        args.push(uuid);

        match self.run(&args, &prompt).await {
            Ok(out) => Ok(out),
            Err(e) => {
                // The CLI may not support session flags, or the session was lost.
                // Drop it and retry once plain so reasoning still succeeds.
                tracing::debug!("cli session call failed ({e:#}); retrying without session");
                self.drop_session(&key);
                self.run(&self.base_args(), &prompt).await
            }
        }
    }
}

/// A v4-shaped UUID derived from the key + time + a counter — unique per process
/// run, and valid for `--session-id`.
fn gen_uuid(key: &str) -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let h1 = fnv(key.as_bytes()) ^ t;
    let h2 = fnv(&n.to_le_bytes()) ^ h1.rotate_left(17) ^ t.rotate_right(13);
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&h1.to_le_bytes());
    b[8..].copy_from_slice(&h2.to_le_bytes());
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC-4122 variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Is `bin` on `PATH`?
pub fn have(bin: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(bin);
        p.is_file() || cfg!(windows) && dir.join(format!("{bin}.exe")).is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_v4_shaped() {
        let u = gen_uuid("thr/123");
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[2].chars().next(), Some('4'), "version nibble");
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn reserve_then_resume() {
        let r = CliReasoner::claude("m".into());
        let (u1, resume1) = r.reserve_session("thr/1");
        assert!(!resume1, "first use opens the session");
        let (u2, resume2) = r.reserve_session("thr/1");
        assert!(resume2, "second use resumes");
        assert_eq!(u1, u2, "same key → same session uuid");
        r.drop_session("thr/1");
        let (_, resume3) = r.reserve_session("thr/1");
        assert!(!resume3, "after drop, re-open");
    }

    #[test]
    fn cli_error_strips_ansi_and_progress_noise() {
        let err = concise_cli_error(
            "codex",
            "\u{1b}[32mINFO\u{1b}[0m noisy\nReading prompt from stdin...\nError: model unavailable\n",
        );
        assert_eq!(err, "Error: model unavailable");
    }

    #[test]
    fn cli_error_explains_sandboxed_codex_state() {
        let err = concise_cli_error(
            "codex",
            "failed to open state db: attempt to write a readonly database",
        );
        assert!(err.contains("Restart MuggleBot from a regular terminal"));
        assert!(!err.contains("readonly database"));
    }
}
