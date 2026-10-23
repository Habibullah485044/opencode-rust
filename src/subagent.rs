//! M2/A3: subagents. The `spawn` tool starts a role (built-in, or from a
//! role file — see `agents::load_subagents`) in a child session with its own
//! tools, model and step limit. A plain spawn waits and returns the child's
//! report; `background: true` returns at once, and the parent manages the
//! child with `agent_wait`, `agent_send`, `agent_stop` and `agent_list`.
//! At most `agent.max_subagents` children of one chat run at the same time;
//! the rest wait in line.

use crate::agents::{self, SubagentDef};
use crate::db::Part;
use crate::provider::{ToolFnSpec, ToolSpec};
use crate::server::{agent_status, run_loop, AgentInfo, LoopOpts, Shared};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

fn spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        kind: "function".into(),
        function: ToolFnSpec { name: name.into(), description: description.into(), parameters },
    }
}

/// Model-facing spec for the `spawn` tool (only present for top-level
/// build-mode runs; plan mode and subagents never see it).
pub fn spawn_spec(roles: &[SubagentDef]) -> ToolSpec {
    let names: Vec<&str> = roles.iter().map(|d| d.name.as_str()).collect();
    spec(
        "spawn",
        &format!(
            "Run a subagent on one self-contained task in its own child session. Roles:\n{}\n\n\
             Split work that has independent parts and spawn one agent per part in the same \
             turn — they run in parallel. Give each a complete task: the goal, the files or \
             area, what done looks like, and what to report back; the agent cannot see this \
             conversation. Give workers separate files. With `background: true` the call \
             returns at once; collect the reports later with agent_wait. When the user asks to \
             spawn agents or to work on several things at once, call spawn right away instead \
             of exploring serially yourself.",
            agents::subagent_index(roles)
        ),
        json!({
            "type": "object",
            "properties": {
                "subagent": { "type": "string", "enum": names, "description": "Which role to run" },
                "task": { "type": "string", "description": "The complete, self-contained task" },
                "background": { "type": "boolean", "description": "Return at once and keep working; collect the report with agent_wait (default false)" }
            },
            "required": ["subagent", "task"]
        }),
    )
}

/// Tools for managing spawned agents (offered together with `spawn`).
pub const AGENT_TOOLS: &[&str] = &["agent_wait", "agent_send", "agent_stop", "agent_list"];

pub fn is_agent_tool(name: &str) -> bool {
    AGENT_TOOLS.contains(&name)
}

pub fn agent_tool_specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "agent_wait",
            "Wait for spawned agents to finish and get their reports. Without `ids`, waits for \
             every agent of this chat whose report you have not read yet.",
            json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" }, "description": "Agent ids (session ids) to wait for" },
                    "timeout_secs": { "type": "integer", "description": "Stop waiting after this many seconds and say who is still running (default 600)" }
                }
            }),
        ),
        spec(
            "agent_send",
            "Send a message to a spawned agent: more instructions, a correction, or a follow-up \
             task. A running agent reads it before its next step; a finished one starts again \
             with it.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The agent id" },
                    "message": { "type": "string", "description": "What to tell the agent" }
                },
                "required": ["id", "message"]
            }),
        ),
        spec(
            "agent_stop",
            "Stop a spawned agent that is no longer needed.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "The agent id" } },
                "required": ["id"]
            }),
        ),
        spec(
            "agent_list",
            "List this chat's spawned agents with their role, state and running time.",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

/// One spawned agent, from start until the app closes.
#[derive(Clone)]
struct Run {
    parent: String,
    role: SubagentDef,
    task: String,
    /// queued | working | done | failed | stopped
    state: &'static str,
    started: Instant,
    ended: Option<Instant>,
    report: Option<String>,
    abort: Arc<AtomicBool>,
    /// Messages from the parent, read before the child's next step.
    inbox: Vec<String>,
    background: bool,
    /// The parent has read the latest report.
    delivered: bool,
}

impl Run {
    fn running(&self) -> bool {
        matches!(self.state, "queued" | "working")
    }

    fn elapsed(&self) -> Duration {
        self.ended.unwrap_or_else(Instant::now).duration_since(self.started)
    }
}

/// Every spawned agent, by child session id.
fn runs() -> &'static Mutex<HashMap<String, Run>> {
    static RUNS: OnceLock<Mutex<HashMap<String, Run>>> = OnceLock::new();
    RUNS.get_or_init(Default::default)
}

fn with_runs<T>(f: impl FnOnce(&mut HashMap<String, Run>) -> T) -> T {
    let mut map = runs().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut map)
}

/// The running places of each parent chat: `agent.max_subagents` permits,
/// so extra spawns wait in line instead of failing.
fn slots(parent: &str, max: usize) -> Arc<tokio::sync::Semaphore> {
    static SLOTS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>> = OnceLock::new();
    let mut map = SLOTS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    map.entry(parent.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(max.max(1))))
        .clone()
}

fn info(id: &str, run: &Run) -> AgentInfo {
    AgentInfo {
        id: id.to_string(),
        parent: run.parent.clone(),
        name: run.role.name.clone(),
        task: run.task.clone(),
    }
}

/// Execute a `spawn` call. A plain spawn waits for the child and returns its
/// report; `background: true` returns the child's id at once.
pub async fn spawn_subagent(
    st: &Shared,
    parent_session_id: &str,
    args: &Value,
    abort: &AtomicBool,
) -> Result<String> {
    let roles = agents::load_subagents(&st.workspace_for(parent_session_id));
    let names = || roles.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ");
    let name = args
        .get("subagent")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing `subagent` (one of: {})", names()))?;
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow!("missing `task` — give the subagent a complete task"))?;
    let role = roles
        .iter()
        .find(|r| r.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("unknown subagent `{name}` (available: {})", names()))?;
    let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
    let id = start(st, parent_session_id, role, task, background)?;
    if background {
        return Ok(format!(
            "[subagent `{name}` → session {id} · started in the background]\n\
             It works on its own now. Collect its report with agent_wait (ids: [\"{id}\"]); \
             send it more instructions with agent_send."
        ));
    }
    wait_for(&[id.clone()], None, abort).await;
    if abort.load(Ordering::SeqCst) {
        stop(&id);
        // Give the child a moment to end, so its last status is out.
        wait_for(&[id.clone()], Some(Duration::from_secs(5)), &AtomicBool::new(false)).await;
        return Err(anyhow!("subagent `{name}` was aborted before it finished"));
    }
    let run = with_runs(|m| {
        let run = m.get_mut(&id)?;
        run.delivered = true;
        Some(run.clone())
    })
    .ok_or_else(|| anyhow!("subagent `{name}` disappeared"))?;
    match run.state {
        "done" => Ok(run.report.unwrap_or_default()),
        _ => Err(anyhow!("{}", run.report.unwrap_or_else(|| format!("subagent `{name}` {}", run.state)))),
    }
}

/// Create the child session, register the run and start it. Returns the
/// child's id (its session id).
fn start(st: &Shared, parent: &str, role: SubagentDef, task: &str, background: bool) -> Result<String> {
    let session = st.db.create_session(&format!("sub:{}", role.name), Some(parent))?;
    st.db
        .add_message(&session.id, "user", &[Part::Text { text: task.to_string() }])?;
    let run = Run {
        parent: parent.to_string(),
        role,
        task: task.to_string(),
        state: "queued",
        started: Instant::now(),
        ended: None,
        report: None,
        abort: Arc::new(AtomicBool::new(false)),
        inbox: Vec::new(),
        background,
        delivered: false,
    };
    agent_status(st, &info(&session.id, &run), "queued", "waiting for a free place…".into());
    with_runs(|m| m.insert(session.id.clone(), run));
    launch(st.clone(), session.id.clone());
    Ok(session.id)
}

/// Run the child as its own task. Deliberately NOT async (like
/// `spawn_claimed_run`): it keeps run_loop → spawn → run_loop from being one
/// recursive future type.
fn launch(st: Shared, id: String) {
    tokio::spawn(async move {
        let (state, report) = match run_child(&st, &id).await {
            Ok((state, report)) => (state, report),
            Err(e) => ("failed", format!("error: {e:#}")),
        };
        finish(&st, &id, state, report);
    });
}

/// Record how a run ended and tell the agents panel (and, for a background
/// agent, the parent chat).
fn finish(st: &Shared, id: &str, state: &'static str, report: String) {
    let Some(run) = with_runs(|m| {
        let run = m.get_mut(id)?;
        run.state = state;
        run.ended = Some(Instant::now());
        run.report = Some(report.clone());
        Some(run.clone())
    }) else {
        return;
    };
    let first = report.lines().nth(1).or(report.lines().next()).unwrap_or("");
    agent_status(st, &info(id, &run), if state == "stopped" { "failed" } else { state }, first.to_string());
    if run.background {
        let _ = st.events.send(crate::server::ServerEvent::Note {
            session_id: run.parent.clone(),
            text: format!("background agent `{}` ({id}) {state}", run.role.name),
        });
    }
}

/// Wait for a place, run the role's loop (again while messages arrive), and
/// build the report. Returns the end state and the report.
async fn run_child(st: &Shared, id: &str) -> Result<(&'static str, String)> {
    let run = with_runs(|m| m.get(id).cloned()).ok_or_else(|| anyhow!("unknown agent {id}"))?;
    let abort = run.abort.clone();
    let role = run.role.clone();
    let agent = info(id, &run);
    let sem = slots(&run.parent, st.cfg.agent.max_subagents);
    let _permit = tokio::select! {
        p = sem.acquire_owned() => p.ok(),
        _ = wait_flag(&abort) => None,
    };
    if abort.load(Ordering::SeqCst) {
        return Ok(("stopped", report(st, id, &run, "stopped", "stopped before it started")));
    }
    with_runs(|m| m.get_mut(id).map(|r| r.state = "working"));
    agent_status(st, &agent, "working", "starting…".into());

    // The child works in its parent chat's project folder, with the project
    // instructions and the role's prompt.
    let project_ws = st.workspace_for(id);
    let agents_md = crate::skills::load_agents_md(&project_ws);
    let system = agents::compose_system(agents::PromptInputs {
        workspace: &project_ws,
        mode: crate::agents::AgentMode::Build,
        configured: Some(&role.system),
        agents_md: agents_md.as_deref(),
        skills: &[],
        subagents: &[],
        ponytail: st.ponytail_level(&run.parent), // P-05: parent's level
        baked: &crate::skills::load_baked(&st.skill_base),
    });
    // B-27: the shared prompt says "ask when unclear"; a child has nobody to ask.
    const CHILD_RULES: &str = "\n\n## Running as a subagent\n\n\
        Nobody can answer questions during this run. Do not ask clarifying \
        questions: make the most reasonable assumption, say which one in your \
        summary, and finish the task. End with a short report: what you found \
        or changed (with file paths), how you checked it, and anything left open.";
    let mut system = format!("{system}{CHILD_RULES}");

    // F-05: an isolated role runs in its own git worktree and keeps to the
    // built-in tools.
    let isolate = role.isolate.unwrap_or(st.cfg.agent.isolate_workers && role.name == "worker");
    let iso = isolate.then(|| crate::worktree::Isolation::start(&project_ws, &st.state_dir, &role.name));
    let iso_ws = iso.as_ref().and_then(|i| i.workspace());
    let mut allow = role.tools.clone();
    if let Some(ws) = &iso_ws {
        system.push_str(&crate::worktree::isolation_prompt(ws));
        allow = Some(crate::worktree::local_tool_names());
    }
    // A read-only role may use the cheaper small model.
    let model = role.model.clone().or_else(|| {
        role.read_only.then(|| st.cfg.provider.small_model.clone()).flatten()
    });
    let opts = LoopOpts {
        system,
        emit_activity: false, // activity goes to the agents panel instead
        allow,
        can_spawn: false, // no recursive spawning
        agent: Some(agent.clone()),
        workspace: iso_ws.or_else(|| st.run_workspace(id)),
        model,
        max_steps: role.max_steps,
    };
    let mut out = run_loop(st, id, &opts, &abort).await;
    // A message that came in after the last step starts another round.
    while out.is_ok() && !abort.load(Ordering::SeqCst) {
        let Some(text) = take_inbox(id) else { break };
        st.db.add_message(id, "user", &[Part::Text { text }])?;
        out = run_loop(st, id, &opts, &abort).await;
    }
    let aborted = abort.load(Ordering::SeqCst);
    let (state, mut summary) = match out {
        // B-27: run_loop returns "aborted" when stopped; that is not a result.
        _ if aborted => ("stopped", "stopped before it finished".to_string()),
        Ok(text) if text.trim() == "aborted" => ("stopped", "stopped before it finished".to_string()),
        Ok(text) if text.trim().is_empty() => ("done", "(no summary)".to_string()),
        Ok(text) => ("done", text),
        Err(e) => ("failed", format!("error: {e:#}")),
    };
    if summary.chars().count() > 4000 {
        summary = summary.chars().take(4000).collect::<String>() + "…";
    }
    // F-05: report what the worktree produced, or why there was none. The
    // user sees it as a note too. A stopped worker's changes are dropped.
    if let Some(iso) = iso.filter(|_| !aborted) {
        let made = iso.finish(&st.state_dir);
        let _ = st.events.send(crate::server::ServerEvent::Note {
            session_id: run.parent.clone(),
            text: format!("{} {id}: {made}", role.name),
        });
        summary.push_str("\n\n");
        summary.push_str(&made);
    }
    Ok((state, report(st, id, &run, state, &summary)))
}

/// Resolves once `flag` is set.
async fn wait_flag(flag: &AtomicBool) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The report the parent reads: one header line with the facts, the files
/// the child wrote, then its own summary.
fn report(st: &Shared, id: &str, run: &Run, state: &str, summary: &str) -> String {
    let mut calls = 0;
    let mut files: Vec<String> = Vec::new();
    for m in st.db.messages(id).unwrap_or_default() {
        for p in m.parts {
            if let Part::ToolCall { name, input, .. } = p {
                calls += 1;
                let path = input.get("path").and_then(Value::as_str);
                if let (true, Some(path)) = (matches!(name.as_str(), "write" | "edit"), path) {
                    if !files.iter().any(|f| f == path) {
                        files.push(path.to_string());
                    }
                }
            }
        }
    }
    let mut head = format!(
        "[subagent `{}` → session {id} · {state} · {:.1}s · {calls} tool call{}",
        run.role.name,
        run.elapsed().as_secs_f64(),
        if calls == 1 { "" } else { "s" },
    );
    if let Ok(u) = st.db.usage_totals(Some(id), None) {
        if u.input + u.output > 0 {
            head.push_str(&format!(" · {} tokens", u.input + u.output));
        }
    }
    head.push(']');
    if !files.is_empty() {
        head.push_str(&format!("\nFiles written: {}", files.join(", ")));
    }
    format!("{head}\n{summary}")
}

/// Wait until none of `ids` is queued or working, `timeout` passes, or
/// `abort` is set.
async fn wait_for(ids: &[String], timeout: Option<Duration>, abort: &AtomicBool) {
    let until = timeout.map(|t| Instant::now() + t);
    loop {
        let busy = with_runs(|m| ids.iter().any(|id| m.get(id).is_some_and(Run::running)));
        if !busy || abort.load(Ordering::SeqCst) || until.is_some_and(|u| Instant::now() >= u) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Ask one agent to stop. False when it is not running.
pub fn stop(id: &str) -> bool {
    with_runs(|m| match m.get(id) {
        Some(run) if run.running() => {
            run.abort.store(true, Ordering::SeqCst);
            true
        }
        _ => false,
    })
}

/// Stop every running agent of a parent chat (the user stopped its run).
pub fn stop_children(parent: &str) {
    with_runs(|m| {
        for run in m.values().filter(|r| r.parent == parent && r.running()) {
            run.abort.store(true, Ordering::SeqCst);
        }
    });
}

fn take_inbox(id: &str) -> Option<String> {
    with_runs(|m| {
        let run = m.get_mut(id)?;
        if run.inbox.is_empty() {
            return None;
        }
        let msgs = std::mem::take(&mut run.inbox);
        Some(format!("Message from the parent agent:\n\n{}", msgs.join("\n\n")))
    })
}

/// Mail for a run before its next step: a child's messages from its parent,
/// or a parent's finished background reports it has not read yet.
pub fn take_mail(session_id: &str) -> Option<String> {
    if let Some(text) = take_inbox(session_id) {
        return Some(text);
    }
    let reports: Vec<String> = with_runs(|m| {
        m.values_mut()
            .filter(|r| r.parent == session_id && r.background && !r.delivered && !r.running())
            .map(|r| {
                r.delivered = true;
                r.report.clone().unwrap_or_default()
            })
            .collect()
    });
    (!reports.is_empty())
        .then(|| format!("Background agents finished:\n\n{}", reports.join("\n\n---\n\n")))
}

/// Execute `agent_wait`, `agent_send`, `agent_stop` or `agent_list` for the
/// agents of `parent`.
pub async fn agent_tool(st: &Shared, parent: &str, name: &str, input: &Value, abort: &AtomicBool) -> (String, bool) {
    match agent_tool_inner(st, parent, name, input, abort).await {
        Ok(out) => (out, true),
        Err(e) => (format!("error: {e:#}"), false),
    }
}

async fn agent_tool_inner(st: &Shared, parent: &str, name: &str, input: &Value, abort: &AtomicBool) -> Result<String> {
    let mine = |id: &str| with_runs(|m| m.get(id).filter(|r| r.parent == parent).cloned());
    let id_arg = || {
        let id = input.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("missing `id`"))?;
        mine(id).map(|run| (id.to_string(), run)).ok_or_else(|| {
            anyhow!("no agent `{id}` in this chat — agent_list shows the ids")
        })
    };
    match name {
        "agent_list" => {
            let mut rows: Vec<(Instant, String)> = with_runs(|m| {
                m.iter()
                    .filter(|(_, r)| r.parent == parent)
                    .map(|(id, r)| {
                        let task: String = r.task.chars().take(80).collect();
                        (r.started, format!("{id} · {} · {} · {:.0}s · {task}", r.role.name, r.state, r.elapsed().as_secs_f64()))
                    })
                    .collect()
            });
            rows.sort_by_key(|(t, _)| *t);
            Ok(if rows.is_empty() {
                "no agents spawned in this chat".into()
            } else {
                rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>().join("\n")
            })
        }
        "agent_stop" => {
            let (id, _) = id_arg()?;
            Ok(if stop(&id) { format!("stopping {id}") } else { format!("{id} is not running") })
        }
        "agent_send" => {
            let (id, run) = id_arg()?;
            let message = input
                .get("message")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| anyhow!("missing `message`"))?;
            if run.running() {
                with_runs(|m| m.get_mut(&id).map(|r| r.inbox.push(message.to_string())));
                return Ok(format!("sent; {id} reads it before its next step"));
            }
            st.db.add_message(&id, "user", &[Part::Text { text: message.to_string() }])?;
            with_runs(|m| {
                m.get_mut(&id).map(|r| {
                    r.state = "queued";
                    r.ended = None;
                    r.delivered = false;
                    r.abort = Arc::new(AtomicBool::new(false));
                })
            });
            agent_status(st, &info(&id, &run), "queued", "new message…".into());
            launch(st.clone(), id.clone());
            Ok(format!("sent; {id} started again — collect its report with agent_wait"))
        }
        "agent_wait" => {
            let ids: Vec<String> = match input.get("ids").and_then(Value::as_array) {
                Some(list) => {
                    let ids: Vec<String> = list.iter().filter_map(Value::as_str).map(String::from).collect();
                    if let Some(bad) = ids.iter().find(|id| mine(id).is_none()) {
                        return Err(anyhow!("no agent `{bad}` in this chat — agent_list shows the ids"));
                    }
                    ids
                }
                None => with_runs(|m| {
                    m.iter()
                        .filter(|(_, r)| r.parent == parent && (r.running() || !r.delivered))
                        .map(|(id, _)| id.clone())
                        .collect()
                }),
            };
            if ids.is_empty() {
                return Ok("no agents to wait for: every report has been read".into());
            }
            let secs = input.get("timeout_secs").and_then(Value::as_u64).unwrap_or(600).clamp(1, 3600);
            wait_for(&ids, Some(Duration::from_secs(secs)), abort).await;
            let mut done = Vec::new();
            let mut busy = Vec::new();
            with_runs(|m| {
                for id in &ids {
                    let Some(r) = m.get_mut(id) else { continue };
                    if r.running() {
                        busy.push(format!("{id} ({}, {}, {:.0}s)", r.role.name, r.state, r.elapsed().as_secs_f64()));
                    } else {
                        r.delivered = true;
                        done.push(r.report.clone().unwrap_or_default());
                    }
                }
            });
            let mut out = done.join("\n\n---\n\n");
            if !busy.is_empty() {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&format!("Still running: {}", busy.join(", ")));
            }
            Ok(out)
        }
        other => Err(anyhow!("unknown agent tool `{other}`")),
    }
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
        let s = spawn_spec(&agents::builtin_subagents());
        assert_eq!(s.function.name, "spawn");
        assert_eq!(s.function.parameters["required"], json!(["subagent", "task"]));
        let names = s.function.parameters["properties"]["subagent"]["enum"].as_array().unwrap();
        let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["explore", "research", "plan", "worker", "review"]);
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
        specs.push(spawn_spec(&agents::builtin_subagents()));
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
            model: None,
            max_steps: None,
            system: "s".into(),
            emit_activity: false,
            allow: None,
            can_spawn: true,
            workspace: None,
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

    /// F-05: with `agent.isolate_workers`, a `worker` edits its own git
    /// worktree. The real workspace stays untouched, the summary carries a
    /// patch that applies, and no worktree is left behind.
    #[tokio::test]
    async fn isolated_worker_returns_a_patch_and_leaves_the_workspace_alone() {
        use axum::{routing::post, Router};
        // The worker's model: a `write` call first, then (after the result) text.
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<Value>| async move {
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("");
                let payload = if last_role == "tool" {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"wrote it\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                        .to_string()
                } else {
                    let args = json!({"path": "from_worker.txt", "content": "hi\n"}).to_string();
                    let frame = json!({"choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": "call_w",
                        "function": {"name": "write", "arguments": args}
                    }]}, "finish_reason": null}]});
                    format!(
                        "data: {frame}\n\n\
                         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                         data: [DONE]\n\n"
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
        let dir = std::env::temp_dir().join(format!("ocrs-iso-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::gitsnap::git(&dir, args).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.autocrlf", "false"]); // exact bytes on Windows too
        std::fs::write(dir.join("readme.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);

        let mut cfg = crate::config::Config::default();
        cfg.agent.isolate_workers = true;
        cfg.permissions.defaults.insert("write".into(), "allow".into());
        cfg.permissions.approval_timeout_secs = 2;
        cfg.provider.base_url = format!("http://{addr}/v1");
        cfg.provider.model = "mock".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("main", None).unwrap();
        let args = json!({"subagent": "worker", "task": "write the file"});
        let out = spawn_subagent(&st, &s.id, &args, &AtomicBool::new(false)).await.unwrap();

        assert!(!dir.join("from_worker.txt").exists(), "the real workspace is untouched");
        assert!(out.contains("[isolated worktree]") && out.contains("from_worker.txt"), "{out}");
        let patch = out
            .lines()
            .find_map(|l| l.strip_prefix("Patch: "))
            .expect("patch path in the summary")
            .to_string();
        git(&["apply", &patch]);
        assert_eq!(std::fs::read_to_string(dir.join("from_worker.txt")).unwrap(), "hi\n");
        let list = git(&["worktree", "list", "--porcelain"]);
        let count = list.lines().filter(|l| l.starts_with("worktree ")).count();
        assert_eq!(count, 1, "only the main worktree is left: {list}");
        let _ = std::fs::remove_file(&patch);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod b27_tests {
    use super::*;

    /// Mock provider: waits `delay_ms`, then answers "got: <last user text>".
    async fn echo_llm(delay_ms: u64) -> String {
        use axum::{routing::post, Router};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::extract::Json<Value>| async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .lines()
                    .last()
                    .unwrap_or("")
                    .to_string();
                let frame = json!({"choices": [{"delta": {"content": format!("got: {last}")}, "finish_reason": "stop"}]});
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    format!("data: {frame}\n\ndata: [DONE]\n\n"),
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

    async fn state_with(delay_ms: u64, max: usize, tag: &str) -> (Shared, String, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ocrs-a3-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = echo_llm(delay_ms).await;
        cfg.provider.model = "mock".into();
        cfg.agent.max_subagents = max;
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let parent = st.db.create_session("p", None).unwrap().id;
        (st, parent, dir)
    }

    fn states(parent: &str) -> Vec<&'static str> {
        let mut v: Vec<_> = with_runs(|m| m.values().filter(|r| r.parent == parent).map(|r| r.state).collect());
        v.sort();
        v
    }

    async fn bg(st: &Shared, parent: &str, task: &str) -> String {
        let args = json!({"subagent": "explore", "task": task, "background": true});
        let out = spawn_subagent(st, parent, &args, &AtomicBool::new(false)).await.unwrap();
        assert!(out.contains("started in the background"), "{out}");
        out.split("session ").nth(1).unwrap().split(' ').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn extra_spawns_wait_in_line_instead_of_failing() {
        let (st, parent, dir) = state_with(400, 1, "queue").await;
        let a = bg(&st, &parent, "first").await;
        let b = bg(&st, &parent, "second").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(states(&parent), ["queued", "working"], "one place: the second waits");
        let no = AtomicBool::new(false);
        let (out, ok) = agent_tool(&st, &parent, "agent_wait", &json!({}), &no).await;
        assert!(ok, "{out}");
        assert!(out.contains(&format!("session {a} · done")) && out.contains("got: first"), "{out}");
        assert!(out.contains(&format!("session {b} · done")) && out.contains("got: second"), "{out}");
        assert!(out.contains("1 tool call") || out.contains("0 tool calls"), "structured header: {out}");
        let (out, _) = agent_tool(&st, &parent, "agent_wait", &json!({}), &no).await;
        assert!(out.contains("every report has been read"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn background_agents_take_messages_and_report_to_the_parent() {
        let (st, parent, dir) = state_with(50, 4, "send").await;
        let id = bg(&st, &parent, "look around").await;
        let no = AtomicBool::new(false);
        wait_for(&[id.clone()], Some(Duration::from_secs(10)), &no).await;
        // Not collected with agent_wait: it arrives as mail before the
        // parent's next step, once.
        let mail = take_mail(&parent).expect("finished background report");
        assert!(mail.contains("got: look around"), "{mail}");
        assert!(take_mail(&parent).is_none(), "delivered once");

        // A finished agent starts again with the new message.
        let (out, ok) = agent_tool(&st, &parent, "agent_send", &json!({"id": id, "message": "now check tests"}), &no).await;
        assert!(ok && out.contains("started again"), "{out}");
        let (out, _) = agent_tool(&st, &parent, "agent_wait", &json!({"ids": [id]}), &no).await;
        assert!(out.contains("got: now check tests"), "{out}");
        let (list, _) = agent_tool(&st, &parent, "agent_list", &json!({}), &no).await;
        assert!(list.contains(&id) && list.contains("explore · done"), "{list}");
        let (out, _) = agent_tool(&st, &parent, "agent_stop", &json!({"id": id}), &no).await;
        assert!(out.contains("is not running"), "{out}");
        // Another chat cannot reach this chat's agents.
        let (out, ok) = agent_tool(&st, "ses_other", "agent_stop", &json!({"id": id}), &no).await;
        assert!(!ok && out.contains("no agent"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stopping_the_parent_stops_its_agents() {
        let (st, parent, dir) = state_with(3000, 4, "stop").await;
        let id = bg(&st, &parent, "slow work").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        st.abort_run(&parent).await;
        let no = AtomicBool::new(false);
        let (out, _) = agent_tool(&st, &parent, "agent_wait", &json!({"timeout_secs": 10}), &no).await;
        assert!(out.contains(&format!("session {id} · stopped")), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
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
