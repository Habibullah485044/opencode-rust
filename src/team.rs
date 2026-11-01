use crate::db::Part;
use crate::server::{one_shot, ServerEvent, Shared};
use crate::worktree::Isolation;
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

/// How one worker ended. Serialized in lower case ("done", "failed",
/// "aborted"): the reviewer reads the reports as JSON, and the synthesis
/// shows the same words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Failed,
    Aborted,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Failed => "failed",
            Outcome::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerReport {
    pub subtask: String,
    pub outcome: Outcome,
    pub summary: String,
}

/// Most subtasks one /team run starts (B-18); the planner is asked for 1-4.
const MAX_SUBTASKS: usize = 4;

/// B-18: longest worker summary kept, in characters.
const MAX_SUMMARY_CHARS: usize = 4000;

/// Stream one line of team progress to the parent chat.
fn say(st: &Shared, session_id: &str, text: String) {
    let _ = st.events.send(ServerEvent::AssistantDelta { session_id: session_id.to_string(), text });
}

pub async fn run_team(st: &Shared, session_id: &str, goal: &str, abort: Arc<AtomicBool>) -> Result<String> {
    // B-18: plan mode is read-only, and team workers can edit files.
    if st.mode_for(session_id) == crate::agents::AgentMode::Plan {
        anyhow::bail!(
            "/team is not available in plan mode: team workers can edit files. \
             Switch with /build first."
        );
    }
    say(st, session_id, format!("[team] decomposing goal: {goal}\n"));
    let (sid, text) = (session_id.to_string(), format!("/team {goal}"));
    st.db.run(move |db| db.add_message(&sid, "user", &[Part::Text { text }])).await?;

    // 1. PLANNER — read-only agent proposes subtasks as JSON.
    let subtasks = crate::server::tagged("team", session_id, plan(st, goal, &abort)).await?;
    announce_plan(st, session_id, &subtasks);

    // 2. WORKERS — one child session per subtask, run in parallel.
    let reports = run_workers(st, session_id, goal, &subtasks, &abort).await;

    // 3. REVIEWER — one call verifies the reports (plan: "reviewer agent
    // verifies"), using the small model when configured.
    // B-18: after an abort nothing finished, so there is nothing to verify.
    let verdict = if abort.load(std::sync::atomic::Ordering::SeqCst) {
        "skipped: the run was aborted".to_string()
    } else {
        match crate::server::tagged("team", session_id, review(st, goal, &reports, &abort)).await {
            Ok(v) => v,
            Err(e) => format!("reviewer unavailable: {e:#}"),
        }
    };
    say(st, session_id, format!("[team] reviewer: {}\n", verdict.lines().next().unwrap_or("-")));

    // 4. SYNTHESIS — verdict + per-report results in the parent session.
    let summary = synthesis(goal, &verdict, &reports);
    let (sid, text) = (session_id.to_string(), summary.clone());
    st.db.run(move |db| db.add_message(&sid, "assistant", &[Part::Text { text }])).await?;
    // B-34: stream the synthesis too. Clients see the per-worker results,
    // and the TUI team panel reaches "done" on its `[team] results for:` line.
    say(st, session_id, summary.clone());
    Ok(summary)
}

/// Stream the planner's subtasks, one numbered line each.
fn announce_plan(st: &Shared, session_id: &str, subtasks: &[Subtask]) {
    say(st, session_id, format!("[team] plan: {} subtask(s)\n", subtasks.len()));
    for (i, t) in subtasks.iter().enumerate() {
        let task = if t.instructions.trim().is_empty() { &t.title } else { &t.instructions };
        say(st, session_id, format!("  {}. {} — {}\n", i + 1, t.title.replace('\n', " "), task.replace('\n', " ")));
    }
}

/// Start one worker per subtask, all at once, and collect their reports in
/// plan order, streaming each worker's end state.
async fn run_workers(
    st: &Shared,
    session_id: &str,
    goal: &str,
    subtasks: &[Subtask],
    abort: &Arc<AtomicBool>,
) -> Vec<WorkerReport> {
    let handles: Vec<_> = subtasks
        .iter()
        .map(|task| {
            let (st2, parent, task, abort2, goal) =
                (st.clone(), session_id.to_string(), task.clone(), abort.clone(), goal.to_string());
            tokio::spawn(async move { worker(&st2, &parent, &task, &goal, abort2).await })
        })
        .collect();
    let mut reports = Vec::new();
    for (n, h) in handles.into_iter().enumerate() {
        let title = subtasks[n].title.clone();
        let (outcome, summary) = match h.await {
            // B-18: run_loop returns "aborted" when stopped; that is not "done".
            Ok(Ok(s)) if s.trim() == "aborted" => (Outcome::Aborted, "stopped before it finished".to_string()),
            Ok(Ok(s)) => (Outcome::Done, s.chars().take(MAX_SUMMARY_CHARS).collect()),
            Ok(Err(e)) => (Outcome::Failed, format!("error: {e:#}")),
            Err(e) => (Outcome::Failed, format!("worker crashed: {e}")),
        };
        // The TUI team panel looks for "done" and "failed": keep those words.
        let status = match outcome {
            Outcome::Done => "done".to_string(),
            Outcome::Aborted => "failed: aborted".to_string(),
            Outcome::Failed => format!("failed: {}", summary.replace('\n', " ")),
        };
        say(st, session_id, format!("[team] worker {n} ({title}) {status}\n"));
        reports.push(WorkerReport { subtask: title, outcome, summary });
    }
    reports
}

/// The final message: the reviewer's verdict, then one line per worker.
fn synthesis(goal: &str, verdict: &str, reports: &[WorkerReport]) -> String {
    let mut summary = format!("[team] results for: {goal}\n");
    summary.push_str(&format!("[team] reviewer: {verdict}\n"));
    for r in reports {
        summary.push_str(&format!("- [{}] {}: {}\n", r.outcome.as_str(), r.subtask, r.summary));
    }
    summary
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
    Ok(parse_subtasks(&text, goal))
}

/// Reviewer: one non-streamed call verifying the worker reports.
async fn review(st: &Shared, goal: &str, reports: &[WorkerReport], abort: &AtomicBool) -> Result<String> {
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

/// B-18: at most MAX_SUBTASKS subtasks; when the planner gave nothing
/// usable, one subtask whose instructions are the user's goal.
fn parse_subtasks(text: &str, goal: &str) -> Vec<Subtask> {
    let mut subtasks = parse_subtask_array(text);
    if subtasks.is_empty() {
        let instructions = goal.chars().take(2000).collect();
        subtasks.push(Subtask { title: "task".into(), instructions });
    }
    subtasks.truncate(MAX_SUBTASKS);
    subtasks
}

/// Robustly extract a JSON array from model output (handles ```json fences etc).
/// B-18: start at the first `[` where a JSON array of objects with a "title"
/// begins; read exactly that one value. Returns an empty list when none.
fn parse_subtask_array(text: &str) -> Vec<Subtask> {
    let span = text.char_indices().filter(|&(_, c)| c == '[').find_map(|(s, _)| {
        let mut values = serde_json::Deserializer::from_str(&text[s..]).into_iter::<Value>();
        match values.next() {
            Some(Ok(Value::Array(items))) if items.iter().any(|v| v.get("title").and_then(Value::as_str).is_some()) => {
                Some((s, s + values.byte_offset() - 1))
            }
            _ => None,
        }
    });
    if let Some((s, e)) = span {
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
                            instructions: v.get("instructions").and_then(Value::as_str).unwrap_or("").to_string(),
                        })
                    })
                    .collect();
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }
    // Nothing usable: `parse_subtasks` falls back to the user's goal (B-18).
    Vec::new()
}

/// Worker: child session with the standard agent loop but scoped tools.
async fn worker(st: &Shared, parent: &str, task: &Subtask, goal: &str, abort: Arc<AtomicBool>) -> Result<String> {
    let (title, parent_id) = (task.title.clone(), parent.to_string());
    let session = st.db.run(move |db| db.create_session(&title, Some(&parent_id))).await?;
    let prompt = format!("Overall goal: {goal}\n\nYour subtask: {}\n{}", task.title, task.instructions);
    run_worker_agent(st, &session.id, &prompt, &abort).await
}

/// Worker variant of the agent loop — now the SHARED loop with the worker's
/// system prompt: no activity events, no spawn tool (M2 dedup).
async fn run_worker_agent(st: &Shared, session_id: &str, prompt: &str, abort: &AtomicBool) -> Result<String> {
    let (sid, text) = (session_id.to_string(), prompt.to_string());
    let parent = st
        .db
        .run(move |db| {
            db.add_message(&sid, "user", &[Part::Text { text }])?;
            Ok(db.get_session(&sid).ok().flatten().and_then(|s| s.parent_id))
        })
        .await?;
    // The worker works in its parent chat's project folder.
    let project_ws = st.workspace_for(session_id);
    let mut system = worker_system(st, session_id, parent.as_deref(), &project_ws);
    // F-05: with `agent.isolate_workers`, each team worker runs in its own
    // git worktree. The prompt names the folder it really works in.
    let iso = match st.cfg.agent.isolate_workers {
        true => Some(Isolation::start_async(project_ws.clone(), st.state_dir.clone(), "team".into()).await),
        false => None,
    };
    let iso_ws = iso.as_ref().and_then(|i| i.workspace());
    if let Some(ws) = &iso_ws {
        system.push_str(&crate::worktree::isolation_prompt(ws));
    }
    let opts = crate::server::LoopOpts {
        model: None,
        max_steps: None,
        agent: None,
        system,
        emit_activity: false,
        // F-05: an isolated worker keeps to the built-in tools.
        allow: iso_ws.as_ref().map(|_| crate::worktree::isolated_tools(None)),
        can_spawn: false,
        workspace: iso_ws.or_else(|| st.run_workspace(session_id)),
    };
    let out = crate::server::run_loop(st, session_id, &opts, abort).await;
    // An aborted worker's changes are dropped.
    let Some(iso) = iso else { return out };
    if abort.load(std::sync::atomic::Ordering::SeqCst) {
        iso.discard_async().await;
        return out;
    }
    // F-05: report what the worktree produced, or why there was none — also
    // when the loop failed, so a failed worker's work is saved, not deleted
    // with its worktree. The report goes first, so a later size cap cannot
    // cut it off, and the user sees it as a note in the parent session.
    let report = iso.finish_async().await;
    if let Some(parent) = parent {
        let _ = st
            .events
            .send(ServerEvent::Note { session_id: parent, text: format!("team worker {session_id}: {report}") });
    }
    match out {
        Ok(text) => Ok(format!("{report}\n\n{text}")),
        Err(e) => Err(anyhow::anyhow!("{report}\n\n{e:#}")),
    }
}

/// The team worker's system prompt: its rules, the project instructions,
/// the parent's ponytail level and the baked-in skills.
fn worker_system(st: &Shared, session_id: &str, parent: Option<&str>, project_ws: &std::path::Path) -> String {
    let mut system = format!(
        "You are a focused worker agent in team mode. Complete only your subtask.\n\n\
         Workspace: {}.\nNobody can answer questions during this run: make reasonable \
         decisions yourself and finish. Report a 1-2 sentence summary when finished.",
        project_ws.display()
    );
    if let Some(md) = crate::skills::load_agents_md(project_ws) {
        system.push_str("\n\n## Project instructions (AGENTS.md)\n\n");
        system.push_str(md.trim());
    }
    // P-05: a team worker follows its parent session's ponytail level.
    let level = st.ponytail_level(parent.unwrap_or(session_id));
    system.push_str(&crate::agents::ponytail_section(level));
    for s in crate::skills::load_baked(&st.skill_base) {
        system.push_str(&format!("\n\n## Skill: {} — always active\n\n", s.name));
        system.push_str(s.body.trim());
    }
    system
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let text = "Sure!\n```json\n[{\"title\":\"a\",\"instructions\":\"do a\"},{\"title\":\"b\",\"instructions\":\"do b\"}]\n```";
        let subs = parse_subtasks(text, "goal");
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].title, "a");
    }

    #[test]
    fn falls_back_to_single() {
        let subs = parse_subtasks("just some prose, no json", "the goal");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].instructions, "the goal");
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
        let v = serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": null}]});
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
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
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

    /// F-05: with `agent.isolate_workers`, team workers run in git worktrees.
    /// The mock worker changes nothing, so its report says so.
    #[tokio::test]
    async fn team_workers_run_isolated_in_a_git_repo() {
        let dir = std::env::temp_dir().join(format!("ocrs-team-iso-{}", std::process::id()));
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
        let mock = MockLlm::start().await;
        cfg.provider.base_url = mock.url.clone();
        cfg.provider.model = "mock".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("team", None).unwrap();
        let abort = Arc::new(AtomicBool::new(false));
        let out = run_team(&st, &s.id, "say hi", abort).await.unwrap();
        assert!(out.contains("[isolated worktree] no file changes."), "{out}");
        let list = git(&["worktree", "list", "--porcelain"]);
        let count = list.lines().filter(|l| l.starts_with("worktree ")).count();
        assert_eq!(count, 1, "only the main worktree is left: {list}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;

    /// Planner, reviewer and a worker that writes a file and then fails
    /// (the provider errors on its second call).
    async fn failing_worker_llm() -> String {
        use axum::{http::StatusCode, response::IntoResponse, routing::post, Router};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<Value>| async move {
                let sys = body["messages"][0]["content"].as_str().unwrap_or("").to_string();
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("").to_string();
                let sse = |frames: Vec<Value>| {
                    let mut out: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
                    out.push_str("data: [DONE]\n\n");
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], out).into_response()
                };
                let text =
                    |t: &str| serde_json::json!({"choices": [{"delta": {"content": t}, "finish_reason": "stop"}]});
                if sys.contains("planning agent") {
                    return sse(vec![text("[{\"title\":\"sub\",\"instructions\":\"write it\"}]")]);
                }
                if sys.contains("reviewer agent") {
                    return sse(vec![text("VERDICT: FAIL\n- the worker failed")]);
                }
                if last_role == "tool" {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "provider down").into_response();
                }
                let args = serde_json::json!({"path": "from_worker.txt", "content": "kept\n"}).to_string();
                sse(vec![
                    serde_json::json!({"choices": [{"delta": {"tool_calls": [{
                        "index": 0, "id": "call_w", "function": {"name": "write", "arguments": args}
                    }]}, "finish_reason": null}]}),
                    serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                ])
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/v1")
    }

    /// Before: the worker's error returned early and dropping its worktree
    /// deleted the file it wrote. Now the failed worker's change is saved as
    /// a patch and its report says where.
    #[tokio::test]
    async fn a_failed_isolated_worker_keeps_its_changes_as_a_patch() {
        let dir = std::env::temp_dir().join(format!("ocrs-team-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| crate::gitsnap::git(&dir, args).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("readme.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        let mut cfg = crate::config::Config::default();
        cfg.agent.isolate_workers = true;
        cfg.permissions.defaults.insert("write".into(), "allow".into());
        cfg.permissions.approval_timeout_secs = 2;
        cfg.provider.base_url = failing_worker_llm().await;
        cfg.provider.model = "mock".into();
        cfg.provider.max_retries = 0;
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("team", None).unwrap();
        let out = run_team(&st, &s.id, "write a file", Arc::new(AtomicBool::new(false))).await.unwrap();
        assert!(out.contains("- [failed] sub:"), "{out}");
        let patch = out
            .lines()
            .find_map(|l| l.strip_prefix("Patch: "))
            .unwrap_or_else(|| panic!("the failed worker's patch is reported: {out}"))
            .to_string();
        assert!(std::fs::read_to_string(&patch).unwrap().contains("from_worker.txt"));
        let list = git(&["worktree", "list", "--porcelain"]);
        assert_eq!(list.lines().filter(|l| l.starts_with("worktree ")).count(), 1, "{list}");
        let _ = std::fs::remove_file(&patch);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod b18_tests {
    use super::*;

    #[test]
    fn plan_parsing_ignores_prose_brackets_and_caps_subtasks() {
        let text = "Plan [draft], see [1]:\n[{\"title\":\"a\",\"instructions\":\"do a\"}]\nDone [ok]";
        let subs = parse_subtasks(text, "goal");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].title, "a");
        let many: Vec<Value> =
            (0..9).map(|i| serde_json::json!({"title": format!("t{i}"), "instructions": "x"})).collect();
        let subs = parse_subtasks(&serde_json::to_string(&many).unwrap(), "goal");
        assert_eq!(subs.len(), MAX_SUBTASKS);
    }

    #[test]
    fn fallback_uses_the_goal_not_the_planner_text() {
        let subs = parse_subtasks("I could not decide.", "write hello.txt");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].instructions, "write hello.txt");
    }

    #[tokio::test]
    async fn team_is_refused_in_plan_mode() {
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = "http://127.0.0.1:9/v1".into();
        let st = crate::server::build_state_ephemeral(cfg, std::env::temp_dir()).await;
        let s = st.db.create_session("team-plan", None).unwrap();
        st.set_mode(&s.id, crate::agents::AgentMode::Plan);
        let err = run_team(&st, &s.id, "edit things", Arc::new(AtomicBool::new(false))).await.unwrap_err();
        assert!(err.to_string().contains("plan mode"), "{err}");
        assert!(st.db.messages(&s.id).unwrap().is_empty(), "nothing stored");
    }
}
