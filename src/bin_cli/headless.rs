//! `--headless`: run one prompt (or one builtin command) without the TUI,
//! print the result, exit.

use super::shared;
use anyhow::{Context, Result};
use opencode_rust::{agents::AgentMode, commands::Command as Cmd, server};
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

type State = Arc<server::AppState>;

/// New (S-02): may headless mode say "yes" to approval prompts by itself?
/// Default: no. `--yes` (before any `--`) or OPENCODE_RUST_AUTO_APPROVE=1
/// turns it on; OPENCODE_RUST_AUTO_DENY=1 always wins and forces "no".
pub(crate) fn auto_approve_policy(args: &[String], approve_env: Option<&str>, deny_env: Option<&str>) -> bool {
    if deny_env.map(str::trim) == Some("1") {
        return false;
    }
    let yes_flag = args.iter().take_while(|a| a.as_str() != "--").any(|a| a == "--yes");
    yes_flag || approve_env.map(str::trim) == Some("1")
}

/// New (S-02): `auto_approve_policy` for this process (its real command
/// line and environment).
fn headless_auto_approve() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let approve = std::env::var("OPENCODE_RUST_AUTO_APPROVE").ok();
    let deny = std::env::var("OPENCODE_RUST_AUTO_DENY").ok();
    auto_approve_policy(&args, approve.as_deref(), deny.as_deref())
}

/// What a builtin command left to do.
enum Outcome {
    /// Done; the process ends without waiting for a run.
    Finished,
    /// It started a run; stream its events until it finishes.
    RunStarted,
}

pub(crate) async fn headless_run(
    state: State,
    addr: std::net::SocketAddr,
    prompt: Option<String>,
    boot_ms: u128,
) -> Result<()> {
    println!("[boot] core ready in {boot_ms}ms at http://{addr}");
    let prompt = prompt.context("--headless requires a prompt argument")?;
    let session = state.db.run(|db| db.create_session("cli", None)).await?;
    println!("[session] {}", session.id);

    // Subscribe before starting so we see every event.
    let rx = state.events.subscribe();
    // A builtin with missing or invalid arguments is a usage mistake, as in
    // the TUI: say so instead of sending it to the model as chat.
    if let Some(msg) = opencode_rust::commands::usage_error(&prompt) {
        eprintln!("{msg}");
        std::process::exit(2);
    }
    // M2: builtin commands that make sense headless.
    let mut team_spawned = false;
    if let Some(cmd) = opencode_rust::commands::classify(&prompt) {
        match run_command(&state, &session.id, cmd).await {
            Outcome::Finished => return Ok(()),
            Outcome::RunStarted => team_spawned = true,
        }
    }
    if !team_spawned {
        // New (B-06): kick_run registers the run in `busy`, so it can be
        // aborted (POST /api/sessions/{id}/abort) and replaced.
        state.kick_run(session.id.clone(), prompt).await;
    }
    stream_events(&state, &session.id, rx).await
}

/// Run one builtin command headless. Failures print and exit the process.
async fn run_command(state: &State, sid: &str, cmd: Cmd) -> Outcome {
    match cmd {
        Cmd::Plan | Cmd::Build => {
            let m = if matches!(cmd, Cmd::Plan) { AgentMode::Plan } else { AgentMode::Build };
            state.set_mode(sid, m);
            println!("[mode] {}", m.as_str());
        }
        Cmd::Skills => {
            let skills = opencode_rust::skills::load_skills(&state.workspace);
            if skills.is_empty() {
                println!("[skills] none found");
            } else {
                println!("{}", opencode_rust::skills::skill_menu(&skills));
            }
        }
        Cmd::Undo | Cmd::UndoForce | Cmd::Redo => {
            snapshot(state, matches!(cmd, Cmd::Redo), matches!(cmd, Cmd::UndoForce)).await
        }
        Cmd::Compact => compact(state, sid).await,
        Cmd::Quit => {}
        Cmd::Edit { path, instruction } => inline_edit(state, sid, &path, &instruction).await,
        Cmd::Team(goal) => {
            shared::kick_team(state, sid.to_string(), goal).await;
            return Outcome::RunStarted;
        }
        // M6: discoverability + control commands, straight to stdout.
        Cmd::Help => println!("{}", opencode_rust::commands::help_text()),
        Cmd::Sessions => {
            let list = state.session_list();
            if list.is_empty() {
                println!("[sessions] none yet");
            }
            for s in list {
                let fork = if s.parent_id.is_some() { "↳" } else { " " };
                println!("{fork} {}  {}", s.id, s.title);
            }
        }
        Cmd::Todos => println!("{}", state.todos_report(sid)),
        Cmd::Usage => println!("{}", state.usage_report(sid)),
        Cmd::Clear | Cmd::New => {
            println!("[{cmd_kind}] headless runs are one session per invocation", cmd_kind = if matches!(cmd, Cmd::Clear) { "clear" } else { "new" });
        }
        Cmd::Init => match opencode_rust::skills::init_agents_md(&state.workspace) {
            Ok(Some(p)) => println!("[init] wrote a starter {}", p.display()),
            Ok(None) => println!("[init] AGENTS.md already exists - left untouched"),
            Err(e) => {
                eprintln!("[init] failed: {e:#}");
                std::process::exit(1);
            }
        },
        Cmd::Model(arg) => model(state, arg).await,
        Cmd::Export(arg) => {
            // New (S-16): inside the workspace only, no silent overwrite.
            match state.export_transcript(sid, arg.as_deref()) {
                Ok(path) => println!("[export] wrote transcript to {}", path.display()),
                Err(e) => {
                    eprintln!("[export] failed: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Review => {
            state.kick_review(sid.to_string()).await;
            return Outcome::RunStarted; // event loop drives it
        }
        Cmd::Debt => println!("{}", state.debt_report(sid)),
        Cmd::Ponytail(level) => {
            // P-05: a level lives in one session; set agent.ponytail for headless.
            println!("[ponytail] level: {}", state.ponytail_level(sid).as_str());
            if level.is_some() {
                println!(
                    "[ponytail] not changed: a level lasts one session, and a headless run ends \
                     here. Use the TUI, or turn the rules on/off with `ponytail` under [agent] \
                     in the config."
                );
            }
        }
        Cmd::Deep { attempts, task } => {
            state.kick_deep(sid.to_string(), task, attempts).await;
            return Outcome::RunStarted;
        }
        Cmd::Mission { kind, goal } => {
            // Same expansion as the TUI: the brief is what runs.
            let (effort, goal) = opencode_rust::mission::split_effort(&goal);
            let mission = opencode_rust::mission::Mission {
                kind: kind.unwrap_or(opencode_rust::mission::MissionKind::Custom),
                effort,
                goal,
            };
            shared::start_mission(state, sid.to_string(), mission, false).await;
            return Outcome::RunStarted;
        }
        Cmd::MissionCtl(what) => match what.as_str() {
            "resume" => {
                state.kick_mission(sid.to_string(), None).await;
                return Outcome::RunStarted;
            }
            "status" => println!("{}", server::mission_report(state, sid)),
            _ => println!("a headless run has no mission running to stop"),
        },
        Cmd::Turbo { prompt } => {
            let on = shared::switch_turbo(state, prompt.is_some());
            println!(
                "[turbo] {}",
                if on { "ON — full access, runs continue on their own" } else { "off" }
            );
            if let Some(p) = prompt {
                // New (B-06): kick_run registers the run in `busy`.
                state.kick_run(sid.to_string(), p).await;
                return Outcome::RunStarted;
            }
        }
        Cmd::SkillInstall(src) => {
            // New (B-06): registered while it runs, so it can be aborted.
            let abort = state.claim_run(sid).await;
            let res = opencode_rust::server::install_skill_flow(state, &src, &abort).await;
            state.release_run(sid, &abort).await;
            match res {
                Ok(report) => println!("[skill] {report}"),
                Err(e) => {
                    eprintln!("[skill] install failed: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Fork | Cmd::Resume(_) | Cmd::Diff(_) => {
            eprintln!("[headless] /fork, /resume and /diff need the TUI (session switching)");
            std::process::exit(2);
        }
    }
    Outcome::Finished
}

/// `/undo`, `/undo --force`, `/redo` on the workspace.
async fn snapshot(state: &State, redo: bool, force: bool) {
    let verb = if redo { "redo" } else { "undo" };
    let res =
        shared::run_snapshot(state.workspace.clone(), state.state_dir.clone(), redo, force).await;
    match res {
        Ok(Ok(Some(m))) => println!(
            "[{verb}] restored snapshot {} ({}, {} file(s))",
            m.id, m.label, m.present.len()
        ),
        Ok(Ok(None)) => println!("[{verb}] nothing to {verb} (not a git repository?)"),
        Ok(Err(e)) => { eprintln!("[{verb}] failed: {e:#}"); std::process::exit(1); }
        Err(e) => { eprintln!("[{verb}] failed: {e}"); std::process::exit(1); }
    }
}

/// `/compact`. New (B-06): registered while it runs, so it can be aborted.
async fn compact(state: &State, sid: &str) {
    let abort = state.claim_run(sid).await;
    let res = opencode_rust::compact::compact(state, sid, &abort).await;
    state.release_run(sid, &abort).await;
    match res {
        Ok(o) => println!("[compact] {} message(s) -> 1 summary + {} kept", o.removed, o.kept),
        Err(e) => { eprintln!("[compact] failed: {e}"); std::process::exit(1); }
    }
}

/// `/model [id]`: switch to `id`, or list the active and available models.
async fn model(state: &State, arg: Option<String>) {
    match arg {
        Some(m) => {
            state.set_model(&m).await;
            println!("[model] {}", state.current_model().await);
        }
        None => {
            let cur = state.current_model().await;
            let avail = state.available_models().await;
            println!("[model] active: {cur}");
            if avail.is_empty() {
                println!("  (endpoint listed no models)");
            }
            for m in avail {
                println!("  {m}");
            }
        }
    }
}

/// `/edit <file> <what>`: print the diff; write it only with `--yes`.
async fn inline_edit(state: &State, sid: &str, path: &str, instruction: &str) {
    if state.mode_for(sid) == AgentMode::Plan {
        eprintln!("[edit] blocked in plan mode (switch with /build)");
        std::process::exit(1);
    }
    let abort = Arc::new(AtomicBool::new(false));
    let proposal =
        match opencode_rust::inline_edit::propose(state, sid, path, instruction, &abort)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[edit] failed: {e:#}");
                std::process::exit(1);
            }
        };
    for d in &proposal.diff {
        println!("{d}");
    }
    // S-15: nobody can answer the preview in headless mode, so
    // write only with the same opt-in switch as tool approvals.
    if !headless_auto_approve() {
        eprintln!(
            "[edit] preview only, nothing written. Re-run with --yes \
             (or set OPENCODE_RUST_AUTO_APPROVE=1) to apply it."
        );
        std::process::exit(2);
    }
    // Snapshot first so /undo can revert the inline edit.
    if let Err(e) = opencode_rust::gitsnap::record_before_run(
        &state.workspace,
        &state.state_dir,
        "pre-edit",
    ) {
        eprintln!("[snapshot] {e:#}");
    }
    let applied = tokio::task::spawn_blocking({
        let p = proposal.clone();
        move || opencode_rust::inline_edit::apply(&p.abs, &p.old, &p.new)
    })
    .await;
    match applied {
        Ok(Ok(())) => println!(
            "[edit] applied {}:{} ({} line(s) changed)",
            proposal.path,
            proposal.line,
            proposal.diff.iter().filter(|l| l.starts_with('+') || l.starts_with('-')).count()
        ),
        Ok(Err(e)) => {
            eprintln!("[edit] failed: {e:#}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[edit] failed: {e}");
            std::process::exit(1);
        }
    }
}

/// What a headless approval answer says after the arrow.
fn approval_verdict(approve: bool) -> &'static str {
    if approve { "auto-approved (--yes)" } else { "denied (pass --yes to allow)" }
}

/// Print the run's events until it finishes: the answer on stdout, thinking
/// and diagnostics on stderr. Exits the process on failure or timeout.
async fn stream_events(
    state: &State,
    sid: &str,
    mut rx: tokio::sync::broadcast::Receiver<server::ServerEvent>,
) -> Result<()> {
    let mut out = std::io::stdout();
    let mut thinking_open = false;
    // New (B-09): a time limit for the whole command, and a check after
    // every 2 s of silence that our run is still alive (its RunFinished may
    // have been lost if we fell behind the event bus).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let limit_env = std::env::var("OPENCODE_RUST_HEADLESS_TIMEOUT").ok();
    let limit = headless_timeout(&argv, limit_env.as_deref());
    let started = tokio::time::Instant::now();
    loop {
        if let Some(limit) = limit {
            if started.elapsed() >= limit {
                state.abort_run(sid).await;
                eprintln!(
                    "\n[error] headless run timed out after {}s (raise it with --timeout N; 0 = no limit)",
                    limit.as_secs()
                );
                opencode_rust::tools::jobs::stop_all().await;
                std::process::exit(124);
            }
        }
        let next = match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
            Ok(next) => next,
            Err(_) => {
                if !state.run_active(sid).await {
                    eprintln!(
                        "\n[error] the run ended but its result was lost; see session {} for the transcript",
                        sid
                    );
                    std::process::exit(3);
                }
                continue;
            }
        };
        match next {
            Ok(server::ServerEvent::AssistantDelta { text, .. }) => {
                if thinking_open {
                    eprintln!();
                    thinking_open = false;
                }
                print!("{text}");
                let _ = out.flush();
            }
            Ok(server::ServerEvent::ReasoningDelta { text, .. }) => {
                // Thinking goes to stderr with a marker so stdout stays clean.
                if !thinking_open {
                    eprintln!("✻ thinking");
                    thinking_open = true;
                }
                eprint!("{text}");
                let _ = std::io::stderr().flush();
            }
            Ok(server::ServerEvent::ToolStarted { tool, detail, .. }) => {
                if thinking_open {
                    eprintln!();
                    thinking_open = false;
                }
                println!("\n⏺ {tool}({detail})");
            }
            Ok(server::ServerEvent::ToolFinished { tool, ok, .. }) => {
                println!("  {} {tool}", if ok { "✓" } else { "✗" });
            }
            Ok(server::ServerEvent::ApprovalNeeded { approval_id, tool, detail, .. }) => {
                // New (S-02): nobody can answer a prompt here, so the default
                // is "deny". Opt in with --yes or OPENCODE_RUST_AUTO_APPROVE=1;
                // OPENCODE_RUST_AUTO_DENY=1 always wins.
                let approve = headless_auto_approve();
                eprintln!("\n[approval] {tool}: {detail} → {}", approval_verdict(approve));
                let _ = state.decide(&approval_id, approve, false);
            }
            Ok(server::ServerEvent::RunFinished { ok, message, .. }) => {
                println!();
                if !ok {
                    eprintln!("[error] {}", message.unwrap_or_default());
                    opencode_rust::tools::jobs::stop_all().await;
                    std::process::exit(1);
                }
                break;
            }
            Ok(server::ServerEvent::Note { text, .. }) => {
                // System notes (e.g. the auto ponytail review) frame what
                // follows — without this the review reads as a second answer.
                if thinking_open {
                    eprintln!();
                    thinking_open = false;
                }
                println!("\n[note] {text}");
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            // New (B-09): we fell behind and lost events. Answer every
            // approval we may have missed (same rule as above, see S-02);
            // the liveness check notices a lost RunFinished.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("\n[warn] {n} event(s) were dropped (output too slow)");
                let approve = headless_auto_approve();
                for ev in state.pending_approvals() {
                    if let server::ServerEvent::ApprovalNeeded { approval_id, tool, detail, .. } = ev {
                        eprintln!("[approval] {tool}: {detail} → {}", approval_verdict(approve));
                        let _ = state.decide(&approval_id, approve, false);
                    }
                }
            }
        }
    }
    Ok(())
}

/// New (B-09): how long a whole headless run may take. `--timeout N`
/// (seconds) or OPENCODE_RUST_HEADLESS_TIMEOUT; default 2 hours; 0 = no
/// limit. A value that is not a number falls back to the default.
pub(crate) fn headless_timeout(args: &[String], env: Option<&str>) -> Option<std::time::Duration> {
    let from_args = args
        .iter()
        .position(|a| a == "--timeout")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str);
    let secs = from_args
        .or(env)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(7200);
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod headless_policy_tests {
    use super::auto_approve_policy;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn headless_denies_by_default() {
        assert!(!auto_approve_policy(&args(&["--headless", "do it"]), None, None));
        assert!(!auto_approve_policy(&args(&["--headless"]), Some("0"), None));
    }

    #[test]
    fn yes_flag_or_env_opts_in() {
        assert!(auto_approve_policy(&args(&["--headless", "--yes", "do it"]), None, None));
        assert!(auto_approve_policy(&args(&["--headless", "do it"]), Some("1"), None));
        // After `--` it is prompt text, not the flag.
        assert!(!auto_approve_policy(&args(&["--headless", "--", "--yes"]), None, None));
    }

    #[test]
    fn auto_deny_always_wins() {
        assert!(!auto_approve_policy(&args(&["--yes"]), Some("1"), Some("1")));
    }
}

#[cfg(test)]
mod headless_timeout_tests {
    use super::headless_timeout;
    use std::time::Duration;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn timeout_default_flag_env_and_zero() {
        assert_eq!(headless_timeout(&args(&["--headless"]), None), Some(Duration::from_secs(7200)));
        assert_eq!(
            headless_timeout(&args(&["--timeout", "30"]), Some("99")),
            Some(Duration::from_secs(30)),
            "the flag beats the environment"
        );
        assert_eq!(headless_timeout(&args(&[]), Some("45")), Some(Duration::from_secs(45)));
        assert_eq!(headless_timeout(&args(&["--timeout", "0"]), None), None, "0 = no limit");
    }
}
