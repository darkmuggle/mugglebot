//! Agent sessions: a coding CLI, running **inside a checkout of the repo**, streamed to the UI.
//!
//! Distinct from the chat surface. Chat assembles context from the index and asks a model a
//! question; this checks the repository out and hands it to an agent that can read the actual
//! files, run commands, and follow its own leads. For "walk me through this codebase" the second
//! is the only one that can answer honestly — the index holds summaries, and a summary cannot be
//! grepped.
//!
//! # What each CLI actually supports
//!
//! Established by reading `--help` and probing, because the flags do not compose the way the
//! names suggest:
//!
//! | | Claude | Codex | Ollama |
//! |---|---|---|---|
//! | Runs in a directory | `--add-dir` + `cwd` | `-C <dir>` | — |
//! | Streamed events | `--output-format stream-json` | `--json` | — |
//! | Session identity | `--session-id <uuid>`, `--resume` | `thread_id`, `exec resume` | — |
//! | Subagent thinking | `--forward-subagent-text` | reasoning items | — |
//!
//! Two findings worth recording. **`--forward-subagent-text` requires `--print`** — it is
//! documented as "only works with --print and --output-format=stream-json" — so `-p` is not
//! replaced by agent mode, it is what agent mode streams *through*. And **`stream-json` requires
//! `--verbose`**, which the CLI enforces with a hard error rather than a warning.
//!
//! **Ollama has no agent mode at all.** `ollama run` is an interactive REPL: no working
//! directory, no tool use, no event stream, no session id. There is nothing to shell out to, and
//! pretending otherwise would produce a session that cannot read the repository it claims to be
//! in. It is refused with that explanation, and the local model remains available for the
//! index-context chat, which is a different and honest thing.

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::event::{AgentChunk, ChunkKind, Event};

/// Which CLI drives a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTool {
    Claude,
    Codex,
    /// Accepted so the refusal can explain itself rather than 400ing on an unknown name.
    Ollama,
}

impl AgentTool {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "anthropic" => Some(AgentTool::Claude),
            "codex" | "openai" | "chatgpt" => Some(AgentTool::Codex),
            "ollama" | "ollama_local" | "local" => Some(AgentTool::Ollama),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentTool::Claude => "claude",
            AgentTool::Codex => "codex",
            AgentTool::Ollama => "ollama",
        }
    }

    fn program(self) -> &'static str {
        match self {
            AgentTool::Claude => "claude",
            AgentTool::Codex => "codex",
            AgentTool::Ollama => "ollama",
        }
    }
}

/// What to run.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// Our id, and — for Claude — the CLI's session id too, so a resume addresses the same
    /// conversation. Codex mints its own `thread_id`, which arrives on the first event.
    pub id: String,
    pub repo: String,
    pub cwd: PathBuf,
    pub tool: AgentTool,
    pub prompt: String,
    /// Custom agent definitions, passed to `--agents`. JSON, per the CLI's own format.
    pub agents: Option<String>,
    /// Resume an existing conversation rather than starting one.
    pub resume: bool,
}

/// The command line for a session.
///
/// A pure function, separate from spawning, because it is the part most likely to be wrong and
/// the only part that can be tested without a subscription, a network, and a minute of wall
/// clock. Every flag here was verified against `--help` or a live probe.
pub fn command_line(spec: &SessionSpec) -> Result<(&'static str, Vec<String>)> {
    let mut args: Vec<String> = Vec::new();
    match spec.tool {
        AgentTool::Claude => {
            // `-p` is required, not optional: both `--output-format stream-json` and
            // `--forward-subagent-text` are documented as only working with it.
            args.push("-p".into());
            args.push(spec.prompt.clone());
            args.push("--output-format".into());
            args.push("stream-json".into());
            // Enforced by the CLI with a hard error, not a warning.
            args.push("--verbose".into());
            // The thinking the operator actually wants to watch.
            args.push("--forward-subagent-text".into());
            args.push("--include-partial-messages".into());
            if spec.resume {
                args.push("--resume".into());
                args.push(spec.id.clone());
            } else {
                args.push("--session-id".into());
                args.push(spec.id.clone());
            }
            // The checkout is the cwd, and named again so the agent may read outside its
            // starting directory only where we said.
            args.push("--add-dir".into());
            args.push(spec.cwd.display().to_string());
            if let Some(agents) = &spec.agents {
                args.push("--agents".into());
                args.push(agents.clone());
            }
        }
        AgentTool::Codex => {
            args.push("exec".into());
            if spec.resume {
                args.push("resume".into());
                args.push(spec.id.clone());
            }
            args.push("--json".into());
            args.push("-C".into());
            args.push(spec.cwd.display().to_string());
            args.push(spec.prompt.clone());
        }
        AgentTool::Ollama => {
            bail!(
                "Ollama has no agent mode: `ollama run` is an interactive REPL with no working \
                 directory, no tool use, and no event stream, so it cannot read the repository a \
                 session is supposed to be in. Use claude or codex for a repo session — the local \
                 model is still what answers the index-context chat."
            );
        }
    }
    Ok((spec.tool.program(), args))
}

/// Turn one line of a CLI's event stream into something the board can show.
///
/// Tolerant by design: two CLIs, two schemas, both evolving, and an unrecognized event is far
/// better dropped than treated as an error that ends the session. `None` means "nothing worth
/// showing", which covers init banners, rate-limit notices, and shapes added after this was
/// written.
pub fn parse_event(tool: AgentTool, line: &str) -> Option<AgentChunk> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let ty = v.get("type")?.as_str()?;
    match tool {
        AgentTool::Claude => match ty {
            // With `--include-partial-messages` the CLI wraps Anthropic's streaming events rather
            // than emitting whole assistant messages, so this is where the token-by-token
            // thinking actually arrives. Handling only `assistant` events drops the entire
            // stream — which it did, silently, until a live run showed nothing on the board while
            // the agent was plainly working.
            "stream_event" => {
                let ev = v.get("event")?;
                let sub = v
                    .get("parent_tool_use_id")
                    .and_then(|p| p.as_str())
                    .map(str::to_string);
                match ev.get("type")?.as_str()? {
                    // A tool call announces itself at block start; the name is the useful part.
                    "content_block_start" => {
                        let block = ev.get("content_block")?;
                        if block.get("type")?.as_str()? != "tool_use" {
                            // An opening text/thinking block is empty — the content follows as
                            // deltas, so emitting here would put a blank line on the board.
                            return None;
                        }
                        let name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        Some(AgentChunk::partial(ChunkKind::Tool, name, sub))
                    }
                    "content_block_delta" => {
                        let delta = ev.get("delta")?;
                        let (kind, text) = match delta.get("type")?.as_str()? {
                            "text_delta" => (ChunkKind::Text, delta.get("text")?.as_str()?),
                            "thinking_delta" => {
                                (ChunkKind::Thinking, delta.get("thinking")?.as_str()?)
                            }
                            _ => return None,
                        };
                        if text.is_empty() {
                            return None;
                        }
                        let mut chunk = AgentChunk::partial(kind, text.to_string(), sub);
                        // Marked so the client appends to the running block instead of starting a
                        // new line per token, which would render one word per row.
                        chunk.delta = true;
                        Some(chunk)
                    }
                    _ => None,
                }
            }
            // Deliberately ignored. Because we pass `--include-partial-messages`, every text,
            // thinking and tool block arrives twice: once as `stream_event` deltas while it is
            // being produced, and again in the assembled `assistant` message when it is done.
            // Handling both showed every answer twice in the transcript. The deltas win, because
            // they are the streaming this feature exists for.
            "assistant" | "user" => None,
            "result" => {
                let text = v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("(done)")
                    .to_string();
                let mut chunk = AgentChunk::partial(ChunkKind::Result, text, None);
                // Surfaced because these sessions are the one thing in MuggleBot that spends
                // money by design, and a number nobody sees is a number nobody weighs.
                chunk.cost_usd = v.get("total_cost_usd").and_then(|c| c.as_f64());
                Some(chunk)
            }
            _ => None,
        },
        AgentTool::Codex => match ty {
            "item.completed" => {
                let item = v.get("item")?;
                let kind = match item.get("type").and_then(|t| t.as_str())? {
                    "agent_message" => ChunkKind::Text,
                    "reasoning" => ChunkKind::Thinking,
                    "command_execution" | "file_change" | "mcp_tool_call" => ChunkKind::Tool,
                    _ => return None,
                };
                let text = item
                    .get("text")
                    .or_else(|| item.get("command"))
                    .or_else(|| item.get("summary"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("(no text)")
                    .to_string();
                Some(AgentChunk::partial(kind, text, None))
            }
            "turn.completed" => Some(AgentChunk::partial(
                ChunkKind::Result,
                "(turn complete)".into(),
                None,
            )),
            // Carries the id needed to resume, which the caller records.
            "thread.started" => {
                let id = v.get("thread_id")?.as_str()?.to_string();
                let mut chunk = AgentChunk::partial(ChunkKind::Started, id.clone(), None);
                chunk.native_session_id = Some(id);
                Some(chunk)
            }
            _ => None,
        },
        AgentTool::Ollama => None,
    }
}

/// A running session.
struct Live {
    repo: String,
    tool: AgentTool,
    /// Kills the child. Sessions outlive a request, so something has to be able to stop one.
    abort: tokio::task::AbortHandle,
}

/// Every agent session this process is running.
pub struct AgentSessions {
    live: Mutex<HashMap<String, Live>>,
    events: broadcast::Sender<Event>,
    checkouts: Arc<crate::checkout::CheckoutCache>,
    github: Option<crate::github::GithubClient>,
}

impl AgentSessions {
    pub fn new(
        events: broadcast::Sender<Event>,
        checkouts: Arc<crate::checkout::CheckoutCache>,
        github: Option<crate::github::GithubClient>,
    ) -> Self {
        Self {
            live: Mutex::new(HashMap::new()),
            events,
            checkouts,
            github,
        }
    }

    /// A registry for tests: no GitHub token, so `start` refuses before it can spawn anything.
    ///
    /// Exists so the tool-surface fixtures don't have to assemble a checkout cache and an event
    /// bus to construct a `Tools` — and so a test can never accidentally launch a real CLI.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        let (events, _rx) = broadcast::channel(8);
        Self {
            live: Mutex::new(HashMap::new()),
            events,
            checkouts: Arc::new(crate::checkout::CheckoutCache::new(
                std::env::temp_dir().join("mugglebot-agent-test"),
                None,
                64,
                0,
            )),
            github: None,
        }
    }

    /// Check the repo out and start an agent in it. Returns the session id.
    pub async fn start(
        &self,
        repo: &str,
        tool: AgentTool,
        prompt: &str,
        agents: Option<String>,
    ) -> Result<String> {
        let Some(gh) = &self.github else {
            bail!("a repo session needs a stored GitHub token to check the repository out");
        };
        // The checkout is the point: an agent that cannot read the files is just a chat.
        let (branch, size_kb) = gh.repo_checkout_info(repo).await?;
        let checkout = self.checkouts.ensure(repo, &branch, size_kb).await?;

        let spec = SessionSpec {
            id: uuid_v4(),
            repo: repo.to_string(),
            cwd: checkout.path.clone(),
            tool,
            prompt: prompt.to_string(),
            agents,
            resume: false,
        };
        // Fails here for Ollama, before a checkout is wasted on a session that cannot run.
        let (program, args) = command_line(&spec)?;
        info!(
            "agent session {}: {} in {}",
            spec.id,
            tool.as_str(),
            checkout.path.display()
        );

        let mut child = Command::new(program)
            .args(&args)
            .current_dir(&spec.cwd)
            // Closed rather than inherited: `codex exec` waits on stdin when it is a tty, and a
            // session that blocks forever reading nothing is indistinguishable from a hang.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!("launching `{program}` failed ({e}); is it on PATH and logged in?")
            })?;

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let events = self.events.clone();
        let id = spec.id.clone();
        let repo_owned = spec.repo.clone();

        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            // stderr is drained concurrently: a full pipe blocks the child, and the useful part
            // of a failed launch (not logged in, bad flag) arrives there rather than on stdout.
            let err_id = id.clone();
            let err_repo = repo_owned.clone();
            let err_events = events.clone();
            tokio::spawn(async move {
                let mut errs = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = errs.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    warn!("agent {err_id}: {line}");
                    let mut chunk = AgentChunk::partial(ChunkKind::Error, line, None);
                    chunk.session_id = err_id.clone();
                    chunk.repo = err_repo.clone();
                    let _ = err_events.send(Event::AgentChunk(Box::new(chunk)));
                }
            });

            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(mut chunk) = parse_event(tool, &line) {
                    chunk.session_id = id.clone();
                    chunk.repo = repo_owned.clone();
                    chunk.tool = tool.as_str().to_string();
                    let _ = events.send(Event::AgentChunk(Box::new(chunk)));
                } else {
                    debug!("agent {id}: unhandled event {}", truncate(&line, 200));
                }
            }
            match child.wait().await {
                Ok(status) if status.success() => debug!("agent {id}: finished"),
                Ok(status) => warn!("agent {id}: exited {status}"),
                Err(e) => warn!("agent {id}: wait failed: {e}"),
            }
            let mut done = AgentChunk::partial(ChunkKind::Exited, "session ended".into(), None);
            done.session_id = id.clone();
            done.repo = repo_owned;
            done.tool = tool.as_str().to_string();
            let _ = events.send(Event::AgentChunk(Box::new(done)));
        });

        self.live.lock().expect("sessions poisoned").insert(
            spec.id.clone(),
            Live {
                repo: spec.repo.clone(),
                tool,
                abort: task.abort_handle(),
            },
        );
        Ok(spec.id)
    }

    /// Stop a session.
    pub fn stop(&self, id: &str) -> bool {
        match self.live.lock().expect("sessions poisoned").remove(id) {
            Some(live) => {
                live.abort.abort();
                info!("agent session {id}: stopped ({})", live.tool.as_str());
                true
            }
            None => false,
        }
    }

    /// Sessions currently running, as `(id, repo, tool)`.
    pub fn list(&self) -> Vec<(String, String, String)> {
        self.live
            .lock()
            .expect("sessions poisoned")
            .iter()
            .map(|(id, l)| (id.clone(), l.repo.clone(), l.tool.as_str().to_string()))
            .collect()
    }
}

/// A v4 UUID, which is the shape `--session-id` requires.
///
/// Hand-rolled off the system RNG rather than adding a dependency for sixteen bytes.
fn uuid_v4() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
        );
        let v = h.finish().to_le_bytes();
        let n = chunk.len().min(8);
        chunk[..n].copy_from_slice(&v[..n]);
    }
    // Version 4, variant 1, as the format requires.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = |r: &[u8]| r.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&bytes[0..4]),
        h(&bytes[4..6]),
        h(&bytes[6..8]),
        h(&bytes[8..10]),
        h(&bytes[10..16])
    )
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

    fn spec(tool: AgentTool) -> SessionSpec {
        SessionSpec {
            id: "11111111-2222-3333-4444-555555555555".into(),
            repo: "o/r".into(),
            cwd: PathBuf::from("/tmp/checkout/o/r"),
            tool,
            prompt: "walk me through this".into(),
            agents: None,
            resume: false,
        }
    }

    /// The flags that the CLI *enforces*, verified against a live probe rather than assumed.
    ///
    /// `stream-json` without `--verbose` is a hard error ("When using --print,
    /// --output-format=stream-json requires --verbose"), and `--forward-subagent-text` is
    /// documented as only working with `--print`. Both were discovered by running it, and both
    /// would otherwise fail at the moment a user pressed the button.
    #[test]
    fn the_claude_line_carries_every_flag_the_cli_insists_on() {
        let (program, args) = command_line(&spec(AgentTool::Claude)).unwrap();
        assert_eq!(program, "claude");
        let joined = args.join(" ");
        assert!(
            args.contains(&"-p".to_string()),
            "stream-json needs --print"
        );
        assert!(joined.contains("--output-format stream-json"));
        assert!(
            args.contains(&"--verbose".to_string()),
            "stream-json without --verbose is refused outright"
        );
        assert!(args.contains(&"--forward-subagent-text".to_string()));
        assert!(joined.contains("--session-id 11111111-2222-3333-4444-555555555555"));
        // The checkout has to be reachable, or the agent is reasoning about a repo it can't read.
        assert!(joined.contains("--add-dir /tmp/checkout/o/r"));
        assert!(args.contains(&"walk me through this".to_string()));
    }

    #[test]
    fn resuming_addresses_the_same_conversation_rather_than_starting_one() {
        let mut s = spec(AgentTool::Claude);
        s.resume = true;
        let (_, args) = command_line(&s).unwrap();
        assert!(args.contains(&"--resume".to_string()));
        assert!(
            !args.contains(&"--session-id".to_string()),
            "--session-id on a resume would mint a second session"
        );

        let mut c = spec(AgentTool::Codex);
        c.resume = true;
        let (_, args) = command_line(&c).unwrap();
        // `exec resume <id>` — the subcommand, in that order.
        let i = args.iter().position(|a| a == "exec").unwrap();
        assert_eq!(args[i + 1], "resume");
        assert_eq!(args[i + 2], s.id);
    }

    #[test]
    fn the_codex_line_runs_in_the_checkout_and_streams_json() {
        let (program, args) = command_line(&spec(AgentTool::Codex)).unwrap();
        assert_eq!(program, "codex");
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        let i = args.iter().position(|a| a == "-C").unwrap();
        assert_eq!(args[i + 1], "/tmp/checkout/o/r");
        // The prompt is last, after the flags, or `-C` would swallow it.
        assert_eq!(args.last().unwrap(), "walk me through this");
    }

    /// Ollama is refused with the reason, before a checkout is spent on it.
    #[test]
    fn ollama_is_refused_and_says_why() {
        let err = command_line(&spec(AgentTool::Ollama)).expect_err("no agent mode");
        let msg = format!("{err:#}");
        assert!(msg.contains("no agent mode"), "{msg}");
        // Naming what is missing is what stops this reading as a bug to file.
        assert!(msg.contains("working directory"), "{msg}");
        assert!(msg.contains("claude or codex"), "{msg}");
    }

    #[test]
    fn agents_json_is_passed_through_when_given() {
        let mut s = spec(AgentTool::Claude);
        s.agents = Some(r#"{"reviewer":{"description":"x"}}"#.into());
        let (_, args) = command_line(&s).unwrap();
        let i = args.iter().position(|a| a == "--agents").unwrap();
        assert_eq!(args[i + 1], r#"{"reviewer":{"description":"x"}}"#);
    }

    /// The real event shapes, captured from a live run of each CLI.
    /// The assembled `assistant` message is ignored, because `--include-partial-messages` means
    /// every block arrives twice — once as deltas, once assembled. Handling both put every answer
    /// on the board twice, which a live run made obvious.
    #[test]
    fn the_assembled_message_is_ignored_so_nothing_appears_twice() {
        for line in [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OK"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"echo"}]}}"#,
        ] {
            assert!(
                parse_event(AgentTool::Claude, line).is_none(),
                "duplicate of a streamed block: {line}"
            );
        }
    }

    /// The shape `--include-partial-messages` actually produces.
    ///
    /// Captured from a live session: the CLI wraps Anthropic's streaming events rather than
    /// emitting whole assistant messages, so handling only `assistant` drops the entire stream.
    /// It did exactly that — the board stayed empty while the agent was plainly working.
    #[test]
    fn streamed_thinking_and_text_deltas_are_recognized() {
        let thinking = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me look at"}},"parent_tool_use_id":null}"#;
        let c = parse_event(AgentTool::Claude, thinking).expect("thinking delta");
        assert_eq!(c.kind, ChunkKind::Thinking);
        assert_eq!(c.text, "let me look at");
        assert!(
            c.delta,
            "a delta must be appended, not started on a new line"
        );

        let text = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"There are 3"}}}"#;
        let c = parse_event(AgentTool::Claude, text).expect("text delta");
        assert_eq!(c.kind, ChunkKind::Text);
        assert!(c.delta);

        // A tool call announces itself at block start, and is *not* a delta — it is its own line.
        let tool = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"Bash","input":{}}}}"#;
        let c = parse_event(AgentTool::Claude, tool).expect("tool start");
        assert_eq!(c.kind, ChunkKind::Tool);
        assert_eq!(c.text, "Bash");
        assert!(!c.delta);

        // An opening text or thinking block is empty — the content arrives as deltas — so
        // emitting on it would put a blank line on the board before every message.
        let opening = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#;
        assert!(parse_event(AgentTool::Claude, opening).is_none());

        // Subagent attribution survives the wrapper, which is the whole point of
        // --forward-subagent-text.
        let sub = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"checking"}},"parent_tool_use_id":"toolu_9"}"#;
        assert_eq!(
            parse_event(AgentTool::Claude, sub)
                .unwrap()
                .subagent_of
                .as_deref(),
            Some("toolu_9")
        );

        // The envelope shapes that carry nothing to show.
        for noise in [
            r#"{"type":"stream_event","event":{"type":"message_start","message":{}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{}}}"#,
            r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        ] {
            assert!(parse_event(AgentTool::Claude, noise).is_none(), "{noise}");
        }
    }

    #[test]
    fn a_claude_result_carries_its_cost() {
        let line =
            r#"{"type":"result","subtype":"success","result":"done","total_cost_usd":0.2515}"#;
        let c = parse_event(AgentTool::Claude, line).expect("result");
        assert_eq!(c.kind, ChunkKind::Result);
        assert_eq!(c.text, "done");
        // These sessions are the one thing here that spends money by design, and a cost nobody
        // sees is a cost nobody weighs.
        assert_eq!(c.cost_usd, Some(0.2515));
    }

    #[test]
    fn codex_items_and_its_thread_id_are_recognized() {
        let started = r#"{"type":"thread.started","thread_id":"019fa535-0583-7b41"}"#;
        let c = parse_event(AgentTool::Codex, started).expect("started");
        assert_eq!(c.kind, ChunkKind::Started);
        // Codex mints its own id; without capturing it there is nothing to resume.
        assert_eq!(c.native_session_id.as_deref(), Some("019fa535-0583-7b41"));

        let msg = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#;
        let c = parse_event(AgentTool::Codex, msg).expect("message");
        assert_eq!(c.kind, ChunkKind::Text);
        assert_eq!(c.text, "OK");

        let reasoning =
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"considering"}}"#;
        assert_eq!(
            parse_event(AgentTool::Codex, reasoning).unwrap().kind,
            ChunkKind::Thinking
        );

        let cmd = r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test"}}"#;
        let c = parse_event(AgentTool::Codex, cmd).expect("command");
        assert_eq!(c.kind, ChunkKind::Tool);
        assert_eq!(c.text, "cargo test");
    }

    /// Unknown and malformed lines are dropped, not fatal.
    ///
    /// Two CLIs with two evolving schemas: an event added next month must not end a session, and
    /// the init banner and rate-limit notices are noise the operator does not need.
    #[test]
    fn noise_and_future_events_are_ignored_rather_than_failing() {
        for line in [
            r#"{"type":"system","subtype":"init","tools":[]}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"something.invented.later","payload":1}"#,
            "not json at all",
            "",
            "{}",
        ] {
            assert!(
                parse_event(AgentTool::Claude, line).is_none(),
                "claude: {line}"
            );
            assert!(
                parse_event(AgentTool::Codex, line).is_none(),
                "codex: {line}"
            );
        }
        // An empty text block carries nothing to show and must not emit a blank line.
        let empty = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"   "}]}}"#;
        assert!(parse_event(AgentTool::Claude, empty).is_none());
    }

    #[test]
    fn a_generated_session_id_is_a_valid_uuid_v4() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36, "{id}");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        // Version and variant nibbles, which `--session-id` validates.
        assert!(parts[2].starts_with('4'), "version nibble: {id}");
        assert!(
            matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "variant nibble: {id}"
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(uuid_v4(), uuid_v4(), "two sessions must not collide");
    }

    #[test]
    fn tool_names_round_trip_from_what_a_ui_would_send() {
        assert_eq!(AgentTool::parse("claude"), Some(AgentTool::Claude));
        assert_eq!(AgentTool::parse("anthropic"), Some(AgentTool::Claude));
        assert_eq!(AgentTool::parse("Codex"), Some(AgentTool::Codex));
        assert_eq!(AgentTool::parse("openai"), Some(AgentTool::Codex));
        assert_eq!(AgentTool::parse("ollama_local"), Some(AgentTool::Ollama));
        assert_eq!(AgentTool::parse("gpt-9"), None);
        for t in [AgentTool::Claude, AgentTool::Codex, AgentTool::Ollama] {
            assert_eq!(AgentTool::parse(t.as_str()), Some(t));
        }
    }
}
