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
    /// Reads a pasted Slack thread with two models. Held here because requesting one is an
    /// operator action, and the fetch runs inline so a bad link is an error on the button
    /// rather than a queued row that fails a minute later.
    pub threads: Arc<crate::thread::Analyser>,
    /// Reads and summarizes pull request diffs, for the one case object state has none yet.
    pub diffs: Arc<crate::prdiff::DiffReader>,
    /// Personas: the modelled people, their profiles, and the predictions made from them.
    pub personas: Arc<crate::persona::Engine>,
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
            "analyse_thread" => self.analyse_thread(args).await,
            "list_thread_analyses" => Ok(json!(self
                .store
                .list_thread_analyses(args.get("limit").and_then(|v| v.as_i64()).unwrap_or(30))?)),
            "get_thread_analysis" => Ok(json!(self
                .store
                .get_thread_analysis(&req_str(args, "id")?)?)),
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
            "list_personas" => self.list_personas(),
            "get_persona" => self.get_persona(args),
            "propose_personas" => self.propose_personas_labelled(args).await,
            "create_persona" => self.create_persona(args).await,
            "update_persona" => self.update_persona(args),
            "delete_persona" => self.delete_persona(args),
            "link_persona_identity" => self.link_persona_identity(args).await,
            "unlink_persona_identity" => self.unlink_persona_identity(args),
            "harvest_persona" => self.harvest_persona(args).await,
            "refresh_persona_profile" => self.refresh_persona_profile(args).await,
            "predict_persona" => self.predict_persona(args).await,
            "list_predictions" => self.list_predictions(args),
            "who_knows" => self.who_knows(args),
            "add_persona_context" => self.add_persona_context(args).await,
            "remove_persona_context" => self.remove_persona_context(args),
            "chat_context" => self.chat_context(args),
            "pr_diff" => self.pr_diff(args).await,
            "pr_review" => self.pr_review(args).await,
            "list_incidents" => self.list_incidents(args),
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
        // Deliberately *all* subjects, incidents included. A caller asking for subjects means
        // all of them; the two boards do their own filtering (`board_views` /
        // `incident_views`), and hiding a kind from the general lister would make an incident
        // unreachable from MCP.
        Ok(json!(self.attributor.subject_views(active_only)?))
    }

    /// The incidents board: open incidents, with whatever each has been mapped to.
    ///
    /// Its own tool rather than a filter argument on `list_subjects`, because "what is on
    /// fire" is a different question from "what does my work need" and the answer is read by
    /// a different screen. `active_only` here means what incident.io says, not what the
    /// operator has read.
    fn list_incidents(&self, args: &Value) -> Result<Value> {
        let active_only = args
            .get("active_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let views = self.attributor.incident_views(active_only)?;
        Ok(json!({
            "open": views.iter().filter(|v| !v.subject.handled.is_handled()).count(),
            "incidents": views,
        }))
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
        // Operator-driven, and the operator named a URL — so this is the browser tier by
        // construction. The Grafana tier is chosen from an alert's *parsed links*, which a
        // hand-typed URL is not.
        let queued = self.store.queue_browser_investigation(
            &anchor.id,
            &url,
            self.browser.brief(&url, context).as_str(),
            "browser",
        )?;
        Ok(json!(queued))
    }

    /// Queue an analysis of one pasted Slack thread.
    ///
    /// The thread is fetched *here*, before the row is written: a mistyped link or a channel
    /// the token cannot see becomes an error on the button, which is where the operator is
    /// looking. Queueing first would put the same failure in a row they would have to go and
    /// find.
    async fn analyse_thread(&self, args: &Value) -> Result<Value> {
        if !self.threads.ready() {
            bail!(
                "thread analysis is off — set [threads].enabled = true and store a `slack` \
                 credential"
            );
        }
        let link = req_str(args, "link")?;
        let queued = self.threads.request(&link).await?;
        // Poke the sweep rather than running two model calls inside this request: the
        // operator gets the row back immediately and watches it fill in.
        self.ingress.start_scheduler("browser-queue").await.ok();
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

    /// Re-review a pull request on a model the operator named.
    ///
    /// Separate from `pr_diff`'s automatic review because the two answer different questions.
    /// `pr_diff` asks "what is this change, and what does the default reviewer make of it?"
    /// and must stay cheap enough to run on a render. This asks "what would *that* model say
    /// about it?", which is only ever worth paying for when somebody asks — so it is a button,
    /// not a fallback, and nothing automatic reaches it.
    ///
    /// The work goes through the `PrDiff` workflow rather than running here. It is several
    /// model passes over the patches — five minutes on an eighteen-file change, measured — and
    /// a request that holds the page for five minutes is a hung button. The dispatch strip
    /// shows it in flight and the pane picks up the verdict from object state.
    async fn pr_review(&self, args: &Value) -> Result<Value> {
        let key = req_str(args, "subject_key")?;
        let provider = req_str(args, "provider")?;
        let model = req_str(args, "model")?;
        if model.trim().is_empty() {
            bail!("a model is required: pick one from `list_models`");
        }
        let Some((repo, number)) = crate::prdiff::parse_pr_key(&key) else {
            bail!("{key} does not name a pull request");
        };
        let pr_key = crate::prdiff::pr_key(&repo, number);

        // The watermark the review will be filed under — the same one the diff uses, so a
        // re-review lands beside the diff it is about rather than inventing a version.
        let watermark = self
            .store
            .signals_for_subject(&pr_key)
            .ok()
            .and_then(|s| s.last().map(|s| s.id.clone()))
            .unwrap_or_else(|| "inline".into());

        // Where it queues is decided by *which* model was picked, not by the fact that a
        // human picked it: an on-device re-review contends for the one GPU exactly like every
        // other local pass, and a cloud one must not be held behind it.
        let label = crate::reasoner::provider_label(&provider);
        let scope = if label == "ollama_local" {
            crate::restate::scopes::LOCAL_LLM
        } else {
            crate::restate::scopes::CLOUD_LLM
        };
        // Re-running the *same* model on an unchanged pull request is a spent key, and free —
        // which is right for a double click and wrong for an operator who read a bad review
        // and wants another sample. `force` is the same escape hatch `pr_diff`'s re-read uses,
        // and for the same reason: per-second, so a double click is still free while a
        // deliberate redo is not.
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut version = crate::prdiff::with_model(&watermark, label, &model);
        if force {
            version.push_str(&format!("#r{}", chrono::Utc::now().timestamp()));
        }
        let wf_key = format!("{pr_key}@{version}");
        let dispatched = self
            .ingress
            .submit_workflow("PrDiff", &wf_key, Some(scope))
            .await
            .unwrap_or(false);
        Ok(json!({
            "subject_key": pr_key,
            "provider": label,
            "model": model,
            // False means Restate refused the key: this PR has already been reviewed on this
            // model at this watermark, and the stored review *is* the answer. Said out loud so
            // the pane can report "already done" rather than showing a button that did nothing.
            "dispatched": dispatched,
            "scope": scope,
        }))
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

    // ---- personas -----------------------------------------------------------
    //
    // The write tools here mutate MuggleBot's own store, like every other write on this
    // surface — nothing reaches GitHub or Slack. A prediction is a private rehearsal.

    /// Every modelled person, with their freshness and how much is behind each profile.
    fn list_personas(&self) -> Result<Value> {
        let mut out = Vec::new();
        for p in self.store.list_personas()? {
            let evidence = self.store.persona_evidence(&p.slug, None)?;
            let (cursor, backfill_complete) = self.store.persona_harvest_cursor(&p.slug)?;
            out.push(json!({
                "slug": p.slug,
                "display_name": p.display_name,
                "role": p.role,
                "identities": p.identities,
                "harvested_at": p.harvested_at,
                "profiled_at": p.profiled_at,
                "traits": self.store.persona_traits(&p.slug)?.len(),
                // Counted, never modelled — see `persona::Stats`.
                "stats": crate::persona::Stats::compute(&evidence),
                "walked_back_to": cursor,
                "backfill_complete": backfill_complete,
                // Why this profile is thinner than it looks like it should be. NULL is the
                // good case; anything else is the difference between "quiet colleague" and
                // "nothing was actually read".
                "harvest_note": self.store.persona_harvest_note(&p.slug)?,
                // The two or three areas worth showing on a row — "who is this person for"
                // answered before you click in.
                "sme": crate::persona::sme::with_depth(
                    crate::persona::sme::areas(&evidence),
                    &self.store.persona_traits(&p.slug)?,
                )
                .into_iter()
                .take(3)
                .collect::<Vec<_>>(),
            }));
        }
        Ok(json!({ "enabled": self.personas.enabled, "personas": out }))
    }

    /// One persona in full: the profile, what verification refused, and recent predictions.
    ///
    /// `removed` is returned rather than hidden for the same reason `subject_explanations`
    /// returns it: a profile that had claims taken out of it is one to read more carefully,
    /// and a filter nobody can see is a filter nobody can debug.
    fn get_persona(&self, args: &Value) -> Result<Value> {
        let slug = req_str(args, "slug")?;
        let Some(profile) = self.store.persona_profile(&slug)? else {
            bail!("no persona '{slug}'");
        };
        let evidence_limit = args
            .get("evidence_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(60) as usize;
        Ok(json!({
            "persona": profile.persona,
            "traits": profile.traits,
            "removed": profile.removed,
            "stats": profile.stats,
            "caveats": profile.caveats(),
            "harvest_note": self.store.persona_harvest_note(&slug)?,
            "sme": profile.sme,
            "context": profile.context,
            "evidence": self.store.persona_evidence(&slug, Some(evidence_limit))?,
            "predictions": self.store.predictions_for_persona(&slug, 20)?,
        }))
    }

    /// Refresh the cached Slack workspace directory if it is stale.
    ///
    /// Lazy and best-effort, on the operator paths that need names. A failure is not an error
    /// here: without the directory, proposals fall back to raw ids and linking still works by
    /// id, so a missing `users:read` scope degrades the feature rather than breaking it.
    async fn refresh_slack_directory(&self) {
        /// A workspace directory changes when somebody joins. Daily is generous.
        const TTL_HOURS: i64 = 24;

        let stale = match self.store.slack_directory_age() {
            Ok((_, 0)) => true,
            Ok((Some(when), _)) => (chrono::Utc::now() - when).num_hours() >= TTL_HOURS,
            Ok((None, _)) => true,
            Err(_) => true,
        };
        if !stale {
            return;
        }
        let Some(token) = self.secrets.get_opt("slack") else {
            debug!("slack directory: no stored slack token, so names stay as ids");
            return;
        };
        match crate::watchers::slack::fetch_users(&reqwest::Client::new(), &token).await {
            Ok(users) => match self.store.put_slack_users(&users) {
                Ok(n) => debug!("slack directory: cached {n} member(s)"),
                Err(e) => debug!("slack directory: storing failed: {e:#}"),
            },
            Err(e) => debug!("slack directory: {e:#}"),
        }
    }

    /// People the signal log has seen, ranked by how much the operator deals with them.
    ///
    /// Proposes; never creates. See [`crate::persona::harvest::propose`] on why modelling
    /// every actor automatically would be both useless and wrong.
    ///
    /// Slack candidates are **labelled from the workspace directory**, because a ranked list of
    /// `U06T7445RHD (94 interactions)` cannot be acted on: the operator has no way to tell
    /// which of those opaque ids is the colleague they meant.
    async fn propose_personas_labelled(&self, args: &Value) -> Result<Value> {
        self.refresh_slack_directory().await;
        let mut out = self.propose_personas(args)?;
        if let Some(candidates) = out.get_mut("candidates").and_then(|c| c.as_array_mut()) {
            for c in candidates.iter_mut() {
                let (Some("slack"), Some(handle)) = (
                    c.get("source").and_then(Value::as_str),
                    c.get("handle").and_then(Value::as_str).map(str::to_string),
                ) else {
                    continue;
                };
                if let Ok(Some(user)) = self.store.slack_user(&handle) {
                    c["label"] = json!(user.label());
                    c["is_bot"] = json!(user.is_bot);
                    c["deleted"] = json!(user.deleted);
                    // The suggested slug becomes the *name* rather than the opaque id, so a
                    // persona created from a proposal is called `pavel-cholakov` and not
                    // `u06t7445rhd`.
                    c["suggested_slug"] = json!(crate::persona::Persona::slugify(&user.name));
                    // Every handle worth linking for this person, so the create form can
                    // offer the GitHub side pre-guessed.
                    c["aliases"] = json!(user.aliases());
                }
            }
            // Automation the directory *knows* is automation, dropped.
            //
            // Strictly better than the name-substring guess in `harvest::propose`, which
            // cannot see through an opaque id: `U06T7445RHD` is the incident.io bot and was
            // the single highest-ranked candidate on this workspace, 95 interactions of
            // alerts. The name filter stays as the fallback for a workspace with no directory
            // cached — there, a missed bot is a junk row rather than an invisible person.
            candidates.retain(|c| {
                !c.get("is_bot").and_then(Value::as_bool).unwrap_or(false)
                    && !c.get("deleted").and_then(Value::as_bool).unwrap_or(false)
            });
        }
        Ok(out)
    }

    fn propose_personas(&self, args: &Value) -> Result<Value> {
        let existing: Vec<String> = self
            .store
            .list_personas()?
            .into_iter()
            .flat_map(|p| {
                // Match proposals against the identities already linked as well as the slug:
                // a persona named `pav` with the GitHub login `pcholakov` linked should not
                // have `pcholakov` proposed back at the operator.
                let mut keys: Vec<String> = p.identities.iter().map(|i| i.handle.clone()).collect();
                keys.push(p.slug);
                keys
            })
            .collect();
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.personas.max_proposals as u64) as usize;
        Ok(json!({
            "candidates": crate::persona::harvest::propose(&self.store, &existing, limit)?,
        }))
    }

    /// Create a persona and arm its harvest loop.
    async fn create_persona(&self, args: &Value) -> Result<Value> {
        if !self.personas.enabled {
            bail!("personas are disabled — set `[personas] enabled = true` in the config");
        }
        let display_name = req_str(args, "display_name")?;
        let slug = crate::persona::Persona::slugify(
            &opt_str(args, "slug").unwrap_or_else(|| display_name.clone()),
        );
        if slug.is_empty() {
            bail!("'{display_name}' does not reduce to a usable slug — pass one explicitly");
        }
        if self.store.get_persona(&slug)?.is_some() {
            bail!("a persona '{slug}' already exists");
        }
        // The directory first, so a Slack handle typed as a name resolves to the id the
        // signal log actually records. Without it the link harvests nothing and looks like a
        // colleague who never posts.
        self.refresh_slack_directory().await;
        let mut identities = identities_from(args)?;
        for identity in &mut identities {
            resolve_slack_handle(&self.store, identity)?;
        }
        let now = chrono::Utc::now();
        let persona = crate::persona::Persona {
            slug: slug.clone(),
            display_name,
            role: opt_str(args, "role"),
            notes: opt_str(args, "notes"),
            identities,
            created_at: now,
            updated_at: now,
            harvested_at: None,
            profiled_at: None,
            evidence_watermark: None,
        };
        self.store.put_persona(&persona)?;
        // Two sends, and both are needed.
        //
        // `start` arms the recurring loop. `poke` does the *first* pass now, at operator
        // priority — and that second one is the fix for a real failure: creating a persona armed
        // the loop and left the first harvest to `tick`, which is `Trigger::Scheduled` and
        // therefore background. So a persona created while the code index held the GitHub budget
        // at its reserve was refused on its opening pass and sat with no GitHub evidence at all,
        // which is precisely the "it isn't working" the operator sees. Creating a persona is an
        // operator action; the loop's ticks are not.
        //
        // Best-effort: an unreachable ingress must not fail the creation, and `start` is
        // idempotent-by-staleness so the next boot sweep arms it.
        let armed = self.arm_persona(&slug).await;
        let harvesting = self.poke_persona(&slug).await;
        Ok(json!({ "persona": persona, "armed": armed, "harvesting": harvesting }))
    }

    /// Edit the operator-asserted parts of a persona.
    ///
    /// Deliberately only those. `role` and `notes` are *asserted*, not inferred, which is why
    /// they bypass trait verification entirely and are fed to the model verbatim; the traits
    /// themselves are not editable here, because a hand-written trait would be an uncited
    /// claim in a store whose whole contract is that every claim has a citation.
    fn update_persona(&self, args: &Value) -> Result<Value> {
        let slug = req_str(args, "slug")?;
        let Some(mut persona) = self.store.get_persona(&slug)? else {
            bail!("no persona '{slug}'");
        };
        if let Some(name) = opt_str(args, "display_name") {
            persona.display_name = name;
        }
        // Present-but-empty clears; absent leaves alone. The two have to be distinguishable
        // or a note can be written and never removed.
        if args.get("role").is_some() {
            persona.role = opt_str(args, "role");
        }
        if args.get("notes").is_some() {
            persona.notes = opt_str(args, "notes");
        }
        persona.updated_at = chrono::Utc::now();
        self.store.put_persona(&persona)?;
        Ok(json!(persona))
    }

    /// Stop modelling somebody, and remove everything derived from them.
    fn delete_persona(&self, args: &Value) -> Result<Value> {
        let slug = req_str(args, "slug")?;
        Ok(json!({ "deleted": self.store.delete_persona(&slug)? }))
    }

    /// Attach a handle to a persona, and harvest through it from the next tick.
    async fn link_persona_identity(&self, args: &Value) -> Result<Value> {
        let slug = req_str(args, "slug")?;
        if self.store.get_persona(&slug)?.is_none() {
            bail!("no persona '{slug}'");
        }
        self.refresh_slack_directory().await;
        let mut identity = one_identity(args)?;
        resolve_slack_handle(&self.store, &mut identity)?;
        self.store.link_persona_identity(&slug, &identity)?;
        // A confirmed identity is new material, so harvest now rather than at the next tick:
        // linking a login is the moment the operator expects the profile to start filling in.
        let harvesting = identity.provenance.confirmed() && self.poke_persona(&slug).await;
        Ok(json!({
            "slug": slug,
            "identity": identity,
            "harvesting": harvesting,
        }))
    }

    fn unlink_persona_identity(&self, args: &Value) -> Result<Value> {
        let source = req_source(args, "source")?;
        let handle = req_str(args, "handle")?;
        Ok(json!({
            "unlinked": self.store.unlink_persona_identity(source, &handle)?,
        }))
    }

    /// Harvest now, rather than waiting for the next tick.
    async fn harvest_persona(&self, args: &Value) -> Result<Value> {
        let slug = req_str(args, "slug")?;
        if self.store.get_persona(&slug)?.is_none() {
            bail!("no persona '{slug}'");
        }
        Ok(json!({ "slug": slug, "harvesting": self.poke_persona(&slug).await }))
    }

    /// Re-distil the profile from everything harvested.
    ///
    /// Submitted as a workflow rather than run inline: it is one local model pass per facet,
    /// which measured in minutes, and an HTTP request held open for it makes the UI hostage to
    /// it. `submitted: false` means the profile is already current for this evidence set — a
    /// success, not a failure.
    async fn refresh_persona_profile(&self, args: &Value) -> Result<Value> {
        let slug = req_str(args, "slug")?;
        if self.store.get_persona(&slug)?.is_none() {
            bail!("no persona '{slug}'");
        }
        let Some(watermark) = self.store.persona_evidence_watermark(&slug)? else {
            bail!(
                "nothing has been harvested for '{slug}' yet — link a GitHub login or Slack \
                 user id, then harvest"
            );
        };
        // An explicit redo on an unchanged evidence set has to bypass the key collision, the
        // same way `IssueTriage`'s `#a{n}` suffix does.
        let key = match args.get("force").and_then(|v| v.as_bool()).unwrap_or(false) {
            true => format!(
                "{}#r{}",
                crate::restate::workflows::persona::PersonaProfile::key(&slug, &watermark),
                chrono::Utc::now().timestamp()
            ),
            false => crate::restate::workflows::persona::PersonaProfile::key(&slug, &watermark),
        };
        let submitted = self
            .ingress
            .submit_workflow(
                "PersonaProfile",
                &key,
                Some(crate::restate::workflows::persona::PersonaProfile::SCOPE),
            )
            .await?;
        Ok(json!({
            "submitted": submitted,
            "workflow": key,
            "note": if submitted {
                "Distilling — one local pass per facet, so this takes a few minutes."
            } else {
                "The profile is already current for everything harvested. Pass force to redo it."
            },
        }))
    }

    /// Predict what one or more personas would do about a subject.
    ///
    /// Takes a list, because the operator's question is "how will this land" and the answer is
    /// the *set* of reactions — a reviewer who will block and a reviewer who will not care are
    /// one answer together, and two separate button presses apart.
    async fn predict_persona(&self, args: &Value) -> Result<Value> {
        let subject_key = req_str(args, "subject_key")?;
        if self.attributor.subject_view(&subject_key)?.is_none() {
            bail!("no subject {subject_key}");
        }
        let slugs = match str_array(args, "personas") {
            empty if empty.is_empty() => vec![req_str(args, "slug")?],
            many => many,
        };
        let kind = match opt_str(args, "kind") {
            Some(k) => crate::persona::PredictionKind::parse(&k).ok_or_else(|| {
                anyhow!("kind must be code_review, issue_response or slack_engagement (got '{k}')")
            })?,
            // The kind follows the subject unless the operator overrides it: offering a code
            // review on a Slack thread would produce one, about a diff that does not exist.
            None => crate::persona::PredictionKind::for_subject(&subject_key),
        };
        let watermark = self.store.subject_watermark(&subject_key);
        // The model rides in the key, so a cloud read sits beside the local one rather than
        // being refused as a duplicate of it — the same arrangement as `SecondOpinion`.
        let produced_by = match (opt_str(args, "provider"), opt_str(args, "model")) {
            (Some(provider), Some(model)) => {
                crate::restate::workflows::persona::PredictKey::model_label(
                    crate::reasoner::provider_label(&provider),
                    &model,
                )
            }
            _ => "local".to_string(),
        };

        let mut submitted = Vec::new();
        for slug in slugs {
            if self.store.get_persona(&slug)?.is_none() {
                bail!("no persona '{slug}'");
            }
            let key = crate::restate::workflows::persona::PredictKey::new(
                &slug,
                kind,
                &produced_by,
                &subject_key,
                &watermark,
            )
            .format();
            let started = self
                .ingress
                .submit_workflow(
                    "PersonaPredict",
                    &key,
                    Some(crate::restate::workflows::persona::PersonaPredict::SCOPE),
                )
                .await?;
            submitted.push(json!({
                "persona": slug,
                "submitted": started,
                "workflow": key,
            }));
        }
        Ok(json!({
            "subject_key": subject_key,
            "kind": kind.as_str(),
            "watermark": watermark,
            "produced_by": produced_by,
            "predictions": submitted,
            // What is already stored, so a caller that submitted nothing new still gets the
            // answer back rather than an empty response that reads as a failure.
            "stored": self.store.predictions_for_subject(&subject_key)?,
        }))
    }

    /// Attach something you know about a person that no excerpt could supply.
    ///
    /// Text is used verbatim. A URL goes through the same `ContextIngest` path as the context
    /// library, so a team charter or an onboarding doc becomes a summary the model can read.
    ///
    /// This bypasses trait verification by design — see [`crate::persona::Context`]. It also
    /// **re-profiles**, because a fact like "owns the release process" changes what their review
    /// comments mean, and a profile distilled before you said so is distilled without it.
    async fn add_persona_context(&self, args: &Value) -> Result<Value> {
        let slug = req_str(args, "slug")?;
        if self.store.get_persona(&slug)?.is_none() {
            bail!("no persona '{slug}'");
        }
        let content = req_str(args, "content")?.trim().to_string();
        if content.is_empty() {
            bail!("`content` cannot be empty");
        }
        let is_url = content.starts_with("http://") || content.starts_with("https://");
        let kind = if is_url { "url" } else { "text" };
        // Fetched and summarized now rather than lazily: the operator is watching, and a URL
        // whose summary arrives on some later tick would be attached-but-empty in the profile
        // pass that runs seconds from here.
        let summary = if is_url {
            match self
                .context
                .add(ContextSourceKind::Url, &content, None, None, None, None)
                .await
            {
                Ok(entry) => entry.summary,
                Err(e) => {
                    debug!("persona context: reading {content} failed: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let id = self
            .store
            .add_persona_context(&slug, kind, &content, summary.as_deref())?;
        let reprofiling = self.reprofile(&slug).await;
        Ok(json!({
            "id": id,
            "slug": slug,
            "kind": kind,
            "summary": summary,
            "reprofiling": reprofiling,
        }))
    }

    /// Remove one attached fact, and re-profile without it.
    fn remove_persona_context(&self, args: &Value) -> Result<Value> {
        let id = req_str(args, "id")?;
        Ok(json!({ "removed": self.store.remove_persona_context(&id)? }))
    }

    /// Submit a fresh profile pass, bypassing the key collision.
    ///
    /// Used by the paths that change what a profile should say without changing the evidence it
    /// is built from — attaching context, editing the role. The watermark is unmoved in those
    /// cases, so a plain submission would be refused as a duplicate and the operator's new fact
    /// would sit unread until the next harvest happened to find something.
    async fn reprofile(&self, slug: &str) -> bool {
        let Ok(Some(watermark)) = self.store.persona_evidence_watermark(slug) else {
            return false;
        };
        let key = format!(
            "{}#r{}",
            crate::restate::workflows::persona::PersonaProfile::key(slug, &watermark),
            chrono::Utc::now().timestamp()
        );
        self.ingress
            .submit_workflow(
                "PersonaProfile",
                &key,
                Some(crate::restate::workflows::persona::PersonaProfile::SCOPE),
            )
            .await
            .unwrap_or(false)
    }

    /// Who to ask about an area of the codebase.
    ///
    /// The question you have *before* you have a persona in mind: not "how does Pavel review"
    /// but "who knows the storage layer". Ranked across every modelled person by where their
    /// review activity actually concentrates — see [`crate::persona::sme`].
    ///
    /// Two honesty properties the ranking has to keep:
    ///
    /// - **Presence and expertise are distinguished, not merged.** An area where the model has
    ///   established that their comments are specific outranks one where they are merely
    ///   active, and both are returned labelled. "They are around" is a useful answer; it is
    ///   not the same answer as "ask them".
    /// - **It only knows who you have modelled.** The real expert may not have a persona at
    ///   all, so the reply says how many people were considered. A ranked list of two is a
    ///   ranked list of two, and reading it as "the two people who know this" would be wrong.
    fn who_knows(&self, args: &Value) -> Result<Value> {
        let area = req_str(args, "area")?.trim().to_ascii_lowercase();
        if area.is_empty() {
            bail!("`area` cannot be empty — pass a repo, a path, or a word from one");
        }
        let personas = self.store.list_personas()?;
        let mut hits = Vec::new();
        for p in &personas {
            let Some(profile) = self.store.persona_profile(&p.slug)? else {
                continue;
            };
            // Substring either way, so `storage` finds `o/r:src/storage` and
            // `restatedev/restate-cloud` finds a persona whose area is the bare repo name.
            let matched: Vec<_> = profile
                .sme
                .iter()
                .filter(|a| {
                    let name = a.area.to_ascii_lowercase();
                    name.contains(&area) || area.contains(&name)
                })
                .collect();
            let Some(best) = matched.iter().max_by(|a, b| {
                a.is_expert()
                    .cmp(&b.is_expert())
                    .then(a.reviews.cmp(&b.reviews))
            }) else {
                continue;
            };
            hits.push(json!({
                "persona": p.slug,
                "display_name": p.display_name,
                "role": p.role,
                "area": best.area,
                "kind": best.kind.as_str(),
                "excerpts": best.excerpts,
                "reviews": best.reviews,
                "share": best.share,
                // The distinction that must not be flattened.
                "established_expertise": best.is_expert(),
                "depth": best.depth,
                "depth_trait": best.depth_trait,
                "evidence": best.evidence,
                "other_matching_areas": matched.len().saturating_sub(1),
            }));
        }
        // Expertise first, then whoever reviews there most.
        hits.sort_by(|a, b| {
            let expert = |v: &Value| v["established_expertise"].as_bool().unwrap_or(false);
            let reviews = |v: &Value| v["reviews"].as_u64().unwrap_or(0);
            expert(b).cmp(&expert(a)).then(reviews(b).cmp(&reviews(a)))
        });
        Ok(json!({
            "area": area,
            "candidates": hits,
            // Said out loud: this ranks the people you model, not the org.
            "personas_considered": personas.len(),
            "note": if hits.is_empty() {
                "Nobody modelled has established activity in this area. That is a statement \
                 about who you have modelled, not about who knows it."
            } else {
                "Ranked over modelled people only — the person who knows this best may not have \
                 a persona."
            },
        }))
    }

    /// Stored predictions, for a subject or for a persona.
    fn list_predictions(&self, args: &Value) -> Result<Value> {
        match (opt_str(args, "subject_key"), opt_str(args, "slug")) {
            (Some(subject), _) => Ok(json!(self.store.predictions_for_subject(&subject)?)),
            (None, Some(slug)) => Ok(json!(self.store.predictions_for_persona(&slug, 50)?)),
            (None, None) => bail!("pass either subject_key or slug"),
        }
    }

    /// Arm a persona's harvest loop. Best-effort — see [`Self::create_persona`].
    async fn arm_persona(&self, slug: &str) -> bool {
        match self
            .ingress
            .send_object_empty("Persona", slug, "start")
            .await
        {
            Ok(_) => true,
            Err(e) => {
                debug!("arming the persona loop for {slug} failed: {e:#}");
                false
            }
        }
    }

    /// Run a harvest pass now, without disturbing the loop's cadence.
    ///
    /// Targets `poke`, **not** `tick`, for the reason the repo indexer does: `tick` arms the
    /// next timer as its first act, so calling it out of band forks the loop and every later
    /// press adds another chain.
    async fn poke_persona(&self, slug: &str) -> bool {
        match self
            .ingress
            .send_object_empty("Persona", slug, "poke")
            .await
        {
            Ok(_) => true,
            Err(e) => {
                debug!("poking the persona harvest for {slug} failed: {e:#}");
                false
            }
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
        ToolDef { name: "analyse_thread", read_only: false,
            description: "Analyse one Slack thread from a pasted permalink (Slack's \"Copy link\"). Two models — Claude and ChatGPT — read it independently and blind to each other, using any persona profiles the participants have; the result marks which findings both models reached and which only one did. Every finding cites the messages it rests on, and a finding quoting words nobody wrote is discarded. Includes a candid section about the operator's own part in the thread. Reads Slack only; never posts.",
            schema: obj(json!({ "link": s() }), &["link"]) },
        ToolDef { name: "list_thread_analyses", read_only: true,
            description: "Slack thread analyses requested so far, newest first, with status (pending/running/completed/failed).",
            schema: obj(json!({ "limit": {"type":"integer"} }), &[]) },
        ToolDef { name: "get_thread_analysis", read_only: true,
            description: "One Slack thread analysis by id: the thread as read, both models' findings, and what they agreed on.",
            schema: obj(json!({ "id": s() }), &["id"]) },
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
        ToolDef { name: "pr_review", read_only: false,
            description: "Re-review a pull request's diff on a model YOU name — a second reader on the same change, when the default reviewer's verdict looks wrong or you want a stronger model on it. `provider` is `anthropic` | `openai` | `ollama_local` | `ollama` and `model` is one of that provider's models (see `list_models`). Runs as a workflow because it is several model passes over the patches; it returns as soon as the work is accepted, and the review appears on the pull request when it lands. Re-requesting the SAME model on an unchanged pull request is free — the key is already spent and the stored review is the answer, reported as `dispatched: false`; pass `force: true` to run it again anyway, for a review that came back badly. Replaces the stored review rather than sitting beside it, so the pane always shows one verdict and says which model produced it.",
            schema: obj(json!({ "subject_key": s(), "provider": s(), "model": s(), "force": {"type":"boolean"} }), &["subject_key", "provider", "model"]) },
        ToolDef { name: "list_incidents", read_only: true,
            description: "Open incident.io incidents, each with what it has been mapped to: the ranked code candidates (repo / component / commit, with evidence), and any issues or pull requests it has been linked to. Tracks what incident.io says is open — `triage`, `active` and `post-incident` — so an incident leaves this list when it is closed upstream rather than when you acknowledge it. Pass `active_only: false` to include closed ones.",
            schema: obj(json!({ "active_only": {"type":"boolean"} }), &[]) },
        // ---- personas -------------------------------------------------------
        //
        // A persona is a candid behavioural model of one colleague, built from things they
        // actually wrote, used to predict how they will respond *before* you ask them.
        // Predictions are private rehearsals: nothing here posts anything anywhere.
        ToolDef { name: "list_personas", read_only: true,
            description: "The modelled people: their identities, how much evidence is behind each profile, and how fresh it is.",
            schema: none() },
        ToolDef { name: "get_persona", read_only: true,
            description: "One persona in full: verified traits with their citations, the claims verification refused and why, counted stats, evidence excerpts, and recent predictions.",
            schema: obj(json!({ "slug": s(), "evidence_limit": {"type":"integer"} }), &["slug"]) },
        ToolDef { name: "propose_personas", read_only: true,
            description: "People seen in the signal log who are not yet modelled, ranked by how much you interact with them. Proposes only — creating a persona is a decision.",
            schema: obj(json!({ "limit": {"type":"integer"} }), &[]) },
        ToolDef { name: "create_persona", read_only: false,
            description: "Start modelling a person. `role` and `notes` are your own words and are used verbatim; identities are the handles to harvest through.",
            schema: obj(json!({
                "display_name": s(), "slug": s(), "role": s(), "notes": s(),
                "identities": { "type": "array", "items": obj(json!({
                    "source": s(), "handle": s(), "provenance": s(), "rationale": s()
                }), &["source", "handle"]) }
            }), &["display_name"]) },
        ToolDef { name: "update_persona", read_only: false,
            description: "Edit a persona's name, role or notes. Traits are not editable — every claim in a profile has to carry a citation.",
            schema: obj(json!({ "slug": s(), "display_name": s(), "role": s(), "notes": s() }), &["slug"]) },
        ToolDef { name: "delete_persona", read_only: false,
            description: "Stop modelling somebody, and delete everything derived from them: evidence, traits, predictions and identities.",
            schema: obj(json!({ "slug": s() }), &["slug"]) },
        ToolDef { name: "link_persona_identity", read_only: false,
            description: "Attach a GitHub login, Slack user id or Granola speaker name to a persona. Evidence is only ever harvested through a confirmed identity.",
            schema: obj(json!({ "slug": s(), "source": s(), "handle": s(), "provenance": s(), "rationale": s() }), &["slug", "source", "handle"]) },
        ToolDef { name: "unlink_persona_identity", read_only: false,
            description: "Detach a handle from whichever persona owns it.",
            schema: obj(json!({ "source": s(), "handle": s() }), &["source", "handle"]) },
        ToolDef { name: "harvest_persona", read_only: false,
            description: "Gather evidence for a persona now: their Slack and meeting activity from the signal log, and a bounded page of their GitHub review history.",
            schema: obj(json!({ "slug": s() }), &["slug"]) },
        ToolDef { name: "refresh_persona_profile", read_only: false,
            description: "Re-distil a persona's traits from everything harvested. Returns submitted=false when the profile is already current, which is a success. Pass force to redo it anyway.",
            schema: obj(json!({ "slug": s(), "force": {"type":"boolean"} }), &["slug"]) },
        ToolDef { name: "predict_persona", read_only: false,
            description: "Predict what one or more personas would do about an issue or pull request: the review they would leave, the comment they would write, or whether they would engage at all. Never posted anywhere.",
            schema: obj(json!({
                "subject_key": s(), "personas": { "type": "array", "items": s() }, "slug": s(),
                "kind": s(), "provider": s(), "model": s()
            }), &["subject_key"]) },
        ToolDef { name: "add_persona_context", read_only: false,
            description: "Attach something you know about a person that no excerpt could supply — 'owns the release process', 'prefers async review', or a URL to their team charter. Used verbatim and never filtered, and re-profiles so it takes effect immediately.",
            schema: obj(json!({ "slug": s(), "content": s() }), &["slug", "content"]) },
        ToolDef { name: "remove_persona_context", read_only: false,
            description: "Remove one attached fact from a persona.",
            schema: obj(json!({ "id": s() }), &["id"]) },
        ToolDef { name: "who_knows", read_only: true,
            description: "Who to ask about an area of the codebase — a repo, a path, or a word from one. Ranked across modelled people by where their review activity concentrates, distinguishing established expertise from mere presence. Only knows the people you have modelled, and says so.",
            schema: obj(json!({ "area": s() }), &["area"]) },
        ToolDef { name: "list_predictions", read_only: true,
            description: "Stored persona predictions, for one subject or by one persona.",
            schema: obj(json!({ "subject_key": s(), "slug": s() }), &[]) },
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

/// A required source name, refused rather than defaulted.
///
/// Defaulting would attach a Slack user id to GitHub, which harvests nothing and reads as a
/// quiet colleague rather than as a wrong argument.
fn req_source(args: &Value, key: &str) -> Result<Source> {
    let raw = req_str(args, key)?;
    Source::parse(&raw)
        .ok_or_else(|| anyhow!("{key} must be github, slack, granola or incident_io (got '{raw}')"))
}

/// Resolve a typed Slack handle to the workspace's own id.
///
/// The operator types `pavel` or `Pavel Cholakov`; the signal log records `U06T7445RHD`. Without
/// this the link silently harvests nothing, which is the worst available outcome: it looks like
/// a colleague who never posts. Exact alias match only — a fuzzy match here would produce a
/// confident wrong join, the one failure the identity model exists to prevent.
///
/// Unresolvable handles are left as typed rather than refused. A workspace with no `users:read`
/// scope has no directory, and a raw `U…` id typed by hand must still work.
fn resolve_slack_handle(store: &Store, identity: &mut crate::persona::Identity) -> Result<()> {
    if identity.source != Source::Slack {
        return Ok(());
    }
    if let Some(user) = store.find_slack_user(&identity.handle)? {
        if !user.id.eq_ignore_ascii_case(&identity.handle) {
            identity.rationale = Some(format!(
                "typed '{}', resolved to {} in the Slack directory",
                identity.handle,
                user.label()
            ));
            identity.handle = user.id;
        }
        return Ok(());
    }
    // Unknown to the workspace — refused *here*, where the mistake was made.
    //
    // The failure this replaces: a persona linked to `lukebond` when the workspace handle is
    // `luke`. `search.messages` answers 200 with zero matches for a handle nobody has, so the
    // persona harvested its GitHub half, no Slack, and reported nothing wrong. Catching it at
    // link time beats a note discovered later, and the near-matches make the fix obvious.
    let (_, members) = store.slack_directory_age()?;
    if members == 0 {
        // No directory to check against — a token without `users:read` must still be able to
        // link a handle, and the harvest reports what it finds.
        return Ok(());
    }
    let hint = store
        .slack_users_like(&identity.handle, 5)?
        .into_iter()
        .map(|u| u.label())
        .collect::<Vec<_>>();
    bail!(
        "'{}' is not a member of the Slack workspace ({members} cached).{}",
        identity.handle,
        if hint.is_empty() {
            " Check their handle in Slack.".to_string()
        } else {
            format!(" Did you mean: {}?", hint.join(", "))
        }
    )
}

/// One `{source, handle, provenance?, rationale?}` identity from the args.
///
/// Provenance defaults to `operator`, because the only way an identity reaches this function is
/// somebody naming it. The *guess* provenance is written by the proposal path, never here.
fn one_identity(args: &Value) -> Result<crate::persona::Identity> {
    let provenance = match opt_str(args, "provenance") {
        Some(p) => crate::persona::IdentityProvenance::parse(&p)
            .ok_or_else(|| anyhow!("provenance must be operator, exact or proposed (got '{p}')"))?,
        None => crate::persona::IdentityProvenance::Operator,
    };
    let mut identity = crate::persona::Identity::new(
        req_source(args, "source")?,
        req_str(args, "handle")?,
        provenance,
    );
    identity.rationale = opt_str(args, "rationale");
    Ok(identity)
}

/// The optional `identities: [{source, handle, ...}]` array, for persona creation.
fn identities_from(args: &Value) -> Result<Vec<crate::persona::Identity>> {
    let Some(list) = args.get("identities").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    list.iter().map(one_identity).collect()
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
            store: store.clone(),
            // Offline: a real ingress here points at 127.0.0.1:8080, which during development
            // is the operator's own running Restate server — so the suite invoked live handlers
            // against a database it never touched. See `Ingress::offline`.
            ingress: Arc::new(crate::restate::ingress::Ingress::offline()),
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
            threads: Arc::new(crate::thread::Analyser::for_tests(store.clone())),
            diffs: Arc::new(
                crate::prdiff::DiffReader::new(None, reasoner.clone(), "local").unwrap(),
            ),
            personas: Arc::new(crate::persona::Engine::for_tests(store)),
        }
    }

    /// One alert signal, with the entities that make it resolve to a Slack-thread subject.
    ///
    /// Extracted from [`seed`] so tests that need a *second* signal — or one with an actor on
    /// it, which is what persona harvesting reads — can build one without a second copy.
    fn sample_signal(external_id: &str) -> Signal {
        Signal {
            id: Signal::make_id(Source::Slack, external_id, None),
            source: Source::Slack,
            external_id: external_id.into(),
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
        }
    }

    fn seed(t: &Tools) -> String {
        let s = sample_signal("1");
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

    /// A persona's whole lifecycle over the tool surface, plus the two refusals that matter.
    ///
    /// The ingress is unreachable in tests, so `armed`/`harvesting` come back false — which is
    /// deliberate on the write path too: an unreachable Restate must not fail the creation,
    /// because the boot sweep arms the loop on the next start.
    #[tokio::test]
    async fn a_persona_is_created_edited_and_deleted_over_the_tool_surface() {
        let t = tools("noop");
        let created = t
            .call(
                "create_persona",
                &json!({
                    "display_name": "Pavel Cholakov",
                    "role": "storage lead",
                    "identities": [{ "source": "github", "handle": "pcholakov" }]
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["persona"]["slug"], "pavel-cholakov");
        assert_eq!(
            created["persona"]["identities"][0]["provenance"],
            "operator"
        );

        // Listed, with the counted stats rather than a modelled summary.
        let listed = t.call("list_personas", &json!({})).await.unwrap();
        assert_eq!(listed["personas"].as_array().unwrap().len(), 1);
        assert_eq!(listed["personas"][0]["stats"]["evidence"], 0);
        assert_eq!(listed["personas"][0]["traits"], 0);

        // A present-but-empty field clears; an absent one leaves alone. The two have to be
        // distinguishable or a note can be written and never removed.
        let edited = t
            .call(
                "update_persona",
                &json!({ "slug": "pavel-cholakov", "notes": "" }),
            )
            .await
            .unwrap();
        assert!(edited["notes"].is_null());
        assert_eq!(edited["role"], "storage lead", "role was not touched");

        // A handle already owned by another persona is refused, naming the owner — silently
        // re-pointing it would build two profiles from one person's writing.
        t.call(
            "create_persona",
            &json!({ "display_name": "Someone Else", "slug": "else" }),
        )
        .await
        .unwrap();
        let err = t
            .call(
                "link_persona_identity",
                &json!({ "slug": "else", "source": "github", "handle": "pcholakov" }),
            )
            .await
            .expect_err("a taken handle must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("already belongs to"), "{msg}");
        assert!(msg.contains("pavel-cholakov"), "{msg}");

        // Profiling before anything is harvested says so, rather than submitting a pass over
        // nothing.
        let err = t
            .call(
                "refresh_persona_profile",
                &json!({ "slug": "pavel-cholakov" }),
            )
            .await
            .expect_err("nothing harvested yet");
        assert!(format!("{err:#}").contains("nothing has been harvested"));

        // Deleting takes the derived material with it: a "deleted" persona that left its
        // harvested excerpts behind would be a profile of somebody you asked to stop
        // modelling.
        t.store
            .put_persona_evidence(&[crate::persona::Evidence {
                id: "e1".into(),
                persona: "pavel-cholakov".into(),
                source: Source::GitHub,
                kind: crate::persona::EvidenceKind::Review,
                subject_key: None,
                url: None,
                excerpt: "this needs a test on the retry path".into(),
                context: None,
                state: Some("CHANGES_REQUESTED".into()),
                occurred_at: chrono::Utc::now(),
                ingested_at: chrono::Utc::now(),
            }])
            .unwrap();
        assert_eq!(
            t.store
                .persona_evidence("pavel-cholakov", None)
                .unwrap()
                .len(),
            1
        );
        let deleted = t
            .call("delete_persona", &json!({ "slug": "pavel-cholakov" }))
            .await
            .unwrap();
        assert_eq!(deleted["deleted"], true);
        assert!(t
            .store
            .persona_evidence("pavel-cholakov", None)
            .unwrap()
            .is_empty());
        // The handle is released, so the other persona can now claim it.
        t.call(
            "link_persona_identity",
            &json!({ "slug": "else", "source": "github", "handle": "pcholakov" }),
        )
        .await
        .expect("the handle is free once its persona is gone");
    }

    /// The prediction kind follows the subject unless overridden, and a subject that does not
    /// exist is refused before any workflow is submitted.
    #[tokio::test]
    async fn predicting_needs_a_real_subject_and_a_real_persona() {
        let t = tools("noop");
        let subject = seed(&t);

        let err = t
            .call(
                "predict_persona",
                &json!({ "subject_key": "o/r#999", "slug": "nobody" }),
            )
            .await
            .expect_err("an unknown subject must be refused");
        assert!(format!("{err:#}").contains("no subject"));

        let err = t
            .call(
                "predict_persona",
                &json!({ "subject_key": subject, "slug": "nobody" }),
            )
            .await
            .expect_err("an unknown persona must be refused");
        assert!(format!("{err:#}").contains("no persona"));

        let err = t
            .call(
                "predict_persona",
                &json!({ "subject_key": subject, "slug": "nobody", "kind": "telepathy" }),
            )
            .await
            .expect_err("an unknown kind must be refused rather than guessed");
        assert!(format!("{err:#}").contains("kind must be"));
    }

    /// Proposals exclude people already modelled — matched on their linked handles as well as
    /// on the slug, or a persona named `pav` would keep having `pcholakov` proposed at it.
    #[tokio::test]
    async fn proposals_skip_handles_already_linked() {
        let t = tools("noop");
        for i in 0..4 {
            let mut s = crate::signal::Signal {
                actor: Some("U0PAVEL".into()),
                ..sample_signal(&format!("p{i}"))
            };
            s.body = Some("the retry path is the risk here, not the pool size".into());
            t.store.insert_signal(&s).unwrap();
        }
        let before = t.call("propose_personas", &json!({})).await.unwrap();
        assert_eq!(before["candidates"].as_array().unwrap().len(), 1);

        t.call(
            "create_persona",
            &json!({
                "display_name": "Pav",
                "slug": "pav",
                "identities": [{ "source": "slack", "handle": "U0PAVEL" }]
            }),
        )
        .await
        .unwrap();
        let after = t.call("propose_personas", &json!({})).await.unwrap();
        assert!(
            after["candidates"].as_array().unwrap().is_empty(),
            "a linked handle must not be proposed back: {}",
            after["candidates"]
        );
    }
}
