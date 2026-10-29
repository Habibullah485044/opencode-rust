//! OpenAI-compatible mock LLM for E2E testing.
//! Script: first call → tool call `write` creating a file; second call → final text.
//!        Prompt "parallel-agents …" → two `spawn` calls in one turn (the
//!        children see "task one"/"task two" and answer immediately).
//! Payloads are built from serde_json to avoid hand-escaping bugs.
//! Run: cargo run --example mock_llm -- 18080
use axum::{extract::Json, routing::get, routing::post, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

fn sse_data(v: &Value) -> String {
    format!("data: {}\n\n", serde_json::to_string(v).unwrap())
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({ "choices": [{ "delta": delta, "finish_reason": finish }] })
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(18080);
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        // GET /v1/models — keeps the heartbeat honest (expects 2xx).
        .route(
            "/v1/models",
            get(|| async {
                axum::Json(serde_json::json!({
                    "object": "list",
                    "data": [{"id": "mock", "object": "model"}]
                }))
            }),
        )
        .route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let calls = calls.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let msgs: Vec<Value> = body["messages"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let has_tool_result = msgs.iter().any(|m| m["role"] == "tool");
                let first_user = msgs
                    .iter()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("");
                // Skill-vetting call (one_shot) asks for WORTHY/SKIP verbatim.
                let is_vetting = msgs.iter().any(|m| m.to_string().contains("WORTHY or SKIP"));
                // Proves a MISSION briefing reached the provider end-to-end.
                let saw_mission = msgs.iter().any(|m| m.to_string().contains("MISSION:"));
                // Goal B E2E: a prompt containing "browser-smoke" drives the
                // embedded panel, then echoes whatever the tool returned.
                let is_browser_smoke = first_user.contains("browser-smoke");
                let last_tool = msgs
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .last()
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                let branch = if is_vetting {
                    "verdict(WORTHY)"
                } else if is_browser_smoke && has_tool_result {
                    "browser echo"
                } else if is_browser_smoke {
                    "tool_call(browser_navigate)"
                } else if has_tool_result {
                    "final text"
                } else if first_user.contains("parallel-agents") {
                    "tool_calls(spawn x2)"
                } else if first_user == "task one" || first_user == "task two" {
                    "child report"
                } else {
                    "tool_call(write)"
                };
                let chunk_body = if is_vetting {
                    format!(
                        "{}{}",
                        sse_data(&chunk(
                            json!({"content": "WORTHY: durable deploy workflow with concrete steps — bake it in."}),
                            Value::Null,
                        )),
                        "data: [DONE]\n\n"
                    )
                } else if is_browser_smoke && has_tool_result {
                    // Echo the tool output so a test can assert on it.
                    format!(
                        "{}{}",
                        sse_data(&chunk(
                            json!({"content": format!(
                                "BROWSER-RESULT: {}",
                                last_tool.chars().take(900).collect::<String>()
                            )}),
                            Value::Null,
                        )),
                        "data: [DONE]\n\n".to_string()
                    )
                } else if is_browser_smoke {
                    let args = json!({"url": "http://127.0.0.1:8791/"}).to_string();
                    format!(
                        "{}{}",
                        sse_data(&chunk(
                            json!({"tool_calls": [{"index": 0, "id": "call_br", "function": {"name": "browser_navigate", "arguments": args}}]}),
                            json!("tool_calls"),
                        )),
                        "data: [DONE]\n\n".to_string()
                    )
                } else if has_tool_result {
                    // Final answer: reasoning first (proves thinking display), then content.
                    let flag = if saw_mission { " [mock-saw-mission-briefing]" } else { "" };
                    format!(
                        "{}{}{}",
                        sse_data(&chunk(
                            json!({"reasoning_content": "The write already succeeded; I should report the result to the user."}),
                            Value::Null,
                        )),
                        sse_data(&chunk(
                            json!({"content": format!("Done. I created e2e-marker.txt via the write tool.{flag}")}),
                            Value::Null,
                        )),
                        "data: [DONE]\n\n".to_string()
                    )
                } else if first_user.contains("text-call") {
                    // A model with no tool parser: the call written as one
                    // line of text, with no `tool_calls` field at all — what
                    // a gateway in front of Ollama produced on 2026-10-26.
                    format!(
                        "{}{}",
                        sse_data(&chunk(
                            json!({"content": "CALL {\"name\":\"write\",\"arguments\":{\"path\":\"text-call-marker.txt\",\"content\":\"written by a text call\"}}"}),
                            Value::Null,
                        )),
                        "data: [DONE]\n\n".to_string()
                    )
                } else if first_user.contains("parallel-agents") {
                    // M7: two spawns in ONE turn — the agents panel shows them
                    // working in parallel.
                    format!(
                        "{}{}",
                        sse_data(&chunk(
                            json!({"tool_calls": [
                                {"index": 0, "id": "call_sp1", "function": {"name": "spawn", "arguments": "{\"subagent\":\"explore\",\"task\":\"task one\"}"}},
                                {"index": 1, "id": "call_sp2", "function": {"name": "spawn", "arguments": "{\"subagent\":\"worker\",\"task\":\"task two\"}"}}
                            ]}),
                            json!("tool_calls"),
                        )),
                        "data: [DONE]\n\n".to_string()
                    )
                } else if first_user == "task one" || first_user == "task two" {
                    // A spawned child: report immediately, no tools.
                    format!(
                        "{}{}",
                        sse_data(&chunk(
                            json!({"content": format!("child report: {first_user} ok")}),
                            Value::Null,
                        )),
                        "data: [DONE]\n\n".to_string()
                    )
                } else {
                    // Tool call: brief reasoning, then arguments split across
                    // two chunks (exercises the accumulator).
                    format!(
                        "{}{}{}{}",
                        sse_data(&chunk(
                            json!({"reasoning_content": "The user wants a marker file; the write tool is the right call."}),
                            Value::Null,
                        )),
                        sse_data(&chunk(
                            json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "write", "arguments": "{\"path\":\"e2e-marker.txt\","}}]}),
                            Value::Null,
                        )),
                        sse_data(&chunk(
                            json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"content\":\"hello from opencode-rust E2E\\n\"}"}}]}),
                            json!("tool_calls"),
                        )),
                        "data: [DONE]\n\n".to_string()
                    )
                };
                println!(
                    "[mock] call #{} → {}{}",
                    n,
                    branch,
                    if saw_mission { " saw-mission" } else { "" }
                );
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], chunk_body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    println!("[mock] OpenAI-compatible mock listening on http://127.0.0.1:{port}/v1");
    let _ = axum::serve(listener, app).await;
}
