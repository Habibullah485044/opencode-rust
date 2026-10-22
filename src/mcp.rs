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

#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// JSON-RPC session over any async line-oriented transport.
pub struct Client<R, W> {
    reader: R,
    writer: W,
    next_id: i64,
}

impl<R, W> Client<R, W>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer, next_id: 0 }
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
        .map_err(|e| anyhow!("mcp server unavailable during `{method}`: {e}"))?;
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(anyhow!("mcp server closed its stdout during `{method}`"));
            }
            let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                continue; // non-JSON noise on stdout: ignore
            };
            if msg.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // notification or another id
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
        let res = self.request("tools/list", json!({})).await?;
        let arr = res
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("tools/list response has no `tools` array"))?;
        arr.iter()
            .map(|t| serde_json::from_value(t.clone()).context("bad tool entry"))
            .collect()
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
            Err(anyhow!("{}", if text.is_empty() { "tool error" } else { text.as_str() }))
        } else if text.is_empty() {
            Ok("(no output)".into())
        } else {
            Ok(text)
        }
    }
}

/// Server names become part of a function name: keep [A-Za-z0-9-], fold the
/// rest to `-`, so `mcp__<server>__<tool>` parsing stays unambiguous.
pub fn sanitize_server(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

/// The model-facing function name for an MCP tool.
pub fn tool_fn_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize_server(server), tool)
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

/// One live MCP server connection (child process + session + tool cache).
pub struct McpConn {
    #[allow(dead_code)] // kept alive; kill_on_drop kills the child on drop
    child: tokio::process::Child,
    pub client: Client<BufReader<tokio::process::ChildStdout>, tokio::process::ChildStdin>,
    pub tools: Vec<McpTool>,
    pub server: String,
}

impl McpConn {
    pub async fn spawn(name: &str, cfg: &McpServerCfg) -> Result<Self> {
        let server = sanitize_server(name);
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
        let tools = client.list_tools().await?;
        Ok(Self { child, client, tools, server })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| spec_for(&self.server, t)).collect()
    }

    pub async fn call(&mut self, tool: &str, args: &Value) -> Result<String> {
        self.client.call_tool(tool, args).await
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
        for cfg in servers {
            if cfg.name.is_empty() || cfg.command.is_empty() {
                continue;
            }
            let key = sanitize_server(&cfg.name);
            if self.conns.contains_key(&key) {
                continue;
            }
            match McpConn::spawn(&cfg.name, cfg).await {
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

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.conns.values().flat_map(|c| c.specs()).collect()
    }

    pub async fn call(&mut self, fn_name: &str, args: &Value) -> Result<String> {
        let (server, tool) =
            parse_fn_name(fn_name).ok_or_else(|| anyhow!("`{fn_name}` is not an MCP tool"))?;
        let conn = self
            .conns
            .get_mut(&server)
            .ok_or_else(|| anyhow!("mcp server `{server}` is not connected"))?;
        conn.call(&tool, args).await
    }

    pub fn connected(&self) -> Vec<&str> {
        self.conns.keys().map(String::as_str).collect()
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
        };
        let mut conn = McpConn::spawn("fake", &cfg).await.expect("spawn node server");
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
