//! Deep mode (`/deep [n] <task>`): the same task tried n times, each attempt
//! in its own git worktree made from the current files. Every attempt is run
//! through the project's checks (the verify command and the page check) and
//! the best one is applied to the project, so the checks pick the winner
//! instead of the model. `/parallel <task>` is `/deep 2 <task>`.
//!
//! Attempts run at the same time; the checks run one attempt after another
//! (a page check starts a browser, which an 8 GB laptop feels).

use super::*;
use crate::worktree::Worktree;

/// Most attempts one `/deep` may start.
pub const MAX_ATTEMPTS: usize = 5;

/// One extra line per attempt so they do not all write the same answer.
const STYLES: [&str; MAX_ATTEMPTS] = [
    "",
    "Aim for the simplest change that fully does it.",
    "Be thorough: handle edge cases and errors.",
    "Read the related code first and follow its patterns closely.",
    "Prefer reusing what the project already has over writing new code.",
];

/// The task as one attempt sees it.
fn attempt_prompt(task: &str, i: usize, can_run_commands: bool) -> String {
    let mut p = format!(
        "{task}\n\n(Deep mode: this is one of several attempts at this task, each in its own \
         copy of the project; the app checks every attempt and keeps the best. Nobody can \
         answer questions during this run: make reasonable assumptions and finish."
    );
    if let Some(style) = STYLES.get(i).filter(|s| !s.is_empty()) {
        p.push(' ');
        p.push_str(style);
    }
    if !can_run_commands {
        p.push_str(" Shell commands are not available here; the app runs the project's checks on your result.");
    }
    p.push(')');
    p
}

/// One attempt after the checks.
struct Scored {
    n: usize,
    /// The run ended with an error (rate limit, network…).
    failed: Option<String>,
    files: Vec<PathBuf>,
    lines: usize,
    problems: usize,
    /// What the checks said, one line.
    checks: String,
    answer: String,
    tokens: u64,
}

impl Scored {
    /// Lower is better: a finished attempt with changes first, then fewer
    /// problems, then the smaller change.
    fn rank(&self) -> (bool, usize, usize) {
        (self.failed.is_some() || self.files.is_empty(), self.problems, self.lines)
    }
}

/// Show an attempt's tool calls on its card in the agents panel.
fn forward_activity(st: &Shared, worker: &Shared, info: &AgentInfo) -> tokio::task::JoinHandle<()> {
    let mut rx = worker.events.subscribe();
    let (st, info) = (st.clone(), info.clone());
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ServerEvent::ToolStarted { tool, detail, .. }) => {
                    let line: String = format!("{tool} {detail}").chars().take(120).collect();
                    agent_status(&st, &info, "working", line);
                }
                Ok(ServerEvent::Note { text, .. }) => {
                    agent_status(&st, &info, "working", text.chars().take(120).collect());
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Run Deep mode in chat `session_id`. Returns the text for the chat.
pub(super) async fn run_deep(
    st: &Shared,
    session_id: &str,
    task: &str,
    attempts: usize,
    abort: &Arc<AtomicBool>,
) -> Result<String> {
    let n = attempts.clamp(1, MAX_ATTEMPTS);
    let note = |text: String| {
        let _ = st.events.send(ServerEvent::Note { session_id: session_id.to_string(), text });
    };
    let ws = st.workspace_for(session_id);
    st.db.add_message(session_id, "user", &[Part::Text { text: format!("[deep mode, {n} attempts] {task}") }])?;
    let started_at = std::time::SystemTime::now() - std::time::Duration::from_secs(1);

    // The worktrees, one after the other (git locks the repository).
    let mut wts = Vec::new();
    for i in 1..=n {
        let (w, sd) = (ws.clone(), st.state_dir.clone());
        let made = tokio::task::spawn_blocking(move || Worktree::create(&w, &sd, &format!("deep{i}")))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        wts.push(made.map_err(|e| anyhow::anyhow!("Deep mode needs a git repository with at least one commit: {e:#}"))?);
    }

    // Attempt settings: no reviews inside an attempt, the model the chat
    // uses now, and no approval waits (nobody can answer them). Writes stay
    // inside the attempt's own copy, so they need no approval; commands do,
    // so outside turbo they are off and the app runs the checks instead.
    let mode = st.perm_mode();
    let can_run_commands = mode == PermissionMode::Turbo;
    let mut cfg = st.cfg.clone();
    cfg.agent.auto_review = false;
    cfg.provider.model = st.current_model().await;
    cfg.permissions.approval_timeout_secs = 0;
    // MCP servers act on the real project, not on an attempt's copy (and
    // each attempt would start them again).
    cfg.mcp.servers.clear();
    for (tool, action) in [("write", "allow"), ("edit", "allow"), ("ask_user", "deny")] {
        cfg.permissions.defaults.insert(tool.into(), action.into());
    }
    if !can_run_commands {
        cfg.permissions.defaults.insert("bash".into(), "deny".into());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let infos: Vec<AgentInfo> = (1..=n)
        .map(|i| AgentInfo {
            id: format!("deep-{stamp}-{i}"),
            parent: session_id.to_string(),
            name: format!("attempt {i}"),
            task: task.to_string(),
        })
        .collect();
    note(format!("deep mode: {n} attempts at the same time, each in its own copy of the project"));
    let runs = futures::future::join_all(wts.iter().zip(&infos).enumerate().map(|(i, (wt, info))| {
        let cfg = cfg.clone();
        let prompt = attempt_prompt(task, i, can_run_commands);
        async move {
            agent_status(st, info, "working", "starting…".into());
            let worker = build_state_ephemeral(cfg, wt.workspace()).await;
            let _ = worker.set_perm_mode(mode);
            let fwd = forward_activity(st, &worker, info);
            let res = match worker.db.create_session(&info.name, None) {
                Ok(s) => agent_inner(&worker, &s.id, &prompt, abort).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            };
            fwd.abort();
            // The attempt's tokens count for this chat too (`/usage`).
            let u = worker.db.usage_totals(None, None).unwrap_or_default();
            if u.input + u.output > 0 {
                let usage = crate::provider::Usage {
                    input: u.input,
                    cached: u.cached,
                    cache_write: u.cache_write,
                    output: u.output,
                };
                st.record_usage(session_id, &usage);
            }
            (res, u.input + u.output)
        }
    }))
    .await;
    if abort.load(Ordering::SeqCst) {
        for info in &infos {
            agent_status(st, info, "failed", "stopped".into());
        }
        anyhow::bail!("aborted");
    }

    // Check the attempts one after the other.
    let timeout = std::time::Duration::from_secs(st.cfg.agent.verify_timeout_secs.max(1));
    let expect = crate::pagecheck::expected_texts(task);
    let mut scored = Vec::new();
    for (i, (wt, (res, tokens))) in wts.iter().zip(runs).enumerate() {
        let info = &infos[i];
        let attempt_ws = wt.workspace();
        let (files, lines) = wt.changes().unwrap_or_default();
        let (failed, answer) = match res {
            Ok(a) if a.trim() == "aborted" => (Some("stopped".to_string()), String::new()),
            Ok(a) => (None, a),
            Err(e) => (Some(e), String::new()),
        };
        let mut s = Scored { n: i + 1, failed, files, lines, problems: 0, checks: String::new(), answer, tokens };
        if let Some(e) = &s.failed {
            s.checks = format!("did not finish: {}", e.chars().take(120).collect::<String>());
        } else if s.files.is_empty() {
            s.checks = "changed no files".into();
        } else {
            agent_status(st, info, "working", "checking the result…".into());
            let prefixed = |t: String| note(format!("attempt {}: {t}", i + 1));
            let pages = if st.cfg.agent.check_pages {
                crate::pagecheck::pages_for(&attempt_ws, &s.files, 3)
            } else {
                Vec::new()
            };
            let mut gate = Gate { cmd: verify_command_for(st, &attempt_ws, &prefixed), pages, expect: expect.clone() };
            match run_gate(st, &attempt_ws, &mut gate, timeout, &prefixed, abort).await {
                Some(r) => {
                    s.problems = r.problems;
                    s.checks = if r.problems == 0 { format!("passed: {}", r.summary) } else { r.summary };
                }
                None => s.checks = "no checks to run".into(),
            }
        }
        let state = if s.failed.is_some() { "failed" } else { "done" };
        agent_status(st, info, state, s.checks.clone());
        scored.push(s);
    }

    // Keep the best attempt.
    let best = scored.iter().min_by_key(|s| s.rank()).filter(|s| !s.rank().0).map(|s| s.n);
    let mut out = format!("**Deep mode** — {n} attempts at: {task}\n\n");
    for s in &scored {
        let mark = if Some(s.n) == best { " ← kept" } else { "" };
        out.push_str(&format!(
            "- attempt {}: {} file(s), {} line(s) changed · {} · {} tokens{mark}\n",
            s.n,
            s.files.len(),
            s.lines,
            s.checks,
            s.tokens
        ));
    }
    let Some(best) = best else {
        out.push_str("\nNo attempt produced a change, so nothing was applied.");
        return Ok(out);
    };
    let winner = &scored[best - 1];
    let wt = &wts[best - 1];
    let patch = wt.write_patch(&st.state_dir.join("deep-patches"))?;
    let Some(patch) = patch else {
        out.push_str("\nThe kept attempt has no changes after all; nothing was applied.");
        return Ok(out);
    };
    // A snapshot first, so `/undo` takes the applied attempt back.
    let (w, sd) = (ws.clone(), st.state_dir.clone());
    let _ = tokio::task::spawn_blocking(move || crate::gitsnap::record_before_run(&w, &sd, "pre-deep")).await;
    match wt.apply_to_repo(&patch) {
        Ok(()) => out.push_str(&format!(
            "\nApplied attempt {best} to the project (`/undo` takes it back).\n"
        )),
        Err(e) => {
            out.push_str(&format!(
                "\nCould not apply attempt {best} ({e:#}); the files changed while it ran. \
                 Its changes are saved in {}.",
                patch.display()
            ));
            return Ok(out);
        }
    }
    if !winner.answer.trim().is_empty() {
        let said: String = winner.answer.trim().chars().take(1500).collect();
        out.push_str(&format!("\nAttempt {best} said:\n\n{said}"));
    }
    // The other attempts' worktrees go when `wts` is dropped. The kept
    // attempt still had problems: the normal fix rounds take over.
    if winner.problems > 0 {
        st.db.add_message(session_id, "assistant", &[Part::Text { text: out.clone() }])?;
        let _ = st.events.send(ServerEvent::AssistantDelta { session_id: session_id.to_string(), text: out.clone() });
        drop(wts);
        check_after_run(st, session_id, task, started_at, true, abort).await;
        return Ok(String::new());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_get_different_hints() {
        let a = attempt_prompt("add a button", 0, false);
        let b = attempt_prompt("add a button", 1, false);
        assert!(a.starts_with("add a button") && a != b);
        assert!(a.contains("not available"));
        assert!(!attempt_prompt("x", 2, true).contains("not available"));
    }

    #[test]
    fn rank_prefers_working_small_changes() {
        let s = |failed: bool, files: usize, problems: usize, lines: usize| Scored {
            n: 0,
            failed: failed.then(|| "x".to_string()),
            files: vec![PathBuf::from("f"); files],
            lines,
            problems,
            checks: String::new(),
            answer: String::new(),
            tokens: 0,
        };
        assert!(s(false, 1, 0, 50).rank() < s(false, 1, 1, 5).rank(), "fewer problems wins");
        assert!(s(false, 1, 0, 5).rank() < s(false, 1, 0, 50).rank(), "then the smaller change");
        assert!(s(false, 1, 3, 5).rank() < s(true, 1, 0, 5).rank(), "a failed run is last");
        assert!(s(false, 1, 3, 5).rank() < s(false, 0, 0, 0).rank(), "no change is last");
    }
}
