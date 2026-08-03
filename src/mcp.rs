//! MCP server (Phase 2) — MuggleBot over the Model Context Protocol.
//!
//! A minimal, dependency-free JSON-RPC 2.0 implementation of MCP, served over two
//! transports:
//!   - **stdio** — newline-delimited JSON messages on stdin/stdout, for a client
//!     that launches MuggleBot as a subprocess.
//!   - **HTTP** — a single POST endpoint (Streamable-HTTP style, JSON responses)
//!     on `localhost` for networked clients.
//!
//! Both dispatch through the shared [`crate::tools`] surface, so an interactive
//! Claude/ChatGPT session reasons over exactly the grounding and tools MuggleBot
//! uses ambiently. Read tools are free; write tools carry risk metadata (surfaced
//! in each tool's annotations).

use anyhow::Result;
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

use crate::tools::{self, Tools};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpServer {
    tools: Arc<Tools>,
}

impl McpServer {
    pub fn new(tools: Arc<Tools>) -> Self {
        Self { tools }
    }

    /// Handle one JSON-RPC message (or batch). Returns the response value, or
    /// `None` for a pure notification / empty batch.
    pub async fn handle(&self, msg: Value) -> Option<Value> {
        if let Some(arr) = msg.as_array() {
            let mut out = Vec::new();
            for item in arr {
                if let Some(resp) = self.handle_one(item).await {
                    out.push(resp);
                }
            }
            return if out.is_empty() {
                None
            } else {
                Some(Value::Array(out))
            };
        }
        self.handle_one(&msg).await
    }

    async fn handle_one(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));

        // A message without an id is a notification: act, don't reply.
        if id.is_none() {
            debug!("mcp notification: {method}");
            return None;
        }
        let id = id.unwrap();

        match method {
            "initialize" => Some(ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {}, "resources": {} },
                    "serverInfo": { "name": "mugglebot", "version": env!("CARGO_PKG_VERSION") },
                }),
            )),
            "ping" => Some(ok(id, json!({}))),
            "tools/list" => {
                let tools: Vec<Value> = tools::definitions()
                    .into_iter()
                    .map(|d| {
                        json!({
                            "name": d.name,
                            "description": d.description,
                            "inputSchema": d.schema,
                            "annotations": { "readOnlyHint": d.read_only },
                        })
                    })
                    .collect();
                Some(ok(id, json!({ "tools": tools })))
            }
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Some(self.tool_call(id, name, &args).await)
            }
            "resources/list" => {
                let resources: Vec<Value> = tools::resources()
                    .into_iter()
                    .map(|r| {
                        json!({
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mimeType": "application/json",
                        })
                    })
                    .collect();
                Some(ok(id, json!({ "resources": resources })))
            }
            "resources/read" => {
                let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                match self.tools.read_resource(uri).await {
                    Ok(v) => Some(ok(
                        id,
                        json!({ "contents": [{
                            "uri": uri,
                            "mimeType": "application/json",
                            "text": serde_json::to_string_pretty(&v).unwrap_or_default(),
                        }] }),
                    )),
                    Err(e) => Some(err(id, -32002, &format!("{e:#}"))),
                }
            }
            other => Some(err(id, -32601, &format!("method not found: {other}"))),
        }
    }

    /// A tool error is reported as a successful result with `isError: true`, per
    /// MCP, so the calling model sees and can react to the failure text.
    async fn tool_call(&self, id: Value, name: &str, args: &Value) -> Value {
        match self.tools.call(name, args).await {
            Ok(v) => ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                    "isError": false,
                }),
            ),
            Err(e) => ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": format!("error: {e:#}") }],
                    "isError": true,
                }),
            ),
        }
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Serve MCP over stdio: read newline-delimited JSON-RPC from stdin, write
/// responses to stdout. Returns when stdin reaches EOF. (Logs must go to stderr —
/// stdout is the transport.)
pub async fn serve_stdio(server: Arc<McpServer>) -> Result<()> {
    info!("mcp: serving over stdio");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!("mcp stdio: bad JSON: {e}");
                continue;
            }
        };
        if let Some(resp) = server.handle(msg).await {
            let mut buf = serde_json::to_vec(&resp)?;
            buf.push(b'\n');
            stdout.write_all(&buf).await?;
            stdout.flush().await?;
        }
    }
    info!("mcp: stdio closed");
    Ok(())
}

/// Serve MCP over HTTP on `addr` (a single JSON-RPC POST endpoint).
pub async fn serve_http(addr: String, server: Arc<McpServer>) -> Result<()> {
    let app = Router::new()
        .route("/", post(rpc))
        .route("/mcp", post(rpc))
        .with_state(server);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("mcp: HTTP JSON-RPC listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn rpc(State(server): State<Arc<McpServer>>, Json(msg): Json<Value>) -> Json<Value> {
    match server.handle(msg).await {
        Some(resp) => Json(resp),
        // Notification: reply with an empty ack object.
        None => Json(json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::correlation::Analyst;
    use crate::embed::HashEmbedder;
    use crate::memory::MemoryManager;
    use crate::reasoner::{MockReasoner, Reasoner};
    use crate::store::Store;
    use crate::subject::Attributor;
    use std::time::Duration;

    fn server() -> Arc<McpServer> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let secrets = crate::secrets::Secrets::for_tests(store.clone());
        let scorer = Arc::new(crate::score::Scorer {
            store: store.clone(),
            embedder: Arc::new(crate::embed::HashEmbedder),
        });
        let embedder = Arc::new(HashEmbedder);
        let reasoner: Arc<dyn Reasoner> = Arc::new(MockReasoner::new("ok"));
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
        Arc::new(McpServer::new(Arc::new(Tools {
            store,
            agents: Arc::new(crate::agent::AgentSessions::for_tests()),
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
            config: Arc::new(crate::config::Config::default()),
            investigator,
            repos,
            browser,
            diffs: Arc::new(crate::prdiff::DiffReader::new(None, reasoner.clone(), "local").unwrap()),
        })))
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let s = server();
        let init = s
            .handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
            .await
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "mugglebot");

        let list = s
            .handle(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "list_signals"));
        assert!(tools.iter().any(|t| t["name"] == "relate"));
    }

    #[tokio::test]
    async fn notification_has_no_response() {
        let s = server();
        let resp = s
            .handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn tools_call_put_and_list_memory() {
        let s = server();
        let call = s
            .handle(json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "put_memory", "arguments": { "text": "restart on OOM" } }
            }))
            .await
            .unwrap();
        assert_eq!(call["result"]["isError"], false);

        let list = s
            .handle(json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "list_memories", "arguments": {} }
            }))
            .await
            .unwrap();
        let text = list["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("restart on OOM"));
    }

    #[tokio::test]
    async fn read_config_resource_is_redacted() {
        let s = server();
        let r = s
            .handle(json!({
                "jsonrpc": "2.0", "id": 5, "method": "resources/read",
                "params": { "uri": "config://redacted" }
            }))
            .await
            .unwrap();
        let text = r["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(text.contains("local_model"), "config is exposed");
        // Behavior is public; secrets are not. Credentials live in the store, and
        // this resource is built from the config struct, which never holds them.
        //
        // Checked over string *values*, not the serialized text: `[secrets]` is a
        // legitimate config section (it holds `encrypt`), so a substring scan of the
        // whole document would trip on a key name and say nothing about a leak.
        let parsed: Value = serde_json::from_str(text).expect("config resource is JSON");
        let mut leaked = Vec::new();
        walk_strings(&parsed, &mut |s| {
            let lower = s.to_ascii_lowercase();
            for marker in ["xoxp-", "xoxb-", "ghp_", "secret", "token"] {
                if lower.contains(marker) {
                    leaked.push(format!("{marker} in {s:?}"));
                }
            }
        });
        assert!(leaked.is_empty(), "config resource leaked: {leaked:?}");
    }

    fn walk_strings(v: &Value, f: &mut impl FnMut(&str)) {
        match v {
            Value::String(s) => f(s),
            Value::Array(items) => items.iter().for_each(|i| walk_strings(i, f)),
            Value::Object(map) => map.values().for_each(|i| walk_strings(i, f)),
            _ => {}
        }
    }
}
