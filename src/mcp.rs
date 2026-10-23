//! M2: minimal MCP (Model Context Protocol) stdio client.
//!
//! Newline-delimited JSON-RPC 2.0 over a child process's stdin/stdout:
//! `initialize` → `notifications/initialized` → `tools/list` → `tools/call`.
//! Tools are exposed to the model as `mcp__<server>__<tool>` functions.
//!
//! The protocol layer ([`Client`]) is generic over async read/write halves so
//! tests can drive it over in-memory duplex pipes; [`McpConn`] binds it to a
//! spawned child process.

use crate::config::McpServerCfg;
use crate::provider::{ToolFnSpec, ToolSpec};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Version we announce; servers answer with their own and we don't enforce it.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest tool result handed to the model, in bytes (B-19).
const MAX_RESULT_BYTES: usize = 60_000;
/// Providers accept function names of at most 64 characters (B-19).
const MAX_FN_NAME: usize = 64;
/// Longest server part of a model-facing tool name (B-19).
const MAX_SERVER_PART: usize = 24;

#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    /// `null` is accepted and becomes "" (B-28).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub description: String,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// Read a string field that may be `null` (B-28).
fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// JSON-RPC session over any async line-oriented transport.
pub struct Client<R, W> {
    reader: R,
    writer: W,
    next_id: i64,
    /// Bytes of a line not finished yet (B-28). `read_until` appends here and
    /// keeps them even when a timeout cancels the read.
    line: Vec<u8>,
}

impl<R, W> Client<R, W>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer, next_id: 0, line: Vec::new() }
    }

    async fn write_msg(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send a request and wait for the response with a matching id.
    /// Notifications and unrelated messages are skipped (minimal client:
    /// server-initiated requests are ignored rather than answered).
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_msg(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))
        .await
        .map_err(|e| {
            anyhow!(
                "mcp server unavailable during `{method}`: {e} (the server process has \
                 probably exited; restart opencode-rust to reconnect it)"
            )
        })?;
        loop {
            let n = self.reader.read_until(b'\n', &mut self.line).await?;
            if n == 0 {
                return Err(anyhow!(
                    "mcp server closed its stdout during `{method}` (the server process has \
                     probably exited; restart opencode-rust to reconnect it)"
                ));
            }
            if self.line.last() != Some(&b'\n') {
                continue; // end of stream inside a line; the next read returns 0
            }
            let raw = std::mem::take(&mut self.line);
            let Ok(msg) = serde_json::from_slice::<Value>(&raw) else {
                continue; // non-JSON noise on stdout: ignore
            };
            // B-28: a message with a `method` comes FROM the server.
            if let Some(server_method) = msg.get("method").and_then(Value::as_str) {
                if let Some(req_id) = msg.get("id").cloned() {
                    let reply = if server_method == "ping" {
                        json!({ "jsonrpc": "2.0", "id": req_id, "result": {} })
                    } else {
                        json!({ "jsonrpc": "2.0", "id": req_id, "error": {
                            "code": -32601,
                            "message": format!("`{server_method}` is not supported by this client")
                        }})
                    };
                    self.write_msg(&reply).await?;
                }
                continue; // requests are answered, notifications ignored
            }
            if msg.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // a reply to an older, timed-out request
            }
            if let Some(err) = msg.get("error") {
                return Err(anyhow!("mcp `{method}` failed: {err}"));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_msg(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    pub async fn initialize(&mut self) -> Result<Value> {
        let res = tokio::time::timeout(
            INIT_TIMEOUT,
            self.request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "opencode-rust", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
        )
        .await
        .context("mcp initialize timed out")??;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(res)
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        // `tools/list` must answer within this time (B-19).
        const LIST_TIMEOUT: Duration = Duration::from_secs(20);
        self.list_tools_within(LIST_TIMEOUT).await
    }

    /// `tools/list` with a time limit, so a silent server cannot hang startup.
    async fn list_tools_within(&mut self, limit: Duration) -> Result<Vec<McpTool>> {
        let res = tokio::time::timeout(limit, self.request("tools/list", json!({})))
            .await
            .context("mcp tools/list timed out")??;
        // B-28: servers may split the list into pages (`nextCursor`), and one
        // malformed entry must not cost the whole server.
        const MAX_TOOL_PAGES: usize = 20;
        let mut tools = Vec::new();
        let mut page = res;
        for _ in 0..MAX_TOOL_PAGES {
            let arr = page
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("tools/list response has no `tools` array"))?;
            for t in arr {
                match serde_json::from_value::<McpTool>(t.clone()) {
                    Ok(tool) => tools.push(tool),
                    Err(e) => eprintln!("[mcp] skipping a tool entry in an unexpected shape: {e}"),
                }
            }
            let Some(cursor) = page.get("nextCursor").and_then(Value::as_str).map(str::to_string)
            else {
                break;
            };
            page = tokio::time::timeout(limit, self.request("tools/list", json!({ "cursor": cursor })))
                .await
                .context("mcp tools/list timed out")??;
        }
        Ok(tools)
    }

    /// Call a tool; returns concatenated text content. Server-reported
    /// `isError` results come back as `Err` carrying the text.
    pub async fn call_tool(&mut self, name: &str, args: &Value) -> Result<String> {
        let res = tokio::time::timeout(
            CALL_TIMEOUT,
            self.request("tools/call", json!({ "name": name, "arguments": args })),
        )
        .await
        .context("mcp tools/call timed out")??;
        let is_error = res.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let text = res
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| {
                        if p.get("type").and_then(Value::as_str) == Some("text") {
                            p.get("text").and_then(Value::as_str).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if is_error {
            Err(anyhow!("{}", if text.is_empty() { "tool error".to_string() } else { cap_result(text) }))
        } else if text.is_empty() {
            Ok("(no output)".into())
        } else {
            Ok(cap_result(text))
        }
    }
}

/// Cut an over-long tool result to MAX_RESULT_BYTES (B-19).
fn cap_result(text: String) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text;
    }
    let mut cut = MAX_RESULT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [mcp result truncated: {} bytes in total]", &text[..cut], text.len())
}

/// Server names become part of a function name: keep [A-Za-z0-9-], fold the
/// rest to `-`, so `mcp__<server>__<tool>` parsing stays unambiguous.
/// B-19: at most MAX_SERVER_PART characters, so the tool part has room.
pub fn sanitize_server(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .take(MAX_SERVER_PART)
        .collect()
}

/// Short, stable hash (FNV-1a, 32 bit): same input, same output, every run.
fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// The model-facing function name for an MCP tool, always matching
/// `^[a-zA-Z0-9_-]{1,64}$` (B-19). [`resolve_tool_name`] maps it back.
pub fn tool_fn_name(server: &str, tool: &str) -> String {
    let prefix = format!("mcp__{}__", sanitize_server(server));
    let clean: String = tool
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let full = format!("{prefix}{clean}");
    if clean == tool && !clean.is_empty() && full.len() <= MAX_FN_NAME {
        return full;
    }
    // Room for the readable part: 64 minus the prefix, minus "_" + 8 hex digits.
    let room = MAX_FN_NAME - prefix.len() - 9;
    let head: String = clean.chars().take(room).collect();
    format!("{prefix}{head}_{}", short_hash(tool))
}

/// Inverse of [`tool_fn_name`]; `None` when the name isn't MCP-shaped.
pub fn parse_fn_name(fn_name: &str) -> Option<(String, String)> {
    let rest = fn_name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_string(), tool.to_string()))
}

/// The real MCP tool name behind the tool part of `mcp__<server>__<part>`
/// (B-19). Falls back to `part` itself when no tool matches.
pub fn resolve_tool_name(server: &str, tools: &[McpTool], part: &str) -> String {
    let wanted = format!("mcp__{server}__{part}");
    tools
        .iter()
        .find(|t| tool_fn_name(server, &t.name) == wanted)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| part.to_string())
}

/// OpenAI function spec for an MCP tool.
pub fn spec_for(server: &str, t: &McpTool) -> ToolSpec {
    let mut params = t.input_schema.clone();
    if !params.is_object() {
        params = json!({ "type": "object", "additionalProperties": true });
    } else if params.get("type").is_none() {
        params["type"] = json!("object");
    }
    let desc = if t.description.is_empty() {
        "no description".to_string()
    } else {
        t.description.clone()
    };
    ToolSpec {
        kind: "function".into(),
        function: ToolFnSpec {
            name: tool_fn_name(server, &t.name),
            description: format!("[mcp:{server}] {desc}"),
            parameters: params,
        },
    }
}

/// The running server process and its JSON-RPC session (B-20). Each server
/// has its own lock around this, so one slow server never blocks another.
pub struct Live {
    #[allow(dead_code)] // kept alive; kill_on_drop kills the child on drop
    child: Option<tokio::process::Child>, // F-03: None for HTTP servers
    pub client: Transport,
}

/// How we talk to one MCP server (F-03).
pub enum Transport {
    /// A child process: newline-delimited JSON-RPC on stdin/stdout.
    Stdio(Client<BufReader<tokio::process::ChildStdout>, tokio::process::ChildStdin>),
    /// Streamable HTTP (`url` in the config).
    Http(crate::mcp_http::HttpClient),
}

impl Transport {
    /// Same contract as the stdio [`Client::call_tool`], with B-19's size cap.
    pub async fn call_tool(&mut self, name: &str, args: &Value) -> Result<String> {
        match self {
            Transport::Stdio(c) => c.call_tool(name, args).await,
            Transport::Http(c) => match c.call_tool(name, args).await {
                Ok(text) => Ok(cap_result(text)),
                Err(e) => Err(anyhow!("{}", cap_result(format!("{e:#}")))),
            },
        }
    }
}

/// One live MCP server connection (child process + session + tool cache).
pub struct McpConn {
    pub live: std::sync::Arc<tokio::sync::Mutex<Live>>,
    pub tools: Vec<McpTool>,
    pub server: String,
}

impl McpConn {
    pub async fn spawn(name: &str, cfg: &McpServerCfg) -> Result<Self> {
        let server = sanitize_server(name);
        // F-03: a `url` means Streamable HTTP; no process is started.
        if let Some(url) = cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            let mut client = crate::mcp_http::HttpClient::new(url, &cfg.headers)?;
            client.initialize().await?;
            let mut tools = client.list_tools().await?;
            let mut seen = std::collections::HashSet::new();
            tools.retain(|t| seen.insert(tool_fn_name(&server, &t.name)));
            let live = Live { child: None, client: Transport::Http(client) };
            let live = std::sync::Arc::new(tokio::sync::Mutex::new(live));
            return Ok(Self { live, tools, server });
        }
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // S-05: MCP servers never see our provider key or API token.
        crate::tools::scrub_secrets(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning `{}`", cfg.command))?;
        let stdin = child.stdin.take().context("mcp child has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("mcp child has no stdout")?);
        let mut client = Client::new(stdout, stdin);
        client.initialize().await?;
        let mut tools = client.list_tools().await?;
        // B-19: two tools with the same model-facing name would be ambiguous.
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| {
            let fresh = seen.insert(tool_fn_name(&server, &t.name));
            if !fresh {
                eprintln!("[mcp] {server}: skipping tool `{}` (its name collides with another tool)", t.name);
            }
            fresh
        });
        let live = Live { child: Some(child), client: Transport::Stdio(client) };
        let live = std::sync::Arc::new(tokio::sync::Mutex::new(live));
        Ok(Self { live, tools, server })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| spec_for(&self.server, t)).collect()
    }

    pub async fn call(&self, tool: &str, args: &Value) -> Result<String> {
        self.live.lock().await.client.call_tool(tool, args).await
    }
}

/// All configured MCP servers, keyed by sanitized server name.
#[derive(Default)]
pub struct Registry {
    conns: std::collections::HashMap<String, McpConn>,
}

impl Registry {
    /// Start every not-yet-running server. Failures are logged, not fatal —
    /// one broken MCP config must not stop the agent from booting.
    pub async fn start_all(&mut self, servers: &[McpServerCfg]) {
        // B-19: all servers start at the same time; each start is bounded by
        // the initialize and tools/list time limits.
        let mut starts = Vec::new();
        let mut planned = std::collections::HashSet::new();
        for cfg in servers {
            let has_url = cfg.url.as_deref().is_some_and(|u| !u.trim().is_empty());
            if cfg.name.is_empty() || (cfg.command.is_empty() && !has_url) {
                continue;
            }
            let key = sanitize_server(&cfg.name);
            if self.conns.contains_key(&key) {
                continue; // already running
            }
            if !planned.insert(key.clone()) {
                eprintln!("[mcp] skipping `{}`: another server already uses the name `{key}`", cfg.name);
                continue;
            }
            starts.push(async move { (key, cfg, McpConn::spawn(&cfg.name, cfg).await) });
        }
        for (key, cfg, result) in futures::future::join_all(starts).await {
            match result {
                Ok(conn) => {
                    eprintln!(
                        "[mcp] {} connected ({} tool(s))",
                        conn.server,
                        conn.tools.len()
                    );
                    self.conns.insert(key, conn);
                }
                Err(e) => eprintln!("[mcp] failed to start `{}`: {e:#}", cfg.name),
            }
        }
    }

    /// Sorted by name: `conns` is a HashMap, and the tool list is part of the
    /// prompt start that providers cache, so its order must never change.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.conns.values().flat_map(|c| c.specs()).collect();
        specs.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        specs
    }

    /// Find the server behind a model-facing MCP name and return a handle
    /// that calls it WITHOUT the registry being locked (B-20).
    pub fn route(&self, fn_name: &str) -> Result<McpRoute> {
        let (server, tool) =
            parse_fn_name(fn_name).ok_or_else(|| anyhow!("`{fn_name}` is not an MCP tool"))?;
        let conn = self
            .conns
            .get(&server)
            .ok_or_else(|| anyhow!("mcp server `{server}` is not connected"))?;
        Ok(McpRoute {
            live: conn.live.clone(),
            tool: resolve_tool_name(&conn.server, &conn.tools, &tool),
        })
    }

    pub async fn call(&mut self, fn_name: &str, args: &Value) -> Result<String> {
        let (server, tool) =
            parse_fn_name(fn_name).ok_or_else(|| anyhow!("`{fn_name}` is not an MCP tool"))?;
        let conn = self
            .conns
            .get_mut(&server)
            .ok_or_else(|| anyhow!("mcp server `{server}` is not connected"))?;
        let real = resolve_tool_name(&conn.server, &conn.tools, &tool);
        conn.call(&real, args).await
    }

    pub fn connected(&self) -> Vec<&str> {
        self.conns.keys().map(String::as_str).collect()
    }
}

/// One MCP tool, ready to call without holding the registry lock (B-20).
pub struct McpRoute {
    live: std::sync::Arc<tokio::sync::Mutex<Live>>,
    tool: String,
}

impl McpRoute {
    pub async fn call(&self, args: &Value) -> Result<String> {
        self.live.lock().await.client.call_tool(&self.tool, args).await
    }
}

/// More MCP tools than this are not listed one by one (every spec is sent
/// with every request): the model gets `mcp_search` to find a tool and
/// `mcp_call` to run it.
pub const INLINE_MAX: usize = 12;

/// The two tools that stand in for many MCP tools.
pub fn lazy_specs(count: usize) -> Vec<ToolSpec> {
    let spec = |name: &str, description: String, parameters: Value| ToolSpec {
        kind: "function".into(),
        function: ToolFnSpec { name: name.into(), description, parameters },
    };
    vec![
        spec(
            "mcp_search",
            format!("Find tools among the {count} connected MCP tools; returns names and arguments for mcp_call."),
            json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
        ),
        spec(
            "mcp_call",
            "Run an MCP tool found with mcp_search.".to_string(),
            json!({"type": "object", "properties": {"name": {"type": "string"}, "arguments": {"type": "object"}}, "required": ["name"]}),
        ),
    ]
}

/// `mcp_call {name, arguments}` → the MCP call it stands for. Only MCP
/// names unwrap, so `mcp_call` cannot reach other tools.
pub fn unwrap_call(name: &str, input: &Value) -> Option<(String, Value)> {
    if name != "mcp_call" {
        return None;
    }
    let inner = input.get("name").and_then(Value::as_str)?.trim();
    parse_fn_name(inner)?;
    let args = input.get("arguments").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    Some((inner.to_string(), args))
}

/// Best matches for `query` among `specs`: name and arguments of each, for
/// `mcp_call`. Name hits count more than description hits.
pub fn search(specs: &[ToolSpec], query: &str, limit: usize) -> String {
    let words: Vec<String> = query.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(String::from).collect();
    let mut ranked: Vec<(usize, &ToolSpec)> = specs
        .iter()
        .map(|s| {
            let name = s.function.name.to_lowercase();
            let desc = s.function.description.to_lowercase();
            let score = words.iter().map(|w| 3 * usize::from(name.contains(w.as_str())) + usize::from(desc.contains(w.as_str()))).sum();
            (score, s)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.function.name.cmp(&b.1.function.name)));
    if ranked.is_empty() {
        let names: Vec<&str> = specs.iter().take(40).map(|s| s.function.name.as_str()).collect();
        return format!("no MCP tool matches \"{query}\". Available: {}", names.join(", "));
    }
    let mut out = String::new();
    for (_, s) in ranked.into_iter().take(limit) {
        let desc: String = s.function.description.chars().take(200).collect();
        let props = s.function.parameters.get("properties").and_then(Value::as_object);
        let required: Vec<&str> = s
            .function
            .parameters
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let args: Vec<String> = props
            .map(|p| {
                p.iter()
                    .map(|(k, v)| {
                        let ty = v.get("type").and_then(Value::as_str).unwrap_or("any");
                        let mark = if required.contains(&k.as_str()) { "" } else { "?" };
                        format!("{k}{mark}: {ty}")
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push_str(&format!("{} — {desc}\n  arguments: {{{}}}\n", s.function.name, args.join(", ")));
    }
    out.push_str("Run one with mcp_call {\"name\": <name>, \"arguments\": {...}}.");
    out
}

#[cfg(test)]
mod lazy_tests {
    use super::*;

    #[test]
    fn search_ranks_by_name_and_mcp_call_only_unwraps_mcp_names() {
        let t = |name: &str, desc: &str| McpTool {
            name: name.into(),
            description: desc.into(),
            input_schema: json!({"type": "object", "properties": {"repo": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["repo"]}),
        };
        let specs = vec![spec_for("gh", &t("list_issues", "List issues")), spec_for("gh", &t("create_pr", "Open a pull request for issues"))];
        let out = search(&specs, "issues", 5);
        assert!(out.find("list_issues").unwrap() < out.find("create_pr").unwrap(), "{out}");
        assert!(out.contains("arguments: {limit?: integer, repo: string}"), "{out}");
        assert!(search(&specs, "weather", 5).starts_with("no MCP tool matches"));
        let call = json!({"name": "mcp__gh__list_issues", "arguments": {"repo": "a/b"}});
        assert_eq!(unwrap_call("mcp_call", &call), Some(("mcp__gh__list_issues".into(), json!({"repo": "a/b"}))));
        assert_eq!(unwrap_call("mcp_call", &json!({"name": "bash", "arguments": {}})), None, "not an MCP tool");
        assert_eq!(lazy_specs(30).len(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, ReadHalf, WriteHalf};

    /// Scripted MCP server over one duplex end: answers each request from a
    /// queue of canned results keyed by method.
    async fn scripted_server(mut stream: tokio::io::DuplexStream, canned: Vec<(String, Value)>) {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 512];
        loop {
            let n = match stream.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let Ok(v) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) else {
                    continue;
                };
                let Some(id) = v.get("id").cloned() else { continue }; // notification
                let method =
                    v.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let result = canned
                    .iter()
                    .find(|(m, _)| *m == method)
                    .map(|(_, r)| r.clone())
                    .unwrap_or(Value::Object(Default::default()));
                let mut out =
                    serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                        .unwrap();
                out.push('\n');
                if stream.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                let _ = stream.flush().await;
            }
        }
    }

    fn canned() -> Vec<(String, Value)> {
        vec![
            (
                "initialize".into(),
                json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "serverInfo": {"name": "fake", "version": "0"}}),
            ),
            (
                "tools/list".into(),
                json!({"tools": [{
                    "name": "echo",
                    "description": "Echoes text back",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
                }]}),
            ),
            (
                "tools/call".into(),
                json!({"content": [{"type": "text", "text": "echo: hi"}], "isError": false}),
            ),
        ]
    }

    fn client_over_duplex() -> (
        Client<BufReader<ReadHalf<tokio::io::DuplexStream>>, WriteHalf<tokio::io::DuplexStream>>,
        tokio::io::DuplexStream,
    ) {
        let (client_end, server_end) = duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_end);
        (Client::new(BufReader::new(r), w), server_end)
    }

    #[tokio::test]
    async fn handshake_list_and_call_over_duplex() {
        let (mut client, server) = client_over_duplex();
        let handle = tokio::spawn(scripted_server(server, canned()));

        client.initialize().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "Echoes text back");
        assert!(tools[0].input_schema["properties"]["text"].is_object());

        let out = client.call_tool("echo", &json!({"text": "hi"})).await.unwrap();
        assert_eq!(out, "echo: hi");
        drop(client); // close the pipe so the server loop sees EOF
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rpc_errors_and_iserror_surface_as_err() {
        let (client_end, server_end) = duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_end);
        let mut client = Client::new(BufReader::new(r), w);
        let handle = tokio::spawn(async move {
            let (rs, mut ws) = tokio::io::split(server_end);
            let mut lines = BufReader::new(rs);
            let mut buf = String::new();
            while lines.read_line(&mut buf).await.unwrap_or(0) > 0 {
                let v: Value = match serde_json::from_str(buf.trim()) {
                    Ok(v) => v,
                    Err(_) => {
                        buf.clear();
                        continue;
                    }
                };
                buf.clear();
                let Some(id) = v.get("id").cloned() else { continue };
                let method = v.get("method").and_then(Value::as_str).unwrap_or("");
                let resp = match method {
                    "tools/list" => json!({"jsonrpc":"2.0","id":id,
                        "error":{"code":-32601,"message":"no tools here"}}),
                    "tools/call" => json!({"jsonrpc":"2.0","id":id,"result":{
                        "content":[{"type":"text","text":"boom"}],"isError":true}}),
                    _ => json!({"jsonrpc":"2.0","id":id,"result":{}}),
                };
                let mut s = serde_json::to_string(&resp).unwrap();
                s.push('\n');
                if ws.write_all(s.as_bytes()).await.is_err() {
                    return;
                }
                let _ = ws.flush().await;
            }
        });

        client.initialize().await.unwrap();
        let err = client.list_tools().await.unwrap_err().to_string();
        assert!(err.contains("no tools here"), "rpc error text surfaced: {err}");
        let err = client.call_tool("x", &json!({})).await.unwrap_err().to_string();
        assert_eq!(err, "boom", "isError content surfaced");
        drop(client); // close the pipe so the server loop sees EOF
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn server_exit_mid_request_is_an_error() {
        let (client_end, server_end) = duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_end);
        let mut client = Client::new(BufReader::new(r), w);
        drop(server_end); // server vanishes before answering
        let err = client.request("tools/list", json!({})).await.unwrap_err();
        // Whether the write fails or the read hits EOF, the error names the
        // method it was in flight for.
        assert!(
            err.to_string().contains("tools/list"),
            "error names the method: {err}"
        );
    }

    #[test]
    fn fn_name_roundtrip_and_sanitization() {
        assert_eq!(tool_fn_name("my-server", "do_thing"), "mcp__my-server__do_thing");
        assert_eq!(tool_fn_name("we ird!", "t"), "mcp__we-ird-__t");
        assert_eq!(
            parse_fn_name("mcp__my-server__do_thing"),
            Some(("my-server".into(), "do_thing".into()))
        );
        // Tool names containing `__` still parse (split at first `__`).
        assert_eq!(parse_fn_name("mcp__srv__a__b"), Some(("srv".into(), "a__b".into())));
        assert_eq!(parse_fn_name("bash"), None);
        assert_eq!(parse_fn_name("mcp____x"), None);
        assert_eq!(parse_fn_name("mcp__onlyserver"), None);
    }

    #[test]
    fn spec_for_coerces_schema_to_object() {
        let t = McpTool {
            name: "echo".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        };
        let s = spec_for("srv", &t);
        assert_eq!(s.function.name, "mcp__srv__echo");
        assert!(s.function.description.contains("[mcp:srv] no description"));
        assert_eq!(s.function.parameters["type"], "object");

        let bare = McpTool { name: "x".into(), description: "d".into(), input_schema: Value::Null };
        let s = spec_for("srv", &bare);
        assert_eq!(s.function.parameters["type"], "object");
    }

    /// Real-process test: a tiny node MCP server over actual stdio pipes.
    #[tokio::test]
    async fn spawn_real_process_over_stdio() {
        let node = match which_node() {
            Some(n) => n,
            None => {
                eprintln!("skip: node not found on PATH");
                return;
            }
        };
        let script = r#"
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
const send = (o) => process.stdout.write(JSON.stringify(o) + '\n');
rl.on('line', (line) => {
  let m; try { m = JSON.parse(line); } catch { return; }
  if (m.id === undefined) return;
  if (m.method === 'initialize') send({jsonrpc:'2.0',id:m.id,result:{protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'fake',version:'0'}}});
  else if (m.method === 'tools/list') send({jsonrpc:'2.0',id:m.id,result:{tools:[{name:'echo',description:'Echoes',inputSchema:{type:'object',properties:{text:{type:'string'}}}}]}});
  else if (m.method === 'tools/call') send({jsonrpc:'2.0',id:m.id,result:{content:[{type:'text',text:'echo: '+m.params.arguments.text}],isError:false}});
  else send({jsonrpc:'2.0',id:m.id,result:{}});
});
"#;
        let dir = std::env::temp_dir().join(format!(
            "ocrs-mcp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fake_mcp.js");
        std::fs::write(&path, script).unwrap();

        let cfg = McpServerCfg {
            name: "fake".into(),
            command: node,
            args: vec![path.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let conn = McpConn::spawn("fake", &cfg).await.expect("spawn node server");
        assert_eq!(conn.tools.len(), 1);
        let specs = conn.specs();
        assert_eq!(specs[0].function.name, "mcp__fake__echo");
        let out = conn.call("echo", &json!({"text": "hi"})).await.unwrap();
        assert_eq!(out, "echo: hi");

        // Registry-level routing by qualified name.
        let mut reg = Registry::default();
        reg.conns.insert("fake".into(), conn);
        let out =
            reg.call("mcp__fake__echo", &json!({"text": "via reg"})).await.unwrap();
        assert_eq!(out, "echo: via reg");
        assert!(reg.call("mcp__nope__x", &json!({})).await.is_err());
        assert!(reg.call("bash", &json!({})).await.is_err());
        assert_eq!(reg.connected(), vec!["fake"]);

        drop(reg); // kills the child via kill_on_drop
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn which_node() -> Option<String> {
        for candidate in ["node", "nodejs"] {
            if let Ok(out) = std::process::Command::new(candidate).arg("--version").output() {
                if out.status.success() {
                    return Some(candidate.to_string());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod b19_names_tests {
    use super::*;

    #[test]
    fn tool_names_are_provider_safe_and_map_back() {
        let long = "x".repeat(100);
        let names = ["read.file", "a b/c", long.as_str(), "plain_name", "a_b", "a.b", ""];
        let tools: Vec<McpTool> = names
            .iter()
            .map(|n| McpTool { name: n.to_string(), description: String::new(), input_schema: Value::Null })
            .collect();
        let server = "my server with a very long name indeed";
        let mut seen = std::collections::HashSet::new();
        for t in &tools {
            let fname = tool_fn_name(server, &t.name);
            let safe = fname.len() <= MAX_FN_NAME
                && fname.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            assert!(safe, "{fname}");
            assert!(seen.insert(fname.clone()), "duplicate name {fname}");
            let (srv, part) = parse_fn_name(&fname).expect("mcp-shaped");
            assert_eq!(resolve_tool_name(&srv, &tools, &part), t.name);
        }
        assert_eq!(tool_fn_name("srv", "plain_name"), "mcp__srv__plain_name");
    }

    #[test]
    fn huge_results_are_cut() {
        let out = cap_result("é".repeat(MAX_RESULT_BYTES));
        assert!(out.len() < MAX_RESULT_BYTES + 100, "{}", out.len());
        assert!(out.contains("truncated"));
        assert_eq!(cap_result("short".into()), "short");
    }
}

#[cfg(test)]
mod b19_startup_tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt};

    #[tokio::test]
    async fn list_tools_gives_up_on_a_silent_server() {
        let (client_end, mut server_end) = duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_end);
        let mut client = Client::new(BufReader::new(r), w);
        // This "server" reads everything and never answers.
        let silent = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = server_end.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });
        let err = client.list_tools_within(Duration::from_millis(200)).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        drop(client);
        let _ = silent.await;
    }

    /// Two servers that each take 1.5 s to answer `initialize` are started
    /// together: the second gets its `initialize` before the first has
    /// answered (one after another, it would come >= 1.5 s later). Comparing
    /// those two moments, not the total time, keeps the test steady on a
    /// busy machine where node itself starts slowly.
    /// Each lists `echo` twice; one is dropped.
    #[tokio::test]
    async fn servers_start_at_the_same_time() {
        let node = ["node", "nodejs"].iter().find(|c| {
            std::process::Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
        });
        let Some(node) = node else {
            eprintln!("skip: node not found on PATH");
            return;
        };
        let script = r#"
const delay = Number(process.argv[2] || 0);
const stamp = process.argv[3];
const rl = require('readline').createInterface({ input: process.stdin });
const send = (o) => process.stdout.write(JSON.stringify(o) + '\n');
rl.on('line', (line) => {
  let m; try { m = JSON.parse(line); } catch { return; }
  if (m.id === undefined) return;
  if (m.method === 'initialize' && stamp) require('fs').writeFileSync(stamp, String(Date.now()));
  if (m.method === 'initialize') setTimeout(() => send({jsonrpc:'2.0',id:m.id,result:{protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'slow',version:'0'}}}), delay);
  else if (m.method === 'tools/list') send({jsonrpc:'2.0',id:m.id,result:{tools:[{name:'echo',inputSchema:{type:'object'}},{name:'echo',inputSchema:{type:'object'}}]}});
  else send({jsonrpc:'2.0',id:m.id,result:{content:[{type:'text',text:'ok'}]}});
});
"#;
        let dir = std::env::temp_dir().join(format!("ocrs-b19-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slow_mcp.js");
        std::fs::write(&path, script).unwrap();
        let cfg = |name: &str| McpServerCfg {
            name: name.into(),
            command: node.to_string(),
            args: vec![
                path.to_string_lossy().into_owned(),
                "1500".into(),
                dir.join(format!("{name}.stamp")).to_string_lossy().into_owned(),
            ],
            ..Default::default() // keeps compiling when McpServerCfg gets more fields (F-03)
        };
        let mut reg = Registry::default();
        reg.start_all(&[cfg("a"), cfg("b")]).await;
        let mut names = reg.connected();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
        let stamp = |n: &str| -> i64 {
            std::fs::read_to_string(dir.join(format!("{n}.stamp"))).unwrap().trim().parse().unwrap()
        };
        let gap = (stamp("a") - stamp("b")).abs();
        assert!(gap < 1500, "the second server waited for the first: initialize {gap} ms apart");
        assert_eq!(reg.specs().len(), 2, "the duplicate `echo` is dropped on each server");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod b20_tests {
    use super::*;

    fn node_bin() -> Option<String> {
        ["node", "nodejs"].iter().find_map(|c| {
            let ok = std::process::Command::new(c)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            ok.then(|| c.to_string())
        })
    }

    /// A slow server (1.5 s per call) must not hold up a fast one, and the
    /// registry lock must be free while the slow call runs.
    #[tokio::test]
    async fn calls_to_different_servers_run_in_parallel() {
        let Some(node) = node_bin() else {
            eprintln!("skip: node not found on PATH");
            return;
        };
        let script = r#"
const delay = Number(process.argv[2] || 0);
const rl = require('readline').createInterface({ input: process.stdin });
const send = (o) => process.stdout.write(JSON.stringify(o) + '\n');
rl.on('line', (line) => {
  let m; try { m = JSON.parse(line); } catch { return; }
  if (m.id === undefined) return;
  if (m.method === 'initialize') send({jsonrpc:'2.0',id:m.id,result:{protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'t',version:'0'}}});
  else if (m.method === 'tools/list') send({jsonrpc:'2.0',id:m.id,result:{tools:[{name:'echo',inputSchema:{type:'object'}}]}});
  else setTimeout(() => send({jsonrpc:'2.0',id:m.id,result:{content:[{type:'text',text:'ok'}]}}), delay);
});
"#;
        let dir = std::env::temp_dir().join(format!("ocrs-b20-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timed_mcp.js");
        std::fs::write(&path, script).unwrap();
        let cfg = |name: &str, delay: &str| McpServerCfg {
            name: name.into(),
            command: node.clone(),
            args: vec![path.to_string_lossy().into_owned(), delay.into()],
            ..Default::default() // keeps compiling when McpServerCfg gets more fields (F-03)
        };
        let mut reg = Registry::default();
        reg.start_all(&[cfg("slow", "1500"), cfg("fast", "0")]).await;
        // Same shape as AppState: the registry behind an async mutex.
        let reg = tokio::sync::Mutex::new(reg);

        let slow = reg.lock().await.route("mcp__slow__echo").unwrap();
        let slow_call = tokio::spawn(async move { slow.call(&serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(reg.try_lock().is_ok(), "registry lock is free during a call");
        let started = std::time::Instant::now();
        let fast = reg.lock().await.route("mcp__fast__echo").unwrap();
        assert_eq!(fast.call(&serde_json::json!({})).await.unwrap(), "ok");
        assert!(started.elapsed() < Duration::from_millis(1000), "fast call waited for the slow one");

        assert_eq!(slow_call.await.unwrap().unwrap(), "ok");
        assert!(reg.lock().await.route("mcp__nope__echo").is_err());
        assert!(reg.lock().await.route("bash").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod b28_reply_tests {
    use super::*;
    use tokio::io::duplex;

    async fn send(ws: &mut (impl AsyncWrite + Unpin), v: Value) {
        ws.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        ws.flush().await.unwrap();
    }

    #[tokio::test]
    async fn server_requests_are_answered_not_taken_as_replies() {
        let (client_end, server_end) = duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_end);
        let mut client = Client::new(BufReader::new(r), w);
        let server = tokio::spawn(async move {
            let (rs, mut ws) = tokio::io::split(server_end);
            let mut lines = BufReader::new(rs).lines();
            let req: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let id = req["id"].clone();
            // The server pings first, re-using the same id number.
            send(&mut ws, json!({"jsonrpc": "2.0", "id": id, "method": "ping"})).await;
            let pong: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            send(&mut ws, json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "t"}]}})).await;
            pong
        });
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        let pong = server.await.unwrap();
        assert!(pong.get("result").is_some() && pong.get("method").is_none(), "{pong}");
    }

    #[tokio::test]
    async fn a_stopped_server_says_how_to_recover() {
        let (client_end, server_end) = duplex(1024);
        let (r, w) = tokio::io::split(client_end);
        let mut client = Client::new(BufReader::new(r), w);
        drop(server_end);
        let err = client.request("tools/call", json!({})).await.unwrap_err().to_string();
        assert!(err.contains("restart opencode-rust"), "{err}");
        assert!(err.contains("tools/call"), "{err}");
    }
}

#[cfg(test)]
mod b28_list_tests {
    use super::*;
    use tokio::io::duplex;

    async fn send(ws: &mut (impl AsyncWrite + Unpin), v: Value) {
        ws.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        ws.flush().await.unwrap();
    }

    #[tokio::test]
    async fn pages_null_descriptions_and_bad_entries() {
        let (client_end, server_end) = duplex(64 * 1024);
        let (r, w) = tokio::io::split(client_end);
        let mut client = Client::new(BufReader::new(r), w);
        let server = tokio::spawn(async move {
            let (rs, mut ws) = tokio::io::split(server_end);
            let mut lines = BufReader::new(rs).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).unwrap();
                let result = if req["params"]["cursor"] == "p2" {
                    json!({"tools": [{"name": "c", "description": null}]})
                } else {
                    json!({"tools": [{"name": "a", "description": null}, {"description": "no name"}, {"name": "b"}],
                           "nextCursor": "p2"})
                };
                send(&mut ws, json!({"jsonrpc": "2.0", "id": req["id"], "result": result})).await;
            }
        });
        let tools = client.list_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(tools[0].description, "");
        drop(client);
        let _ = server.await;
    }
}

#[cfg(test)]
mod f10_tests {
    #[test]
    fn example_config_still_parses() {
        let cfg: crate::config::Config =
            toml::from_str(include_str!("../config.example.toml")).expect("config.example.toml");
        assert!(cfg.mcp.servers.is_empty(), "MCP entries stay commented out");
    }
}
