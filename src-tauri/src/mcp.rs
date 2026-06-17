//! Minimal MCP (Model Context Protocol) client over stdio.
//!
//! Servers are configured in the `mcp/servers` app setting as a JSON array:
//! `[{"name": "github", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
//!    "env": {"GITHUB_TOKEN": "..."}}]`
//!
//! Connections are spawned lazily on the first chat turn that needs tool defs,
//! kept alive for the life of the app, and exposed to the LLM as tools named
//! `mcp__<server>__<tool>`. The wire protocol is newline-delimited JSON-RPC 2.0
//! (initialize → notifications/initialized → tools/list → tools/call).

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::db::get_app_setting_value;
use crate::AppState;

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Don't retry a failed server more often than this — a broken config would
/// otherwise respawn a doomed child process on every chat turn.
const FAIL_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Deserialize, Clone)]
pub(crate) struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Clone)]
pub(crate) struct McpToolDef {
    pub server: String,
    pub tool: String,
    pub full_name: String,
    pub description: String,
    pub schema: Value,
}

struct McpConn {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpConn {
    async fn send_line(&mut self, msg: &Value) -> Result<(), String> {
        let line = format!("{}\n", msg);
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("MCP write failed: {}", e))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("MCP flush failed: {}", e))
    }

    /// Send a request and read lines until the response with our id arrives.
    /// Server-initiated notifications (no id / different id) are skipped.
    async fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send_line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;

        let read = async {
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = self
                    .stdout
                    .read_line(&mut buf)
                    .await
                    .map_err(|e| format!("MCP read failed: {}", e))?;
                if n == 0 {
                    return Err("MCP server closed its stdout".to_string());
                }
                let msg: Value = match serde_json::from_str(buf.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if msg.get("id").and_then(Value::as_i64) == Some(id) {
                    if let Some(err) = msg.get("error") {
                        return Err(format!("MCP error from '{}': {}", method, err));
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        };
        tokio::time::timeout(timeout, read)
            .await
            .map_err(|_| format!("MCP request '{}' timed out", method))?
    }
}

#[derive(Default)]
struct McpState {
    conns: HashMap<String, McpConn>,
    tools: Vec<McpToolDef>,
    /// server name → (when it failed, why). Cleared on reload.
    failed: HashMap<String, (Instant, String)>,
}

#[derive(Default)]
pub struct McpManager {
    inner: tokio::sync::Mutex<McpState>,
}

/// Keep tool names within the providers' `[a-zA-Z0-9_-]{1,64}` constraint.
fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn parse_configs(state: &AppState) -> Vec<McpServerConfig> {
    let raw = get_app_setting_value(state, "mcp/servers")
        .ok()
        .flatten()
        .unwrap_or_default();
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let configs: Vec<McpServerConfig> = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[MCP] invalid mcp/servers JSON: {}", e);
            return Vec::new();
        }
    };
    configs
        .into_iter()
        .filter(|c| !c.name.trim().is_empty() && !c.command.trim().is_empty())
        .collect()
}

/// GUI apps on macOS get a minimal PATH; prepend the usual install locations so
/// `npx`, `uvx`, etc. resolve the way they do in the user's terminal.
fn child_path() -> String {
    let base = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    format!(
        "/opt/homebrew/bin:/usr/local/bin:{home}/.local/bin:{home}/.cargo/bin:{base}",
    )
}

async fn spawn_server(cfg: &McpServerConfig) -> Result<(McpConn, Vec<McpToolDef>), String> {
    let mut child = Command::new(&cfg.command)
        .args(&cfg.args)
        .envs(&cfg.env)
        .env("PATH", child_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn '{}': {}", cfg.command, e))?;

    let stdin = child.stdin.take().ok_or("no stdin handle")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("no stdout handle")?);
    let mut conn = McpConn { _child: child, stdin, stdout, next_id: 0 };

    conn.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "muninn", "version": env!("CARGO_PKG_VERSION")}
        }),
        INIT_TIMEOUT,
    )
    .await?;
    conn.send_line(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await?;

    let result = conn.request("tools/list", json!({}), INIT_TIMEOUT).await?;
    let server_key = sanitize_name(cfg.name.trim());
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let tool = t.get("name").and_then(Value::as_str)?.to_string();
                    let mut full_name = format!("mcp__{}__{}", server_key, sanitize_name(&tool));
                    full_name.truncate(64);
                    Some(McpToolDef {
                        server: cfg.name.clone(),
                        tool,
                        full_name,
                        description: t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        schema: t
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok((conn, tools))
}

impl McpManager {
    /// Connect any configured-but-unconnected servers, then return all known tool defs.
    /// Failures are recorded (and retried after a cooldown) rather than propagated —
    /// a broken MCP server must never break the chat turn.
    pub(crate) async fn connected_tools(&self, state: &AppState) -> Vec<McpToolDef> {
        let configs = parse_configs(state);
        let mut st = self.inner.lock().await;

        // Drop connections whose config was removed.
        let names: Vec<String> = configs.iter().map(|c| c.name.clone()).collect();
        st.conns.retain(|name, _| names.contains(name));
        st.tools.retain(|t| names.contains(&t.server));

        for cfg in &configs {
            if st.conns.contains_key(&cfg.name) {
                continue;
            }
            if let Some((when, _)) = st.failed.get(&cfg.name) {
                if when.elapsed() < FAIL_RETRY_AFTER {
                    continue;
                }
            }
            match spawn_server(cfg).await {
                Ok((conn, tools)) => {
                    eprintln!("[MCP] connected '{}' ({} tools)", cfg.name, tools.len());
                    st.conns.insert(cfg.name.clone(), conn);
                    st.tools.retain(|t| t.server != cfg.name);
                    st.tools.extend(tools);
                    st.failed.remove(&cfg.name);
                }
                Err(e) => {
                    eprintln!("[MCP] failed to connect '{}': {}", cfg.name, e);
                    st.failed.insert(cfg.name.clone(), (Instant::now(), e));
                }
            }
        }
        st.tools.clone()
    }

    /// Call an MCP tool by its merged `mcp__server__tool` name.
    pub(crate) async fn call_tool(&self, full_name: &str, args: &Value) -> Result<String, String> {
        let mut st = self.inner.lock().await;
        let def = st
            .tools
            .iter()
            .find(|t| t.full_name == full_name)
            .cloned()
            .ok_or_else(|| format!("unknown MCP tool '{}'", full_name))?;
        let conn = st
            .conns
            .get_mut(&def.server)
            .ok_or_else(|| format!("MCP server '{}' is not connected", def.server))?;

        let arguments = if args.is_object() { args.clone() } else { json!({}) };
        let result = conn
            .request(
                "tools/call",
                json!({"name": def.tool, "arguments": arguments}),
                CALL_TIMEOUT,
            )
            .await;

        // A dead pipe means the server crashed — drop the connection so the next
        // turn respawns it instead of failing forever on a broken handle.
        let result = match result {
            Err(e) if e.contains("closed its stdout") || e.contains("write failed") => {
                st.conns.remove(&def.server);
                st.tools.retain(|t| t.server != def.server);
                Err(e)
            }
            other => other,
        };
        let result = result?;

        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                        Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                        Some(other) => Some(format!("[unsupported {} content]", other)),
                        None => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        if is_error {
            Err(if text.is_empty() { "MCP tool reported an error".to_string() } else { text })
        } else {
            Ok(text)
        }
    }

    /// Status for the settings UI: (name, connected, tool_count, last_error).
    pub(crate) async fn status(&self, state: &AppState) -> Vec<(String, bool, usize, Option<String>)> {
        let configs = parse_configs(state);
        let st = self.inner.lock().await;
        configs
            .iter()
            .map(|cfg| {
                let connected = st.conns.contains_key(&cfg.name);
                let tool_count = st.tools.iter().filter(|t| t.server == cfg.name).count();
                let error = st.failed.get(&cfg.name).map(|(_, e)| e.clone());
                (cfg.name.clone(), connected, tool_count, error)
            })
            .collect()
    }

    /// Kill all connections and clear failure cooldowns; the next chat turn (or an
    /// immediate `connected_tools`) reconnects from the current config.
    pub(crate) async fn reset(&self) {
        let mut st = self.inner.lock().await;
        st.conns.clear();
        st.tools.clear();
        st.failed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_provider_safe_chars() {
        assert_eq!(sanitize_name("git-hub_1"), "git-hub_1");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_name("my server.io"), "my_server_io");
        assert_eq!(sanitize_name("a/b:c"), "a_b_c");
    }
}
