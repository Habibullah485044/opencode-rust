use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// F-02: the Anthropic Messages API lives in `src/provider/anthropic.rs`.
mod anthropic;

/// OpenAI-compatible chat client (works with OpenRouter, Ollama, vLLM,
/// the user's Cloudflare-tunnel endpoint, etc). Streaming via SSE.
/// Cheap to clone (B-29): the inner reqwest client shares its pool.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
    /// Wire protocol (`provider.api`).
    api: ProviderApi,
    /// `max_tokens` for Anthropic requests (`provider.max_tokens`).
    max_tokens: u32,
    /// Anthropic only: recent tool-calling replies with their thinking
    /// blocks (F-02). Shared by all clones of this client.
    turns: anthropic::TurnCache,
    /// Max silence allowed between SSE reads before the stream errors out.
    /// Without it a wedged provider (dead tunnel, stuck backend) freezes the
    /// agent loop until the total request timeout fires — or forever.
    stall: std::time::Duration,
    /// Extra tries after a failed request (`provider.max_retries`).
    max_retries: u32,
    /// Wait before the first retry; doubles each time. Tests make it tiny.
    retry_base: std::time::Duration,
    /// Longest total wait for a rate limit (`provider.rate_limit_wait_secs`).
    rate_limit_wait: std::time::Duration,
    /// Shared by all clones: when one request hits a rate limit, every
    /// request of the app waits until it is over.
    gate: std::sync::Arc<RateGate>,
    /// When to send prompt-caching markers (`provider.prompt_cache`).
    prompt_cache: crate::config::PromptCache,
    /// Ask OpenAI-style servers for usage at the end (`provider.stream_usage`).
    stream_usage: bool,
    /// `provider.reasoning_effort`, sent when set.
    reasoning_effort: Option<String>,
    /// `provider.images`: auto / on / off.
    images: String,
}

/// Token counts of one reply, normalised across providers. `input` is the
/// whole prompt (cached part included); `cached` of it was read from the
/// provider's prompt cache (cheap), `cache_write` was newly stored there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl Usage {
    /// Read an OpenAI-style `usage` object (OpenAI, OpenRouter, DeepSeek,
    /// vLLM, Ollama). `None` when it carries no counts.
    pub fn from_openai(u: &Value) -> Option<Self> {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let input = n(&u["prompt_tokens"]);
        let output = n(&u["completion_tokens"]);
        if input == 0 && output == 0 {
            return None;
        }
        let details = &u["prompt_tokens_details"];
        // DeepSeek reports `prompt_cache_hit_tokens` instead of the details.
        let cached = n(&details["cached_tokens"]).max(n(&u["prompt_cache_hit_tokens"]));
        let cache_write = n(&details["cache_write_tokens"]);
        Some(Self { input, cached, cache_write, output })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: String, // system | user | assistant | tool
    /// B-21: always sent (`null` when empty); some servers reject a missing key.
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
    /// Token counts for this reply. Always sent before `Done`, which is
    /// where every reader stops.
    Usage(Usage),
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)] // B-21: usage-only chunks have no `choices`
    choices: Vec<SseChoice>,
    #[serde(default)]
    usage: Option<Value>,
}

#[derive(Deserialize)]
struct SseChoice {
    #[serde(default)] // B-21: some servers send the last chunk without `delta`
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
            // Split over the host's addresses: a dead one costs 3 s (two
            // addresses) before the next is tried.
            .connect_timeout(std::time::Duration::from_secs(6))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            api: ProviderApi::OpenAi,
            max_tokens: 8192,
            turns: Default::default(),
            stall,
            max_retries: 0,
            retry_base: std::time::Duration::from_secs(1),
            rate_limit_wait: std::time::Duration::ZERO,
            gate: Default::default(),
            prompt_cache: crate::config::PromptCache::Auto,
            stream_usage: true,
            reasoning_effort: None,
            images: "auto".into(),
        })
    }

    /// Build the client from the `[provider]` config section.
    pub fn from_cfg(base_url: &str, p: &crate::config::ProviderCfg) -> Result<Self> {
        let mut c = Self::new(base_url, &p.api_key, &p.model, p.request_timeout_secs)?;
        c.max_retries = p.max_retries;
        c.rate_limit_wait = std::time::Duration::from_secs(p.rate_limit_wait_secs);
        c.api = p.api;
        c.max_tokens = p.max_tokens;
        c.prompt_cache = p.prompt_cache;
        c.stream_usage = p.stream_usage;
        c.reasoning_effort = p.reasoning_effort.clone().filter(|e| !e.trim().is_empty());
        c.images = p.images.clone();
        Ok(c)
    }

    /// Send a request; return the response once its status is 2xx.
    /// Temporary failures (connection errors, 429/500/502/503/504/529) are
    /// sent again up to `max_retries` times, after 1s, 2s, 4s … (max 30s)
    /// plus jitter, or after the `Retry-After` seconds the server asked for.
    /// `build` makes a fresh request each time (a request is sent only once).
    /// Never prints: stderr output would corrupt the TUI.
    async fn send_with_retry(
        &self,
        url: &str,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let attempts = self.max_retries.saturating_add(1);
        let mut attempt: u32 = 0;
        let started = std::time::Instant::now();
        loop {
            // Another request of the app hit a rate limit: wait it out too.
            self.gate.wait().await;
            attempt += 1;
            // Each arm gives: error text, "worth retrying?", server's wait,
            // "rate limited?".
            let (mut msg, retryable, retry_after, limited) = match build().send().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = retry_after_secs(resp.headers());
                    let text = resp.text().await.unwrap_or_default();
                    let msg = format!(
                        "provider returned {status}: {}",
                        text.chars().take(400).collect::<String>()
                    );
                    (msg, is_retryable_status(status.as_u16()), retry_after, status.as_u16() == 429)
                }
                Err(e) => {
                    let msg = format!(
                        "provider request to {url} failed: {e:?} \
                         (tunnel down or endpoint unreachable — check the base URL / heartbeat in the header)"
                    );
                    (msg, is_retryable_send_error(&e), None, false)
                }
            };
            // A rate limit is waited out (all requests together) for up to
            // `rate_limit_wait` in total, however many tries that takes.
            if limited && !self.rate_limit_wait.is_zero() {
                let wait = retry_after
                    .map(std::time::Duration::from_secs)
                    .unwrap_or_else(|| backoff_delay(self.retry_base, attempt) + jitter());
                let waited = started.elapsed();
                if waited + wait > self.rate_limit_wait {
                    msg.push_str(&format!(
                        " (rate limited: the provider needs {}s more; waited {}s of the \
                         {}s allowed by provider.rate_limit_wait_secs. Try again later, \
                         or use another model or provider)",
                        wait.as_secs(),
                        waited.as_secs(),
                        self.rate_limit_wait.as_secs()
                    ));
                    return Err(anyhow!(msg));
                }
                self.gate.pause(wait);
                continue;
            }
            if !retryable || attempt >= attempts {
                if attempt > 1 {
                    msg.push_str(&format!(" (gave up after {attempt} attempts)"));
                }
                return Err(anyhow!(msg));
            }
            let wait = match retry_after {
                Some(secs) if secs > MAX_RETRY_WAIT.as_secs() => {
                    msg.push_str(&format!(
                        " (the provider asked to wait {secs}s before retrying; \
                         not waiting that long; gave up after {attempt} attempt(s))"
                    ));
                    return Err(anyhow!(msg));
                }
                Some(secs) => std::time::Duration::from_secs(secs),
                None => backoff_delay(self.retry_base, attempt) + jitter(),
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Stream a chat completion. `tools` may be empty for plain chat.
    pub async fn stream(
        &self,
        model: &str,
        messages: &[ChatMsg],
        tools: &[ToolSpec],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent>>> {
        if self.api == ProviderApi::Anthropic {
            return self.stream_anthropic(model, messages, tools).await;
        }
        let mut body = json!({ "model": model, "messages": messages, "stream": true });
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools)?;
        }
        if self.stream_usage {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(effort) = &self.reasoning_effort {
            // OpenAI's own API takes `reasoning_effort`; OpenRouter and most
            // routers take `reasoning.effort`.
            if self.base_url.contains("api.openai.com") {
                body["reasoning_effort"] = json!(effort);
            } else {
                body["reasoning"] = json!({ "effort": effort });
            }
        }
        if crate::attachments::model_sees_images(&self.images, model) {
            crate::attachments::attach_images(&mut body);
        }
        if self.cache_markers(model) {
            add_openai_cache_markers(&mut body);
        }
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .send_with_retry(&url, || {
                let req = self.http.post(&url).json(&body);
                if self.api_key.is_empty() {
                    req
                } else {
                    req.bearer_auth(&self.api_key)
                }
            })
            .await?;
    let byte_stream = resp.bytes_stream();
    let event_stream = Box::pin(sse_event_stream(byte_stream, self.stall));
    Ok(event_stream)
    }

    /// Send prompt-caching markers for this model? (`provider.prompt_cache`)
    /// "auto": always for Anthropic's API; on OpenAI-style routers only for
    /// Claude models (others cache a repeated prompt start on their own).
    fn cache_markers(&self, model: &str) -> bool {
        use crate::config::PromptCache;
        match self.prompt_cache {
            PromptCache::On => true,
            PromptCache::Off => false,
            PromptCache::Auto => {
                let m = model.to_ascii_lowercase();
                self.api == ProviderApi::Anthropic || m.contains("claude") || m.contains("anthropic/")
            }
        }
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
        let req = with_auth(self.http.get(format!("{}/models", self.base_url)), self.api, &self.api_key);
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
/// Mark where an OpenAI-style request may be cached (`cache_control`, the
/// form OpenRouter passes on to Anthropic): the system prompt, the last user
/// message (where this turn started) and the newest message. Each later
/// request repeats all of that unchanged, so it is read from the cache
/// instead of being paid in full. Anthropic allows 4 markers; this uses 3.
fn add_openai_cache_markers(body: &mut Value) {
    let Some(msgs) = body["messages"].as_array_mut() else { return };
    let mut marks: Vec<usize> = Vec::new();
    if let Some(i) = msgs.iter().position(|m| m["role"] == "system") {
        marks.push(i);
    }
    if let Some(i) = msgs.iter().rposition(|m| m["role"] == "user") {
        marks.push(i);
    }
    if !msgs.is_empty() {
        marks.push(msgs.len() - 1);
    }
    marks.sort_unstable();
    marks.dedup();
    for i in marks {
        let m = &mut msgs[i];
        // Only plain text can carry the marker; empty text is rejected.
        let Some(text) = m["content"].as_str().filter(|t| !t.trim().is_empty()) else { continue };
        let text = text.to_string();
        m["content"] = json!([{ "type": "text", "text": text, "cache_control": { "type": "ephemeral" } }]);
    }
}

/// How long to wait for the usage chunk after a finish_reason when the
/// server sends no `[DONE]`.
const USAGE_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

fn sse_event_stream(
    byte_stream: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    stall: std::time::Duration,
) -> impl futures::Stream<Item = Result<StreamEvent>> + Send + 'static {
    // Pin the inner stream so `next()` works inside the unfold closure.
    let byte_stream = Box::pin(byte_stream);
    // State: (source stream, line buffer, pending parsed events, the `Done`
    // held back while the usage chunk may still come, terminal seen).
    futures::stream::unfold(
        (byte_stream, Vec::<u8>::new(), std::collections::VecDeque::new(), None::<StreamEvent>, false),
        move |(mut src, mut buf, mut pending, mut held, mut done)| async move {
            loop {
                // 1) Emit any already-parsed event first.
                if let Some(ev) = pending.pop_front() {
                    return Some((Ok(ev), (src, buf, pending, held, done)));
                }
                // 2) If we saw terminal [DONE] and drained pending, stop.
                if done {
                    return None;
                }
                // 3) Extract complete lines from the buffer.
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&line).trim().to_string();
                    // B-21: an `error` frame fails the stream loudly.
                    if let Some(msg) = sse_error_message(&line) {
                        return Some((Err(anyhow!("provider error: {msg}")), (src, buf, pending, held, true)));
                    }
                    if let Some((events, line_done)) = parse_sse_line(&line) {
                        // B-21: `[DONE]` or a finish_reason ends the reply. A
                        // finish_reason is held back until `[DONE]` (or a short
                        // quiet spell): the usage chunk comes after it.
                        for ev in events {
                            match ev {
                                StreamEvent::Done { .. } if held.is_none() => held = Some(ev),
                                StreamEvent::Usage(_) => pending.push_back(ev),
                                _ if held.is_some() => {} // nothing else after the finish
                                _ => pending.push_back(ev),
                            }
                        }
                        if line_done {
                            pending.extend(held.take());
                            done = true;
                        }
                    }
                    continue;
                }
                // 4) Need more bytes from the transport. Bound the wait: a
                //    silent stream means a dead provider/tunnel — surface an
                //    error instead of hanging the agent loop forever.
                // After a finish_reason, wait only briefly for the usage chunk.
                let wait = if held.is_some() { USAGE_GRACE } else { stall };
                match tokio::time::timeout(wait, src.next()).await {
                    Err(_) if held.is_some() => {
                        pending.extend(held.take());
                        done = true;
                    }
                    Err(_) => {
                        return Some((
                            Err(anyhow!(
                                "provider stream stalled: no data for {}s",
                                stall.as_secs()
                            )),
                            (src, buf, pending, held, true),
                        ));
                    }
                    Ok(Some(Ok(bytes))) => buf.extend_from_slice(&bytes),
                    Ok(Some(Err(e))) => {
                        return Some((Err(anyhow!("stream error: {e}")), (src, buf, pending, held, true)));
                    }
                    Ok(None) if held.is_some() => {
                        // Closed after a finish_reason: the reply is complete.
                        let rest = String::from_utf8_lossy(&buf).trim().to_string();
                        buf.clear();
                        if let Some((events, _)) = parse_sse_line(&rest) {
                            pending.extend(events.into_iter().filter(|e| matches!(e, StreamEvent::Usage(_))));
                        }
                        pending.extend(held.take());
                        done = true;
                    }
                    Ok(None) => {
                        // B-21: closed before `[DONE]` and before any finish_reason.
                        // A last line without its newline still counts.
                        let rest = String::from_utf8_lossy(&buf).trim().to_string();
                        buf.clear();
                        if let Some((events, line_done)) = parse_sse_line(&rest) {
                            if line_done || events.iter().any(|e| matches!(e, StreamEvent::Done { .. })) {
                                pending.extend(events);
                                done = true;
                                continue;
                            }
                        }
                        return Some((
                            Err(anyhow!(
                                "provider stream ended early: the connection closed before \
                                 the reply was finished"
                            )),
                            (src, buf, pending, held, true),
                        ));
                    }
                }
            }
        },
    )
}

/// The message of a mid-stream error frame such as
/// `data: {"error": {"message": "..."}}` (B-21), or `None` for other lines.
fn sse_error_message(line: &str) -> Option<String> {
    let data = line.strip_prefix("data:")?.trim();
    if !data.starts_with('{') || !data.contains("\"error\"") {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    let err = v.get("error").filter(|e| !e.is_null())?;
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string());
    Some(msg.chars().take(400).collect())
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
        if let Some(fr) = choice.finish_reason.as_ref().filter(|f| !f.is_empty()) {
            // Emitted AFTER any deltas on this same line so nothing is lost.
            events.push(StreamEvent::Done { finish_reason: Some(fr.clone()) });
        }
    }
    // Usage comes in its own chunk after the finish_reason (or on the same
    // line); it always goes before `Done`, where readers stop.
    if let Some(u) = chunk.usage.as_ref().and_then(Usage::from_openai) {
        let at = events.iter().position(|e| matches!(e, StreamEvent::Done { .. })).unwrap_or(events.len());
        events.insert(at, StreamEvent::Usage(u));
    }
    Some((events, false))
}

/// Accumulates streamed tool-call fragments into final calls.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: std::collections::BTreeMap<usize, (String, String, String)>, // slot -> (id, name, args)
    /// B-21: provider `index` -> slot in `calls`.
    slot_of: std::collections::HashMap<usize, usize>,
    next_slot: usize,
}

impl ToolCallAccumulator {
    pub fn push(&mut self, ev: &StreamEvent) {
        if let StreamEvent::ToolCallDelta { index, id, name, args_delta } = ev {
            let new_id = id.as_deref().filter(|s| !s.is_empty());
            // B-21: some servers send every call as `index: 0`. A different id
            // on a known index means a new call, not more of the old one.
            let slot = match self.slot_of.get(index).copied() {
                Some(s) => {
                    let known = self.calls.get(&s).map(|c| c.0.as_str()).unwrap_or("");
                    let other_call = matches!(new_id, Some(n) if !known.is_empty() && known != n);
                    if other_call { self.new_slot(*index) } else { s }
                }
                None => self.new_slot(*index),
            };
            let entry = self.calls.entry(slot).or_default();
            if let Some(n) = new_id {
                entry.0 = n.to_string();
            }
            if let Some(name) = name {
                if !name.is_empty() {
                    entry.1 = name.clone();
                }
            }
            entry.2.push_str(args_delta);
        }
    }

    /// B-21: start a new call slot for provider `index`.
    fn new_slot(&mut self, index: usize) -> usize {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.slot_of.insert(index, slot);
        slot
    }

    /// Returns final calls; args parsed as JSON (falls back to {"raw": args}).
    /// B-21: empty args become `{}`; a missing id gets a generated one.
    pub fn finish(self) -> Vec<(String, String, Value)> {
        self.calls
            .into_values()
            .map(|(id, name, args)| {
                let v = if args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&args)
                        .ok()
                        .or_else(|| repair_cut_args(&name, &args))
                        .unwrap_or_else(|| json!({ "raw": args }))
                };
                let id = if id.is_empty() { crate::db::new_id("call") } else { id };
                (id, name, v)
            })
            .collect()
    }
}

/// Some servers drop the last stream chunk, so args arrive as
/// `{"question": "What next?` with the closing quote and braces missing.
/// Close them, but only for tools where a lost tail is harmless: a cut
/// `write` or `edit` must still fail rather than save half a file.
fn repair_cut_args(name: &str, args: &str) -> Option<Value> {
    const SAFE: [&str; 5] = ["ask_user", "read", "list", "glob", "grep"];
    if !SAFE.contains(&name) {
        return None;
    }
    let (mut in_str, mut escaped) = (false, false);
    let mut open = Vec::new();
    for c in args.chars() {
        if in_str {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => open.push('}'),
            '[' => open.push(']'),
            '}' | ']' => {
                open.pop();
            }
            _ => {}
        }
    }
    if !in_str && open.is_empty() {
        return None;
    }
    let mut fixed = args.to_string();
    if escaped {
        fixed.pop();
    }
    if in_str {
        fixed.push('"');
    }
    let trimmed = fixed.trim_end().strip_suffix(',').unwrap_or(fixed.trim_end()).to_string();
    let closed: String = trimmed.chars().chain(open.iter().rev().copied()).collect();
    serde_json::from_str::<Value>(&closed).ok().filter(Value::is_object)
}

/// When the app may send its next request (B: shared rate-limit wait).
#[derive(Default)]
pub struct RateGate {
    until: std::sync::Mutex<Option<std::time::Instant>>,
}

impl RateGate {
    /// Hold every request for `wait` from now (a longer pause stays).
    fn pause(&self, wait: std::time::Duration) {
        let mut until = self.until.lock().unwrap_or_else(|e| e.into_inner());
        let end = std::time::Instant::now() + wait;
        if until.is_none_or(|u| u < end) {
            *until = Some(end);
        }
    }

    /// Return once no pause is in force.
    async fn wait(&self) {
        loop {
            let until = *self.until.lock().unwrap_or_else(|e| e.into_inner());
            match until {
                Some(u) if u > std::time::Instant::now() => tokio::time::sleep_until(u.into()).await,
                _ => return,
            }
        }
    }
}

/// Longest wait between two retries. A longer `Retry-After` is not waited
/// out: the request fails at once and the error says why.
const MAX_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// "Busy, try again soon": rate limit, server errors, Anthropic's 529.
/// Anything else (400, 401, 404 …) means the request itself is wrong.
fn is_retryable_status(code: u16) -> bool {
    matches!(code, 429 | 500 | 502 | 503 | 504 | 529)
}

/// The connection failed or broke before the response headers. Not
/// retried: a full request timeout (it already waited) and a bad URL.
fn is_retryable_send_error(e: &reqwest::Error) -> bool {
    e.is_connect() || (e.is_request() && !e.is_timeout())
}

/// `Retry-After: <seconds>`. The rare HTTP-date form is ignored.
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Wait before retry number `attempt` (1, 2, 3 …): base, 2×base, 4×base …
fn backoff_delay(base: std::time::Duration, attempt: u32) -> std::time::Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(16);
    base.saturating_mul(factor).min(MAX_RETRY_WAIT)
}

/// 0–249 ms extra, so many clients do not retry at the same moment.
/// `RandomState` is randomly keyed per process: enough for jitter.
fn jitter() -> std::time::Duration {
    use std::hash::BuildHasher;
    let n = std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now());
    std::time::Duration::from_millis(n % 250)
}

use crate::config::ProviderApi;

/// Value of the `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Put the API key on a request the way the configured API expects it.
/// OpenAI-compatible: `Authorization: Bearer <key>` (skipped for an empty
/// key, e.g. local Ollama). Anthropic: `x-api-key` plus `anthropic-version`.
pub fn with_auth(req: reqwest::RequestBuilder, api: ProviderApi, api_key: &str) -> reqwest::RequestBuilder {
    match api {
        ProviderApi::OpenAi if api_key.is_empty() => req,
        ProviderApi::OpenAi => req.bearer_auth(api_key),
        ProviderApi::Anthropic => {
            let req = req.header("anthropic-version", ANTHROPIC_VERSION);
            if api_key.is_empty() {
                req
            } else {
                req.header("x-api-key", api_key)
            }
        }
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

#[cfg(test)]
mod b21_stream_tests {
    use super::*;

    async fn collect(body: &str) -> Vec<Result<StreamEvent>> {
        let src = futures::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(body.to_string()))]);
        let mut s = Box::pin(sse_event_stream(src, std::time::Duration::from_secs(5)));
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            let stop = ev.is_err();
            out.push(ev);
            if stop {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn error_frame_fails_the_stream() {
        let out = collect("data: {\"error\":{\"message\":\"upstream overloaded\",\"code\":502}}\n\n").await;
        let err = out.last().unwrap().as_ref().unwrap_err().to_string();
        assert!(err.contains("upstream overloaded"), "{err}");
    }

    #[tokio::test]
    async fn early_close_is_an_error() {
        let out = collect("data: {\"choices\":[{\"delta\":{\"content\":\"hal\"}}]}\n\n").await;
        assert!(matches!(out[0], Ok(StreamEvent::Text(ref t)) if t == "hal"));
        let err = out.last().unwrap().as_ref().unwrap_err().to_string();
        assert!(err.contains("ended early"), "{err}");
    }

    #[tokio::test]
    async fn finish_without_done_and_missing_delta_are_fine() {
        // The last chunk has no `delta`, no newline, and there is no `[DONE]`.
        let out = collect(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"stop\"}]}",
        )
        .await;
        assert_eq!(out.len(), 2, "text + done, no error");
        assert!(matches!(out[1], Ok(StreamEvent::Done { finish_reason: Some(ref f) }) if f == "stop"));
    }
}

#[cfg(test)]
mod b21_calls_tests {
    use super::*;

    fn delta(index: usize, id: Option<&str>, args: &str) -> StreamEvent {
        StreamEvent::ToolCallDelta {
            index,
            id: id.map(str::to_string),
            name: Some("read".into()),
            args_delta: args.into(),
        }
    }

    #[test]
    fn calls_that_reuse_index_zero_stay_separate() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(&delta(0, Some("a"), "{\"path\":\"x\"}"));
        acc.push(&delta(0, Some("b"), "{\"path\":\"y\"}"));
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].0.as_str(), calls[0].2["path"].as_str()), ("a", Some("x")));
        assert_eq!((calls[1].0.as_str(), calls[1].2["path"].as_str()), ("b", Some("y")));
    }

    #[test]
    fn missing_ids_and_empty_args_are_filled_in() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(&delta(0, None, ""));
        acc.push(&delta(1, None, ""));
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].0.is_empty() && calls[0].0 != calls[1].0, "{calls:?}");
        assert_eq!(calls[0].2, json!({}));
    }

    #[test]
    fn cut_args_are_closed_for_safe_tools_only() {
        let cut = "{\"options\": [\"Fix a bug\", \"Write tests\"], \"question\": \"What next?";
        let call = |name: &str| {
            let mut acc = ToolCallAccumulator::default();
            acc.push(&StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("a".into()),
                name: Some(name.into()),
                args_delta: cut.into(),
            });
            acc.finish().remove(0).2
        };
        let asked = call("ask_user");
        assert_eq!(asked["question"], "What next?");
        assert_eq!(asked["options"][1], "Write tests");
        // A cut write keeps the raw marker, so it errors instead of saving half a file.
        assert!(call("write").get("raw").is_some());
        // A dangling key cannot be repaired.
        assert_eq!(repair_cut_args("read", "{\"path\": \"x\", \"lim"), None);
        // An escaped backslash before the cut does not end the string early.
        assert_eq!(repair_cut_args("read", "{\"path\": \"a\\\\b"), Some(json!({"path": "a\\b"})));
    }

    #[test]
    fn assistant_tool_call_message_sends_null_content() {
        let msg = ChatMsg { role: "assistant".into(), content: None, tool_calls: Some(vec![]), tool_call_id: None };
        let v = serde_json::to_value(&msg).unwrap();
        assert!(v.as_object().unwrap().contains_key("content"), "{v}");
        assert!(v["content"].is_null());
    }
}

#[cfg(test)]
mod f01_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn retry_rules() {
        let s = Duration::from_secs;
        assert_eq!(backoff_delay(s(1), 1), s(1));
        assert_eq!(backoff_delay(s(1), 3), s(4));
        assert_eq!(backoff_delay(s(1), 6), s(30), "capped at 30s");
        assert_eq!(backoff_delay(s(1), 99), s(30), "no overflow");
        assert!([429, 500, 502, 503, 504, 529].iter().all(|c| is_retryable_status(*c)));
        assert!([400, 401, 403, 404].iter().all(|c| !is_retryable_status(*c)));
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_secs(&h), None);
        h.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after_secs(&h), Some(7));
        assert!(jitter() < Duration::from_millis(250));
    }

    /// Request N gets `script[N]` (a status and an optional Retry-After);
    /// after that, a valid streamed reply "hello". Returns base URL + hit count.
    async fn scripted_provider(script: Vec<(u16, Option<&'static str>)>) -> (String, Arc<AtomicUsize>) {
        use axum::http::{header, HeaderValue, StatusCode};
        use axum::{response::IntoResponse, routing::post, Router};
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let step = script.get(counter.fetch_add(1, Ordering::SeqCst)).copied();
                async move {
                    if let Some((code, retry_after)) = step {
                        let mut resp = (StatusCode::from_u16(code).unwrap(), "busy").into_response();
                        if let Some(v) = retry_after {
                            resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static(v));
                        }
                        return resp;
                    }
                    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/v1"), hits)
    }

    fn fast_client(base: &str, max_retries: u32) -> Client {
        let mut c = Client::new(base, "", "stub", 30).unwrap();
        c.max_retries = max_retries;
        c.retry_base = Duration::from_millis(10);
        c
    }

    #[tokio::test]
    async fn retries_503_twice_then_streams_the_reply() {
        // The first 503 has no Retry-After (backoff path), the second says 0s.
        let (base, hits) = scripted_provider(vec![(503, None), (503, Some("0"))]).await;
        let client = fast_client(&base, 3);
        let mut s = client.stream("stub", &[], &[]).await.expect("third try succeeds");
        let mut text = String::new();
        while let Some(ev) = s.next().await {
            if let StreamEvent::Text(t) = ev.unwrap() {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "hello");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_and_counts_attempts() {
        let (base, hits) = scripted_provider(vec![(503, None); 10]).await;
        let err = fast_client(&base, 2).stream("stub", &[], &[]).await.err().expect("fails");
        assert!(err.to_string().contains("gave up after 3 attempts"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 3, "1 try + 2 retries");
    }

    #[tokio::test]
    async fn rate_limits_are_waited_out_beyond_max_retries() {
        let (base, hits) = scripted_provider(vec![(429, Some("0")), (429, None), (429, Some("0"))]).await;
        let mut c = fast_client(&base, 0);
        c.rate_limit_wait = Duration::from_secs(10);
        assert!(c.stream("stub", &[], &[]).await.is_ok(), "the reply comes after the limit");
        assert_eq!(hits.load(Ordering::SeqCst), 4);

        let (base, hits) = scripted_provider(vec![(429, Some("120"))]).await;
        let mut c = fast_client(&base, 3);
        c.rate_limit_wait = Duration::from_secs(10);
        let err = c.stream("stub", &[], &[]).await.err().expect("fails");
        assert!(err.to_string().contains("rate_limit_wait_secs"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a wait beyond the budget fails at once");
    }

    #[tokio::test]
    async fn reasoning_effort_is_sent_only_when_set() {
        use axum::{routing::post, Router};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::extract::Json<Value>| {
                let log = log.clone();
                async move {
                    log.lock().unwrap().push(body.0);
                    "data: [DONE]\n\n"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut c = fast_client(&format!("http://{addr}/v1"), 0);
        let _ = c.stream("stub", &[], &[]).await.unwrap();
        c.reasoning_effort = Some("low".into());
        let _ = c.stream("stub", &[], &[]).await.unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen[0].get("reasoning").is_none(), "unset: nothing extra is sent");
        assert_eq!(seen[1]["reasoning"]["effort"], "low");
    }

    #[tokio::test]
    async fn one_rate_limit_holds_every_request() {
        let gate = RateGate::default();
        gate.pause(Duration::from_millis(300));
        gate.pause(Duration::from_millis(50)); // a shorter pause does not cut it
        let t = std::time::Instant::now();
        gate.wait().await;
        assert!(t.elapsed() >= Duration::from_millis(250), "{:?}", t.elapsed());
        let t = std::time::Instant::now();
        gate.wait().await;
        assert!(t.elapsed() < Duration::from_millis(50), "no pause, no wait");
    }

    #[tokio::test]
    async fn bad_request_and_long_retry_after_are_not_retried() {
        let (base, hits) = scripted_provider(vec![(400, None)]).await;
        let err = fast_client(&base, 3).stream("stub", &[], &[]).await.err().expect("fails");
        assert!(err.to_string().contains("400"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let (base, hits) = scripted_provider(vec![(429, Some("120"))]).await;
        let err = fast_client(&base, 3).stream("stub", &[], &[]).await.err().expect("fails");
        assert!(err.to_string().contains("120s"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a 2-minute Retry-After is not waited out");
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    async fn events(chunks: Vec<&str>) -> Vec<StreamEvent> {
        let src = futures::stream::iter(
            chunks.into_iter().map(|c| Ok::<_, reqwest::Error>(bytes::Bytes::from(c.to_string()))).collect::<Vec<_>>(),
        );
        let mut s = Box::pin(sse_event_stream(src, std::time::Duration::from_secs(5)));
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev.unwrap());
        }
        out
    }

    /// The usage chunk comes after the finish_reason; readers stop at `Done`,
    /// so `Usage` must be sent before it.
    #[tokio::test]
    async fn usage_after_finish_reason_comes_before_done() {
        let out = events(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":20,\
             \"prompt_tokens_details\":{\"cached_tokens\":900}}}\n\n",
            "data: [DONE]\n\n",
        ])
        .await;
        assert!(matches!(out[0], StreamEvent::Text(_)));
        let want = Usage { input: 1000, cached: 900, cache_write: 0, output: 20 };
        assert!(matches!(&out[1], StreamEvent::Usage(u) if *u == want), "{out:?}");
        assert!(matches!(out[2], StreamEvent::Done { .. }));
        assert_eq!(out.len(), 3);
    }

    /// A server that never sends usage or `[DONE]` still ends the reply.
    #[tokio::test]
    async fn finish_without_done_still_ends() {
        let out = events(vec!["data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\n"]).await;
        assert!(matches!(out.last(), Some(StreamEvent::Done { .. })), "{out:?}");
    }

    /// The server keeps the connection open after the finish_reason and
    /// never sends `[DONE]`: the reply still ends after a short grace.
    #[tokio::test]
    async fn open_connection_after_finish_ends_after_grace() {
        let first = futures::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}

",
        ))]);
        let src = first.chain(futures::stream::pending());
        let mut s = Box::pin(sse_event_stream(src, std::time::Duration::from_secs(60)));
        let t0 = std::time::Instant::now();
        let mut last = None;
        while let Some(ev) = s.next().await {
            last = Some(ev.unwrap());
        }
        assert!(matches!(last, Some(StreamEvent::Done { .. })));
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "{:?}", t0.elapsed());
    }

    #[test]
    fn usage_reads_openai_and_deepseek_shapes() {
        let o = Usage::from_openai(&json!({"prompt_tokens": 50, "completion_tokens": 5,
            "prompt_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 3}})).unwrap();
        assert_eq!(o, Usage { input: 50, cached: 40, cache_write: 3, output: 5 });
        let d = Usage::from_openai(&json!({"prompt_tokens": 50, "completion_tokens": 5,
            "prompt_cache_hit_tokens": 30})).unwrap();
        assert_eq!(d.cached, 30);
        assert!(Usage::from_openai(&json!({})).is_none());
    }

    #[test]
    fn openai_cache_markers_go_on_system_turn_start_and_newest() {
        let mut body = json!({"messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "old question"},
            {"role": "assistant", "content": "answer"},
            {"role": "user", "content": "new question"},
            {"role": "assistant", "content": null, "tool_calls": []},
            {"role": "tool", "content": "file text", "tool_call_id": "c1"},
        ]});
        add_openai_cache_markers(&mut body);
        let m = body["messages"].as_array().unwrap();
        let marked = |i: usize| m[i]["content"][0]["cache_control"]["type"] == "ephemeral";
        assert!(marked(0) && marked(3) && marked(5), "{body}");
        assert_eq!(m[1]["content"], "old question", "older messages stay plain text");
        assert_eq!(m[3]["content"][0]["text"], "new question");
    }

    #[test]
    fn auto_markers_only_where_they_are_needed() {
        let mut c = Client::new("http://x", "", "m", 10).unwrap();
        assert!(!c.cache_markers("gpt-5"), "OpenAI caches on its own");
        assert!(!c.cache_markers("deepseek-chat"));
        assert!(c.cache_markers("anthropic/claude-sonnet-5-5"), "Claude behind a router");
        c.prompt_cache = crate::config::PromptCache::Off;
        assert!(!c.cache_markers("anthropic/claude-sonnet-5-5"));
        c.prompt_cache = crate::config::PromptCache::On;
        assert!(c.cache_markers("llama3"));
        c.prompt_cache = crate::config::PromptCache::Auto;
        c.api = ProviderApi::Anthropic;
        assert!(c.cache_markers("anything"));
    }
}
