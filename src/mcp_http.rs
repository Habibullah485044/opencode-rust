//! F-03: MCP client over Streamable HTTP (protocol revision 2025-11-25).
//! One endpoint; each JSON-RPC message is its own POST. Answers come as one
//! JSON object or as an SSE stream. The `Mcp-Session-Id` from `initialize`
//! goes on every later request, with `MCP-Protocol-Version`; a 404 then
//! means "session expired". Not supported: the optional GET stream,
//! resuming broken streams, servers that ONLY speak revision 2026-07-28.

use crate::mcp::McpTool;
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

/// The newest MCP revision that still starts with an `initialize` handshake.
pub const HTTP_PROTOCOL_VERSION: &str = "2025-11-25";

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// One MCP server reached over HTTP.
pub struct HttpClient {
    http: reqwest::Client,
    url: String,
    /// Headers from the config (for example `Authorization`), sent every time.
    extra: HeaderMap,
    next_id: i64,
    /// From the `Mcp-Session-Id` header of the `initialize` answer, if any.
    session_id: Option<String>,
    /// The protocol version the server chose in `initialize`.
    protocol: Option<String>,
}

impl HttpClient {
    /// Check the config headers. No network traffic yet; call `initialize` next.
    pub fn new(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        let mut extra = HeaderMap::new();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .with_context(|| format!("bad header name `{k}` in [[mcp.servers]] headers"))?;
            let value = HeaderValue::from_str(v)
                .with_context(|| format!("bad value for header `{k}` in [[mcp.servers]] headers"))?;
            extra.insert(name, value);
        }
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build()?;
        Ok(Self { http, url: url.to_string(), extra, next_id: 0, session_id: None, protocol: None })
    }

    /// POST one JSON-RPC message with the headers this transport needs.
    async fn post(&self, msg: &Value) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .post(&self.url)
            .headers(self.extra.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .json(msg);
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(version) = &self.protocol {
            req = req.header("MCP-Protocol-Version", version);
        }
        req.send().await.with_context(|| format!("mcp http request to {} failed", self.url))
    }

    /// Send a request, return its `result`. If the server forgot our session
    /// (404), start a new session and send the request once more.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        if let Some(res) = self.request_once(method, &params).await? {
            return Ok(res);
        }
        self.initialize().await?;
        self.request_once(method, &params)
            .await?
            .ok_or_else(|| anyhow!("mcp server dropped the new session too (`{method}`)"))
    }

    /// One POST. `Ok(None)` = 404 on a request that carried a session id.
    async fn request_once(&mut self, method: &str, params: &Value) -> Result<Option<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let resp = self.post(&msg).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND && self.session_id.is_some() {
            self.session_id = None;
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("mcp `{method}`: server returned {status}: {}", body.chars().take(300).collect::<String>());
        }
        if method == "initialize" {
            if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                self.session_id = Some(sid.to_string());
            }
        }
        let content_type = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
        let reply = if content_type.is_some_and(|v| v.starts_with("text/event-stream")) {
            self.read_sse(resp, id, method).await?
        } else {
            resp.json::<Value>().await.with_context(|| format!("mcp `{method}`: answer is not JSON"))?
        };
        if let Some(err) = reply.get("error") {
            bail!("mcp `{method}` failed: {err}");
        }
        if reply.get("id").and_then(Value::as_i64) != Some(id) {
            bail!("mcp `{method}`: answer has the wrong id: {reply}");
        }
        Ok(Some(reply.get("result").cloned().unwrap_or(Value::Null)))
    }

    /// Read an SSE answer until the response with our `id` arrives.
    async fn read_sse(&mut self, resp: reqwest::Response, id: i64, method: &str) -> Result<Value> {
        let mut stream = Box::pin(resp.bytes_stream());
        let mut buf: Vec<u8> = Vec::new();
        let mut data = String::new(); // the `data:` lines of the current event
        loop {
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim_end_matches(['\r', '\n']);
                if let Some(d) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d.strip_prefix(' ').unwrap_or(d));
                } else if line.is_empty() && !data.is_empty() {
                    // A blank line ends one event.
                    let event = std::mem::take(&mut data);
                    if let Some(reply) = self.handle_sse_event(&event, id).await? {
                        return Ok(reply);
                    }
                }
                // `event:`, `id:`, `retry:` and `:` comment lines are ignored.
            }
            match stream.next().await {
                Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                Some(Err(e)) => bail!("mcp `{method}`: the answer stream broke: {e}"),
                None => {
                    // The last event may lack its closing blank line.
                    let event = std::mem::take(&mut data);
                    if !event.is_empty() {
                        if let Some(reply) = self.handle_sse_event(&event, id).await? {
                            return Ok(reply);
                        }
                    }
                    bail!("mcp `{method}`: server closed the stream without an answer");
                }
            }
        }
    }

    /// One SSE event: returned when it is our response. Server requests are
    /// answered; notifications (progress, logs) are skipped.
    async fn handle_sse_event(&mut self, event: &str, id: i64) -> Result<Option<Value>> {
        let Ok(msg) = serde_json::from_str::<Value>(event) else { return Ok(None) };
        if msg.get("method").is_some() && msg.get("id").is_some() {
            let answer = if msg["method"] == "ping" {
                json!({ "jsonrpc": "2.0", "id": msg["id"], "result": {} })
            } else {
                let error = json!({ "code": -32601, "message": "not supported by this client" });
                json!({ "jsonrpc": "2.0", "id": msg["id"], "error": error })
            };
            let _ = self.post(&answer).await; // the server replies 202
            return Ok(None);
        }
        Ok((msg.get("id").and_then(Value::as_i64) == Some(id)).then_some(msg))
    }

    /// Send a notification; servers answer 202 with no body.
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let resp = self.post(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await?;
        if !resp.status().is_success() {
            bail!("mcp notification `{method}`: server returned {}", resp.status());
        }
        Ok(())
    }

    /// Start a session: `initialize`, keep the session id and the version the
    /// server chose, then send `notifications/initialized`.
    pub async fn initialize(&mut self) -> Result<Value> {
        self.session_id = None;
        self.protocol = None;
        let params = json!({
            "protocolVersion": HTTP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "opencode-rust", "version": env!("CARGO_PKG_VERSION") }
        });
        let answer = tokio::time::timeout(INIT_TIMEOUT, self.request_once("initialize", &params))
            .await
            .context("mcp initialize timed out")?;
        let res = match answer {
            Ok(Some(res)) => res,
            Ok(None) => bail!("mcp initialize: unexpected 404"),
            Err(e) => {
                // A server that only speaks 2026-07-28 rejects `initialize`
                // and names the versions it supports.
                let text = format!("{e:#}");
                if text.contains("2026-07-28") {
                    bail!("{text} (this server only speaks MCP 2026-07-28, which has no `initialize`; \
                           opencode-rust's HTTP client speaks {HTTP_PROTOCOL_VERSION})");
                }
                return Err(e);
            }
        };
        let version = res.get("protocolVersion").and_then(Value::as_str).unwrap_or(HTTP_PROTOCOL_VERSION);
        self.protocol = Some(version.to_string()); // accept whatever the server chose
        self.notify("notifications/initialized", json!({})).await?;
        Ok(res)
    }

    /// `tools/list`, following `nextCursor` to read every page.
    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _page in 0..100 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let res = tokio::time::timeout(LIST_TIMEOUT, self.request("tools/list", params))
                .await
                .context("mcp tools/list timed out")??;
            let arr = res
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("tools/list response has no `tools` array"))?;
            // One malformed entry must not drop the whole server (as in B-28).
            tools.extend(arr.iter().filter_map(|t| serde_json::from_value::<McpTool>(t.clone()).ok()));
            cursor = res.get("nextCursor").and_then(Value::as_str).map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// `tools/call`: the text parts joined by newlines; `isError: true` comes
    /// back as `Err` with that text (same rules as the stdio client).
    pub async fn call_tool(&mut self, name: &str, args: &Value) -> Result<String> {
        let params = json!({ "name": name, "arguments": args });
        let res = tokio::time::timeout(CALL_TIMEOUT, self.request("tools/call", params))
            .await
            .context("mcp tools/call timed out")??;
        let is_error = res.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let parts = res.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        let text = parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if is_error {
            bail!("{}", if text.is_empty() { "tool error" } else { text.as_str() });
        }
        Ok(if text.is_empty() { "(no output)".to_string() } else { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::{extract::State, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    /// What the fake MCP server remembers. `pub(super)`: Stage 3 reuses it.
    #[derive(Default)]
    pub(super) struct Fake {
        sse: bool,          // answer with SSE streams instead of JSON bodies
        session: String,    // current session id: "s1", then "s2", …
        sessions: u32,      // how many sessions were started
        expire_once: bool,  // answer the next request with 404 once
        ping_answers: u32,  // how many of our pings the client answered
    }

    pub(super) type FakeState = Arc<Mutex<Fake>>;

    async fn fake_mcp(State(st): State<FakeState>, headers: HeaderMap, Json(msg): Json<Value>) -> Response {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        if header("x-test") != "1" {
            return StatusCode::UNAUTHORIZED.into_response(); // the config header must arrive
        }
        let mut f = st.lock().unwrap();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            if msg["result"].is_object() {
                f.ping_answers += 1; // the client's answer to our ping
            }
            return StatusCode::ACCEPTED.into_response();
        };
        if method == "initialize" {
            f.sessions += 1;
            f.session = format!("s{}", f.sessions);
            let result = json!({ "protocolVersion": HTTP_PROTOCOL_VERSION, "capabilities": { "tools": {} } });
            let reply = json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result });
            return ([("mcp-session-id", f.session.clone())], Json(reply)).into_response();
        }
        if f.expire_once || header("mcp-session-id") != f.session {
            f.expire_once = false;
            return StatusCode::NOT_FOUND.into_response();
        }
        if header("mcp-protocol-version") != HTTP_PROTOCOL_VERSION {
            return StatusCode::BAD_REQUEST.into_response();
        }
        if msg.get("id").is_none() {
            return StatusCode::ACCEPTED.into_response(); // notifications/initialized
        }
        let tool = |name: &str| json!({ "name": name, "inputSchema": { "type": "object" } });
        let result = match method {
            "tools/list" if msg["params"]["cursor"] == "p2" => json!({ "tools": [tool("shout")] }),
            "tools/list" => json!({ "tools": [tool("echo")], "nextCursor": "p2" }),
            "tools/call" => {
                let text = msg["params"]["arguments"]["text"].as_str().unwrap_or("");
                json!({ "content": [{ "type": "text", "text": format!("echo: {text}") }] })
            }
            _ => json!({}),
        };
        let reply = json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result });
        if !f.sse {
            return Json(reply).into_response();
        }
        // SSE: a progress notification, a `ping` request, then the answer.
        let progress = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} });
        let ping = json!({ "jsonrpc": "2.0", "id": "srv-1", "method": "ping" });
        let body = format!("data: {progress}\n\nevent: message\ndata: {ping}\n\ndata: {reply}\n\n");
        ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
    }

    /// Start the fake server; returns its endpoint URL and its state.
    pub(super) async fn start(fake: Fake) -> (String, FakeState) {
        let st: FakeState = Arc::new(Mutex::new(fake));
        let app = Router::new().route("/mcp", post(fake_mcp)).with_state(st.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/mcp"), st)
    }

    /// The config header the fake server requires (stands in for a token).
    pub(super) fn test_headers() -> HashMap<String, String> {
        HashMap::from([("X-Test".to_string(), "1".to_string())])
    }

    async fn handshake_list_call(sse: bool) {
        let (url, st) = start(Fake { sse, ..Default::default() }).await;
        let mut c = HttpClient::new(&url, &test_headers()).unwrap();
        c.initialize().await.unwrap();
        let names: Vec<String> = c.list_tools().await.unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["echo", "shout"], "both pages of tools/list are read");
        assert_eq!(c.call_tool("echo", &json!({ "text": "hi" })).await.unwrap(), "echo: hi");
        if sse {
            assert_eq!(st.lock().unwrap().ping_answers, 3, "every server ping was answered");
        }
    }

    #[tokio::test]
    async fn json_answers_work() {
        handshake_list_call(false).await;
    }

    #[tokio::test]
    async fn sse_answers_work() {
        handshake_list_call(true).await;
    }

    #[tokio::test]
    async fn expired_session_is_renewed_once() {
        let (url, st) = start(Fake::default()).await;
        let mut c = HttpClient::new(&url, &test_headers()).unwrap();
        c.initialize().await.unwrap();
        st.lock().unwrap().expire_once = true;
        assert_eq!(c.call_tool("echo", &json!({ "text": "again" })).await.unwrap(), "echo: again");
        assert_eq!(st.lock().unwrap().sessions, 2, "a new session was started");
    }

    #[tokio::test]
    async fn missing_header_and_bad_header_names_are_clear_errors() {
        let (url, _st) = start(Fake::default()).await;
        let err = HttpClient::new(&url, &HashMap::new()).unwrap().initialize().await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
        let bad = HashMap::from([("bad name".to_string(), "x".to_string())]);
        assert!(HttpClient::new(&url, &bad).is_err());
    }
}

#[cfg(test)]
mod registry_tests {
    use super::tests::{start, test_headers, Fake};
    use crate::config::McpServerCfg;
    use serde_json::json;

    #[tokio::test]
    async fn registry_starts_and_calls_an_http_server() {
        let (url, _st) = start(Fake::default()).await;
        let cfg = McpServerCfg { name: "web".into(), url: Some(url), headers: test_headers(), ..Default::default() };
        let mut reg = crate::mcp::Registry::default();
        reg.start_all(&[cfg]).await;
        assert_eq!(reg.connected(), vec!["web"]);
        let names: Vec<String> = reg.specs().into_iter().map(|s| s.function.name).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let out = reg.call("mcp__web__echo", &json!({ "text": "via registry" })).await.unwrap();
        assert_eq!(out, "echo: via registry");
        let route = reg.route("mcp__web__echo").unwrap(); // B-20's lock-free path
        assert_eq!(route.call(&json!({ "text": "via route" })).await.unwrap(), "echo: via route");
    }
}
