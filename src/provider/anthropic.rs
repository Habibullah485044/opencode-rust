//! F-02: Anthropic Messages API (`POST {base_url}/messages`). Turns the
//! OpenAI-style `ChatMsg` history into an Anthropic request body, and the
//! Anthropic stream back into the same `StreamEvent`s.

use super::{ChatMsg, ToolSpec};
use serde_json::{json, Value};

/// Build the JSON body for `POST {base_url}/messages`.
/// - Leading system messages become the top-level `system` string; a
///   later one (compaction summary, slash command) stays in place as user
///   text "[system note] …".
/// - Tool results become `tool_result` blocks in a user message.
/// - Messages with the same role in a row are merged.
/// - The result starts with a user message and never ends with an
///   assistant message.
/// - `turns`: tool-calling replies exactly as the model sent them (thinking
///   blocks included), keyed by `turn_key`. Assistant messages after the
///   last user message (the current turn) are sent back from there.
pub(super) fn request_body(
    model: &str,
    max_tokens: u32,
    messages: &[ChatMsg],
    tools: &[ToolSpec],
    turns: &[(String, Vec<Value>)],
) -> Value {
    let turn_start = messages.iter().rposition(|m| m.role == "user").unwrap_or(0);
    let mut system: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        let text = m.content.clone().unwrap_or_default();
        let (role, blocks) = match m.role.as_str() {
            "system" if out.is_empty() => {
                if !text.trim().is_empty() {
                    system.push(text);
                }
                continue;
            }
            "system" => ("user", text_block(&format!("[system note]\n{text}"))),
            "user" => ("user", text_block(&text)),
            "assistant" => {
                let calls: Vec<_> = m.tool_calls.iter().flatten().collect();
                let key = turn_key(calls.iter().map(|tc| clean_id(&tc.id)));
                let saved = turns.iter().rev().find(|(k, _)| i > turn_start && !calls.is_empty() && *k == key);
                if let Some((_, blocks)) = saved {
                    ("assistant", blocks.clone()) // exactly as the model sent it
                } else {
                    let mut blocks = text_block(&text);
                    for tc in calls {
                        // `arguments` is a JSON string; Anthropic wants an object.
                        let input = serde_json::from_str::<Value>(&tc.function.arguments)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": clean_id(&tc.id),
                            "name": tc.function.name,
                            "input": input,
                        }));
                    }
                    ("assistant", blocks)
                }
            }
            "tool" => {
                // server.rs starts the text of a failed tool run with "[error]".
                let is_error = text.starts_with("[error]");
                let content = if text.is_empty() { "(no output)".to_string() } else { text };
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": clean_id(m.tool_call_id.as_deref().unwrap_or("")),
                    "content": content,
                    "is_error": is_error,
                });
                ("user", vec![block])
            }
            _ => continue,
        };
        push_merged(&mut out, role, blocks);
    }
    if out.first().map_or(true, |m| m["role"] != "user") {
        out.insert(0, json!({ "role": "user", "content": [{ "type": "text", "text": "(continued)" }] }));
    }
    if out.last().map_or(false, |m| m["role"] == "assistant") {
        push_merged(&mut out, "user", text_block("Continue."));
    }
    let mut body = json!({ "model": model, "max_tokens": max_tokens, "messages": out, "stream": true });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        let tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

/// Prompt caching (`cache_control`): mark the end of the tools, of the
/// system prompt, of the message that started this turn and of the newest
/// message. Each later request repeats all of that unchanged, so Anthropic
/// reads it from its cache (about a tenth of the input price) instead of
/// charging it in full. That is the 4 markers the API allows.
pub(super) fn add_cache_markers(body: &mut Value) {
    let mark = json!({ "type": "ephemeral" });
    if let Some(last) = body["tools"].as_array_mut().and_then(|t| t.last_mut()) {
        last["cache_control"] = mark.clone();
    }
    if let Some(sys) = body["system"].as_str().map(str::to_string) {
        body["system"] = json!([{ "type": "text", "text": sys, "cache_control": mark.clone() }]);
    }
    let Some(msgs) = body["messages"].as_array_mut() else { return };
    // The turn starts at the last user message that has text (not only
    // tool results).
    let has_text = |m: &Value| m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "text"));
    let turn = msgs.iter().rposition(|m| m["role"] == "user" && has_text(m));
    let mut targets: Vec<usize> = turn.into_iter().collect();
    if !msgs.is_empty() && turn != Some(msgs.len() - 1) {
        targets.push(msgs.len() - 1);
    }
    for i in targets {
        // Thinking blocks cannot carry a marker; use the last other block.
        let Some(blocks) = msgs[i]["content"].as_array_mut() else { continue };
        if let Some(b) = blocks.iter_mut().rev().find(|b| !b["type"].as_str().unwrap_or("").ends_with("thinking")) {
            b["cache_control"] = mark.clone();
        }
    }
}

/// The text of `system`, whether it is a string or (with caching) blocks.
#[cfg(test)]
pub(super) fn system_text(body: &Value) -> String {
    match &body["system"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("

"),
        _ => String::new(),
    }
}

/// One text block, or none for blank text (Anthropic rejects empty text).
fn text_block(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": text })]
    }
}

/// Cache key of one tool-calling reply: its tool_use ids joined by ",".
fn turn_key(ids: impl Iterator<Item = String>) -> String {
    ids.collect::<Vec<_>>().join(",")
}

/// Anthropic ids must match `^[a-zA-Z0-9_-]+$`; ids made by other
/// providers may not, so other characters become `_`.
fn clean_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if s.is_empty() {
        "toolu_missing".to_string()
    } else {
        s
    }
}

/// Append a message, or merge its blocks into the previous one when the
/// role repeats. `tool_result` blocks move to the front (stable sort).
fn push_merged(out: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last["role"] == role {
            if let Some(arr) = last["content"].as_array_mut() {
                arr.extend(blocks);
                arr.sort_by_key(|b| b["type"] != "tool_result");
            }
            return;
        }
    }
    out.push(json!({ "role": role, "content": blocks }));
}

#[cfg(test)]
mod translate_tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCallReq, ToolFnSpec};

    fn msg(role: &str, content: Option<&str>) -> ChatMsg {
        ChatMsg { role: role.into(), content: content.map(String::from), tool_calls: None, tool_call_id: None }
    }

    fn call(id: &str, args: &str) -> ToolCallReq {
        let function = FunctionCall { name: "read".into(), arguments: args.into() };
        ToolCallReq { id: id.into(), kind: "function".into(), function }
    }

    #[test]
    fn history_is_translated_to_anthropic_shape() {
        let mut asst = msg("assistant", Some("Let me look."));
        asst.tool_calls = Some(vec![call("call_1", r#"{"path":"a.rs"}"#)]);
        let mut result = msg("tool", Some("fn main() {}"));
        result.tool_call_id = Some("call_1".into());
        let history = vec![
            msg("system", Some("You are a coder.")),
            msg("user", Some("Read a.rs")),
            asst,
            result,
            msg("system", Some("[slash command expanded]")),
            msg("user", Some("Now explain it.")),
        ];
        let read = ToolFnSpec { name: "read".into(), description: "Read".into(), parameters: json!({"type": "object"}) };
        let tools = vec![ToolSpec { kind: "function".into(), function: read }];
        let body = request_body("claude-sonnet-5-5", 1000, &history, &tools, &[]);
        assert_eq!(body["max_tokens"], 1000);
        assert_eq!(body["system"], "You are a coder.");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3, "user / assistant / user (tool result + notes merged)");
        assert_eq!(m[1]["content"][1]["type"], "tool_use");
        assert_eq!(m[1]["content"][1]["input"]["path"], "a.rs");
        let blocks = m[2]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result", "tool results come first");
        assert_eq!(blocks[0]["tool_use_id"], "call_1");
        assert!(blocks[1]["text"].as_str().unwrap().starts_with("[system note]"));
        assert_eq!(blocks[2]["text"], "Now explain it.");
    }

    #[test]
    fn edge_cases_still_give_a_valid_request() {
        // Starts and ends with the assistant, bad JSON arguments, an error
        // result, a blank user message, an id Anthropic would reject.
        let mut asst = msg("assistant", None);
        asst.tool_calls = Some(vec![call("call:9", "not json")]);
        let mut failed = msg("tool", Some("[error]\nboom"));
        failed.tool_call_id = Some("call:9".into());
        let history = vec![asst, failed, msg("user", Some("  ")), msg("assistant", Some("Done."))];
        let body = request_body("m", 10, &history, &[], &[]);
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.first().unwrap()["role"], "user");
        assert_eq!(m.last().unwrap()["role"], "user");
        assert!(body.get("system").is_none() && body.get("tools").is_none());
        assert_eq!(m[1]["content"][0]["id"], "call_9");
        assert_eq!(m[1]["content"][0]["input"], json!({}));
        assert_eq!(m[2]["content"][0]["tool_use_id"], "call_9");
        assert_eq!(m[2]["content"][0]["is_error"], true);
    }

    #[test]
    fn current_turn_is_sent_back_from_the_saved_reply() {
        let with_call = |id: &str| {
            let mut a = msg("assistant", None);
            a.tool_calls = Some(vec![call(id, "{}")]);
            a
        };
        let result = |id: &str| {
            let mut r = msg("tool", Some("ok"));
            r.tool_call_id = Some(id.into());
            r
        };
        let history =
            vec![msg("user", Some("one")), with_call("t0"), result("t0"), msg("user", Some("two")), with_call("t1"), result("t1")];
        let saved = |id: &str| {
            let thinking = json!({ "type": "thinking", "thinking": "", "signature": "sig" });
            (id.to_string(), vec![thinking, json!({ "type": "tool_use", "id": id, "name": "read", "input": {} })])
        };
        let body = request_body("m", 10, &history, &[], &[saved("t0"), saved("t1")]);
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 5);
        assert_eq!(m[1]["content"][0]["type"], "tool_use", "an older turn goes without thinking");
        assert_eq!(m[3]["content"][0]["signature"], "sig", "the current turn is sent back as saved");
    }
}

use super::StreamEvent;
use anyhow::{anyhow, Result};
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Tool-calling replies exactly as the model sent them (thinking blocks and
/// signatures included), keyed by `turn_key` of their tool ids. Shared by
/// all clones of one `Client`; `request_body` reads it.
pub(super) type TurnCache = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<Value>)>>>;

/// How many such replies to keep (one per agent step that used tools).
const TURN_CACHE_MAX: usize = 256;

/// Anthropic `stop_reason` → the OpenAI `finish_reason` the app understands.
fn finish_reason(stop: &str) -> String {
    match stop {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        other => other,
    }
    .to_string()
}

/// Append `s` to the string field `key` of a content block.
fn append(block: &mut Value, key: &str, s: &str) {
    let joined = format!("{}{s}", block[key].as_str().unwrap_or(""));
    block[key] = Value::String(joined);
}

/// What we remember while reading one stream.
#[derive(Default)]
struct SseState {
    /// The reply's content blocks so far, by block index.
    blocks: BTreeMap<usize, Value>,
    /// Argument JSON received so far for each open `tool_use` block.
    tool_json: HashMap<usize, String>,
    /// A `Done` was already sent (from `message_delta`).
    done_sent: bool,
    /// `message_stop` arrived: the reply is complete.
    finished: bool,
    /// Where a finished tool-calling reply is saved.
    turns: TurnCache,
    /// Token counts from `message_start`, completed by `message_delta`.
    usage: super::Usage,
}

impl SseState {
    /// Turn one SSE line into zero or more events.
    fn line(&mut self, line: &str) -> Result<Vec<StreamEvent>> {
        let Some(data) = line.strip_prefix("data:") else { return Ok(Vec::new()) };
        let Ok(v) = serde_json::from_str::<Value>(data.trim()) else { return Ok(Vec::new()) };
        let index = v["index"].as_u64().unwrap_or(0) as usize;
        let text = |s: &Value| s.as_str().unwrap_or("").to_string();
        let mut events = Vec::new();
        match v["type"].as_str().unwrap_or("") {
            "content_block_start" => {
                let block = v["content_block"].clone();
                if block["type"] == "text" && !text(&block["text"]).is_empty() {
                    events.push(StreamEvent::Text(text(&block["text"])));
                }
                if block["type"] == "tool_use" {
                    self.tool_json.insert(index, String::new());
                    events.push(StreamEvent::ToolCallDelta {
                        index,
                        id: block["id"].as_str().map(String::from),
                        name: block["name"].as_str().map(String::from),
                        args_delta: String::new(),
                    });
                }
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                let block = self.blocks.entry(index).or_insert_with(|| json!({}));
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = text(&d["text"]);
                        append(block, "text", &t);
                        if !t.is_empty() {
                            events.push(StreamEvent::Text(t));
                        }
                    }
                    "thinking_delta" => {
                        let t = text(&d["thinking"]);
                        append(block, "thinking", &t);
                        if !t.is_empty() {
                            events.push(StreamEvent::Reasoning(t));
                        }
                    }
                    "signature_delta" => block["signature"] = d["signature"].clone(),
                    "input_json_delta" => {
                        let part = text(&d["partial_json"]);
                        if !part.is_empty() {
                            self.tool_json.entry(index).or_default().push_str(&part);
                            events.push(StreamEvent::ToolCallDelta { index, id: None, name: None, args_delta: part });
                        }
                    }
                    _ => {} // citations and newer delta types
                }
            }
            "content_block_stop" => {
                if let Some(raw) = self.tool_json.remove(&index) {
                    if raw.is_empty() {
                        // A tool called with no arguments may get no JSON at all.
                        let args_delta = "{}".to_string();
                        events.push(StreamEvent::ToolCallDelta { index, id: None, name: None, args_delta });
                    }
                    let input = serde_json::from_str::<Value>(&raw).ok().filter(Value::is_object);
                    if let Some(block) = self.blocks.get_mut(&index) {
                        block["input"] = input.unwrap_or_else(|| json!({}));
                    }
                }
            }
            "message_start" => self.read_usage(&v["message"]["usage"]),
            "message_delta" => {
                self.read_usage(&v["usage"]);
                if let Some(stop) = v["delta"]["stop_reason"].as_str() {
                    if !self.done_sent {
                        self.done_sent = true;
                        self.save_turn();
                        if self.usage != super::Usage::default() {
                            events.push(StreamEvent::Usage(self.usage));
                        }
                        events.push(StreamEvent::Done { finish_reason: Some(finish_reason(stop)) });
                    }
                }
            }
            "message_stop" => {
                self.finished = true;
                if !self.done_sent {
                    self.done_sent = true;
                    self.save_turn();
                    if self.usage != super::Usage::default() {
                        events.push(StreamEvent::Usage(self.usage));
                    }
                    events.push(StreamEvent::Done { finish_reason: None });
                }
            }
            "error" => {
                return Err(anyhow!(
                    "provider stream error: {}: {}",
                    v["error"]["type"].as_str().unwrap_or("error"),
                    v["error"]["message"].as_str().unwrap_or("(no message)")
                ));
            }
            _ => {} // message_start, ping, event types added later
        }
        Ok(events)
    }

    /// Merge an Anthropic `usage` object. `input_tokens` excludes the cached
    /// parts, so the prompt total is the sum of all three; `output_tokens`
    /// is a running total.
    fn read_usage(&mut self, u: &Value) {
        let n = |k: &str| u[k].as_u64();
        let read = n("cache_read_input_tokens");
        let write = n("cache_creation_input_tokens");
        if let Some(input) = n("input_tokens") {
            self.usage.input = input + read.unwrap_or(0) + write.unwrap_or(0);
        }
        if let Some(r) = read {
            self.usage.cached = r;
        }
        if let Some(w) = write {
            self.usage.cache_write = w;
        }
        if let Some(o) = n("output_tokens") {
            self.usage.output = o;
        }
    }

    /// Keep this reply for the next request when it called tools and has
    /// thinking blocks: the API wants that turn sent back unchanged.
    fn save_turn(&self) {
        let blocks: Vec<Value> = self.blocks.values().cloned().collect();
        let kind = |b: &Value| b["type"].as_str().unwrap_or("").to_string();
        let ids = blocks.iter().filter(|b| kind(b) == "tool_use");
        let key = turn_key(ids.map(|b| clean_id(b["id"].as_str().unwrap_or(""))));
        if key.is_empty() || !blocks.iter().any(|b| kind(b).ends_with("thinking")) {
            return;
        }
        if let Ok(mut turns) = self.turns.lock() {
            turns.retain(|(k, _)| *k != key);
            turns.push((key, blocks));
            if turns.len() > TURN_CACHE_MAX {
                turns.remove(0);
            }
        }
    }
}

/// Read Anthropic's SSE bytes and yield `StreamEvent`s (same line splitting
/// and stall timeout as `sse_event_stream` in provider.rs).
pub(super) fn event_stream(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    stall: std::time::Duration,
    turns: TurnCache,
) -> impl futures::Stream<Item = Result<StreamEvent>> + Send + 'static {
    let src = Box::pin(byte_stream);
    let state = SseState { turns, ..Default::default() };
    futures::stream::unfold(
        (src, Vec::<u8>::new(), VecDeque::<StreamEvent>::new(), state, false),
        move |(mut src, mut buf, mut pending, mut st, over)| async move {
            loop {
                if let Some(ev) = pending.pop_front() {
                    return Some((Ok(ev), (src, buf, pending, st, over)));
                }
                if over || st.finished {
                    return None;
                }
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line).trim().to_string();
                    match st.line(&line) {
                        Ok(events) => pending.extend(events),
                        Err(e) => return Some((Err(e), (src, buf, pending, st, true))),
                    }
                    continue;
                }
                let err = match tokio::time::timeout(stall, src.next()).await {
                    Ok(Some(Ok(bytes))) => {
                        buf.extend_from_slice(&bytes);
                        continue;
                    }
                    Err(_) => anyhow!("provider stream stalled: no data for {}s", stall.as_secs()),
                    Ok(Some(Err(e))) => anyhow!("stream error: {e}"),
                    Ok(None) if st.done_sent => return None,
                    Ok(None) => anyhow!("provider closed the stream before the reply was complete"),
                };
                return Some((Err(err), (src, buf, pending, st, true)));
            }
        },
    )
}

#[cfg(test)]
mod sse_tests {
    use super::*;
    use crate::provider::ToolCallAccumulator;

    /// Tool-use stream recorded from Anthropic's streaming docs (shortened).
    const SAMPLE: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_014p7gG3wDgGV9EUtLvnow3U","type":"message","role":"assistant","model":"claude-sonnet-5-5","stop_sequence":null,"usage":{"input_tokens":472,"output_tokens":2},"content":[],"stop_reason":null}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Okay, let's check the weather:"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01T1x1fJ34qAmk2tNTrN7Up6","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\": \"San Francisco, CA\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}

event: message_stop
data: {"type":"message_stop"}

"#;

    /// Feed `text` in 7-byte pieces (so lines arrive split), collect the events.
    async fn run_with(text: &str, turns: TurnCache) -> Vec<Result<StreamEvent>> {
        let pieces: Vec<reqwest::Result<bytes::Bytes>> =
            text.as_bytes().chunks(7).map(|c| Ok(bytes::Bytes::copy_from_slice(c))).collect();
        let stall = std::time::Duration::from_secs(5);
        event_stream(futures::stream::iter(pieces), stall, turns).collect().await
    }

    async fn run(text: &str) -> Vec<Result<StreamEvent>> {
        run_with(text, TurnCache::default()).await
    }

    /// Split events into (text, tool calls, finish_reason); panics on Err.
    async fn collect(text: &str) -> (String, Vec<(String, String, Value)>, Option<String>) {
        let (mut out, mut acc, mut finish) = (String::new(), ToolCallAccumulator::default(), None);
        for ev in run(text).await {
            match ev.unwrap() {
                StreamEvent::Text(t) => out.push_str(&t),
                StreamEvent::Done { finish_reason } => finish = finish_reason,
                ev => acc.push(&ev),
            }
        }
        (out, acc.finish(), finish)
    }

    #[tokio::test]
    async fn recorded_tool_use_stream_parses() {
        let (text, calls, finish) = collect(SAMPLE).await;
        assert_eq!(text, "Okay, let's check the weather:");
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "toolu_01T1x1fJ34qAmk2tNTrN7Up6");
        assert_eq!(calls[0].1, "get_weather");
        assert_eq!(calls[0].2["location"], "San Francisco, CA");
    }

    #[tokio::test]
    async fn errors_cut_off_streams_and_tools_without_arguments() {
        let overloaded = "data: {\"type\": \"error\", \"error\": {\"type\": \"overloaded_error\", \"message\": \"Overloaded\"}}\n";
        let err = run(overloaded).await.pop().unwrap().err().expect("error event must surface");
        assert!(err.to_string().contains("overloaded_error"), "{err}");

        let cut = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n";
        let evs = run(cut).await;
        assert!(matches!(evs.first(), Some(Ok(StreamEvent::Text(t))) if t == "Hel"));
        assert!(evs.last().unwrap().is_err(), "a cut-off reply must not look complete");

        let no_args = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"todo\",\"input\":{}}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        );
        let (_, calls, finish) = collect(no_args).await;
        assert_eq!(calls[0].2, json!({}));
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn thinking_tool_turn_is_saved_for_the_next_request() {
        let stream = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Read it."}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}
data: {"type":"content_block_stop","index":0}
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"read","input":{}}}
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}
data: {"type":"content_block_stop","index":1}
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}
"#;
        let turns = TurnCache::default();
        let evs = run_with(stream, turns.clone()).await;
        assert!(matches!(&evs[0], Ok(StreamEvent::Reasoning(t)) if t == "Read it."));
        let saved = turns.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].0, "toolu_9");
        assert_eq!(saved[0].1[0], json!({ "type": "thinking", "thinking": "Read it.", "signature": "sig-1" }));
        assert_eq!(saved[0].1[1]["input"], json!({ "path": "a" }));
    }
}

impl super::Client {
    /// `stream()` for `provider.api = "anthropic"`: POST `{base_url}/messages`
    /// (with the F-01 retries), then parse the SSE reply.
    pub(super) async fn stream_anthropic(
        &self,
        model: &str,
        messages: &[ChatMsg],
        tools: &[ToolSpec],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent>>> {
        let mut body = {
            let turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
            request_body(model, self.max_tokens, messages, tools, &turns)
        };
        if self.cache_markers(model) {
            add_cache_markers(&mut body);
        }
        let url = format!("{}/messages", self.base_url);
        let resp = self
            .send_with_retry(&url, || super::with_auth(self.http.post(&url), self.api, &self.api_key).json(&body))
            .await?;
        Ok(Box::pin(event_stream(resp.bytes_stream(), self.stall, self.turns.clone())))
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCallAccumulator, ToolCallReq};
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::{response::IntoResponse, routing::get, routing::post, Json, Router};

    /// Reply 1: thinking + a `read` tool call.
    const FIRST: &str = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}
data: {"type":"content_block_stop","index":0}
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}
data: {"type":"content_block_stop","index":1}
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}
"#;

    /// Reply 2, after the tool result.
    const SECOND: &str = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}
"#;

    fn auth_ok(h: &HeaderMap) -> bool {
        h.get("x-api-key").is_some_and(|v| v == "sk-test")
            && h.get("anthropic-version").is_some_and(|v| v == crate::provider::ANTHROPIC_VERSION)
            && h.get(header::AUTHORIZATION).is_none()
    }

    type Collected = (String, Vec<(String, String, Value)>, Option<String>);

    async fn collect(mut s: futures::stream::BoxStream<'static, Result<StreamEvent>>) -> Collected {
        let (mut text, mut acc, mut finish) = (String::new(), ToolCallAccumulator::default(), None);
        while let Some(ev) = s.next().await {
            match ev.unwrap() {
                StreamEvent::Text(t) => text.push_str(&t),
                StreamEvent::Done { finish_reason } => finish = finish_reason,
                ev => acc.push(&ev),
            }
        }
        (text, acc.finish(), finish)
    }

    /// Two steps of a tool loop against a fake endpoint. The second request
    /// is only answered when the thinking block came back unchanged.
    #[tokio::test]
    async fn tool_round_trip_sends_thinking_back() {
        let messages = post(|h: HeaderMap, Json(body): Json<Value>| async move {
            let echoed = &body["messages"][1]["content"][0];
            let sse = if !(auth_ok(&h) && system_text(&body) == "sys" && body["max_tokens"] == 321) {
                return (StatusCode::BAD_REQUEST, format!("unexpected: {body}")).into_response();
            } else if body["messages"].as_array().map_or(0, Vec::len) == 1 {
                FIRST
            } else if echoed["type"] == "thinking" && echoed["signature"] == "sig-1" {
                SECOND
            } else {
                return (StatusCode::BAD_REQUEST, format!("thinking not sent back: {body}")).into_response();
            };
            ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        });
        let models = get(|h: HeaderMap| async move {
            if !auth_ok(&h) {
                return (StatusCode::UNAUTHORIZED, "no key").into_response();
            }
            r#"{"data":[{"id":"claude-sonnet-5-5"}]}"#.into_response()
        });
        let app = Router::new().route("/v1/messages", messages).route("/v1/models", models);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = crate::config::ProviderCfg::default();
        cfg.api_key = "sk-test".into();
        cfg.model = "claude-sonnet-5-5".into();
        cfg.api = crate::config::ProviderApi::Anthropic;
        cfg.max_tokens = 321;
        let client = crate::provider::Client::from_cfg(&format!("http://{addr}/v1"), &cfg).unwrap();
        let msg = |role: &str, text: Option<&str>| ChatMsg {
            role: role.into(),
            content: text.map(String::from),
            tool_calls: None,
            tool_call_id: None,
        };
        let mut history = vec![msg("system", Some("sys")), msg("user", Some("read a"))];
        let (_, calls, finish) = collect(client.stream(&client.model, &history, &[]).await.unwrap()).await;
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        assert_eq!((calls[0].0.as_str(), calls[0].1.as_str()), ("toolu_1", "read"));

        // Store the call and its result the way run_loop does, then go on.
        let mut asst = msg("assistant", None);
        let function = FunctionCall { name: calls[0].1.clone(), arguments: calls[0].2.to_string() };
        asst.tool_calls = Some(vec![ToolCallReq { id: calls[0].0.clone(), kind: "function".into(), function }]);
        let mut result = msg("tool", Some("file text"));
        result.tool_call_id = Some(calls[0].0.clone());
        history.extend([asst, result]);
        let (text, _, finish) = collect(client.stream(&client.model, &history, &[]).await.unwrap()).await;
        assert_eq!((text.as_str(), finish.as_deref()), ("done", Some("stop")));
        assert_eq!(client.list_models().await.unwrap(), vec!["claude-sonnet-5-5"]);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn markers_on_tools_system_turn_start_and_newest_message() {
        let mut body = json!({
            "system": "sys",
            "tools": [{"name": "a"}, {"name": "b"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "old"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "user", "content": [{"type": "text", "text": "fix it"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hm", "signature": "s"},
                    {"type": "tool_use", "id": "t1", "name": "read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "text"},
                    {"type": "thinking", "thinking": "never here"}
                ]}
            ]
        });
        add_cache_markers(&mut body);
        let eph = json!({"type": "ephemeral"});
        assert_eq!(body["tools"][1]["cache_control"], eph);
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["system"][0]["cache_control"], eph);
        assert_eq!(system_text(&body), "sys");
        let m = &body["messages"];
        assert_eq!(m[2]["content"][0]["cache_control"], eph, "turn start");
        assert_eq!(m[4]["content"][0]["cache_control"], eph, "newest, not the thinking block");
        assert!(m[4]["content"][1].get("cache_control").is_none());
        assert!(m[0]["content"][0].get("cache_control").is_none());
        let count = body.to_string().matches("cache_control").count();
        assert!(count <= 4, "Anthropic allows 4 markers, got {count}");
    }

    #[test]
    fn usage_adds_cached_parts_to_the_prompt_total() {
        let mut st = SseState::default();
        st.line(r#"data: {"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":3000,"cache_creation_input_tokens":200,"output_tokens":1}}}"#).unwrap();
        let ev = st.line(r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":55}}"#).unwrap();
        let want = crate::provider::Usage { input: 3212, cached: 3000, cache_write: 200, output: 55, cost_micros: 0 };
        assert!(matches!(&ev[0], StreamEvent::Usage(u) if *u == want), "{ev:?}");
        assert!(matches!(ev[1], StreamEvent::Done { .. }));
    }
}
