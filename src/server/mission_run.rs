//! Mission mode (`/mission <goal>`): the agent works on its own until the
//! goal is reached, without the user.
//!
//! A coordinator (one plain model call each time, no tools) splits the goal
//! into steps. Each step runs as its own worker run in a fresh child chat,
//! with a short brief (the goal, the plan, what earlier steps did) instead
//! of the whole history, so a long mission does not resend a growing chat
//! and fits small context windows. After each step the project's checks run
//! (with the usual keep-best fix rounds), the step's receipt is saved
//! (summary, files, checks), and the coordinator decides what comes next:
//! go on, redo the step, add a step, or finish.
//!
//! While a mission runs, approvals are handled as in turbo (nobody is there
//! to click them) and the worker may not ask questions. The mission, its
//! steps and their receipts are kept in the database, so `/mission resume`
//! carries on after a stop, a pause or a restart.

use super::*;
use crate::db::{Mission, MissionStep};

/// Tries per step before it is marked failed and the mission moves on.
const MAX_ATTEMPTS: i64 = 3;
/// Step runs that end in an error (provider down, network) in a row before
/// the mission pauses instead of burning through its steps.
const MAX_ERRORS_IN_A_ROW: usize = 3;

const COORDINATOR: &str = "You coordinate an autonomous coding mission. Workers with full tool \
access do the steps you assign, one at a time, in the project folder. You never do the work \
yourself and you never ask the user anything: they are away until the mission is complete.";

/// The first numbered or bulleted list in a reply, as plain lines.
fn parse_steps(reply: &str) -> Vec<String> {
    let mut steps = Vec::new();
    for line in reply.lines() {
        let l = line.trim();
        let rest = l
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .strip_prefix(['.', ')', ':'])
            .filter(|_| l.starts_with(|c: char| c.is_ascii_digit()))
            .or_else(|| l.strip_prefix("- "))
            .or_else(|| l.strip_prefix("* "));
        if let Some(step) = rest.map(|r| r.trim().trim_matches('*').trim()).filter(|r| r.len() > 3) {
            steps.push(step.to_string());
        } else if !steps.is_empty() && l.is_empty() {
            continue;
        }
    }
    steps.truncate(12);
    steps
}

/// What the coordinator wants after a step.
#[derive(Debug, PartialEq)]
enum Verdict {
    Next,
    Redo(String),
    Add(String),
    Done,
}

/// The first line that starts with a known word wins; anything else means
/// go on (a weak model's chatter must not stop the mission).
fn parse_verdict(reply: &str) -> Verdict {
    for line in reply.lines() {
        let l = line.trim().trim_start_matches(['*', '#', '-', ' ', '`']);
        let upper = l.to_ascii_uppercase();
        let after = |word: &str| l[word.len()..].trim_start_matches([':', ' ', '-', '—']).trim().to_string();
        if upper.starts_with("REDO") {
            return Verdict::Redo(after("REDO"));
        }
        if upper.starts_with("ADD") {
            let step = after("ADD");
            if !step.is_empty() {
                return Verdict::Add(step);
            }
        }
        if upper.starts_with("DONE") {
            return Verdict::Done;
        }
        if upper.starts_with("NEXT") {
            return Verdict::Next;
        }
    }
    Verdict::Next
}

fn mark(status: &str) -> &'static str {
    match status {
        "done" => "[x]",
        "working" => "[>]",
        "failed" => "[!]",
        _ => "[ ]",
    }
}

/// The plan with each step's state, for the coordinator and the brief.
fn plan_text(m: &Mission) -> String {
    m.steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{} {}. {}", mark(&s.status), i + 1, s.title))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first line of each finished step's receipt.
fn done_so_far(m: &Mission) -> String {
    let lines: Vec<String> = m
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.status == "done" || s.status == "failed")
        .map(|(i, s)| {
            let first: String = s.result.lines().next().unwrap_or("").chars().take(300).collect();
            format!("- step {} ({}): {first}", i + 1, s.status)
        })
        .collect();
    if lines.is_empty() { "nothing yet".into() } else { lines.join("\n") }
}

/// What a worker is told for one step.
fn step_brief(m: &Mission, n: usize, step: &MissionStep, redo: Option<&str>) -> String {
    let mut b = format!(
        "[mission step {n} of {}] {}\n\nMission goal: {}\n\nPlan:\n{}\n\nDone so far:\n{}\n\n\
         Do only this step, fully, in the project folder. Nobody can answer questions: decide \
         yourself. Check your work (run the tests or open the page). When the step is done, \
         reply with a short report: what you changed (file paths) and how you checked it.",
        m.steps.len(),
        step.title,
        m.goal,
        plan_text(m),
        done_so_far(m)
    );
    if let Some(why) = redo.filter(|w| !w.is_empty()) {
        b.push_str(&format!("\n\nThis step was tried before and is not done yet: {why}"));
    }
    b
}

async fn plan(st: &Shared, ws: &std::path::Path, goal: &str, abort: &AtomicBool) -> Vec<String> {
    let map = crate::repomap::render(&crate::repomap::scan(ws, &st.state_dir), goal, &[], 600);
    let user = format!(
        "Mission goal:\n{goal}\n{map}\n\nSplit the mission into 2 to 8 steps, in order. Each step is one \
         line: a concrete result one focused work session can finish and check. Make the last step \
         \"Check that the whole result works\". Reply with only the numbered list."
    );
    match one_shot(st, COORDINATOR, &user, abort, false).await {
        Ok(reply) => {
            let steps = parse_steps(&reply);
            if steps.is_empty() { vec![goal.to_string()] } else { steps }
        }
        Err(_) => vec![goal.to_string()],
    }
}

async fn review(st: &Shared, m: &Mission, n: usize, receipt: &str, abort: &AtomicBool) -> Verdict {
    let user = format!(
        "Mission goal:\n{}\n\nPlan ([x] done, [>] just worked on, [!] failed, [ ] to do):\n{}\n\n\
         Report of step {n}:\n{}\n\nDecide what happens next. Reply with exactly one line:\n\
         NEXT — step {n} is done, go on with the plan\n\
         REDO: <what is missing> — step {n} is not done yet\n\
         ADD: <new step> — step {n} is done, and the mission needs one more step right after it\n\
         DONE — the whole mission goal is reached",
        m.goal,
        plan_text(m),
        receipt.chars().take(3000).collect::<String>()
    );
    match one_shot(st, COORDINATOR, &user, abort, false).await {
        Ok(reply) => parse_verdict(&reply),
        Err(_) => Verdict::Next,
    }
}

/// Tell the chat: a line in the transcript and a live event.
fn say(st: &Shared, session_id: &str, text: String) {
    let _ = st.db.add_message(session_id, "assistant", &[Part::Text { text: text.clone() }]);
    let _ = st.events.send(ServerEvent::AssistantDelta { session_id: session_id.to_string(), text: format!("{text}\n\n") });
}

/// Start a new mission (`goal: Some`) or carry on with the chat's mission
/// (`None`). Returns the final summary.
pub(super) async fn run_mission(
    st: &Shared,
    session_id: &str,
    goal: Option<&str>,
    abort: &Arc<AtomicBool>,
) -> Result<String> {
    let note = |text: String| {
        let _ = st.events.send(ServerEvent::Note { session_id: session_id.to_string(), text });
    };
    let ws = st.workspace_for(session_id);
    if let Some(goal) = goal {
        st.db.add_message(session_id, "user", &[Part::Text { text: format!("[mission] {goal}") }])?;
        note("mission: planning the steps…".into());
        let steps = plan(st, &ws, goal, abort).await;
        if abort.load(Ordering::SeqCst) {
            anyhow::bail!("aborted");
        }
        st.db.mission_start(session_id, goal, &steps)?;
        let m = st.db.mission(session_id)?.expect("just started");
        say(st, session_id, format!("**Mission plan** — I keep working until it is done:\n{}", plan_text(&m)));
    } else {
        let Some(m) = st.db.mission(session_id)? else {
            anyhow::bail!("this chat has no mission to resume; start one with /mission <goal>");
        };
        if m.status == "done" {
            return Ok("This mission is already complete.".into());
        }
        st.db.mission_set_status(session_id, "running")?;
        note(format!("mission: resuming — {} step(s) left", m.steps.iter().filter(|s| s.status != "done" && s.status != "failed").count()));
    }

    // Nobody is there to approve: run like turbo, and put the user's mode
    // back afterwards.
    let before = st.perm_mode();
    if before != PermissionMode::Turbo {
        st.set_perm_mode(PermissionMode::Turbo);
        note("mission: approvals are automatic until the mission ends (deny rules still apply)".into());
    }
    let result = steps_loop(st, session_id, &ws, abort).await;
    if before != PermissionMode::Turbo {
        st.set_perm_mode(before);
    }
    let status = match &result {
        Ok(true) => "done",
        Ok(false) => "paused",
        Err(_) if abort.load(Ordering::SeqCst) => "stopped",
        Err(_) => "paused",
    };
    st.db.mission_set_status(session_id, status)?;
    let m = st.db.mission(session_id)?.expect("mission exists");
    let counts = |s: &str| m.steps.iter().filter(|x| x.status == s).count();
    let summary = match status {
        "done" => format!(
            "**Mission complete** — {} step(s) done{}.\n{}",
            counts("done"),
            if counts("failed") > 0 { format!(", {} failed", counts("failed")) } else { String::new() },
            plan_text(&m)
        ),
        "stopped" => format!("**Mission stopped.** `/mission resume` carries on.\n{}", plan_text(&m)),
        _ => format!(
            "**Mission paused** ({}). `/mission resume` carries on.\n{}",
            match &result {
                Err(e) => format!("{e:#}"),
                _ => "step limit reached (agent.mission_max_steps)".into(),
            },
            plan_text(&m)
        ),
    };
    Ok(summary)
}

/// Work through the steps. `Ok(true)` = mission complete, `Ok(false)` = the
/// step limit was reached.
async fn steps_loop(st: &Shared, session_id: &str, ws: &std::path::Path, abort: &Arc<AtomicBool>) -> Result<bool> {
    let limit = st.cfg.agent.mission_max_steps;
    let mut runs = 0usize;
    let mut errors = 0usize;
    let mut redo_reason: Option<String> = None;
    loop {
        if abort.load(Ordering::SeqCst) {
            anyhow::bail!("aborted");
        }
        let m = st.db.mission(session_id)?.expect("mission exists");
        let Some((i, step)) = m.steps.iter().enumerate().find(|(_, s)| s.status == "pending" || s.status == "working") else {
            return Ok(true);
        };
        if limit > 0 && runs >= limit {
            return Ok(false);
        }
        runs += 1;
        let n = i + 1;
        let step = step.clone();
        let attempts = step.attempts + 1;
        let child = st.db.create_session(&format!("mission step {n}"), Some(session_id))?;
        st.db.mission_step_update(step.id, "working", attempts, &step.result, Some(&child.id))?;
        let info = AgentInfo {
            id: child.id.clone(),
            parent: session_id.to_string(),
            name: format!("step {n}/{}", m.steps.len()),
            task: step.title.clone(),
        };
        agent_status(st, &info, "working", if attempts > 1 { format!("try {attempts}") } else { "starting…".into() });
        let m_now = st.db.mission(session_id)?.expect("mission exists");
        let brief = step_brief(&m_now, n, &step, redo_reason.as_deref());
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
        let run = agent_inner(st, &child.id, &brief, abort).await;
        if abort.load(Ordering::SeqCst) {
            st.db.mission_step_update(step.id, "pending", attempts - 1, &step.result, None)?;
            agent_status(st, &info, "failed", "stopped".into());
            anyhow::bail!("aborted");
        }
        let report = match &run {
            Ok(_) => last_assistant_text(st, &child.id).unwrap_or_else(|| "(no report)".into()),
            Err(e) => format!("the step run failed: {e:#}"),
        };
        if run.is_err() {
            errors += 1;
            st.db.mission_step_update(step.id, "pending", attempts - 1, &report, None)?;
            agent_status(st, &info, "failed", report.chars().take(120).collect());
            if errors >= MAX_ERRORS_IN_A_ROW {
                anyhow::bail!("{errors} step runs failed in a row ({})", report.chars().take(200).collect::<String>());
            }
            continue;
        }
        errors = 0;
        let mut files = crate::keepbest::written_since(ws, since);
        files.extend(crate::pagecheck::recently_modified(ws, since));
        files.sort();
        files.dedup();
        let checks = if files.is_empty() {
            None
        } else {
            agent_status(st, &info, "working", "checking the result…".into());
            check_after_run(st, &child.id, &brief, since, true, abort).await
        };
        let shown: Vec<String> = files
            .iter()
            .take(12)
            .map(|f| f.strip_prefix(ws).unwrap_or(f).to_string_lossy().replace('\\', "/"))
            .collect();
        let receipt = format!(
            "{}\nFiles changed: {}\nChecks: {}",
            report.trim(),
            if shown.is_empty() { "none".to_string() } else { shown.join(", ") },
            match &checks {
                Some((true, s)) => format!("passed ({s})"),
                Some((false, s)) => format!("still failing ({s})"),
                None => "none ran".into(),
            }
        );
        // The coordinator sees this step as the one just worked on.
        st.db.mission_step_update(step.id, "working", attempts, &receipt, None)?;
        let m = st.db.mission(session_id)?.expect("mission exists");
        let verdict = review(st, &m, n, &receipt, abort).await;
        let first_line: String = report.trim().lines().next().unwrap_or("").chars().take(200).collect();
        redo_reason = None;
        match verdict {
            Verdict::Redo(why) if attempts < MAX_ATTEMPTS => {
                st.db.mission_step_update(step.id, "pending", attempts, &receipt, None)?;
                agent_status(st, &info, "failed", format!("redo: {why}"));
                say(st, session_id, format!("↻ Step {n} again — {}", if why.is_empty() { "not done yet".to_string() } else { why.clone() }));
                redo_reason = Some(why);
            }
            Verdict::Redo(why) => {
                st.db.mission_step_update(step.id, "failed", attempts, &receipt, None)?;
                agent_status(st, &info, "failed", "gave up after 3 tries".into());
                say(st, session_id, format!("✗ Step {n} failed after {MAX_ATTEMPTS} tries ({why}); moving on."));
            }
            Verdict::Add(new_step) => {
                st.db.mission_step_update(step.id, "done", attempts, &receipt, None)?;
                st.db.mission_step_insert_after(session_id, step.id, &new_step)?;
                agent_status(st, &info, "done", first_line.clone());
                say(st, session_id, format!("✓ Step {n}: {} — {first_line}\n➕ Added next: {new_step}", step.title));
            }
            Verdict::Done => {
                st.db.mission_step_update(step.id, "done", attempts, &receipt, None)?;
                agent_status(st, &info, "done", first_line.clone());
                say(st, session_id, format!("✓ Step {n}: {} — {first_line}", step.title));
                // The coordinator says the goal is reached: what is left is not needed.
                return Ok(true);
            }
            Verdict::Next => {
                st.db.mission_step_update(step.id, "done", attempts, &receipt, None)?;
                agent_status(st, &info, "done", first_line.clone());
                say(st, session_id, format!("✓ Step {n}: {} — {first_line}", step.title));
            }
        }
    }
}

/// `/mission status`: the plan with each step's state and receipt line.
pub fn mission_report(st: &AppState, session_id: &str) -> String {
    match st.db.mission(session_id) {
        Ok(Some(m)) => format!("Mission ({}): {}\n{}\n\nReceipts:\n{}", m.status, m.goal, plan_text(&m), done_so_far(&m)),
        _ => "This chat has no mission. Start one with /mission <goal>.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_are_read_from_lists() {
        let reply = "Here is the plan:\n1. Build the page layout\n2) Add the cart\n3. **Check that the whole result works**\n\nGood luck";
        assert_eq!(parse_steps(reply), ["Build the page layout", "Add the cart", "Check that the whole result works"]);
        assert_eq!(parse_steps("- one step\n- two step"), ["one step", "two step"]);
        assert!(parse_steps("I will do it all at once.").is_empty());
    }

    #[test]
    fn verdicts_are_read_leniently() {
        assert_eq!(parse_verdict("NEXT"), Verdict::Next);
        assert_eq!(parse_verdict("**REDO**: the cart total is wrong"), Verdict::Redo("the cart total is wrong".into()));
        assert_eq!(parse_verdict("Looks good.\nADD: write tests for the cart"), Verdict::Add("write tests for the cart".into()));
        assert_eq!(parse_verdict("DONE — all goals met"), Verdict::Done);
        assert_eq!(parse_verdict("I think it is fine"), Verdict::Next, "chatter goes on");
    }
}
