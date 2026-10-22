use crate::db::Part;
use crate::server::{one_shot, Shared, ServerEvent};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// MiniMax-style team mode: one goal → planner decomposes into subtasks →
/// parallel worker sessions execute → reviewer verifies → synthesis.
/// Deliberately reuses the session/tool machinery — this is orchestration,
/// not new machinery.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtask {
    pub title: String,
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerReport {
    pub subtask: String,
    pub outcome: String, // "done" | "failed"
    pub summary: String,
}

pub async fn run_team(
    st: &Shared,
    session_id: &str,
    goal: &str,
    abort: Arc<AtomicBool>,
) -> Result<String> {
    let _ = st.events.send(ServerEvent::AssistantDelta {
        session_id: session_id.to_string(),
        text: format!("[team] decomposing goal: {goal}\n"),
    });

    st.db.add_message(session_id, "user", &[Part::Text { text: format!("/team {goal}") }])?;

    // 1. PLANNER — read-only agent proposes subtasks as JSON.
    let subtasks = plan(st, goal, &abort).await?;
    let _ = st.events.send(ServerEvent::AssistantDelta {
        session_id: session_id.to_string(),
        text: format!("[team] plan: {} subtask(s)\n", subtasks.len()),
    });
    for (i, t) in subtasks.iter().enumerate() {
        let _ = st.events.send(ServerEvent::AssistantDelta {
            session_id: session_id.to_string(),
            text: format!("  {}. {} — {}\n", i + 1, t.title, t.instructions),
        });
    }

    // 2. WORKERS — one child session per subtask, run in parallel.
    let mut reports = Vec::new();
    let mut handles = Vec::new();
    for (i, task) in subtasks.iter().enumerate() {
        let st2 = st.clone();
        let parent = session_id.to_string();
        let task = task.clone();
        let abort2 = abort.clone();
        let goal = goal.to_string();
        handles.push(tokio::spawn(async move {
            let r = worker(&st2, &parent, i, &task, &goal, abort2).await;
            (i, task.title.clone(), r)
        }));
    }
    for h in handles {
        match h.await {
            Ok((i, title, Ok(summary))) => {
                let _ = st.events.send(ServerEvent::AssistantDelta {
                    session_id: session_id.to_string(),
                    text: format!("[team] worker {i} ({title}) done\n"),
                });
                reports.push(WorkerReport { subtask: title, outcome: "done".into(), summary });
            }
            Ok((i, title, Err(e))) => {
                let _ = st.events.send(ServerEvent::AssistantDelta {
                    session_id: session_id.to_string(),
                    text: format!("[team] worker {i} ({title}) failed: {e}\n"),
                });
                reports.push(WorkerReport {
                    subtask: title,
                    outcome: "failed".into(),
                    summary: format!("error: {e}"),
                });
            }
            Err(e) => {
                reports.push(WorkerReport {
                    subtask: "unknown".into(),
                    outcome: "failed".into(),
                    summary: format!("join error: {e}"),
                });
            }
        }
    }

    // 3. REVIEWER — one call verifies the reports (plan: "reviewer agent
    // verifies"), using the small model when configured.
    let verdict = match review(st, goal, &reports, &abort).await {
        Ok(v) => v,
        Err(e) => format!("reviewer unavailable: {e}"),
    };
    let _ = st.events.send(ServerEvent::AssistantDelta {
        session_id: session_id.to_string(),
        text: format!("[team] reviewer: {}\n", verdict.lines().next().unwrap_or("-")),
    });

    // 4. SYNTHESIS — verdict + per-report results in the parent session.
    let mut summary = format!("[team] results for: {goal}\n");
    summary.push_str(&format!("[team] reviewer: {verdict}\n"));
    for r in &reports {
        summary.push_str(&format!("- [{}] {}: {}\n", r.outcome, r.subtask, r.summary));
    }
    st.db.add_message(session_id, "assistant", &[Part::Text { text: summary.clone() }])?;
    Ok(summary)
}

/// Planner: one non-streamed call asking for JSON array of subtasks.
async fn plan(st: &Shared, goal: &str, abort: &AtomicBool) -> Result<Vec<Subtask>> {
    let text = one_shot(
        st,
        "You are a planning agent. Decompose the user's goal into 1-4 independent \
         subtasks that can be worked in parallel. Respond ONLY with a JSON array: \
         [{\"title\": string, \"instructions\": string}]. No prose.",
        goal,
        abort,
        true,
    )
    .await?;
    Ok(parse_subtasks(&text))
}

/// Reviewer: one non-streamed call verifying the worker reports.
async fn review(
    st: &Shared,
    goal: &str,
    reports: &[WorkerReport],
    abort: &AtomicBool,
) -> Result<String> {
    let reports_json = serde_json::to_string_pretty(reports)?;
    let user = format!("GOAL: {goal}\n\nWORKER REPORTS:\n{reports_json}");
    let text = one_shot(
        st,
        "You are the reviewer agent in team mode. Verify the worker reports against the \
         goal. Start with exactly \"VERDICT: PASS\" or \"VERDICT: FAIL\", then at most \
         3 short bullets.",
        &user,
        abort,
        true,
    )
    .await?;
    Ok(text.trim().to_string())
}

/// Robustly extract a JSON array from model output (handles ```json fences etc).
fn parse_subtasks(text: &str) -> Vec<Subtask> {
    let start = text.find('[');
    let end = text.rfind(']');
    if let (Some(s), Some(e)) = (start, end) {
        if e > s {
            if let Ok(arr) = serde_json::from_str::<Vec<Subtask>>(&text[s..=e]) {
                if !arr.is_empty() {
                    return arr;
                }
            }
            // Maybe fields differ; try generic.
            if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&text[s..=e]) {
                let out: Vec<Subtask> = arr
                    .iter()
                    .filter_map(|v| {
                        Some(Subtask {
                            title: v.get("title")?.as_str()?.to_string(),
                            instructions: v
                                .get("instructions")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(), 
                        })
                    })
                    .collect();
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }
    // Fallback: single subtask = the whole goal.
    vec![Subtask { title: "task".into(), instructions: text.chars().take(2000).collect() }]
}

/// Worker: child session with the standard agent loop but scoped tools.
async fn worker(
    st: &Shared,
    parent: &str,
    _idx: usize,
    task: &Subtask,
    goal: &str,
    abort: Arc<AtomicBool>,
) -> Result<String> {
    let session = st.db.create_session(&task.title, Some(parent))?;
    let prompt = format!(
        "Overall goal: {goal}\n\nYour subtask: {}\n{}",
        task.title, task.instructions
    );
    run_worker_agent(st, &session.id, &prompt, &abort).await
}

/// Worker variant of the agent loop — now the SHARED loop with the worker's
/// system prompt: no activity events, no spawn tool (M2 dedup).
async fn run_worker_agent(
    st: &Shared,
    session_id: &str,
    prompt: &str,
    abort: &AtomicBool,
) -> Result<String> {
    st.db.add_message(session_id, "user", &[Part::Text { text: prompt.to_string() }])?;
    let mut system = format!(
        "You are a focused worker agent in team mode. Complete only your subtask.\n\n\
         Workspace: {}.\nReport a 1-2 sentence summary when finished.",
        st.workspace.display()
    );
    if st.cfg.agent.ponytail {
        system.push_str("\n\n## Ponytail — lazy senior dev discipline\n\n");
        system.push_str(crate::agents::ponytail_rules().trim());
    }
    for s in crate::skills::load_baked(&st.skill_base) {
        system.push_str(&format!("\n\n## Skill: {} — always active\n\n", s.name));
        system.push_str(s.body.trim());
    }
    let opts = crate::server::LoopOpts {
        agent: None,
        system,
        emit_activity: false,
        allow: None,
        can_spawn: false,
    };
    crate::server::run_loop(st, session_id, &opts, abort).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let text = "Sure!\n```json\n[{\"title\":\"a\",\"instructions\":\"do a\"},{\"title\":\"b\",\"instructions\":\"do b\"}]\n```";
        let subs = parse_subtasks(text);
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].title, "a");
    }

    #[test]
    fn falls_back_to_single() {
        let subs = parse_subtasks("just some prose, no json");
        assert_eq!(subs.len(), 1);
    }

    #[tokio::test]
    async fn team_runs_with_stub_provider() {
        // End-to-end team orchestration without a real model: point the client
        // at a local mock that always answers "no subtasks, just text".
        let dir = std::env::temp_dir().join(format!("ocrs-team-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.permissions.defaults.insert("bash".into(), "allow".into());
        cfg.permissions.defaults.insert("write".into(), "allow".into());
        cfg.permissions.defaults.insert("edit".into(), "allow".into());
        cfg.permissions.approval_timeout_secs = 2;
        // Mock provider
        let mock = MockLlm::start().await;
        cfg.provider.base_url = mock.url.clone();
        cfg.provider.model = "mock".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("team", None).unwrap();
        let abort = Arc::new(AtomicBool::new(false));
        let out = run_team(&st, &s.id, "write hello.txt saying hi", abort).await.unwrap();
        assert!(out.contains("[team]"));
        // The mock always says done with empty summary → we still get a synthesis.
        assert!(out.contains("results for"));
        // M3: the reviewer verified the reports before synthesis.
        assert!(out.contains("reviewer"));
        assert!(out.contains("VERDICT: PASS"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One-shot SSE response built with serde_json + real newlines — the
    /// known-good pattern from `examples/mock_llm.rs`. (Hand-written raw
    /// strings used literal `\n` and never produced valid SSE.)
    fn sse(content: &str) -> String {
        let v =
            serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": null}]});
        format!("data: {}\n\ndata: [DONE]\n\n", v)
    }

    /// Minimal OpenAI-compatible mock: streams one text chunk, no tools.
    struct MockLlm {
        url: String,
    }

    impl MockLlm {
        async fn start() -> Self {
            use axum::{routing::post, Router};
            let app = Router::new().route(
                "/v1/chat/completions",
                post(|body: axum::extract::Json<Value>| async move {
                    let sys = body["messages"][0]["content"].as_str().unwrap_or("");
                    let content = if sys.contains("planning agent") {
                        "[{\"title\":\"sub\",\"instructions\":\"say hi\"}]"
                    } else if sys.contains("reviewer agent") {
                        "VERDICT: PASS\n- subtask reports done"
                    } else {
                        "subtask complete"
                    };
                    let payload = sse(content);
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        payload,
                    )
                }),
            );
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self { url: format!("http://{addr}/v1") }
        }
    }
}
