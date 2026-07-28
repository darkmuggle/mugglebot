//! The shared tool surface.
//!
//! One implementation of every MuggleBot capability — read tools (the board,
//! subjects, timelines, search, alerts, health), correlation writes
//! (relate / split / attach-context / reanalyze), grounding (memory + context
//! CRUD and semantic recall), and live-assist (list / dismiss hints). Both the
//! MCP server and the built-in agent chat dispatch through here, so the two
//! reason over identical grounding with identical tools.
//!
//! Read tools are free; write tools carry `read_only = false` risk metadata so a
//! client (or the MCP gate) can treat them differently. Nothing here mutates a
//! production system — the writes are all to MuggleBot's own store.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

use crate::config::{self, Config};
use crate::context::ContextSourceKind;
use crate::correlation::{Analyst, ContextKind, RelationKind};
use crate::live::HintState;
use crate::memory::MemoryManager;
use crate::reasoner::{CompletionRequest, Reasoner};
use crate::signal::{Signal, SignalKind, Source};
use crate::store::{SignalFilter, Store};
use crate::subject::{Attributor, Handled, SubjectKey};

/// Truncate on a character boundary, marking that it happened.
pub fn truncate_for_prompt(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n… (diff truncated)")
}

pub struct Tools {
    pub store: Arc<Store>,
    /// Agent sessions — a coding CLI in a checkout of the repo. The only surface here that runs
    /// a subprocess with tool access, which is why starting one is always an explicit action.
    pub agents: Arc<crate::agent::AgentSessions>,
    /// Submits the expensive pipelines as Restate workflows.
    pub ingress: Arc<crate::restate::ingress::Ingress>,
    /// Ranks repos, components and commits against an issue, over the code index.
    pub scorer: Arc<crate::score::Scorer>,
    /// Write-only credential store: tools may set a secret and ask whether one is
    /// set; no tool returns a value.
    pub secrets: Arc<crate::secrets::Secrets>,
    pub attributor: Arc<Attributor>,
    pub analyst: Arc<Analyst>,
    pub memory: Arc<MemoryManager>,
    pub context: Arc<crate::context::ContextManager>,
    /// Heavy reasoner, for on-demand deep work like postmortem drafting.
    pub reasoner: Arc<dyn Reasoner>,
    pub config: Arc<Config>,
    /// Root-cause investigation over the repo index.
    pub investigator: Arc<crate::rootcause::Investigator>,
    pub repos: Arc<crate::repos::RepoIndex>,
    pub browser: Arc<crate::browser::BrowserDriver>,
    /// Reads and summarizes pull request diffs, for the one case object state has none yet.
    pub diffs: Arc<crate::prdiff::DiffReader>,
}

/// Pull the indexing invocations out of a raw Restate introspection result.
///
/// Reshaped rather than passed through: the panel wants "which repo, doing what, and is it
/// stuck", and `sys_invocation` answers that in a target string plus seven columns of
/// bookkeeping. Tolerant of the envelope shape (`{rows: [...]}` or a bare array) because this
/// is the one place MuggleBot reads Restate's SQL surface rather than its API.
fn index_invocations(raw: &Value) -> Value {
    let rows = raw
        .get("rows")
        .and_then(Value::as_array)
        .or_else(|| raw.as_array())
        .cloned()
        .unwrap_or_default();
    let out: Vec<Value> = rows
        .into_iter()
        .filter_map(|r| {
            let target = r.get("target").and_then(Value::as_str)?;
            // `RepoIndexer` is the per-repo crunching object; `RepoIndex` is the org-wide
            // card refresh. Both are "the index working", and an operator watching this
            // panel does not care which service name it is.
            if !target.starts_with("RepoIndexer/") && !target.starts_with("RepoIndex/") {
                return None;
            }
            // `Service/key/handler` — the key is the repo, and it can contain `/`.
            let rest = target.split_once('/').map(|x| x.1).unwrap_or("");
            let (key, handler) = match rest.rsplit_once('/') {
                Some((k, h)) => (k, h),
                None => (rest, ""),
            };
            Some(json!({
                "repo": key,
                "handler": handler,
                "status": r.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                "scope": r.get("scope"),
                "failure": r.get("completion_failure"),
                "created_at": r.get("created_at"),
                "completed_at": r.get("completed_at"),
            }))
        })
        .collect();
    json!(out)
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub schema: Value,
}

pub struct ResourceDef {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

impl Tools {
    /// Dispatch a tool call by name. Unknown names error.
    pub async fn call(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            // ---- read ----
            "list_signals" => self.list_signals(args),
            "get_signal" => self.get_signal(args),
            "list_subjects" => self.list_subjects(args),
            "get_subject" => self.get_subject(args),
            "timeline" => self.timeline(args),
            "search" => self.search(args),
            "list_alerts" => self.list_alerts(args),
            "list_browser_investigations" => self.list_browser_investigations(args),
            "get_root_cause" => self.get_root_cause(args),
            "list_repos" => Ok(json!(self.repos.list()?)),
            "list_issue_triage" => Ok(json!(self.store.list_issue_triage()?)),
            "get_issue_triage" => self.get_issue_triage(args),
            "list_pr_fixes" => Ok(json!(self
                .store
                .pr_fixes_for_issue(&req_str(args, "issue_key")?)?)),
            "source_health" => Ok(json!(self.store.source_health()?)),
            "list_workflows" => self.list_workflows(args).await,
            "list_dispatches" => Ok(json!({ "dispatches": match opt_str(args, "subject_key") {
                Some(key) => crate::dispatch::for_subject(&key),
                None => crate::dispatch::all(),
            } })),
            "list_unattributed" => Ok(json!(self.store.unattributed_signals(200)?)),
            "score_issue" => self.score_issue(args).await,
            "list_components" => Ok(json!(self
                .store
                .components_for_repo(&req_str(args, "repo")?)?)),
            "index_status" => self.index_status().await,
            "set_repo_kind" => {
                let repo = req_str(args, "repo")?;
                match opt_str(args, "kind") {
                    // An explicit kind is a human decision and is pinned against the crawl's
                    // name-matching guess.
                    Some(k) => {
                        let kind = crate::store::RepoKind::parse(&k).ok_or_else(|| {
                            anyhow!("kind must be one of code, example, docs (got '{k}')")
                        })?;
                        self.store.set_repo_kind(&repo, kind)?;
                        Ok(json!({ "repo": repo, "kind": kind.as_str(), "pinned": true }))
                    }
                    // Omitting it hands the repo back to the guess.
                    None => {
                        self.store.clear_repo_kind(&repo)?;
                        Ok(json!({ "repo": repo, "kind": null, "pinned": false }))
                    }
                }
            }
            "repo_index_detail" => self.repo_index_detail(args).await,
            "chat_context" => self.chat_context(args),
            "pr_diff" => self.pr_diff(args).await,
            "start_agent_session" => {
                let repo = req_str(args, "repo")?;
                let tool = opt_str(args, "tool").unwrap_or_else(|| "claude".into());
                let tool = crate::agent::AgentTool::parse(&tool).ok_or_else(|| {
                    anyhow!("tool must be claude, codex or ollama (got '{tool}')")
                })?;
                // Defaults to the same opening the chat context uses, so the button works without
                // the operator composing a prompt first.
                let prompt = opt_str(args, "prompt").unwrap_or_else(|| {
                    format!("Walk me through {repo}: what it does, how it is laid out, and where its risk is.")
                });
                let id = self
                    .agents
                    .start(&repo, tool, &prompt, opt_str(args, "agents"))
                    .await?;
                Ok(json!({ "session_id": id, "repo": repo, "tool": tool.as_str() }))
            }
            "stop_agent_session" => {
                let id = req_str(args, "session_id")?;
                Ok(json!({ "stopped": self.agents.stop(&id) }))
            }
            "list_agent_sessions" => Ok(json!(self
                .agents
                .list()
                .into_iter()
                .map(|(id, repo, tool)| json!({ "session_id": id, "repo": repo, "tool": tool }))
                .collect::<Vec<_>>())),
            "repo_deps" => {
                let repo = req_str(args, "repo")?;
                let (out, inbound) = self.store.repo_deps(&repo)?;
                Ok(json!({ "repo": repo, "depends_on": out, "depended_on_by": inbound }))
            }
            "merge" => self.merge(args).await,
            "reattribute" => self.reattribute(args).await,
            "resolve_gate" => self.resolve_gate(args).await,
            "get_explanation" => Ok(json!(self
                .store
                .explanations(&req_str(args, "subject_key")?)?)),
            "explain" => self.explain(args).await,
            "draft_postmortem" => self.draft_postmortem(args).await,
            "distill_memory" => self.distill_memory(args).await,
            // ---- correlation (write) ----
            "relate" => self.relate(args).await,
            "split_subject" => self.split_subject(args).await,
            "attach_context" => self.attach_context(args).await,
            "reanalyze" => self.reanalyze(args).await,
            "record_browser_investigation" => self.record_browser_investigation(args).await,
            "investigate_root_cause" => self.investigate_root_cause(args).await,
            "investigate_link" => self.investigate_link(args).await,
            "refresh_repo_index" => {
                let summarized = self.repos.sync().await?;
                Ok(json!({ "ok": true, "summarized": summarized }))
            }
            "retriage_issue" => self.retriage_issue(args).await,
            // ---- grounding ----
            "search_memory" => self.search_memory(args).await,
            "search_context" => self.search_context(args).await,
            "list_memories" => Ok(json!(self.memory.list()?)),
            "get_memory" => Ok(json!(self.memory.get(req_str(args, "id")?.as_str())?)),
            "put_memory" => self.put_memory(args).await,
            "edit_memory" => self.edit_memory(args).await,
            "tag_memory" => self.tag_memory(args).await,
            "delete_memory" => {
                self.memory.delete(req_str(args, "id")?.as_str())?;
                Ok(json!({ "ok": true }))
            }
            "list_context" => Ok(json!(self.context.list()?)),
            "get_context" => Ok(json!(self.context.get(req_str(args, "id")?.as_str())?)),
            "add_context" => self.add_context(args).await,
            "tag_context" => self.tag_context(args).await,
            "list_tags" => Ok(json!(self.store.list_tags()?)),
            "edit_tag" => self.edit_tag(args).await,
            "delete_tag" => self.delete_tag(args),
            "merge_tags" => self.merge_tags(args),
            "set_subject_tags" => self.set_subject_tags(args).await,
            "refresh_context" => {
                let changed = self.context.refresh(req_str(args, "id")?.as_str()).await?;
                Ok(json!({ "changed": changed }))
            }
            "remove_context" => {
                self.context.remove(req_str(args, "id")?.as_str())?;
                Ok(json!({ "ok": true }))
            }
            // ---- live assist ----
            "list_hints" => Ok(json!(self
                .store
                .list_hints(opt_str(args, "subject_key").as_deref())?)),
            "dismiss_hint" => self.dismiss_hint(args).await,
            // ---- secrets (write-only) ----
            "list_secrets" => Ok(json!({
                "secrets": self.secrets.status(crate::secrets::KNOWN_SECRETS)?
            })),
            "set_secret" => {
                self.secrets
                    .set(&req_str(args, "name")?, &req_str(args, "value")?)?;
                Ok(json!({ "ok": true }))
            }
            "delete_secret" => {
                self.secrets.delete(&req_str(args, "name")?)?;
                Ok(json!({ "ok": true }))
            }
            other => bail!("unknown tool '{other}'"),
        }
    }

    // ---- read tools ---------------------------------------------------------

    fn list_signals(&self, args: &Value) -> Result<Value> {
        let filter = SignalFilter {
            source: opt_str(args, "source").and_then(|s| Source::parse(&s)),
            since: opt_str(args, "since")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            min_severity: opt_str(args, "severity").map(|s| config::severity_from_str(&s)),
            upstream_gone: args.get("upstream_gone").and_then(|v| v.as_bool()),
            limit: args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize),
        };
        Ok(json!(self.store.list_signals(&filter)?))
    }

    fn get_signal(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        Ok(json!(self.store.get_signal(&id)?))
    }

    fn list_subjects(&self, args: &Value) -> Result<Value> {
        let active_only = args
            .get("active_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        Ok(json!(self.attributor.subject_views(active_only)?))
    }

    fn get_subject(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        Ok(json!(self.attributor.subject_view(&id)?))
    }

    fn timeline(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "subject_key")?;
        let events: Vec<Value> = self
            .store
            .signals_for_subject(&id)?
            .into_iter()
            .map(|s| {
                json!({
                    "signal_id": s.id,
                    "occurred_at": s.occurred_at.to_rfc3339(),
                    "source": s.source.as_str(),
                    "kind": s.kind,
                    "upstream_gone": s.upstream_gone,
                    "severity": s.severity,
                    "actor": s.actor,
                    "keys": s.keys,
                    "title": s.title,
                    "body": s.body,
                    "url": s.url,
                    "ci_outcome": s.raw.get("ci_outcome"),
                    "ci_log_url": s.raw.get("ci_log_url"),
                })
            })
            .collect();
        Ok(json!({ "subject_key": id, "events": events }))
    }

    fn search(&self, args: &Value) -> Result<Value> {
        let q = req_str(args, "query")?;
        Ok(json!(self.store.search_signals(&q, 50)?))
    }

    fn list_alerts(&self, args: &Value) -> Result<Value> {
        // Triage state is a property of the subject now, so "which alerts are
        // handled?" is answered by the subject each alert resolved to rather than by
        // a per-signal column.
        let handled_filter = opt_str(args, "handled").and_then(|s| Handled::parse(&s));
        let alerts: Vec<_> = self
            .store
            .list_signals(&SignalFilter {
                source: Some(Source::Slack),
                limit: Some(500),
                ..Default::default()
            })?
            .into_iter()
            .filter(|s| s.kind == SignalKind::Alert)
            .filter(|s| match handled_filter {
                None => true,
                Some(want) => s
                    .subject
                    .as_deref()
                    .and_then(|k| self.store.get_subject(k).ok().flatten())
                    .is_some_and(|subj| subj.handled == want),
            })
            .collect();
        Ok(json!(alerts))
    }

    fn list_browser_investigations(&self, args: &Value) -> Result<Value> {
        let subject_key = req_str(args, "subject_key")?;
        Ok(json!(self
            .store
            .browser_investigations_for_subject(&subject_key)?))
    }

    async fn record_browser_investigation(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let findings = req_str(args, "findings")?;
        if findings.trim().is_empty() {
            bail!("findings cannot be empty");
        }
        let investigation = self.store.complete_browser_investigation(&id, &findings)?;
        if let Some(subject_key) = investigation.subject_key.as_deref() {
            self.analyst.reanalyze(subject_key).await?;
        }
        Ok(json!(investigation))
    }

    /// Queue (or re-queue) a browser investigation of one link on a subject, so the
    /// operator can point MuggleBot at a dashboard the watcher didn't pick up.
    /// The worker picks it up and drives Chrome; this returns immediately.
    async fn investigate_link(&self, args: &Value) -> Result<Value> {
        let subject_key = req_str(args, "subject_key")?;
        let url = req_str(args, "url")?;
        if !self.browser.enabled() {
            bail!("browser control is disabled — set [browser].enabled = true");
        }
        // Anchor the investigation to a signal so its findings reach the subject the
        // same way an automatically-queued one does.
        let signals = self.store.signals_for_subject(&subject_key)?;
        let anchor = signals
            .first()
            .ok_or_else(|| anyhow!("subject {subject_key} has no signals to anchor to"))?;
        let context = anchor.body.as_deref().unwrap_or(&anchor.title);
        let queued = self.store.queue_browser_investigation(
            &anchor.id,
            &url,
            self.browser.brief(&url, context).as_str(),
        )?;
        Ok(json!(queued))
    }

    /// Triage for one issue, by `owner/repo#number` — or everything on a subject.
    fn get_issue_triage(&self, args: &Value) -> Result<Value> {
        if let Some(key) = opt_str(args, "issue_key") {
            return Ok(json!(self.store.get_issue_triage(&key)?));
        }
        let subject_key = opt_str(args, "subject_key")
            .ok_or_else(|| anyhow!("provide either `issue_key` or `subject_key`"))?;
        Ok(json!(self.store.issue_triage_for_subject(&subject_key)?))
    }

    fn get_root_cause(&self, args: &Value) -> Result<Value> {
        let subject_key = req_str(args, "subject_key")?;
        Ok(json!(self.investigator.get(&subject_key)?))
    }

    /// Run the root-cause investigation for a subject and return the report.
    /// Slow — it walks the GitHub search and commit APIs — so the UI kicks it off
    /// and reads the persisted report as it progresses.
    async fn investigate_root_cause(&self, args: &Value) -> Result<Value> {
        let subject_key = req_str(args, "subject_key")?;
        // Keyed `{subject}@{watermark}`: nothing new has arrived since the last
        // report means the same key, which Restate refuses — so the answer comes back
        // without a single model call.
        let watermark = self
            .store
            .signals_for_subject(&subject_key)?
            .last()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "empty".into());
        let key = format!("{subject_key}@{watermark}");
        let fresh = self
            .ingress
            .submit_workflow(
                "RootCause",
                &key,
                Some(crate::restate::workflows::root_cause::SCOPE),
            )
            .await?;
        Ok(json!({
            "submitted": fresh,
            "workflow": key,
            "note": if fresh {
                "investigating"
            } else {
                "already investigated at this watermark"
            },
        }))
    }

    /// Rank repos, components and commits against an issue.
    ///
    /// Takes either a `subject_key` (an issue on the board, whose text and repo are read
    /// from the store) or raw `text`. The subject form is the useful one: it supplies the
    /// origin repo, which is what lets the dependency graph propagate a score to the
    /// repository the symptom is *not* in.
    async fn score_issue(&self, args: &Value) -> Result<Value> {
        let (text, origin) = match opt_str(args, "subject_key") {
            Some(key) => {
                let Some(view) = self.attributor.subject_view(&key)? else {
                    bail!("no subject {key}");
                };
                // Title plus bodies: an issue's mechanism is usually in the body, and the
                // title alone is what makes a search return the whole repo.
                let mut text = view.subject.title.clone();
                for sig in view.signals.iter().take(20) {
                    if let Some(body) = &sig.body {
                        text.push('\n');
                        text.push_str(body);
                    }
                }
                if let Some(summary) = &view.subject.summary {
                    text.push('\n');
                    text.push_str(summary);
                }
                // The repo the issue was filed in, from its own key.
                let origin = SubjectKey::parse(&key)
                    .ok()
                    .and_then(|k| k.repo().map(str::to_string));
                (text, origin)
            }
            None => (req_str(args, "text")?, opt_str(args, "repo")),
        };
        Ok(json!(self.scorer.score(&text, origin.as_deref()).await?))
    }

    /// Collapse two subjects into one, as the `Merge` workflow.
    ///
    /// Multi-step and it must be exactly-once: re-pointing the signals, rewriting the
    /// edges and carrying the artifacts are separate writes, and a failure between them
    /// used to leave a half-merged pair.
    async fn merge(&self, args: &Value) -> Result<Value> {
        let keep = SubjectKey::parse(&req_str(args, "keep")?)?;
        let drop = SubjectKey::parse(&req_str(args, "drop")?)?;
        if keep == drop {
            bail!("a subject cannot be merged into itself");
        }
        for k in [&keep, &drop] {
            if self.attributor.subject_view(k.as_str())?.is_none() {
                bail!("no subject {k}");
            }
        }
        let key = crate::restate::workflows::rest::Merge::key(keep.as_str(), drop.as_str());
        let fresh = self.ingress.submit_workflow("Merge", &key, None).await?;
        Ok(json!({ "submitted": fresh, "workflow": key, "canonical": keep.as_str() }))
    }

    /// Override the ranked climb for one signal.
    ///
    /// `subject_key` absent means "this belongs to nothing" — pinned to the
    /// unattributed lane, which is a decision and not the same as never having been
    /// attributed. Either way the pin survives a re-ingest of the same event.
    async fn reattribute(&self, args: &Value) -> Result<Value> {
        let signal_id = req_str(args, "signal_id")?;
        let to = match opt_str(args, "subject_key") {
            Some(k) => Some(SubjectKey::parse(&k)?),
            None => None,
        };
        self.analyst.reattribute(&signal_id, to.as_ref()).await?;
        Ok(json!({
            "ok": true,
            "signal_id": signal_id,
            "subject_key": to.map(|k| k.into_string()),
        }))
    }

    /// Answer a pending human gate.
    ///
    /// Approval resolves the durable promise the blocked handler is awaiting;
    /// rejection fails that invocation with the reason recorded, rather than leaving it
    /// hanging. Both are audited by the invocation itself — which is the point of a
    /// promise over a dialog box.
    async fn resolve_gate(&self, args: &Value) -> Result<Value> {
        let invocation_id = req_str(args, "invocation_id")?;
        let approve = args
            .get("approve")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow!("`approve` (boolean) is required — a gate has no default"))?;
        let reason = opt_str(args, "reason");
        self.ingress
            .resolve_gate(&invocation_id, approve, reason.as_deref())
            .await?;
        Ok(json!({ "ok": true, "invocation_id": invocation_id, "approved": approve }))
    }

    /// Distil a subject and everything under it into something readable.
    ///
    /// Keyed on the subject's watermark, so asking twice about unchanged work is free
    /// — which matters because this is the one pass that deliberately reads
    /// *everything*: the events, the PR critiques and their review conversations, the
    /// root cause, the triage, the attached context.
    /// Explain a subject on the local model.
    ///
    /// `second_opinion: true` asks the cloud model instead. That flag is the *entire*
    /// interface between MuggleBot and a metered model outside the chat pane: without it,
    /// nothing here can reach one.
    async fn explain(&self, args: &Value) -> Result<Value> {
        let subject_key = req_str(args, "subject_key")?;
        if self.attributor.subject_view(&subject_key)?.is_none() {
            bail!("no subject {subject_key}");
        }
        let second_opinion = args
            .get("second_opinion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let watermark = self
            .store
            .signals_for_subject(&subject_key)?
            .last()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "empty".into());
        // Anything already-produced downstream of this subject changes the answer even
        // when no new signal has arrived, so the key folds in how many PR critiques
        // exist. Otherwise the first explanation of an issue would be pinned forever
        // while its attempts were still being judged.
        let critiques = self.store.pr_fixes_for_issue(&subject_key)?.len();
        let key = format!("{subject_key}@{watermark}+{critiques}");
        let (workflow, scope) = if second_opinion {
            (
                "SecondOpinion",
                crate::restate::workflows::explain::SecondOpinion::SCOPE,
            )
        } else {
            (
                "Explain",
                crate::restate::workflows::explain::Explain::SCOPE,
            )
        };
        let fresh = self
            .ingress
            .submit_workflow(workflow, &key, Some(scope))
            .await?;
        Ok(json!({
            "submitted": fresh,
            "workflow": format!("{workflow}/{key}"),
            "produced_by": if second_opinion {
                crate::store::EXPLAIN_CLOUD
            } else {
                crate::store::EXPLAIN_LOCAL
            },
            "note": if fresh {
                "explaining"
            } else {
                "nothing has changed since the last explanation"
            },
        }))
    }

    /// The code index's progress across every watched repo, plus what is being crunched
    /// right now.
    ///
    /// Two halves, because they answer different questions and only one of them lives in
    /// SQLite. The per-repo rows say *how much has been built* — durable, survives a Restate
    /// wipe. The invocation list says *what is happening this second* — which repo an indexer
    /// is inside, what is queued behind the one-at-a-time local model, what failed. A panel
    /// with only the first can't distinguish "stalled" from "working"; with only the second it
    /// can't tell you whether any of it is finished.
    async fn index_status(&self) -> Result<Value> {
        let repos = self.store.index_progress_all()?;
        // The trial of reading object state cross-key instead of re-deriving it from SQLite.
        //
        // Reported *alongside* the SQLite figures rather than replacing them, on purpose: this
        // is the step that decides whether the whole read path can move onto `state`, and the
        // only way to know is to have both accounts of the same facts and look at where they
        // disagree. `state_check` is that comparison, and it is what should be believed before
        // anything else is migrated.
        let state_check = self.state_progress_check(&repos).await;
        let totals = json!({
            "repos": repos.len(),
            // "Indexed" means carded, not complete: one component is the point at which
            // scoring can route to this repo at all.
            "repos_with_components": repos.iter().filter(|r| r.components > 0).count(),
            "repos_untouched": repos.iter().filter(|r| r.components == 0).count(),
            "components": repos.iter().map(|r| r.components).sum::<i64>(),
            "commits_cached": repos.iter().map(|r| r.commits_cached).sum::<i64>(),
            "commits_summarized": repos.iter().map(|r| r.commits_summarized).sum::<i64>(),
            "dep_edges": repos.iter().map(|r| r.depends_on).sum::<i64>(),
        });

        // Best-effort: the panel is still worth rendering when Restate is down, and the
        // durable half is the half that matters.
        let active = match self.ingress.invocations(None).await {
            Ok(v) => index_invocations(&v),
            Err(e) => {
                debug!("index_status: invocations unavailable: {e:#}");
                json!([])
            }
        };
        Ok(json!({
            "totals": totals,
            "repos": repos,
            "active": active,
            "state_check": state_check,
        }))
    }

    /// Read every `RepoIndexer`'s own progress from Restate's `state` table and compare it with
    /// the SQLite-derived figures.
    ///
    /// Two things are being measured. **Agreement**: does the object's account of its progress
    /// match a count of the rows it wrote? Any disagreement is either a stale publish or a bug,
    /// and either way it is the thing that would silently corrupt a board built on `state`.
    /// **Cost**: one HTTP round trip and a Datafusion scan, timed, at whatever scale the org
    /// happens to be — the number that decides if a per-repaint read is viable.
    async fn state_progress_check(&self, sqlite: &[crate::store::RepoIndexProgress]) -> Value {
        use crate::restate::state as st;
        let reader = st::StateReader::new(&self.config.restate);
        let started = std::time::Instant::now();
        let by_repo = match reader.service_state("RepoIndexer").await {
            Ok(v) => v,
            Err(e) => {
                return json!({ "available": false, "why": format!("{e:#}") });
            }
        };
        let elapsed_ms = started.elapsed().as_millis();

        let mut compared = 0usize;
        let mut disagreements: Vec<Value> = Vec::new();
        for row in sqlite {
            let Some(state) = by_repo.get(&row.full_name) else {
                // No state at all means this repo's indexer has never ticked. Not a
                // disagreement — there is nothing to disagree with.
                continue;
            };
            compared += 1;
            let mut diffs = serde_json::Map::new();
            let mut note = |field: &str, from_state: Option<i64>, from_sql: i64| {
                if let Some(v) = from_state {
                    if v != from_sql {
                        diffs.insert(field.to_string(), json!({ "state": v, "sqlite": from_sql }));
                    }
                }
            };
            note(
                "components",
                st::as_i64(state, "components"),
                row.components,
            );
            note(
                "commits_cached",
                st::as_i64(state, "commits_cached"),
                row.commits_cached,
            );
            note(
                "commits_summarized",
                st::as_i64(state, "commits_summarized"),
                row.commits_summarized,
            );
            note("dep_edges", st::as_i64(state, "dep_edges"), row.depends_on);
            if !diffs.is_empty() {
                disagreements.push(json!({ "repo": row.full_name, "fields": diffs }));
            }
        }

        json!({
            "available": true,
            "query_ms": elapsed_ms,
            "objects_in_state": by_repo.len(),
            "repos_compared": compared,
            // Truncated: a systematic disagreement shows up in the first few, and a hundred
            // identical entries would bury the count that matters.
            "disagreements": disagreements.iter().take(10).collect::<Vec<_>>(),
            "disagreement_count": disagreements.len(),
        })
    }

    /// A pull request's diff, summarized — from the pull request's own object state.
    ///
    /// **Read from the object first.** The diff is a fact about the PR, it is read far more
    /// often than it changes (from the PR's card, from the issue it attempts, and again after
    /// clicking in), and re-deriving it paid one GitHub call plus one model pass for an answer
    /// that had not moved. Analysis warms it in the background — see `SubjectOps::warm_diffs`
    /// — so the ordinary case is a single ingress read.
    ///
    /// A PR with nothing stored is still fetched inline, exactly as this used to work: the
    /// first open costs what it always cost, and it is the last time. What is fetched is
    /// then stored, trimmed to what is worth replicating.
    ///
    /// Accepts either a PR subject key (`owner/repo!987`) or an issue key, in which case every
    /// PR attempting that issue is returned — which is what makes the pane work from an issue
    /// card without the UI having to know which PRs are attached.
    async fn pr_diff(&self, args: &Value) -> Result<Value> {
        let key = req_str(args, "subject_key")?;
        let subject = SubjectKey::parse(&key).map_err(|e| anyhow!("{e}"))?;
        // An explicit re-read, for a PR that has moved without producing a signal — a
        // force-push notifies nobody, so the watermark cannot see it.
        let refresh = args
            .get("refresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Answer only from object state, never fetching. This is what lets the pane open
        // itself: a state read is cheap enough to do on render for every attempt on an
        // issue, and an API call plus a model pass is not. A PR with nothing stored is
        // simply absent from the answer, and the pane offers the read as a button.
        let stored_only = args
            .get("stored_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Which PRs to diff: the subject itself if it is one, otherwise its attempts.
        let targets: Vec<(String, i64)> = match subject.rank() {
            crate::subject::SubjectRank::PullRequest => match crate::prdiff::parse_pr_key(&key) {
                Some(t) => vec![t],
                None => bail!("{key} does not name a pull request"),
            },
            _ => self
                .store
                .pr_fixes_for_issue(&key)?
                .into_iter()
                .map(|f| (f.pr_repo, f.pr_number))
                .collect(),
        };
        if targets.is_empty() {
            return Ok(json!({ "subject_key": key, "diffs": [], "target_count": 0 }));
        }
        // How many PRs *could* be shown, so the pane can tell "nothing is stored yet" from
        // "there is nothing to show".
        let target_count = targets.len().min(crate::prdiff::MAX_DIFF_PRS);

        let mut diffs = Vec::new();
        for (repo, number) in targets.into_iter().take(crate::prdiff::MAX_DIFF_PRS) {
            let pr_key = crate::prdiff::pr_key(&repo, number);
            let stored = if refresh {
                None
            } else {
                // A failure to reach Restate is not a failure to show a diff: fall through
                // to the inline read rather than emptying the pane.
                match crate::prdiff::stored(&self.ingress, &repo, number).await {
                    Ok(s) => s,
                    Err(e) => {
                        debug!("pr_diff: reading stored diff for {pr_key} failed: {e:#}");
                        None
                    }
                }
            };
            // The review lives beside the diff and is read the same way. A diff with no
            // review yet renders as a diff — the pane says the review is missing rather than
            // implying an empty one means "no comments".
            // `refresh` means the operator asked for the change to be looked at again, so it
            // has to reach the review too. Keeping the stored one here is how a re-read came
            // back with a fresh diff and yesterday's verdict.
            let review = if refresh {
                None
            } else {
                match crate::prdiff::stored_review(&self.ingress, &repo, number).await {
                    Ok(r) => r,
                    Err(e) => {
                        debug!("pr_diff: reading the stored review for {pr_key} failed: {e:#}");
                        None
                    }
                }
            };
            if let Some(stored) = stored {
                // A stored diff with no review beside it: either the model failed to produce
                // one, or this diff predates reviews existing. Either way the workflow key is
                // spent, so nothing will ever fill it in — so fill it in here, on the read
                // that noticed. `stored_only` opts out: that call is on a render path and must
                // stay a state read.
                let mut review = review;
                if review.is_none() && !stored_only && stored.report.error.is_none() {
                    if let Some(fresh) = self.diffs.review(&repo, number, &stored.report).await {
                        let payload = crate::prdiff::StoredReview {
                            watermark: stored.watermark.clone(),
                            reviewed_at: chrono::Utc::now(),
                            review: fresh,
                        };
                        if let Err(e) = self
                            .ingress
                            .send_object("PullRequest", &pr_key, "put_review", None, &payload)
                            .await
                        {
                            debug!("pr_diff: storing the review for {pr_key} failed: {e:#}");
                        }
                        review = Some(payload);
                    }
                }
                diffs.push(json!({
                    "repo": repo,
                    "number": number,
                    "files": stored.report.files,
                    "file_count": stored.report.file_count,
                    "additions": stored.report.additions,
                    "deletions": stored.report.deletions,
                    "summary": stored.report.summary,
                    "truncated": stored.report.truncated,
                    "error": stored.report.error,
                    // Said out loud so the pane can offer a re-read: a stored diff is as old
                    // as the last activity on the PR, and a force-push produces no activity.
                    "stored": true,
                    "fetched_at": stored.fetched_at,
                    "review": review.as_ref().map(|r| &r.review),
                    "reviewed_at": review.as_ref().map(|r| r.reviewed_at),
                }));
                continue;
            }
            if stored_only {
                continue;
            }

            let report = self.diffs.read(&repo, number).await;
            let report = crate::prdiff::trim_for_state(report);
            // Store what was just read. `send`, not a call: the pane has its answer already,
            // and the write exists so the *next* reader doesn't pay for this again.
            let watermark = self
                .store
                .signals_for_subject(&pr_key)
                .ok()
                .and_then(|s| s.last().map(|s| s.id.clone()))
                .unwrap_or_else(|| "inline".into());
            let payload = crate::prdiff::StoredDiff {
                watermark: watermark.clone(),
                fetched_at: chrono::Utc::now(),
                report: report.clone(),
            };
            if let Err(e) = self
                .ingress
                .send_object("PullRequest", &pr_key, "put_diff", None, &payload)
                .await
            {
                debug!("pr_diff: storing the diff for {pr_key} failed: {e:#}");
            }
            // The review runs in the background rather than on this call. It is several model
            // passes over the patches — five minutes on an eighteen-file change, measured —
            // and a button that holds the page for five minutes is a hung button. The diff
            // comes back now; the pane polls object state for the verdict, and the dispatch
            // strip shows the work in flight meanwhile.
            self.submit_pr_review(&pr_key, &watermark, refresh).await;
            diffs.push(json!({
                "repo": repo,
                "number": number,
                "files": report.files,
                "file_count": report.file_count,
                "additions": report.additions,
                "deletions": report.deletions,
                "summary": report.summary,
                "truncated": report.truncated,
                "error": report.error,
                "stored": false,
                "fetched_at": payload.fetched_at,
                "review": review,
                "reviewed_at": review.as_ref().map(|_| payload.fetched_at),
            }));
        }
        Ok(json!({ "subject_key": key, "diffs": diffs, "target_count": target_count }))
    }

    /// Ask for a review of a pull request whose diff is already known.
    ///
    /// Keyed with a `#review` suffix on the watermark, so it is distinct from the diff-only key
    /// for the same activity — already spent by the time anybody notices a review is missing —
    /// while staying deterministic: pressing the button twice is still free.
    ///
    /// `force` is what an explicit RE-READ needs: without it the key for this watermark is
    /// already spent, Restate refuses the submission as a redo, and the operator gets the old
    /// verdict back with a fresh diff — which is exactly what happened the first time this ran.
    async fn submit_pr_review(&self, pr_key: &str, watermark: &str, force: bool) {
        let suffix = if force {
            // Per-second, so a double click is still free while a deliberate re-read is not.
            format!("#r{}", chrono::Utc::now().timestamp())
        } else {
            String::new()
        };
        let wf_key = format!("{pr_key}@{watermark}#review{suffix}");
        match self
            .ingress
            .submit_workflow(
                "PrDiff",
                &wf_key,
                Some(crate::restate::workflows::rest::PrDiff::SCOPE),
            )
            .await
        {
            Ok(true) => debug!("reviewing {pr_key}"),
            Ok(false) => debug!("{pr_key} is already being reviewed at this watermark"),
            Err(e) => debug!("submitting a review for {pr_key} failed: {e:#}"),
        }
    }

    /// Assemble a context block for a chat about a repo, or about one commit in it.
    ///
    /// Everything the index already knows, rendered for a model to read: the repo card, its
    /// component cards, its dependency edges, and recent commit summaries — or for a single
    /// commit, that commit with its message, files and summary, plus enough of the repo to place
    /// it.
    ///
    /// Assembled **deterministically from the store**, and that is the whole point. The chat
    /// then shells out to whichever CLI the operator picked, and a model asked to go and *find*
    /// this context would invent some. The index is already the answer to "what is in this
    /// repository"; this hands it over rather than re-deriving it.
    ///
    /// Returns the block plus a suggested opening question, so the chat pane can seed an input
    /// the operator edits rather than a prompt they have to compose.
    fn chat_context(&self, args: &Value) -> Result<Value> {
        let repo = req_str(args, "repo")?;
        let Some(entry) = self.store.get_repo(&repo)? else {
            bail!("{repo} is not in the repo index");
        };
        let sha = opt_str(args, "sha");
        let mut b = String::new();

        b.push_str(&format!("=== REPOSITORY {repo} ===\n"));
        if let Some(lang) = &entry.language {
            b.push_str(&format!("Language: {lang}\n"));
        }
        if let Some(kind) = entry.kind {
            b.push_str(&format!("Kind: {}\n", kind.as_str()));
        }
        if let Some(desc) = &entry.description {
            b.push_str(&format!("Description: {desc}\n"));
        }
        if let Some(summary) = &entry.summary {
            b.push_str(&format!("\nWhat it is:\n{summary}\n"));
        }

        match &sha {
            // One commit: the change is the subject, and the repo is context for it.
            Some(sha) => {
                let found = self
                    .store
                    .commit_summaries_for_repo(&repo, 500)?
                    .into_iter()
                    .find(|c| c.sha.starts_with(sha.as_str()));
                let Some(commit) = found else {
                    bail!("no indexed commit in {repo} starting {sha}");
                };
                b.push_str(&format!("\n=== COMMIT {} ===\n", commit.sha));
                if let Some(subject) = &commit.subject {
                    b.push_str(&format!("Subject: {subject}\n"));
                }
                if let Some(author) = &commit.author {
                    b.push_str(&format!("Author: {author}\n"));
                }
                if let Some(when) = &commit.committed_at {
                    b.push_str(&format!("Date: {when}\n"));
                }
                if let Some(url) = &commit.url {
                    b.push_str(&format!("URL: {url}\n"));
                }
                if !commit.components.is_empty() {
                    b.push_str(&format!(
                        "Components touched: {}\n",
                        commit.components.join(", ")
                    ));
                }
                b.push_str(&format!(
                    "\nWhat MuggleBot's local model made of it:\n{}\n",
                    commit.summary
                ));
            }
            // The whole repo: its components and edges are the map.
            None => {
                let components = self.store.components_for_repo(&repo)?;
                if !components.is_empty() {
                    b.push_str(&format!("\n=== COMPONENTS ({}) ===\n", components.len()));
                    for c in components.iter().take(60) {
                        b.push_str(&format!("- {}", c.path));
                        if let Some(p) = &c.purpose {
                            b.push_str(&format!(": {p}"));
                        }
                        if let Some(sym) = &c.symptoms {
                            b.push_str(&format!("\n    symptoms: {sym}"));
                        }
                        b.push('\n');
                    }
                }
                let (out, inbound) = self.store.repo_deps(&repo)?;
                if !out.is_empty() || !inbound.is_empty() {
                    b.push_str("\n=== DEPENDENCIES ===\n");
                    for e in &out {
                        b.push_str(&format!(
                            "- depends on {} (via `{}`)\n",
                            e.to_repo, e.dep_name
                        ));
                    }
                    for e in &inbound {
                        b.push_str(&format!(
                            "- used by {} (via `{}`)\n",
                            e.from_repo, e.dep_name
                        ));
                    }
                }
                let commits = self.store.commit_summaries_for_repo(&repo, 20)?;
                if !commits.is_empty() {
                    b.push_str(&format!("\n=== RECENT COMMITS ({}) ===\n", commits.len()));
                    for c in &commits {
                        b.push_str(&format!(
                            "- {} {}: {}\n",
                            &c.sha[..c.sha.len().min(8)],
                            c.committed_at
                                .as_deref()
                                .unwrap_or("")
                                .chars()
                                .take(10)
                                .collect::<String>(),
                            c.summary
                        ));
                    }
                }
                // Said explicitly, because a thin index is a different thing from a simple repo
                // and a model cannot tell them apart from an empty section.
                if components.is_empty() && commits.is_empty() {
                    b.push_str(
                        "\nNOTHING ELSE IS INDEXED for this repository yet — no component cards \
                         and no commit summaries. Do not infer that it is small or simple; the \
                         index has not read it.\n",
                    );
                }
            }
        }

        let opening = match &sha {
            Some(sha) => format!(
                "What does commit {} in {repo} actually change, and what could it have broken?",
                &sha[..sha.len().min(8)]
            ),
            None => format!(
                "Walk me through {repo}: what it does, how it is laid out, and where its risk is."
            ),
        };
        Ok(json!({
            "repo": repo,
            "sha": sha,
            "context": b,
            "opening": opening,
            // The chat pane prefixes the context and appends the question, so the operator edits
            // a question rather than a wall of dossier.
            "prompt": format!("{b}\n=== YOUR TASK ===\n{opening}"),
        }))
    }

    /// Everything the index holds about one repo: its card, its components, its dependency
    /// edges both ways, and the commit summaries it has actually written.
    ///
    /// The commit summaries are the part worth surfacing that nothing else does. A count of
    /// "40 commits summarized" is unfalsifiable from the outside; reading three of them tells
    /// you immediately whether the local model is describing behaviour or paraphrasing the
    /// commit message back at you.
    async fn repo_index_detail(&self, args: &Value) -> Result<Value> {
        let repo = req_str(args, "repo")?;
        let entry = self.store.get_repo(&repo)?;
        if entry.is_none() {
            bail!("{repo} is not in the repo index");
        }
        let (depends_on, depended_on_by) = self.store.repo_deps(&repo)?;
        let limit = args
            .get("commit_limit")
            .and_then(Value::as_u64)
            .unwrap_or(25)
            .clamp(1, 200) as usize;
        Ok(json!({
            "repo": repo,
            "entry": entry,
            "components": self.store.components_for_repo(&repo)?,
            "depends_on": depends_on,
            "depended_on_by": depended_on_by,
            "commit_summaries": self.store.commit_summaries_for_repo(&repo, limit)?,
            "history_back_to": self.store.oldest_commit_at(&repo)?,
        }))
    }

    /// In-flight and recent invocations, from Restate's own introspection.
    ///
    /// "Why is there no triage yet?" is a question about an invocation — queued behind
    /// a concurrency limit, retrying, or failed — and none of that is visible in
    /// MuggleBot's logs. Answering it without opening the Restate UI is the point.
    async fn list_workflows(&self, args: &Value) -> Result<Value> {
        let subject = opt_str(args, "subject_key");
        let rows = self.ingress.invocations(subject.as_deref()).await?;
        Ok(json!({ "invocations": rows }))
    }

    /// Re-read an assigned issue's code and re-propose.
    ///
    /// As a workflow, keyed `{issue}@{sha}`. Same code means the same key, which
    /// Restate refuses — so a redundant re-triage is free rather than a second pass
    /// over the local coder model. `force` bumps an attempt suffix, which is the one
    /// case that must bypass it: the operator explicitly asked for the work again.
    async fn retriage_issue(&self, args: &Value) -> Result<Value> {
        let issue_key = req_str(args, "issue_key")?;
        let Some(existing) = self.store.get_issue_triage(&issue_key)? else {
            bail!("no assigned issue {issue_key}");
        };
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        // The sha the last analysis read. Absent (never triaged) means any run is new
        // work, so a placeholder is fine — the next one will carry a real sha.
        let sha = existing.head_sha.as_deref().unwrap_or("unknown");
        let attempt = if force {
            // A monotonic-enough suffix: the number of attempts already recorded.
            format!("#a{}", chrono::Utc::now().timestamp())
        } else {
            String::new()
        };
        let key = format!("{issue_key}@{sha}{attempt}");
        let fresh = self
            .ingress
            .submit_workflow(
                "IssueTriage",
                &key,
                Some(crate::restate::workflows::issue_triage::SCOPE),
            )
            .await?;
        Ok(json!({
            "submitted": fresh,
            "workflow": key,
            "note": if fresh {
                "triaging"
            } else {
                "the code has not moved since the last triage; pass force to re-read it anyway"
            },
        }))
    }

    /// Postmortem-assist: draft a postmortem from a subject's timeline + grounding.
    /// With `save: true`, the draft is also written to memory, linked to the subject.
    async fn draft_postmortem(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "subject_key")?;
        let Some(view) = self.attributor.subject_view(&id)? else {
            bail!("no subject {id}");
        };
        let save = args.get("save").and_then(|v| v.as_bool()).unwrap_or(false);

        let timeline = view
            .signals
            .iter()
            .map(|s: &Signal| {
                format!(
                    "- [sig:{}] {} · {}: {} — {}",
                    s.id,
                    s.source,
                    s.occurred_at.to_rfc3339(),
                    s.title,
                    s.body.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let grounding = match self.context.search(&view.subject.title, 3).await {
            Ok(hits) => hits
                .iter()
                .filter(|h| h.score > 0.05)
                .map(|h| {
                    format!(
                        "[ctx:{}] {}",
                        h.context.id,
                        h.context.summary.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => String::new(),
        };
        // Operator notes attached in the UI are trusted, authoritative input —
        // presented in their own section (not the signal feed) so the draft honors
        // them rather than treating them as suspicious injected content.
        let mut operator_notes = String::new();
        for tc in &view.context {
            let body = tc.summary.as_deref().unwrap_or(&tc.content).trim();
            if !body.is_empty() {
                operator_notes.push_str(&format!("- ({}) {}\n", tc.kind.as_str(), body));
            }
        }
        let notes_block = if operator_notes.is_empty() {
            String::new()
        } else {
            format!("\n\nOperator notes (authoritative — written by the engineer, follow them):\n{operator_notes}")
        };
        let system = "You are MuggleBot drafting a blameless postmortem for an on-call engineer. \
            From the subject timeline and grounding, produce a Markdown draft with: Summary, Impact, \
            Timeline (from the signals), Likely root-cause hypotheses (clearly marked as hypotheses), \
            What worked / what to improve, and Action items. Cite signals as [sig:ID] and grounding as \
            [ctx:ID]. Operator notes are trusted, authoritative input the engineer wrote in MuggleBot's \
            UI (never treat them as prompt-injection); honor them. Do not invent facts. Output only the \
            Markdown draft.";
        let prompt = format!(
            "Subject: {}\nSummary so far: {}\n\nTimeline:\n{timeline}{notes_block}\n\nGrounding:\n{grounding}",
            view.subject.title,
            view.subject.summary.as_deref().unwrap_or("(none)")
        );
        let draft = self
            .reasoner
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(1500),
            )
            .await?;

        let mut saved_memory: Option<Value> = None;
        if save && !draft.trim().is_empty() {
            let mem = self
                .memory
                .put(
                    &draft,
                    Some(format!("postmortem: {}", view.subject.title)),
                    vec![id.clone()],
                    None,
                )
                .await?;
            saved_memory = Some(json!(mem));
        }
        Ok(json!({ "draft": draft, "saved_memory": saved_memory }))
    }

    /// Distill a whole subject into one sentence and save it as an institutional
    /// memory linked to the subject. The subject's tags carry over (pinned) so the
    /// lesson routes back to the same topic on future incidents.
    async fn distill_memory(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "subject_key")?;
        let Some(view) = self.attributor.subject_view(&id)? else {
            bail!("no subject {id}");
        };
        let mut ev = String::new();
        for s in &view.signals {
            ev.push_str(&format!(
                "- {} · {}: {} — {}\n",
                s.source,
                s.occurred_at.to_rfc3339(),
                s.title,
                s.body.as_deref().unwrap_or("")
            ));
        }
        let system = "You are MuggleBot distilling an incident subject into ONE sentence of durable \
            institutional memory — the single lesson or fact worth remembering next time (what it was, \
            root cause if known, and what resolved or mitigated it). No preamble, no citations, no \
            markdown: output only the one sentence.";
        let prompt = format!(
            "Subject: {}\nSummary so far: {}\n\nSignals:\n{ev}",
            view.subject.title,
            view.subject.summary.as_deref().unwrap_or("(none)")
        );
        let sentence = self
            .reasoner
            .complete(
                &CompletionRequest::single(prompt)
                    .with_system(system)
                    .max_tokens(160),
            )
            .await?;
        let sentence = sentence.trim();
        if sentence.is_empty() {
            bail!("reasoner returned an empty summary");
        }
        // Carry the subject's tags over as pinned tags when it has them, so the
        // memory routes to the same topic; otherwise let the auto-tagger fill them.
        let tags = (!view.subject.tags.is_empty()).then(|| view.subject.tags.clone());
        let mem = self
            .memory
            .put(sentence, Some(sentence.to_string()), vec![id.clone()], tags)
            .await?;
        Ok(json!(mem))
    }

    // ---- correlation writes -------------------------------------------------

    async fn relate(&self, args: &Value) -> Result<Value> {
        let a = req_str(args, "subject_a")?;
        let b = req_str(args, "subject_b")?;
        let kind = RelationKind::parse(&req_str(args, "kind")?)
            .ok_or_else(|| anyhow!("kind must be same|related|distinct"))?;
        let canonical = self.analyst.relate(&a, &b, kind).await?;
        Ok(json!({ "ok": true, "canonical_thread": canonical }))
    }

    async fn split_subject(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "subject_key")?;
        let signal_ids: Vec<String> = args
            .get("signal_ids")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if signal_ids.is_empty() {
            bail!("signal_ids must be a non-empty array");
        }
        let new_id = self.analyst.split_subject(&id, &signal_ids).await?;
        Ok(json!({ "ok": true, "new_thread": new_id }))
    }

    async fn attach_context(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "subject_key")?;
        let (kind, content) = if let Some(url) = opt_str(args, "url") {
            (ContextKind::Url, url)
        } else if let Some(text) = opt_str(args, "text") {
            (ContextKind::Text, text)
        } else {
            bail!("provide either `text` or `url`");
        };
        let tc = self
            .analyst
            .attach_subject_context(&id, kind, &content)
            .await?;
        Ok(json!(tc))
    }

    async fn reanalyze(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "subject_key")?;
        // Optional one-off model override: reconsider the subject on a chosen
        // provider/model without touching the daemon's configured reasoners.
        let reasoner = match (opt_str(args, "provider"), opt_str(args, "model")) {
            (Some(provider), Some(model)) => {
                let ollama_key = self.secrets.get_opt("ollama");
                Some(crate::reasoner::build(
                    crate::reasoner::provider_label(&provider),
                    &model,
                    &self.config.reasoner,
                    ollama_key,
                ))
            }
            _ => None,
        };
        // Recorded on the dispatch strip like the workflows are, even though this one is
        // synchronous: the operator pressed a button and a model pass started, and which
        // execution model carries it is not the question they are asking.
        crate::dispatch::running("Reanalyze", &id);
        match self.analyst.reanalyze_with(&id, reasoner).await {
            Ok(()) => crate::dispatch::done("Reanalyze", &id),
            Err(e) => {
                crate::dispatch::failed("Reanalyze", &id, format!("{e:#}"));
                return Err(e);
            }
        }
        Ok(json!({ "ok": true }))
    }

    // ---- grounding ----------------------------------------------------------

    async fn search_memory(&self, args: &Value) -> Result<Value> {
        let q = req_str(args, "query")?;
        let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        Ok(json!(self.memory.search(&q, k).await?))
    }

    async fn search_context(&self, args: &Value) -> Result<Value> {
        let q = req_str(args, "query")?;
        let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        Ok(json!(self.context.search(&q, k).await?))
    }

    async fn put_memory(&self, args: &Value) -> Result<Value> {
        let text = req_str(args, "text")?;
        let summary = opt_str(args, "summary");
        let links = str_array(args, "links");
        let tags = args.get("tags").map(|_| str_array(args, "tags"));
        Ok(json!(self.memory.put(&text, summary, links, tags).await?))
    }

    async fn tag_memory(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let tags = str_array(args, "tags");
        match self.memory.set_tags(&id, tags)? {
            Some(m) => Ok(json!(m)),
            None => bail!("no memory {id}"),
        }
    }

    async fn edit_memory(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let text = req_str(args, "text")?;
        let summary = opt_str(args, "summary");
        match self.memory.edit(&id, &text, summary).await? {
            Some(m) => Ok(json!(m)),
            None => bail!("no memory {id}"),
        }
    }

    async fn add_context(&self, args: &Value) -> Result<Value> {
        let (kind, location) = if let Some(url) = opt_str(args, "url") {
            (ContextSourceKind::Url, url)
        } else if let Some(path) = opt_str(args, "path") {
            (ContextSourceKind::File, path)
        } else {
            bail!("provide either `url` or `path`");
        };
        let credential = opt_str(args, "credential");
        let header = opt_str(args, "header");
        let refresh = opt_str(args, "refresh_interval");
        let tags = args.get("tags").map(|_| str_array(args, "tags"));
        let ctx = self
            .context
            .add(kind, &location, credential, header, refresh, tags)
            .await?;
        Ok(json!(ctx))
    }

    async fn tag_context(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let tags = str_array(args, "tags");
        Ok(json!(self.context.set_tags(&id, tags)?))
    }

    /// Set (pin) a subject's tags from a human edit on the board, then re-run its
    /// analysis so the corrected routing propagates — mirrors relation pins.
    async fn set_subject_tags(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "subject_key")?;
        let tags = crate::tags::normalize_tags(str_array(args, "tags"));
        for t in &tags {
            self.store.ensure_tag(t, "", chrono::Utc::now())?;
        }
        self.store.set_subject_tags(&id, &tags, true)?;
        self.analyst.reanalyze(&id).await?;
        Ok(json!({ "ok": true, "tags": tags }))
    }

    async fn edit_tag(&self, args: &Value) -> Result<Value> {
        let name = crate::tags::normalize_tag(&req_str(args, "name")?)
            .ok_or_else(|| anyhow!("invalid tag name"))?;
        let summary = req_str(args, "summary")?;
        self.store
            .set_tag_summary(&name, &summary, chrono::Utc::now())?;
        Ok(json!(self.store.get_tag(&name)?))
    }

    /// Remove a tag from the vocabulary and strip the label off all content that
    /// carried it, so the classifier no longer offers it and nothing keeps a
    /// dangling reference.
    fn delete_tag(&self, args: &Value) -> Result<Value> {
        let name = crate::tags::normalize_tag(&req_str(args, "name")?)
            .ok_or_else(|| anyhow!("invalid tag name"))?;
        let stripped = self.store.rewrite_tag_in_content(&name, None)?;
        self.store.delete_tag(&name)?;
        Ok(json!({ "ok": true, "stripped_from": stripped }))
    }

    /// Merge one tag into another (also serves rename when `into` is new):
    /// rewrite the label across all content, carry the source summary if the
    /// target has none, and drop the source from the vocabulary.
    fn merge_tags(&self, args: &Value) -> Result<Value> {
        let from = crate::tags::normalize_tag(&req_str(args, "from")?)
            .ok_or_else(|| anyhow!("invalid `from` tag"))?;
        let into = crate::tags::normalize_tag(&req_str(args, "into")?)
            .ok_or_else(|| anyhow!("invalid `into` tag"))?;
        if from == into {
            bail!("`from` and `into` are the same tag");
        }
        let now = chrono::Utc::now();
        // Ensure the target exists, carrying the source's summary if it has none.
        let carry = self
            .store
            .get_tag(&from)?
            .map(|t| t.summary)
            .unwrap_or_default();
        self.store.ensure_tag(&into, &carry, now)?;
        let moved = self.store.rewrite_tag_in_content(&from, Some(&into))?;
        self.store.delete_tag(&from)?;
        Ok(json!({ "ok": true, "into": into, "moved": moved }))
    }

    // ---- live assist --------------------------------------------------------

    async fn dismiss_hint(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        let false_positive = args
            .get("false_positive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let Some(hint) = self.store.get_hint(&id)? else {
            bail!("no hint {id}");
        };
        let state = if false_positive {
            HintState::FalsePositive
        } else {
            HintState::Dismissed
        };
        self.store.set_hint_state(&id, state)?;
        // A false-positive teaches memory not to re-raise the same thing.
        if false_positive {
            let text = format!(
                "False positive (do not re-flag): {}. Rationale was: {}",
                hint.text,
                hint.rationale.as_deref().unwrap_or("n/a")
            );
            let _ = self
                .memory
                .put(
                    &text,
                    Some("live-assist false positive".into()),
                    vec![hint.subject_key.clone()],
                    None,
                )
                .await;
        }
        Ok(json!({ "ok": true }))
    }

    // ---- resources ----------------------------------------------------------

    pub async fn read_resource(&self, uri: &str) -> Result<Value> {
        match uri {
            "board://current" => {
                // Cache stats ride along here rather than changing `source_health`'s
                // shape, which clients consume as a plain array.
                let (entries, hits) = self.store.completion_cache_stats()?;
                Ok(json!({
                    "signals": self.store.recent(200)?,
                    "subjects": self.attributor.subject_views(true)?,
                    "health": self.store.source_health()?,
                    "completion_cache": { "entries": entries, "hits": hits },
                }))
            }
            "config://redacted" => Ok(serde_json::to_value(&*self.config)?),
            "memory://" => Ok(json!(self.memory.list()?)),
            "context://" => Ok(json!(self.context.list()?)),
            "live://hints" => Ok(json!(self.store.list_hints(None)?)),
            other => bail!("unknown resource '{other}'"),
        }
    }
}

/// The tool catalog for MCP `tools/list` and the chat system prompt.
pub fn definitions() -> Vec<ToolDef> {
    // Small schema builders keep the list readable.
    fn obj(props: Value, required: &[&str]) -> Value {
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        })
    }
    let s = || json!({ "type": "string" });
    let none = || json!({ "type": "object", "properties": {}, "additionalProperties": false });

    vec![
        ToolDef { name: "list_signals", read_only: true,
            description: "The current board: recent signals, optionally filtered by source, since (RFC3339), minimum severity, or state.",
            schema: obj(json!({ "source": s(), "since": s(), "severity": s(), "state": s(), "limit": {"type":"integer"} }), &[]) },
        ToolDef { name: "get_signal", read_only: true,
            description: "Full detail for one signal, including deep-link and raw payload.",
            schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "list_subjects", read_only: true,
            description: "Correlated topics (subjects) as views with their signals, summary, severity, state, relation edges, and attached context.",
            schema: obj(json!({ "active_only": {"type":"boolean"} }), &[]) },
        ToolDef { name: "get_subject", read_only: true,
            description: "One subject view: signals + summary + timeline + relation graph + context.",
            schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "timeline", read_only: true,
            description: "Reconstructed, ordered event timeline for a subject.",
            schema: obj(json!({ "subject_key": s() }), &["subject_key"]) },
        ToolDef { name: "search", read_only: true,
            description: "Keyword search across ingested signals (title + body).",
            schema: obj(json!({ "query": s() }), &["query"]) },
        ToolDef { name: "list_alerts", read_only: true,
            description: "Signals from Slack alert channels, optionally filtered by state.",
            schema: obj(json!({ "state": s() }), &[]) },
        ToolDef { name: "list_browser_investigations", read_only: true,
            description: "Browser investigations of dashboard links on one subject, with status (pending/running/completed/failed) and the findings read off each page. MuggleBot drives the operator's signed-in Chrome read-only; it never mutates a dashboard.",
            schema: obj(json!({ "subject_key": s() }), &["subject_key"]) },
        ToolDef { name: "get_root_cause", read_only: true,
            description: "The stored root-cause report for a subject: the symptoms searched, repos routed to, and the ranked issue/PR/commit/code candidates with confidences and rationales. Null if none has been run. Candidates are hypotheses with citations, never confirmed causes.",
            schema: obj(json!({ "subject_key": s() }), &["subject_key"]) },
        ToolDef { name: "list_issue_triage", read_only: true,
            description: "Every issue assigned to you that MuggleBot has triaged: what the local coder model made of it after reading the repository's source, the candidate patch approaches it proposed, and the plain-English summary. Assigned issues appear here (and on the board) whether or not they ever produced a notification.",
            schema: none() },
        ToolDef { name: "get_issue_triage", read_only: true,
            description: "Triage for one assigned issue by `issue_key` (owner/repo#number), or every triaged issue on a subject via `subject_key`. Patches are proposed approaches with files, risk, and effort — never applied.",
            schema: obj(json!({ "issue_key": s(), "subject_key": s() }), &[]) },
        ToolDef { name: "list_pr_fixes", read_only: true,
            description: "Open pull requests that may already fix an assigned issue (`issue_key` = owner/repo#number) — often written by somebody else. Each carries what the PR actually implements (read from the diff), a skeptical critique of whether it really fixes the issue, other issues it would also resolve, and which model tier judged it.",
            schema: obj(json!({ "issue_key": s() }), &["issue_key"]) },
        ToolDef { name: "list_repos", read_only: true,
            description: "The repo index: every repository in the watched org with a purpose/symptom card derived by reading its CODE (layout, manifests, module names) rather than its README. This is the routing table that maps a symptom to the repos worth searching.",
            schema: none() },
        ToolDef { name: "draft_postmortem", read_only: false,
            description: "Draft a blameless postmortem from a subject's timeline + grounding. `save: true` also stores it to memory.",
            schema: obj(json!({ "subject_key": s(), "save": {"type":"boolean"} }), &["subject_key"]) },
        ToolDef { name: "source_health", read_only: true,
            description: "Per-watcher status: last poll, last success, current error, cursor.",
            schema: none() },
        ToolDef { name: "relate", read_only: false,
            description: "Pin a same|related|distinct edge between two subjects (associate, mark duplicate/merge, or dissociate). Triggers re-analysis; pins always win.",
            schema: obj(json!({ "subject_a": s(), "subject_b": s(), "kind": s() }), &["subject_a","subject_b","kind"]) },
        ToolDef { name: "split_subject", read_only: false,
            description: "Pull wrongly-grouped signals out of a subject into a new one, then re-analyze both.",
            schema: obj(json!({ "subject_key": s(), "signal_ids": {"type":"array","items": s()} }), &["subject_key","signal_ids"]) },
        ToolDef { name: "attach_context", read_only: false,
            description: "Attach ad-hoc grounding (free `text` or a `url`) to a subject; triggers re-analysis.",
            schema: obj(json!({ "subject_key": s(), "text": s(), "url": s() }), &["subject_key"]) },
        ToolDef { name: "reanalyze", read_only: false,
            description: "Force the LLM correlation pass to re-run for a subject. Optional `provider` (anthropic|openai|ollama|ollama_local) and `model` reconsider it on a chosen model for this run only.",
            schema: obj(json!({ "subject_key": s(), "provider": s(), "model": s() }), &["subject_key"]) },
        ToolDef { name: "record_browser_investigation", read_only: false,
            description: "Record findings for a browser investigation by hand (the manual path, when the browser worker can't reach Chrome). Writes only MuggleBot's local evidence store and re-analyzes the subject; it never changes the dashboard.",
            schema: obj(json!({ "id": s(), "findings": s() }), &["id","findings"]) },
        ToolDef { name: "investigate_root_cause", read_only: false,
            description: "Find what caused a subject: extract symptoms, route to repos via the README index, search issues/PRs, scan the commit log over the incident window, and rank the candidates — falling back to code search when nothing explains it. Returns hypotheses with citations, never a confirmed cause, and never runs on a handled (snoozed/resolved/acknowledged) subject. Slow; the report is persisted as it progresses.",
            schema: obj(json!({ "subject_key": s() }), &["subject_key"]) },
        ToolDef { name: "investigate_link", read_only: false,
            description: "Queue a read-only browser investigation of one URL on a subject — for a dashboard the watcher didn't pick up automatically. The worker drives the operator's signed-in Chrome (navigate + read only) and files the findings back to the subject.",
            schema: obj(json!({ "subject_key": s(), "url": s() }), &["subject_key","url"]) },
        ToolDef { name: "retriage_issue", read_only: false,
            description: "Re-run triage for an assigned issue (`issue_key` = owner/repo#number): re-pull the code, re-read it, and propose fresh patch approaches. Queued for the worker; returns immediately.",
            schema: obj(json!({ "issue_key": s() }), &["issue_key"]) },
        ToolDef { name: "refresh_repo_index", read_only: false,
            description: "Re-crawl the watched org's repositories, re-characterizing any whose code has moved since it was last read (keyed on the indexed commit, so an unchanged repo costs no model call).",
            schema: none() },
        ToolDef { name: "distill_memory", read_only: false,
            description: "Summarize a subject down to a single-sentence institutional-memory entry (linked to the subject) and save it. Returns the created memory.",
            schema: obj(json!({ "subject_key": s() }), &["subject_key"]) },
        ToolDef { name: "search_memory", read_only: true,
            description: "Semantic recall over the memory store.",
            schema: obj(json!({ "query": s(), "k": {"type":"integer"} }), &["query"]) },
        ToolDef { name: "search_context", read_only: true,
            description: "Semantic recall over the curated context library.",
            schema: obj(json!({ "query": s(), "k": {"type":"integer"} }), &["query"]) },
        ToolDef { name: "list_memories", read_only: true, description: "Browse memory entries.", schema: none() },
        ToolDef { name: "get_memory", read_only: true, description: "Get one memory entry.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "put_memory", read_only: false,
            description: "Create a memory entry (one fact + one-line summary), optionally linked to signal/subject ids. Optional `tags` (array) pin routing tags; omit them to auto-suggest tags from the fact.",
            schema: obj(json!({ "text": s(), "summary": s(), "links": {"type":"array","items": s()}, "tags": {"type":"array","items": s()} }), &["text"]) },
        ToolDef { name: "edit_memory", read_only: false, description: "Edit a memory entry (re-tags automatically unless tags are pinned).",
            schema: obj(json!({ "id": s(), "text": s(), "summary": s() }), &["id","text"]) },
        ToolDef { name: "tag_memory", read_only: false,
            description: "Set (pin) a memory entry's tags; registers any new tags in the vocabulary.",
            schema: obj(json!({ "id": s(), "tags": {"type":"array","items": s()} }), &["id","tags"]) },
        ToolDef { name: "delete_memory", read_only: false, description: "Delete a memory entry.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "list_context", read_only: true, description: "Browse the context library.", schema: none() },
        ToolDef { name: "get_context", read_only: true, description: "Get one context source.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "add_context", read_only: false,
            description: "Add a context source: a `url` (optionally `credential`/`header` for authed fetch) or a local `path`. Optional `tags` (array) pin categorical routing tags; omit them to have the ingest pipeline auto-suggest tags.",
            schema: obj(json!({ "url": s(), "path": s(), "credential": s(), "header": s(), "refresh_interval": s(), "tags": {"type":"array","items": s()} }), &[]) },
        ToolDef { name: "tag_context", read_only: false,
            description: "Set (pin) a context source's tags. Overwrites auto-suggested tags with the given list; registers any new tags in the vocabulary.",
            schema: obj(json!({ "id": s(), "tags": {"type":"array","items": s()} }), &["id","tags"]) },
        ToolDef { name: "list_tags", read_only: true,
            description: "The tag vocabulary: each tag with the short summary the classifier reads to decide which tags apply to an issue.",
            schema: none() },
        ToolDef { name: "edit_tag", read_only: false,
            description: "Set a tag's summary (the description used to route issues to this tag).",
            schema: obj(json!({ "name": s(), "summary": s() }), &["name","summary"]) },
        ToolDef { name: "delete_tag", read_only: false,
            description: "Remove a tag from the vocabulary and strip the label off all content that carried it.",
            schema: obj(json!({ "name": s() }), &["name"]) },
        ToolDef { name: "merge_tags", read_only: false,
            description: "Merge one tag into another (also renames when `into` is new): rewrites the label across all content and drops the source tag.",
            schema: obj(json!({ "from": s(), "into": s() }), &["from","into"]) },
        ToolDef { name: "set_subject_tags", read_only: false,
            description: "Set (pin) the tags on a subject/issue on the board and re-run its analysis so grounding re-routes. Pinned tags are not overwritten by the classifier.",
            schema: obj(json!({ "subject_key": s(), "tags": {"type":"array","items": s()} }), &["subject_key","tags"]) },
        ToolDef { name: "refresh_context", read_only: false, description: "Force an immediate re-fetch of a context source.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "remove_context", read_only: false, description: "Remove a context source.", schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "list_hints", read_only: true, description: "Active live-assist hints, suggestions, and flags, optionally scoped to a subject.",
            schema: obj(json!({ "subject_key": s() }), &[]) },
        ToolDef { name: "dismiss_hint", read_only: false,
            description: "Dismiss a hint/flag. `false_positive: true` feeds it back to memory so it isn't re-raised.",
            schema: obj(json!({ "id": s(), "false_positive": {"type":"boolean"} }), &["id"]) },
        // ---- secrets (write-only) ----
        ToolDef { name: "score_issue", read_only: true,
            description: "Rank which repo, component and commit an issue is likely about, over the code index. Pass a `subject_key` (preferred — it supplies the origin repo, so the dependency graph can point at the repository the symptom is not in) or raw `text` plus an optional `repo`. Each candidate carries the passes that found it and what they matched; they are ranked hypotheses, never a confirmed cause.",
            schema: obj(json!({ "subject_key": s(), "text": s(), "repo": s() }), &[]) },
        ToolDef { name: "set_repo_kind", read_only: false,
            description: "Tag what a repo is for: `code`, `example`, or `docs`. The crawl guesses from the name and topics (anything named example/demo/sample/template, or doc/docs/website), and a tag set here is pinned so the next crawl cannot overwrite it. Omit `kind` to drop the tag and hand the repo back to the guess.",
            schema: obj(json!({ "repo": s(), "kind": s() }), &["repo"]) },
        ToolDef { name: "start_agent_session", read_only: false,
            description: "Check a repo out and run a coding agent inside it, streaming its output — text, thinking, and tool calls — to the board. `tool` is claude or codex; ollama is refused because it has no agent mode (no working directory, no tool use, no event stream). Unlike chat, the agent reads the actual files rather than the index's summaries. This spends money and runs commands, so it is only ever started deliberately.",
            schema: obj(json!({ "repo": s(), "tool": s(), "prompt": s(), "agents": s() }), &["repo"]) },
        ToolDef { name: "stop_agent_session", read_only: false,
            description: "Kill a running agent session by id.",
            schema: obj(json!({ "session_id": s() }), &["session_id"]) },
        ToolDef { name: "list_agent_sessions", read_only: true,
            description: "Agent sessions running right now, with their repo and which CLI is driving them.",
            schema: obj(json!({}), &[]) },
        ToolDef { name: "pr_diff", read_only: true,
            description: "A pull request's diff, the summary of what it changes, and the code review of it — recommendation (approve / comment / request_changes), the general rationale, and inline comments resolved to lines of the patch. Pass a PR subject key for that PR, or an issue key for every PR attempting it. Read from the pull request's own object state, so it costs a state read rather than an API call and a model pass; a PR with nothing stored yet is fetched inline once and then kept. `stored_only` answers from state alone and omits what isn't there — for a pane opening itself. `refresh` forces a re-read, for a PR that moved without notifying anybody.",
            schema: obj(json!({ "subject_key": s(), "stored_only": {"type":"boolean"}, "refresh": {"type":"boolean"} }), &["subject_key"]) },
        ToolDef { name: "chat_context", read_only: true,
            description: "Assemble everything the code index knows about a repo — its card, component cards, dependency edges and recent commit summaries — or about one commit in it (pass `sha`), as a context block ready to hand to a chat. Built deterministically from the store: a model asked to go and find this would invent some of it.",
            schema: obj(json!({ "repo": s(), "sha": s() }), &["repo"]) },
        ToolDef { name: "index_status", read_only: true,
            description: "How far the code index has got, per repo and in total: components carded, commits fetched and summarized, dependency edges, and how far back history has been walked — plus the indexing invocations in flight right now, so a stalled index is distinguishable from a working one.",
            schema: obj(json!({}), &[]) },
        ToolDef { name: "repo_index_detail", read_only: true,
            description: "Everything the code index holds about one repo: its card, its component PURPOSE/SYMPTOMS cards, its dependency edges both ways, and the commit summaries actually written (newest first). Reading a few summaries is the only way to tell a thin index from a wrong one.",
            schema: obj(json!({ "repo": s(), "commit_limit": {"type":"integer"} }), &["repo"]) },
        ToolDef { name: "list_components", read_only: true,
            description: "A repo's components — module roots derived from the checkout — each with the PURPOSE/SYMPTOMS card that routes an incident to it, and the commit it was summarized from.",
            schema: obj(json!({ "repo": s() }), &["repo"]) },
        ToolDef { name: "repo_deps", read_only: true,
            description: "The dependency edges into and out of a repo, from manifests actually present in its checkout. Only edges to repos MuggleBot also indexes are recorded — an edge to somewhere it can't look propagates a score to nowhere.",
            schema: obj(json!({ "repo": s() }), &["repo"]) },
        ToolDef { name: "list_unattributed", read_only: true,
            description: "Signals that resolved to no subject — a CI failure on a commit with no PR, a meeting action item naming nothing. Deliberately not given subjects of their own: minting one per unresolvable event is how the board fills with near-identical one-signal cards.",
            schema: obj(json!({}), &[]) },
        ToolDef { name: "merge", read_only: false,
            description: "Collapse `drop` into `keep`: re-attribute its signals, rewrite its relation edges, carry its artifacts across, and forward future activity. Runs as a workflow so a failure part-way finishes rather than leaving a half-merged pair.",
            schema: obj(json!({ "keep": s(), "drop": s() }), &["keep", "drop"]) },
        ToolDef { name: "reattribute", read_only: false,
            description: "Move one signal to a specific subject, overriding the ranked climb. Omit `subject_key` to pin it to the unattributed lane. The override is remembered, so re-ingesting the same event doesn't undo it.",
            schema: obj(json!({ "signal_id": s(), "subject_key": s() }), &["signal_id"]) },
        ToolDef { name: "resolve_gate", read_only: false,
            description: "Answer a pending human gate: approve to resolve the durable promise the blocked handler awaits, or reject to fail that invocation with the reason recorded. No gated action ships yet — the mechanism exists so authorization isn't retrofitted onto a pipeline that already acts.",
            schema: obj(json!({ "invocation_id": s(), "approve": {"type":"boolean"}, "reason": s() }), &["invocation_id", "approve"]) },
        ToolDef { name: "explain", read_only: false,
            description: "Distil a subject and everything under it — its events, the pull requests attempting it with their critiques and review conversations, the proposed causes, the triage, and any attached context — into a readable explanation. Run it on an issue for the whole situation, or on one PR for just that change. Free when nothing has changed since the last one. Runs on the LOCAL model; pass second_opinion=true to ask the cloud model for its own read of the same dossier, which is stored alongside rather than replacing it.",
            schema: obj(json!({ "subject_key": s(), "second_opinion": {"type":"boolean"} }), &["subject_key"]) },
        ToolDef { name: "get_explanation", read_only: true,
            description: "The stored explanations for a subject — the local one, and the cloud second opinion if one was asked for — each with the watermark it was built from (so a stale one is visibly stale), which facets it drew on, and any claims the dossier check removed.",
            schema: obj(json!({ "subject_key": s() }), &["subject_key"]) },
        ToolDef { name: "list_dispatches", read_only: true,
            description: "What the AI is doing right now: each dispatched pass with its state (queued behind a concurrency limit, running, done, refused as a duplicate because the same key already ran, or failed with its message), newest first. Optionally scoped to one `subject_key`. This is the daemon's own in-memory view — `list_workflows` is the durable one, read from Restate.",
            schema: obj(json!({ "subject_key": s() }), &[]) },
        ToolDef { name: "list_workflows", read_only: true,
            description: "In-flight and recent Restate invocations — target, status, scope, and failure — optionally filtered to one `subject_key`. A queued invocation and a broken one look identical from the board without this.",
            schema: obj(json!({ "subject_key": s() }), &[]) },
        ToolDef { name: "list_secrets", read_only: true,
            description: "Which credentials are set, and when each last changed. Never returns a value — there is no tool that does.",
            schema: obj(json!({}), &[]) },
        ToolDef { name: "set_secret", read_only: false,
            description: "Store or replace a credential by name (e.g. `github`, `slack`, or the name an authed context source references).",
            schema: obj(json!({ "name": s(), "value": s() }), &["name", "value"]) },
        ToolDef { name: "delete_secret", read_only: false,
            description: "Delete a stored credential.",
            schema: obj(json!({ "name": s() }), &["name"]) },
    ]
}

pub fn resources() -> Vec<ResourceDef> {
    vec![
        ResourceDef {
            uri: "board://current",
            name: "Board",
            description: "Live board snapshot: signals, subjects, source health.",
        },
        ResourceDef {
            uri: "config://redacted",
            name: "Config",
            description: "Effective configuration (no secrets — those live in the database).",
        },
        ResourceDef {
            uri: "memory://",
            name: "Memory",
            description: "Browsable institutional-memory store.",
        },
        ResourceDef {
            uri: "context://",
            name: "Context",
            description: "Browsable context library.",
        },
        ResourceDef {
            uri: "live://hints",
            name: "Live hints",
            description: "Active live-assist hints and flags.",
        },
    ]
}

// ---- arg helpers ------------------------------------------------------------

fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required string arg `{key}`"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    /// A truncated diff must say so.
    ///
    /// A diff that silently stops is how a reader concludes a change is smaller than it is, which
    /// is the one wrong impression a review pane can give.
    #[test]
    fn a_truncated_diff_is_marked_as_truncated() {
        let short = "one line";
        assert_eq!(truncate_for_prompt(short, 100), short);

        let long = "x".repeat(200);
        let cut = truncate_for_prompt(&long, 50);
        assert!(cut.contains("diff truncated"), "{cut}");
        assert!(cut.starts_with(&"x".repeat(50)));

        // Multi-byte content must not be split mid-character.
        let unicode = "→".repeat(100);
        let cut = truncate_for_prompt(&unicode, 10);
        assert!(cut.starts_with(&"→".repeat(10)));
    }

    /// The repo key can contain `/` (`owner/repo`), so splitting the target on `/` naively
    /// gives "owner" as the repo and "repo" as the handler. The panel would then group every
    /// repo in the org under its owner and claim they were all being indexed at once.
    #[test]
    fn an_indexing_target_splits_into_the_whole_repo_and_the_handler() {
        let raw = json!({"rows": [
            {"target": "RepoIndexer/restatedev/restate-cloud/tick", "status": "running",
             "scope": "local-llm", "completion_failure": null,
             "created_at": 1, "completed_at": null},
            {"target": "RepoIndex/restatedev/run", "status": "succeeded",
             "scope": null, "completion_failure": null, "created_at": 2, "completed_at": 3},
            // Not indexing — must not appear on this panel.
            {"target": "Issue/restatedev/restate#412/analyze", "status": "running",
             "scope": null, "completion_failure": null, "created_at": 4, "completed_at": null},
        ]});
        let out = index_invocations(&raw);
        let rows = out.as_array().expect("an array");
        assert_eq!(rows.len(), 2, "only the indexing services: {out}");

        assert_eq!(rows[0]["repo"], "restatedev/restate-cloud");
        assert_eq!(rows[0]["handler"], "tick");
        assert_eq!(rows[0]["status"], "running");
        assert_eq!(rows[0]["scope"], "local-llm");

        assert_eq!(rows[1]["repo"], "restatedev");
        assert_eq!(rows[1]["handler"], "run");
    }

    /// A bare array and a `{rows: …}` envelope must both work: this is the only place
    /// MuggleBot reads Restate's SQL surface, and an envelope change would silently empty
    /// the panel rather than fail.
    #[test]
    fn either_envelope_shape_is_accepted() {
        let one = json!([{"target": "RepoIndexer/o/r/tick", "status": "running"}]);
        assert_eq!(index_invocations(&one).as_array().unwrap().len(), 1);
        let two = json!({"rows": [{"target": "RepoIndexer/o/r/tick", "status": "running"}]});
        assert_eq!(index_invocations(&two).as_array().unwrap().len(), 1);
        // And neither shape present is an empty panel, not a panic.
        assert_eq!(index_invocations(&json!({})).as_array().unwrap().len(), 0);
    }

    /// A failure has to reach the panel. An indexer that is retrying forever on a bad token
    /// looks identical to one that is working, unless the reason is shown.
    #[test]
    fn a_failed_indexing_invocation_carries_its_reason() {
        let raw = json!({"rows": [{
            "target": "RepoIndexer/o/r/tick", "status": "failed",
            "completion_failure": "GitHub returned 401", "scope": "github",
            "created_at": 1, "completed_at": 2,
        }]});
        let rows = index_invocations(&raw);
        assert_eq!(rows[0]["status"], "failed");
        assert_eq!(rows[0]["failure"], "GitHub returned 401");
    }

    use super::*;
    use crate::embed::HashEmbedder;
    use crate::reasoner::MockReasoner;
    use crate::signal::{ResolutionKey, Severity};
    use chrono::Utc;
    use std::time::Duration;

    fn tools(reasoner_response: &str) -> Tools {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let secrets = crate::secrets::Secrets::for_tests(store.clone());
        let scorer = Arc::new(crate::score::Scorer {
            store: store.clone(),
            embedder: Arc::new(crate::embed::HashEmbedder),
        });
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new(reasoner_response));
        let memory = Arc::new(MemoryManager::new(
            store.clone(),
            embedder.clone(),
            reasoner.clone(),
            reasoner.clone(),
        ));
        let context = Arc::new(crate::context::ContextManager::new(
            store.clone(),
            secrets.clone(),
            embedder,
            reasoner.clone(),
            reasoner.clone(),
            "6h".into(),
        ));
        let attributor = Arc::new(Attributor::new(store.clone()));
        let (investigator, repos, browser) =
            crate::rootcause::offline_stack(store.clone(), attributor.clone(), reasoner.clone());
        let analyst = Arc::new(Analyst::new(
            store.clone(),
            attributor.clone(),
            reasoner.clone(),
            reasoner.clone(),
            memory.clone(),
            context.clone(),
            0.8,
            false,
            0.6,
            Duration::from_secs(1800),
        ));
        Tools {
            agents: Arc::new(crate::agent::AgentSessions::for_tests()),
            store,
            ingress: Arc::new(crate::restate::ingress::Ingress::new(
                &crate::config::RestateConfig::default(),
            )),
            scorer: scorer.clone(),
            secrets,
            attributor,
            analyst,
            memory,
            context,
            reasoner: reasoner.clone(),
            config: Arc::new(Config::default()),
            investigator,
            repos,
            browser,
            diffs: Arc::new(crate::prdiff::DiffReader::new(None, reasoner.clone()).unwrap()),
        }
    }

    fn seed(t: &Tools) -> String {
        let s = Signal {
            id: Signal::make_id(Source::Slack, "1", None),
            source: Source::Slack,
            external_id: "1".into(),
            kind: SignalKind::Alert,
            title: "service-foo 5xx spike".into(),
            body: Some("connection pool exhausted".into()),
            url: None,
            actor: None,
            keys: vec![
                ResolutionKey::new("service", "foo"),
                ResolutionKey::new("slack_thread", "C1/1721822400.001"),
            ],
            severity: Severity::Critical,
            version: None,
            upstream_gone: false,
            occurred_at: Utc::now(),
            ingested_at: Utc::now(),
            subject: None,
            raw: serde_json::json!({ "is_alert": true }),
            tags: Vec::new(),
        };
        t.store.insert_signal(&s).unwrap();
        t.attributor
            .attach(&s)
            .unwrap()
            .expect("attributed")
            .into_string()
    }

    #[tokio::test]
    async fn draft_postmortem_saves_to_memory() {
        let t = tools("## Postmortem\nService foo saturated. [sig:x]");
        let tid = seed(&t);
        let r = t
            .call(
                "draft_postmortem",
                &json!({ "subject_key": tid, "save": true }),
            )
            .await
            .unwrap();
        assert!(r["draft"].as_str().unwrap().contains("Postmortem"));
        assert!(!r["saved_memory"].is_null());
        assert_eq!(t.memory.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn distill_memory_saves_one_sentence() {
        let t = tools(
            "Pool exhaustion under load saturates service-foo; raising the pool ceiling clears it.",
        );
        let tid = seed(&t);
        let r = t
            .call("distill_memory", &json!({ "subject_key": tid }))
            .await
            .unwrap();
        // The created memory's summary is the distilled sentence, linked to the subject.
        assert!(r["summary"].as_str().unwrap().contains("Pool exhaustion"));
        assert_eq!(r["links"][0].as_str().unwrap(), tid);
        assert_eq!(t.memory.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let t = tools("noop");
        assert!(t.call("nonexistent", &json!({})).await.is_err());
    }
}
