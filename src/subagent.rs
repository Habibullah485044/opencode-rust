//! M2: subagent spawn — the `spawn` tool runs a named subagent
//! (`explore`/`worker`) in a child session with a restricted toolset and
//! returns its summary to the parent as a tool result.

use crate::agents;
use crate::db::Part;
use crate::provider::{ToolFnSpec, ToolSpec};
use crate::server::{agent_status, run_loop, AgentInfo, LoopOpts, Shared};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;

/// Model-facing spec for the `spawn` tool (only present for top-level
/// build-mode runs; plan mode and subagents never see it).
pub fn spawn_spec() -> ToolSpec {
    let names: Vec<&str> = agents::builtin_subagents().iter().map(|d| d.name).collect();
    ToolSpec {
        kind: "function".into(),
        function: ToolFnSpec {
            name: "spawn".into(),
            description: "Run a subagent on an isolated task in a child session. \
                 `explore` is read-only (search/read/report); `worker` has the full \
                 toolset. Returns the subagent's summary. Use for work that can be \
                 done independently of the current conversation. When the user asks \
                 to spawn agents or to work on several things at once, call spawn \
                 right away — one spawn per independent piece — instead of \
                 exploring serially yourself."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "subagent": { "type": "string", "enum": names, "description": "Which subagent to run" },
                    "task": { "type": "string", "description": "The complete, self-contained task" }
                },
                "required": ["subagent", "task"]
            }),
        },
    }
}

/// Most subagents one parent session may run at the same time (B-27).
pub const MAX_RUNNING_SUBAGENTS: usize = 4;

/// Running children per parent session (B-27).
fn running() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    static RUNNING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// One running child (B-27). The slot is given back when this value is
/// dropped, also on error, abort or panic.
struct SpawnSlot(String);

impl SpawnSlot {
    fn take(parent: &str) -> Result<Self> {
        let mut map = running().lock().map_err(|_| anyhow!("subagent counter is poisoned"))?;
        let count = map.entry(parent.to_string()).or_insert(0);
        if *count >= MAX_RUNNING_SUBAGENTS {
            return Err(anyhow!(
                "too many subagents at once: at most {MAX_RUNNING_SUBAGENTS} can run for this \
                 session. Wait for their results, then spawn the rest."
            ));
        }
        *count += 1;
        Ok(SpawnSlot(parent.to_string()))
    }
}

impl Drop for SpawnSlot {
    fn drop(&mut self) {
        if let Ok(mut map) = running().lock() {
            if let Some(count) = map.get_mut(&self.0) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    map.remove(&self.0);
                }
            }
        }
    }
}

/// Execute a `spawn` call: child session + restricted loop, summary back.
pub async fn spawn_subagent(
    st: &Shared,
    parent_session_id: &str,
    args: &Value,
    abort: &AtomicBool,
) -> Result<String> {
    let name = args
        .get("subagent")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing `subagent` (one of: explore, worker)"))?;
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow!("missing `task` — give the subagent a complete task"))?;
    let def = agents::find_subagent(name)
        .ok_or_else(|| anyhow!("unknown subagent `{name}` (available: explore, worker)"))?;
    // B-27: held until this function returns.
    let _slot = SpawnSlot::take(parent_session_id)?;

    let session = st.db.create_session(&format!("sub:{name}"), Some(parent_session_id))?;
    st.db
        .add_message(&session.id, "user", &[Part::Text { text: task.to_string() }])?;

    // Restricted system prompt: def.system as base + project instructions.
    let agents_md = crate::skills::load_agents_md(&st.workspace);
    let system = agents::compose_system(agents::PromptInputs {
        workspace: &st.workspace,
        mode: crate::agents::AgentMode::Build,
        configured: Some(def.system),
        agents_md: agents_md.as_deref(),
        skills: &[],
        subagents: &[],
        ponytail: st.cfg.agent.ponytail,
        baked: &crate::skills::load_baked(&st.skill_base),
    });
    // B-27: the shared prompt says "ask when unclear"; a child has nobody to ask.
    const CHILD_RULES: &str = "\n\n## Running as a subagent\n\n\
        Nobody can answer questions during this run. Do not ask clarifying \
        questions: make the most reasonable assumption, say which one in your \
        summary, and finish the task.";
    let system = format!("{system}{CHILD_RULES}");
    // M7: the agents panel watches this run through `AgentStatus` events.
    let agent = AgentInfo {
        id: session.id.clone(),
        name: name.to_string(),
        task: task.to_string(),
    };
    agent_status(st, &agent, "working", "starting…".into());

    let opts = LoopOpts {
        system,
        emit_activity: false, // activity goes to the agents panel instead
        allow: def.tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
        can_spawn: false, // no recursive spawning
        agent: Some(agent.clone()),
    };
    let out = match run_loop(st, &session.id, &opts, abort).await {
        Ok(out) => out,
        Err(e) => {
            agent_status(st, &agent, "failed", format!("{e:#}"));
            return Err(e);
        }
    };
    // B-27: run_loop returns "aborted" when stopped; that is not a result.
    if abort.load(std::sync::atomic::Ordering::SeqCst) || out.trim() == "aborted" {
        agent_status(st, &agent, "failed", "aborted".into());
        return Err(anyhow!("subagent `{name}` was aborted before it finished"));
    }
    let summary = if out.trim().is_empty() { "(no summary)".into() } else { out };
    let mut summary = summary;
    if summary.chars().count() > 4000 {
        summary = summary.chars().take(4000).collect::<String>() + "…";
    }
    agent_status(
        st,
        &agent,
        "done",
        summary.lines().next().unwrap_or("").to_string(),
    );
    Ok(format!("[subagent `{name}` → session {}]\n{}", session.id, summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_allowed;

    /// Mock OpenAI-compatible provider: first turn (last msg not a tool
    /// result) emits a `spawn` tool call; afterwards a plain text reply.
    /// The reply echoes whether `bash` was present in the request's tools —
    /// so callers can assert spec filtering end-to-end.
    async fn mock_spawn_llm() -> String {
        use axum::{routing::post, Router};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<Value>| async move {
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("");
                let has_bash = body["tools"]
                    .as_array()
                    .map(|ts| ts.iter().any(|t| t["function"]["name"] == "bash"))
                    .unwrap_or(false);
                let payload = if last_role != "tool" {
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_sp","function":{"name":"spawn","arguments":"{\"subagent\":\"explore\",\"task\":\"inspect the tree\"}"}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]

"#
                    .to_string()
                } else {
                    let content = if has_bash { "done bash=yes" } else { "done bash=no" };
                    format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
                    )
                };
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
        format!("http://{addr}/v1")
    }

    #[test]
    fn spawn_spec_shape() {
        let s = spawn_spec();
        assert_eq!(s.function.name, "spawn");
        assert_eq!(s.function.parameters["required"], json!(["subagent", "task"]));
        let names = s.function.parameters["properties"]["subagent"]["enum"].as_array().unwrap();
        let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["explore", "worker"]);
    }

    #[tokio::test]
    async fn spawn_rejects_bad_args_and_unknown_subagent() {
        let dir = std::env::temp_dir().join(format!("ocrs-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = "http://127.0.0.1:9".into();
        cfg.provider.model = "stub".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("p", None).unwrap();
        let abort = AtomicBool::new(false);

        let err = spawn_subagent(&st, &s.id, &json!({}), &abort).await.unwrap_err();
        assert!(err.to_string().contains("missing `subagent`"));
        let err = spawn_subagent(
            &st,
            &s.id,
            &json!({"subagent": "explore", "task": ""}),
            &abort,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("missing `task`"));
        let err = spawn_subagent(
            &st,
            &s.id,
            &json!({"subagent": "nope", "task": "t"}),
            &abort,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown subagent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end: parent run spawns `explore`; child runs the shared loop
    /// with a restricted toolset; parent gets the summary as a tool result.
    #[tokio::test]
    async fn parent_spawn_runs_child_session_and_returns_summary() {
        let dir = std::env::temp_dir().join(format!(
            "ocrs-spawn-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.permissions.defaults.insert("bash".into(), "allow".into());
        cfg.permissions.defaults.insert("spawn".into(), "allow".into());
        cfg.permissions.approval_timeout_secs = 2;
        cfg.provider.base_url = mock_spawn_llm().await;
        cfg.provider.model = "mock".into();

        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("main", None).unwrap();
        let abort = AtomicBool::new(false);
        crate::server::run_agent(&st, &s.id, "please explore", abort.into()).await;

        let msgs = st.db.messages(&s.id).unwrap();
        // user → assistant spawn call → tool result → assistant final text.
        assert!(msgs.iter().any(|m| m.role == "assistant"
            && m.parts.iter().any(|p| matches!(p, Part::ToolCall { name, .. } if name == "spawn"))));
        let tool_msg = msgs.iter().find(|m| m.role == "tool").expect("spawn result");
        let text = match &tool_msg.parts[0] {
            Part::ToolResult { output, is_error, .. } => {
                assert!(!is_error, "spawn succeeded: {output}");
                output.clone()
            }
            other => panic!("wrong part {other:?}"),
        };
        assert!(text.contains("[subagent `explore`"), "summary wrapper: {text}");

        // Child session exists, parented, with the task as its user message.
        let all = st.db.list_sessions().unwrap();
        let child = all.iter().find(|x| x.parent_id.as_deref() == Some(s.id.as_str()))
            .expect("child session created");
        assert!(child.title.starts_with("sub:"));
        let child_msgs = st.db.messages(&child.id).unwrap();
        assert!(child_msgs.iter().any(|m| m.role == "user"
            && m.parts.iter().any(|p| matches!(p, Part::Text { text } if text == "inspect the tree"))));

        // Spec filtering proven end-to-end: the PARENT ran with base tools
        // (bash present → its final text echoes that)…
        let parent_final = msgs
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .and_then(|m| m.parts.first())
            .and_then(|p| match p {
                Part::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(parent_final, "done bash=yes");
        // …while the explore CHILD ran read-only (bash absent in its specs).
        let child_final = st
            .db
            .messages(&child.id)
            .unwrap()
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .and_then(|m| m.parts.first())
            .and_then(|p| match p {
                Part::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(child_final, "done bash=no");
        assert!(text.contains("done bash=no"), "summary carries child output");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plan mode: no `spawn` tool in specs, and direct calls are blocked.
    #[tokio::test]
    async fn plan_mode_has_no_spawn_tool() {
        let dir = std::env::temp_dir().join(format!("ocrs-spawn-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = "http://127.0.0.1:9".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("plan", None).unwrap();
        st.set_mode(&s.id, crate::agents::AgentMode::Plan);

        // Build the same spec list run_loop builds, via the public pieces.
        let mut specs = crate::tools::tool_specs();
        specs.extend(st.mcp.lock().await.specs());
        specs.push(spawn_spec());
        let kept: Vec<String> = crate::agents::AgentMode::Plan
            .filter_specs(specs)
            .into_iter()
            .map(|x| x.function.name)
            .collect();
        assert!(!kept.contains(&"spawn".to_string()));
        assert!(!kept.contains(&"bash".to_string()));
        assert!(kept.contains(&"read".to_string()));

        // And the mode gate blocks a direct call, even with can_spawn.
        let opts = LoopOpts {
            system: "s".into(),
            emit_activity: false,
            allow: None,
            can_spawn: true,
            agent: None,
        };
        assert!(!tool_allowed(crate::agents::AgentMode::Plan, &opts, "spawn"));
        assert!(!tool_allowed(crate::agents::AgentMode::Plan, &opts, "bash"));
        assert!(tool_allowed(crate::agents::AgentMode::Plan, &opts, "read"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M7: two spawn calls in one turn run CONCURRENTLY — both "starting…"
    /// statuses land before either finishes — tool results are recorded in
    /// call order, and the agents panel gets live status for both agents.
    #[tokio::test]
    async fn parallel_spawns_run_together_and_report_agent_status() {
        use axum::{routing::post, Router};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<Value>| async move {
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("");
                let payload = if last_role != "tool" {
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_sp1","function":{"name":"spawn","arguments":"{\"subagent\":\"explore\",\"task\":\"task one\"}"}},{"index":1,"id":"call_sp2","function":{"name":"spawn","arguments":"{\"subagent\":\"worker\",\"task\":\"task two\"}"}}]},"finish_reason":null}]}
data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}
data: [DONE]

"#
                        .to_string()
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n"
                        .to_string()
                };
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

        let dir = std::env::temp_dir().join(format!(
            "ocrs-spawn-par-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.permissions.defaults.insert("bash".into(), "allow".into());
        cfg.permissions.defaults.insert("spawn".into(), "allow".into());
        cfg.permissions.approval_timeout_secs = 2;
        cfg.provider.base_url = format!("http://{addr}/v1");
        cfg.provider.model = "mock".into();

        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("main", None).unwrap();
        let mut rx = st.events.subscribe();
        let abort = AtomicBool::new(false);
        crate::server::run_agent(&st, &s.id, "split the work", abort.into()).await;

        // Both children ran, parented to the main session.
        let all = st.db.list_sessions().unwrap();
        let kids: Vec<_> = all
            .iter()
            .filter(|x| x.parent_id.as_deref() == Some(s.id.as_str()))
            .collect();
        assert_eq!(kids.len(), 2, "one child session per spawn");

        // Tool results recorded in call order, with each subagent's summary.
        let msgs = st.db.messages(&s.id).unwrap();
        let results: Vec<String> = msgs
            .iter()
            .filter_map(|m| match m.parts.first() {
                Some(Part::ToolResult { tool_id, output, .. }) => {
                    Some(format!("{tool_id}:{output}"))
                }
                _ => None,
            })
            .collect();
        assert!(results.len() >= 2, "two spawn results, got {results:?}");
        assert!(results[0].starts_with("call_sp1:"), "call order: {}", results[0]);
        assert!(results[1].starts_with("call_sp2:"), "call order: {}", results[1]);
        assert!(results[0].contains("[subagent `explore`"), "{}", results[0]);
        assert!(results[1].contains("[subagent `worker`"), "{}", results[1]);

        // Agents panel statuses: both agents started before the first one
        // finished (they really ran in parallel), and each reached `done`.
        let mut evs = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let crate::server::ServerEvent::AgentStatus { agent_id, state, .. } = ev {
                evs.push((agent_id, state));
            }
        }
        let first_done = evs
            .iter()
            .position(|(_, state)| state == "done")
            .expect("a done event");
        let started: std::collections::HashSet<&str> =
            evs[..first_done].iter().map(|(id, _)| id.as_str()).collect();
        assert!(started.len() >= 2, "both agents started before the first finish: {evs:?}");
        let done: std::collections::HashSet<&String> = evs
            .iter()
            .filter(|(_, state)| state == "done")
            .map(|(id, _)| id)
            .collect();
        assert_eq!(done.len(), 2, "both agents reached done: {evs:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod b27_tests {
    use super::*;

    #[test]
    fn running_subagents_are_capped_per_parent() {
        let parent = format!("ses_b27_cap_{}", std::process::id());
        let slots: Vec<SpawnSlot> =
            (0..MAX_RUNNING_SUBAGENTS).map(|_| SpawnSlot::take(&parent).unwrap()).collect();
        let err = SpawnSlot::take(&parent).err().expect("cap reached").to_string();
        assert!(err.contains("at most"), "{err}");
        assert!(SpawnSlot::take("ses_b27_other").is_ok(), "other sessions are not affected");
        drop(slots);
        assert!(SpawnSlot::take(&parent).is_ok(), "slots come back when children finish");
    }

    #[tokio::test]
    async fn aborted_subagent_is_not_reported_done() {
        let dir = std::env::temp_dir().join(format!("ocrs-b27-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = "http://127.0.0.1:9".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let parent = st.db.create_session("p", None).unwrap();
        let mut rx = st.events.subscribe();
        let abort = AtomicBool::new(true);
        let args = json!({"subagent": "explore", "task": "look around"});
        let err = spawn_subagent(&st, &parent.id, &args, &abort).await.unwrap_err();
        assert!(err.to_string().contains("aborted"), "{err}");
        let mut states = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let crate::server::ServerEvent::AgentStatus { state, .. } = ev {
                states.push(state);
            }
        }
        assert!(!states.iter().any(|s| s == "done"), "{states:?}");
        assert_eq!(states.last().map(String::as_str), Some("failed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

}
