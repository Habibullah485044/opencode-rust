use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// OpenAI-compatible chat client (works with OpenRouter, Ollama, vLLM,
/// the user's Cloudflare-tunnel endpoint, etc). Streaming via SSE.
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
    /// Max silence allowed between SSE reads before the stream errors out.
    /// Without it a wedged provider (dead tunnel, stuck backend) freezes the
    /// agent loop until the total request timeout fires — or forever.
    stall: std::time::Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: String, // system | user | assistant | tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallReq>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallReq {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // "function"
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String, // JSON string
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String, // "function"
    pub function: ToolFnSpec,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFnSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON Schema
}

/// One streamed event from the model.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    /// Chain-of-thought (`delta.reasoning_content` — Ollama/vLLM/OpenRouter
    /// style). Displayed to the user but never sent back to the provider.
    Reasoning(String),
    ToolCallDelta { index: usize, id: Option<String>, name: Option<String>, args_delta: String },
    Done { finish_reason: Option<String> },
}

#[derive(Deserialize)]
struct SseChunk {
    choices: Vec<SseChoice>,
}

#[derive(Deserialize)]
struct SseChoice {
    delta: SseDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    /// Thinking text. `reasoning_content` is the Ollama/vLLM convention;
    /// `reasoning` is what OpenRouter sends. Both are accepted.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<SseToolCallDelta>,
}

#[derive(Deserialize)]
struct SseToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<SseFnDelta>,
}

#[derive(Deserialize)]
struct SseFnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl Client {
    pub fn new(base_url: &str, api_key: &str, model: &str, timeout_secs: u64) -> Result<Self> {
        // `timeout_secs` bounds the whole request; `stall` bounds SILENCE on
        // an established stream so a wedged provider surfaces in ≤2 min
        // instead of freezing the run until the total timeout fires.
        let stall = std::time::Duration::from_secs(timeout_secs.clamp(5, 120));
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            stall,
        })
    }

    /// Stream a chat completion. `tools` may be empty for plain chat.
    pub async fn stream(
        &self,
        model: &str,
        messages: &[ChatMsg],
        tools: &[ToolSpec],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent>>> {
        let mut body = json!({ "model": model, "messages": messages, "stream": true });
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools)?;
        }
        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await.map_err(|e| anyhow!(
            "provider request to {}/chat/completions failed: {e:?} \
             (tunnel down or endpoint unreachable — check the base URL / heartbeat in the header)",
            self.base_url
        ))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("provider returned {status}: {}", text.chars().take(400).collect::<String>()));
        }
    let byte_stream = resp.bytes_stream();
    let event_stream = Box::pin(sse_event_stream(byte_stream, self.stall));
    Ok(event_stream)
    }

    /// Switch the active model in place (used by `/model <id>`). The client is
    /// shared behind a mutex, so every later call — agent loop, one-shot
    /// helpers, team workers — picks up the new model.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    /// `GET {base}/models` — the ids the endpoint advertises. Best-effort:
    /// endpoints that do not implement it (or answer with a different shape)
    /// simply yield an empty list instead of erroring the caller.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let mut req = self.http.get(format!("{}/models", self.base_url));
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        // OpenAI shape: { data: [{ id }] }. Ollama-ish: { models: [{ name }] }.
        let mut ids: Vec<String> = Vec::new();
        if let Some(arr) = body.get("data").and_then(Value::as_array) {
            for m in arr {
                if let Some(id) = m.get("id").and_then(Value::as_str) {
                    ids.push(id.to_string());
                }
            }
        } else if let Some(arr) = body.get("models").and_then(Value::as_array) {
            for m in arr {
                let id = m
                    .get("id")
                    .or_else(|| m.get("name"))
                    .and_then(Value::as_str);
                if let Some(id) = id {
                    ids.push(id.to_string());
                }
            }
        } else if let Some(arr) = body.as_array() {
            for m in arr {
                if let Some(id) = m.as_str() {
                    ids.push(id.to_string());
                }
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

/// Convert the raw SSE byte stream into StreamEvents, handling partial lines.
/// A single `data:` line can yield several events (e.g. a tool_call delta *and*
/// a finish_reason), so we buffer parsed events in a queue and emit one per poll.
fn sse_event_stream(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    stall: std::time::Duration,
) -> impl futures::Stream<Item = Result<StreamEvent>> + Send + 'static {
    // Pin the inner stream so `next()` works inside the unfold closure.
    let byte_stream = Box::pin(byte_stream);
    // State: (source stream, line buffer, pending parsed events, terminal [DONE] seen).
    futures::stream::unfold(
        (byte_stream, Vec::<u8>::new(), std::collections::VecDeque::new(), false),
        move |(mut src, mut buf, mut pending, mut done)| async move {
            loop {
                // 1) Emit any already-parsed event first.
                if let Some(ev) = pending.pop_front() {
                    return Some((Ok(ev), (src, buf, pending, done)));
                }
                // 2) If we saw terminal [DONE] and drained pending, stop.
                if done {
                    return None;
                }
                // 3) Extract complete lines from the buffer.
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line).trim().to_string();
                    if let Some((events, line_done)) = parse_sse_line(&line) {
                        pending.extend(events);
                        // Only the explicit `[DONE]` sentinel terminates the stream;
                        // a per-choice finish_reason is just another event.
                        if line_done {
                            done = true;
                        }
                    }
                    continue;
                }
                // 4) Need more bytes from the transport. Bound the wait: a
                //    silent stream means a dead provider/tunnel — surface an
                //    error instead of hanging the agent loop forever.
                match tokio::time::timeout(stall, src.next()).await {
                    Err(_) => {
                        return Some((
                            Err(anyhow!(
                                "provider stream stalled: no data for {}s",
                                stall.as_secs()
                            )),
                            (src, buf, pending, true),
                        ));
                    }
                    Ok(Some(Ok(bytes))) => buf.extend_from_slice(&bytes),
                    Ok(Some(Err(e))) => {
                        return Some((Err(anyhow!("stream error: {e}")), (src, buf, pending, true)));
                    }
                    Ok(None) => {
                        // Transport closed without [DONE]; flush nothing, then stop.
                        done = true;
                    }
                }
            }
        },
    )
}

/// Parse one SSE line. Returns the events it produced (possibly empty) and
/// whether this was the terminal `[DONE]` sentinel. `None` = not a data line.
fn parse_sse_line(line: &str) -> Option<(Vec<StreamEvent>, bool)> {
    let data = line.strip_prefix("data:")?.trim();
    if data.is_empty() {
        return Some((Vec::new(), false));
    }
    if data == "[DONE]" {
        return Some((Vec::new(), true));
    }
    let chunk: SseChunk = serde_json::from_str(data).ok()?;
    let mut events = Vec::new();
    for choice in &chunk.choices {
        // Thinking first: reasoning precedes content within a turn, and on the
        // rare line carrying both, the order must stay reasoning → text.
        let reasoning = choice
            .delta
            .reasoning_content
            .as_ref()
            .or(choice.delta.reasoning.as_ref());
        if let Some(text) = reasoning {
            if !text.is_empty() {
                events.push(StreamEvent::Reasoning(text.clone()));
            }
        }
        if let Some(text) = &choice.delta.content {
            if !text.is_empty() {
                events.push(StreamEvent::Text(text.clone()));
            }
        }
        for tc in &choice.delta.tool_calls {
            let (name, args_delta) = tc
                .function
                .as_ref()
                .map(|f| (f.name.clone(), f.arguments.clone().unwrap_or_default()))
                .unwrap_or((None, String::new()));
            events.push(StreamEvent::ToolCallDelta {
                index: tc.index,
                id: tc.id.clone(),
                name,
                args_delta,
            });
        }
        if let Some(fr) = &choice.finish_reason {
            // Emitted AFTER any deltas on this same line so nothing is lost.
            events.push(StreamEvent::Done { finish_reason: Some(fr.clone()) });
        }
    }
    Some((events, false))
}

/// Accumulates streamed tool-call fragments into final calls.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: std::collections::BTreeMap<usize, (String, String, String)>, // idx -> (id, name, args)
}

impl ToolCallAccumulator {
    pub fn push(&mut self, ev: &StreamEvent) {
        if let StreamEvent::ToolCallDelta { index, id, name, args_delta } = ev {
            let entry = self.calls.entry(*index).or_default();
            if let Some(id) = id {
                if !id.is_empty() {
                    entry.0 = id.clone();
                }
            }
            if let Some(name) = name {
                if !name.is_empty() {
                    entry.1 = name.clone();
                }
            }
            entry.2.push_str(args_delta);
        }
    }

    /// Returns final calls; args parsed as JSON (falls back to {"raw": args}).
    pub fn finish(self) -> Vec<(String, String, Value)> {
        self.calls
            .into_values()
            .map(|(id, name, args)| {
                let v = serde_json::from_str(&args)
                    .unwrap_or_else(|_| json!({ "raw": args }));
                (id, name, v)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an SSE `data:` line from a real JSON value — no hand-escaping.
    fn sse_data(v: &Value) -> String {
        format!("data: {}\n\n", serde_json::to_string(v).unwrap())
    }

    /// One streamed chunk as OpenAI sends it.
    fn chunk(delta: Value, finish: Value) -> Value {
        json!({ "choices": [{ "delta": delta, "finish_reason": finish }] })
    }

    #[test]
    fn sse_text_line_parses() {
        let line = sse_data(&chunk(json!({"content": "hi"}), Value::Null));
        let (events, done) = parse_sse_line(line.trim()).unwrap();
        assert!(!done);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Text(t) if t == "hi"));
    }

    #[test]
    fn sse_reasoning_content_parses_as_reasoning() {
        let line = sse_data(&chunk(
            json!({"reasoning_content": "let me think…"}),
            Value::Null,
        ));
        let (events, done) = parse_sse_line(line.trim()).unwrap();
        assert!(!done);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::Reasoning(t) if t == "let me think…"));
    }

    #[test]
    fn sse_openrouter_reasoning_field_and_order() {
        // OpenRouter uses `reasoning`; if one line carries reasoning AND
        // content, reasoning must be emitted first.
        let line = sse_data(&chunk(
            json!({"reasoning": "hmm", "content": "answer"}),
            Value::Null,
        ));
        let (events, _) = parse_sse_line(line.trim()).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Reasoning(t) if t == "hmm"));
        assert!(matches!(&events[1], StreamEvent::Text(t) if t == "answer"));
    }

    #[test]
    fn sse_done_sentinel() {
        let (_, done) = parse_sse_line("data: [DONE]").unwrap();
        assert!(done);
    }

    #[test]
    fn finish_reason_does_not_clobber_tool_call() {
        // The critical regression: one line carrying BOTH a tool_call delta and
        // finish_reason must yield the delta AND a Done, not just Done.
        let line = sse_data(&chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "write", "arguments": "{}"}}]}),
            json!("tool_calls"),
        ));
        let (events, done) = parse_sse_line(line.trim()).unwrap();
        assert!(!done, "per-choice finish_reason must not terminate the stream");
        assert_eq!(events.len(), 2, "expected tool_call delta + Done");
        assert!(matches!(&events[0], StreamEvent::ToolCallDelta { name, .. } if name.as_deref() == Some("write")));
        assert!(matches!(&events[1], StreamEvent::Done { finish_reason: Some(fr) } if fr == "tool_calls"));
    }

    #[tokio::test]
    async fn unfold_parses_real_tool_call_payload() {
        // Two chunks that split the write-tool arguments, then [DONE].
        let payload = format!(
            "{}{}{}",
            sse_data(&chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "write", "arguments": "{\"path\":\"e2e-marker.txt\","}}]}),
                Value::Null,
            )),
            sse_data(&chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"content\":\"hello\"}"}}]}),
                json!("tool_calls"),
            )),
            "data: [DONE]\n\n",
        );
        let byte_stream = futures::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(payload))]);
        let mut s = Box::pin(sse_event_stream(byte_stream, std::time::Duration::from_secs(5)));
        let mut acc = ToolCallAccumulator::default();
        while let Some(ev) = s.next().await {
            acc.push(&ev.unwrap());
        }
        let calls = acc.finish();
        assert_eq!(calls.len(), 1, "expected one tool call from direct byte stream");
        assert_eq!(calls[0].1, "write");
        assert_eq!(calls[0].2["path"], "e2e-marker.txt");
        assert_eq!(calls[0].2["content"], "hello");
    }

    #[tokio::test]
    async fn stalled_transport_errors_instead_of_hanging() {
        use axum::{body::Body, routing::post, Router};
        // Emits one real SSE event, then the connection stays open but the
        // body never yields another byte — a wedged provider / dead tunnel.
        let event = sse_data(&chunk(json!({"content": "hi"}), Value::Null));
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let event = event.clone();
                async move {
                    let body = futures::stream::once(async move {
                        Ok::<_, std::io::Error>(bytes::Bytes::from(event))
                    })
                    .chain(futures::stream::pending());
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(body),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut client = Client::new(&format!("http://{addr}/v1"), "", "stub", 30).unwrap();
        client.stall = std::time::Duration::from_millis(300);
        let mut s = client.stream(&client.model, &[], &[]).await.unwrap();

        // First event arrives fine…
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), s.next())
            .await
            .expect("first event should arrive")
            .expect("stream should be open")
            .unwrap();
        assert!(matches!(ev, StreamEvent::Text(t) if t == "hi"));

        // …then the silence must surface as an error, not a hang.
        let stalled = tokio::time::timeout(std::time::Duration::from_secs(5), s.next())
            .await
            .expect("stall error should fire inside the stream, not hang the caller");
        let stalled = stalled.expect("stream item");
        assert!(stalled.is_err(), "expected a stall error after the silence");
    }

    #[test]
    fn accumulator_builds_call() {
        let mut acc = ToolCallAccumulator::default();
        for ev in [
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("bash".into()),
                args_delta: "{\"comm".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                args_delta: "and\":\"ls\"}".into(),
            },
        ] {
            acc.push(&ev);
        }
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "bash");
        assert_eq!(calls[0].2["command"], "ls");
    }
}
