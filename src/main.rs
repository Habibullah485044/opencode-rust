use anyhow::{Context, Result};
use opencode_rust::{agents::AgentMode, config::Config, commands::Command as Cmd, server, tui};
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

fn main() -> Result<()> {
    install_crash_log();
    let res = run();
    if let Err(e) = &res {
        // `main -> Result` already prints `Error: ...`; this line only makes
        // sure the reason survives when the window was started without a
        // console (double-printing it made every failure look like two bugs).
        write_crash_log(&format!("fatal: {e:#}"));
    }
    res
}

fn crash_log_path() -> std::path::PathBuf {
    opencode_rust::config::default_state_dir().join("crash.log")
}

/// Append one line to `crash.log` (best effort — never panic while reporting).
fn write_crash_log(msg: &str) {
    let path = crash_log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{stamp}] {msg}");
    }
}

/// Panics vanish when the window was launched without a console; leave a file.
fn install_crash_log() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("{info}");
        write_crash_log(&format!("panic: {info}"));
    }));
}

fn run() -> Result<()> {
    let t0 = Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("opencode-rust {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    // M4: `preview <url>` — open the embedded preview browser and block.
    // Handled before the core server/TUI so it stays a pure window process.
    if args.first().map(String::as_str) == Some("preview") {
        let url = args
            .get(1)
            .context("preview requires a URL: opencode-rust preview http://127.0.0.1:5173")?
            .clone();
        let title = args
            .iter()
            .position(|a| a == "--title")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "preview".to_string());
        let exit_after = args
            .iter()
            .position(|a| a == "--exit-after-ms")
            .and_then(|i| args.get(i + 1))
            .and_then(|s| s.parse::<u64>().ok())
            .map(std::time::Duration::from_millis);
        #[cfg(feature = "gui")]
        {
            return opencode_rust::preview::open_preview(&url, &title, exit_after);
        }
        #[cfg(not(feature = "gui"))]
        {
            let _ = (url, title, exit_after);
            anyhow::bail!("this build has no GUI support; rebuild with: cargo build --features gui");
        }
    }

    let workspace = std::env::current_dir().context("determining cwd")?;
    let mut cfg = Config::load(&workspace)?;

    // --headless: run the agent once from CLI args, no TUI.
    let headless = args.iter().any(|a| a == "--headless");
    // --gui: the desktop app (egui shell) instead of the terminal UI.
    let gui = args.iter().any(|a| a == "--gui");
    // --port N overrides config port.
    if let Some(i) = args.iter().position(|a| a == "--port") {
        if let Some(p) = args.get(i + 1).and_then(|s| s.parse::<u16>().ok()) {
            cfg.server.port = p;
        }
    }
    // --model / --base-url quick overrides.
    if let Some(i) = args.iter().position(|a| a == "--model") {
        if let Some(m) = args.get(i + 1) {
            cfg.provider.model = m.clone();
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--base-url") {
        if let Some(u) = args.get(i + 1) {
            cfg.provider.base_url = u.clone();
        }
    }
    // Positional prompt: every word that is not an option or an option's
    // value, joined with spaces. New (B-07): see `positional_prompt`.
    let prompt: Option<String> = match positional_prompt(&args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}\nRun `opencode-rust --help` for the list of options.");
            std::process::exit(2);
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    rt.block_on(async move {
        // Start the core server (loopback + ephemeral or configured port).
        let port = cfg.server.port;
        let state = server::build_state(cfg, workspace.clone()).await?;
        let addr = server::serve(state.clone(), port).await?;
        // M5: endpoint health + tunnel keepalive. New (B-08): only the TUI
        // and the GUI show it; a headless run does not need it.
        if !headless {
            state.start_heartbeat();
        }
        let boot_ms = t0.elapsed().as_millis();
        // New (S-01): say where the HTTP API token is (never the token).
        eprintln!("[api] {}", server::api_token_note(&state.state_dir));

        if headless {
            return headless_run(state, addr, prompt, boot_ms).await;
        }
        if gui {
            #[cfg(feature = "gui")]
            {
                return opencode_rust::gui::run(state, addr, prompt, boot_ms).await;
            }
            #[cfg(not(feature = "gui"))]
            {
                anyhow::bail!("this build has no GUI support; rebuild with: cargo build --features gui");
            }
        }
        tui_run(state, addr, prompt, boot_ms).await
    })
}

/// New (B-07): options that take a value. Their value is never part of the
/// prompt. (`--timeout` is used by task B-09.)
const VALUE_FLAGS: &[&str] = &["--port", "--model", "--base-url", "--timeout"];
/// New (B-07): options without a value. (`--yes` is used by task S-02.)
const SWITCH_FLAGS: &[&str] = &["--headless", "--gui", "--yes", "--version", "-V", "--help", "-h"];

/// New (B-07): the prompt from the command line. Options and their values
/// are skipped, the remaining words are joined with spaces, and everything
/// after `--` is prompt text (so a prompt may start with `-`).
/// Err = an option this program does not know.
fn positional_prompt(args: &[String]) -> Result<Option<String>, String> {
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            words.extend(args[i + 1..].iter().map(String::as_str));
            break;
        }
        if VALUE_FLAGS.contains(&a) {
            i += 2; // skip the option and its value
            continue;
        }
        if SWITCH_FLAGS.contains(&a) {
            i += 1;
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            return Err(format!("unknown option: {a}"));
        }
        words.push(a);
        i += 1;
    }
    let prompt = words.join(" ");
    Ok(if prompt.trim().is_empty() { None } else { Some(prompt) })
}

/// `/undo` / `/redo`: git snapshot restore off the async runtime (subprocess).
async fn snapshot_command(st: std::sync::Arc<server::AppState>, sid: String, redo: bool) {
    let ws = st.workspace.clone();
    let sd = st.state_dir.clone();
    let res = tokio::task::spawn_blocking(move || {
        if redo {
            opencode_rust::gitsnap::redo(&ws, &sd)
        } else {
            opencode_rust::gitsnap::undo(&ws, &sd)
        }
    })
    .await;
    let verb = if redo { "redo" } else { "undo" };
    let text = match res {
        Ok(Ok(Some(m))) => format!(
            "[{verb}] restored snapshot {} ({}, {} file(s))\n",
            m.id,
            m.label,
            m.present.len()
        ),
        Ok(Ok(None)) => {
            format!("[{verb}] nothing to {verb} - no snapshot recorded (not a git repository?)\n")
        }
        Ok(Err(e)) => format!("[{verb}] failed: {e:#}\n"),
        Err(e) => format!("[{verb}] failed: {e}\n"),
    };
    let _ = st.events.send(server::ServerEvent::AssistantDelta {
        session_id: sid,
        text,
    });
}

fn print_help() {
    println!(
        "opencode-rust {} — lightweight terminal AI coding agent

USAGE:
  opencode-rust [OPTIONS] [PROMPT...]
  (prompt words are joined; put -- before a prompt that starts with -)

OPTIONS:
  --headless       Run one prompt without the TUI (prints result, exits)
  --yes            Headless only: approve tool calls without asking (default: deny)
  --gui            Desktop app window (WebView2; needs the `gui` feature)
  --port N         Server port (default: ephemeral, printed at start)
  --timeout N      Headless only: give up after N seconds, exit code 124 (default 7200, 0 = none)
  --base-url URL   OpenAI-compatible provider base URL (…/v1)
  --model NAME     Model id at the provider
  -V, --version    Print version
  -h, --help       This help

CONFIG:
  ~/.config/opencode-rust/config.toml  (or $OPENCODE_RUST_CONFIG)
  Env: OPENCODE_RUST_BASE_URL, OPENCODE_RUST_MODEL, OPENCODE_RUST_API_KEY
  HTTP API: send Authorization: Bearer <token>. Token = $OPENCODE_RUST_API_TOKEN,
            else a new random one per start (its file is printed at start).
  Headless approvals: denied unless --yes or OPENCODE_RUST_AUTO_APPROVE=1;
       OPENCODE_RUST_AUTO_DENY=1 always forces deny

COMMANDS (in TUI, most also headless — type / for the palette, F1 for the reference):
  Enter            Send message                     Tab   Complete a command
  /help  (F1)      Full command + key reference
  /sessions        Browse sessions and switch to one
  /new             Start a fresh session
  /clear           Clear the transcript view (history kept)
  /model [id]      Show the active model, or switch to <id>
  /export [path]   Write the transcript to markdown
  /todos           Show this session's todo list
  /init            Create a starter AGENTS.md
  /team <goal>     Team mode: plan → parallel workers → synthesis
  /plan /build     Read-only plan mode vs normal build mode
  /mission [kind] <goal>  Run a briefing at an effort level
  /edit <file> <what>     Inline edit: one completion, diff preview
  /compact         Summarize old history, keep the recent tail
  /fork            Branch the current session into a child session
  /resume <id|n>   Switch to another session (id, prefix, or index)
  /diff [id|n]     Diff this session against its parent or another one
  /undo /redo      Restore the workspace to a pre-run git snapshot
  /review /debt    Senior-dev review of the diff / line-debt report
  /parallel <task> Run one task in two isolated worktrees
  /turbo [prompt]  Full access, no approvals, keeps working
  /skills          List loaded skills
  /skill install   Vet, install and bake in a skill
  /quit            Exit

M4:
  preview <url> [--title T] [--exit-after-ms N]
                     Embedded preview browser (tao + wry / WebView2)",
        env!("CARGO_PKG_VERSION")
    );
}

/// New (S-02): may headless mode say "yes" to approval prompts by itself?
/// Default: no. `--yes` (before any `--`) or OPENCODE_RUST_AUTO_APPROVE=1
/// turns it on; OPENCODE_RUST_AUTO_DENY=1 always wins and forces "no".
fn auto_approve_policy(args: &[String], approve_env: Option<&str>, deny_env: Option<&str>) -> bool {
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

async fn headless_run(
    state: std::sync::Arc<server::AppState>,
    addr: std::net::SocketAddr,
    prompt: Option<String>,
    boot_ms: u128,
) -> Result<()> {
    println!("[boot] core ready in {boot_ms}ms at http://{addr}");
    let prompt = prompt.context("--headless requires a prompt argument")?;
    let session = state.db.create_session("cli", None)?;
    println!("[session] {}", session.id);

    // Subscribe before starting so we see every event.
    let mut rx = state.events.subscribe();
    // M2: builtin commands that make sense headless.
    let mut team_spawned = false;
    if let Some(cmd) = opencode_rust::commands::classify(&prompt) {
        match cmd {
            Cmd::Plan | Cmd::Build => {
                let m = if matches!(cmd, Cmd::Plan) { AgentMode::Plan } else { AgentMode::Build };
                state.set_mode(&session.id, m);
                println!("[mode] {}", m.as_str());
                return Ok(());
            }
            Cmd::Skills => {
                let skills = opencode_rust::skills::load_skills(&state.workspace);
                if skills.is_empty() {
                    println!("[skills] none found");
                } else {
                    println!("{}", opencode_rust::skills::skill_index(&skills));
                }
                return Ok(());
            }
            Cmd::Undo | Cmd::Redo => {
                let ws = state.workspace.clone();
                let sd = state.state_dir.clone();
                let redo = matches!(cmd, Cmd::Redo);
                let verb = if redo { "redo" } else { "undo" };
                let res = tokio::task::spawn_blocking(move || {
                    if redo { opencode_rust::gitsnap::redo(&ws, &sd) }
                    else { opencode_rust::gitsnap::undo(&ws, &sd) }
                })
                .await;
                match res {
                    Ok(Ok(Some(m))) => println!(
                        "[{verb}] restored snapshot {} ({}, {} file(s))",
                        m.id, m.label, m.present.len()
                    ),
                    Ok(Ok(None)) => println!("[{verb}] nothing to {verb} (not a git repository?)"),
                    Ok(Err(e)) => { eprintln!("[{verb}] failed: {e:#}"); std::process::exit(1); }
                    Err(e) => { eprintln!("[{verb}] failed: {e}"); std::process::exit(1); }
                }
                return Ok(());
            }
            Cmd::Compact => {
                // New (B-06): registered while it runs, so it can be aborted.
                let abort = state.claim_run(&session.id).await;
                let res = opencode_rust::compact::compact(&state, &session.id, &abort).await;
                state.release_run(&session.id, &abort).await;
                match res {
                    Ok(o) => println!("[compact] {} message(s) -> 1 summary + {} kept", o.removed, o.kept),
                    Err(e) => { eprintln!("[compact] failed: {e}"); std::process::exit(1); }
                }
                return Ok(());
            }
            Cmd::Quit => return Ok(()),
            Cmd::Edit { path, instruction } => {
                if state.mode_for(&session.id) == AgentMode::Plan {
                    eprintln!("[edit] blocked in plan mode (switch with /build)");
                    std::process::exit(1);
                }
                let abort = Arc::new(AtomicBool::new(false));
                let proposal =
                    match opencode_rust::inline_edit::propose(&state, &path, &instruction, &abort)
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
                return Ok(());
            }
            Cmd::Team(goal) => {
                // New (B-06): registered in `busy`, so it can be aborted.
                state
                    .kick_task(session.id.clone(), move |st2, sid2, abort| async move {
                        let (ok, message) =
                            match opencode_rust::team::run_team(&st2, &sid2, &goal, abort).await {
                                Ok(s) => (true, Some(s.lines().last().unwrap_or("team done").to_string())),
                                Err(e) => (false, Some(format!("team failed: {e}"))),
                            };
                        let _ = st2.events.send(server::ServerEvent::RunFinished {
                            session_id: sid2,
                            ok,
                            message,
                        });
                    })
                    .await;
                team_spawned = true;
            }
            // M6: discoverability + control commands, straight to stdout.
            Cmd::Help => {
                println!("{}", opencode_rust::commands::help_text());
                return Ok(());
            }
            Cmd::Sessions => {
                let list = state.session_list();
                if list.is_empty() {
                    println!("[sessions] none yet");
                }
                for s in list {
                    let fork = if s.parent_id.is_some() { "↳" } else { " " };
                    println!("{fork} {}  {}", s.id, s.title);
                }
                return Ok(());
            }
            Cmd::Todos => {
                println!("{}", state.todos_report(&session.id));
                return Ok(());
            }
            Cmd::Clear | Cmd::New => {
                println!("[{cmd_kind}] headless runs are one session per invocation", cmd_kind = if matches!(cmd, Cmd::Clear) { "clear" } else { "new" });
                return Ok(());
            }
            Cmd::Init => {
                match opencode_rust::skills::init_agents_md(&state.workspace) {
                    Ok(Some(p)) => println!("[init] wrote a starter {}", p.display()),
                    Ok(None) => println!("[init] AGENTS.md already exists - left untouched"),
                    Err(e) => {
                        eprintln!("[init] failed: {e:#}");
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }
            Cmd::Model(arg) => {
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
                return Ok(());
            }
            Cmd::Export(arg) => {
                let path = match arg {
                    Some(p) => {
                        let p = std::path::PathBuf::from(p);
                        if p.is_absolute() { p } else { state.workspace.join(p) }
                    }
                    None => state
                        .workspace
                        .join(format!("opencode-rust-transcript-{}.md", session.id)),
                };
                let md = state.transcript_markdown(&session.id);
                match std::fs::write(&path, md) {
                    Ok(()) => println!("[export] wrote transcript to {}", path.display()),
                    Err(e) => {
                        eprintln!("[export] failed: {e}");
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }
            Cmd::Review => {
                state.kick_review(session.id.clone()).await;
                team_spawned = true; // event loop drives it
            }
            Cmd::Debt => {
                println!("{}", state.debt_report(&session.id));
                return Ok(());
            }
            Cmd::Parallel(task) => {
                state.kick_parallel(session.id.clone(), task).await;
                team_spawned = true;
            }
            Cmd::Mission { kind, goal } => {
                // Same expansion as the TUI: the brief is what runs.
                let mission = opencode_rust::mission::Mission {
                    kind: kind.unwrap_or(opencode_rust::mission::MissionKind::Custom),
                    effort: opencode_rust::mission::Effort::default(),
                    goal,
                };
                // New (B-06): kick_run registers the run in `busy`.
                state.kick_run(session.id.clone(), mission.briefing()).await;
                team_spawned = true;
            }
            Cmd::Turbo { prompt } => {
                let on = match &prompt {
                    Some(_) => {
                        state.set_perm_mode(server::PermissionMode::Turbo);
                        true
                    }
                    None => state.toggle_turbo(),
                };
                println!(
                    "[turbo] {}",
                    if on { "ON — full access, runs continue on their own" } else { "off" }
                );
                match prompt {
                    Some(p) => {
                        // New (B-06): kick_run registers the run in `busy`.
                        state.kick_run(session.id.clone(), p).await;
                        team_spawned = true;
                    }
                    None => return Ok(()),
                }
            }
            Cmd::SkillInstall(src) => {
                // New (B-06): registered while it runs, so it can be aborted.
                let abort = state.claim_run(&session.id).await;
                let res = opencode_rust::server::install_skill_flow(&state, &src, &abort).await;
                state.release_run(&session.id, &abort).await;
                match res {
                    Ok(report) => println!("[skill] {report}"),
                    Err(e) => {
                        eprintln!("[skill] install failed: {e:#}");
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }
            Cmd::Fork | Cmd::Resume(_) | Cmd::Diff(_) => {
                eprintln!("[headless] /fork, /resume and /diff need the TUI (session switching)");
                std::process::exit(2);
            }
        }
    }
    if !team_spawned {
        // New (B-06): kick_run registers the run in `busy`, so it can be
        // aborted (POST /api/sessions/{id}/abort) and replaced.
        state.kick_run(session.id.clone(), prompt).await;
    }

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
                state.abort_run(&session.id).await;
                eprintln!(
                    "\n[error] headless run timed out after {}s (raise it with --timeout N; 0 = no limit)",
                    limit.as_secs()
                );
                std::process::exit(124);
            }
        }
        let next = match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
            Ok(next) => next,
            Err(_) => {
                if !state.run_active(&session.id).await {
                    eprintln!(
                        "\n[error] the run ended but its result was lost; see session {} for the transcript",
                        session.id
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
                eprintln!(
                    "\n[approval] {tool}: {detail} → {}",
                    if approve { "auto-approved (--yes)" } else { "denied (pass --yes to allow)" }
                );
                let _ = state.decide(&approval_id, approve, false);
            }
            Ok(server::ServerEvent::RunFinished { ok, message, .. }) => {
                println!();
                if !ok {
                    eprintln!("[error] {}", message.unwrap_or_default());
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
                        eprintln!(
                            "[approval] {tool}: {detail} → {}",
                            if approve { "auto-approved (--yes)" } else { "denied (pass --yes to allow)" }
                        );
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
fn headless_timeout(args: &[String], env: Option<&str>) -> Option<std::time::Duration> {
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

async fn tui_run(
    state: std::sync::Arc<server::AppState>,
    addr: std::net::SocketAddr,
    prompt: Option<String>,
    boot_ms: u128,
) -> Result<()> {
    // Create initial session.
    let session = state.db.create_session("tui", None)?;
    let mut app = tui::App::new();
    app.session_id = Some(session.id.clone());
    app.push(
        tui::EntryKind::System,
        format!("[boot] {boot_ms}ms · core at http://{addr} · session {}", session.id),
    );

    // If a prompt was passed on the CLI, pre-fill and send. (kick_run
    // registers the run so a later prompt can abort/replace it.)
    if let Some(p) = prompt {
        app.push(tui::EntryKind::User, format!("> {p}"));
        app.busy = true;
        state.kick_run(session.id.clone(), p).await;
    }

    // Bridge: server events → TUI channel; TUI actions → HTTP calls.
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
    let (act_tx, mut act_rx) = tokio::sync::mpsc::unbounded_channel::<tui::InputAction>();

    // SSE forwarder task.
    {
        let mut rx = state.events.subscribe();
        let st_fwd = state.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if evt_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    // New (B-09): the TUI fell behind and lost events. Re-send
                    // every pending approval so no prompt is silently lost.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        for ev in st_fwd.pending_approvals() {
                            let _ = evt_tx.send(ev);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Action handler task: talks to the local core exactly like an API client.
    {
        let st = state.clone();
        let mut sid = session.id.clone();
        tokio::spawn(async move {
            while let Some(action) = act_rx.recv().await {
                match action {
                    tui::InputAction::Prompt(content) => {
                        // Registers the run: a new prompt aborts/replaces a
                        // stuck one instead of racing it in parallel.
                        st.kick_run(sid.clone(), content).await;
                    }
                    tui::InputAction::Mission(mission) => {
                        // The mission expands into the well-worded brief —
                        // that brief is what the agent runs.
                        st.kick_run(sid.clone(), mission.briefing()).await;
                    }
                    tui::InputAction::Turbo(prompt) => {
                        // Core is the authority: toggle on bare /turbo, force
                        // ON when a prompt comes with it.
                        let on = match &prompt {
                            Some(_) => {
                                st.set_perm_mode(server::PermissionMode::Turbo);
                                true
                            }
                            None => st.toggle_turbo(),
                        };
                        let _ = st.events.send(server::ServerEvent::Note {
                            session_id: sid.clone(),
                            text: if on {
                                "turbo ON — full access granted; I'll keep working on my own"
                                    .into()
                            } else {
                                "turbo off — back to asking before anything risky".into()
                            },
                        });
                        if let Some(p) = prompt {
                            st.kick_run(sid.clone(), p).await;
                        }
                    }
                    tui::InputAction::SkillInstall(src) => {
                        // New (B-06): registered in `busy`, so it can be stopped.
                        st.kick_task(sid.clone(), move |st2, sid2, abort| async move {
                            match opencode_rust::server::install_skill_flow(&st2, &src, &abort).await {
                                Ok(report) => {
                                    let _ = st2.events.send(server::ServerEvent::AssistantDelta {
                                        session_id: sid2,
                                        text: report,
                                    });
                                }
                                Err(e) => {
                                    let _ = st2.events.send(server::ServerEvent::RunFinished {
                                        session_id: sid2,
                                        ok: false,
                                        message: Some(format!("skill install failed: {e:#}")),
                                    });
                                }
                            }
                        })
                        .await;
                    }
                    tui::InputAction::Team(goal) => {
                        // New (B-06): registered, so a new prompt or an abort
                        // stops it instead of running in parallel.
                        st.kick_task(sid.clone(), move |st2, sid2, abort| async move {
                            match opencode_rust::team::run_team(&st2, &sid2, &goal, abort).await {
                                Ok(summary) => {
                                    let _ = st2.events.send(server::ServerEvent::RunFinished {
                                        session_id: sid2,
                                        ok: true,
                                        message: Some(summary.lines().last().unwrap_or("team done").to_string()),
                                    });
                                }
                                Err(e) => {
                                    let _ = st2.events.send(server::ServerEvent::RunFinished {
                                        session_id: sid2,
                                        ok: false,
                                        message: Some(format!("team failed: {e}")),
                                    });
                                }
                            }
                        })
                        .await;
                    }
                    tui::InputAction::SetMode(m) => {
                        // Core is the authority; the TUI already flipped its display.
                        st.set_mode(&sid, m);
                    }
                    tui::InputAction::Compact => {
                        // New (B-06): registered in `busy`, so it can be stopped.
                        st.kick_task(sid.clone(), |st2, sid2, abort| async move {
                            let (ok, message) =
                                match opencode_rust::compact::compact(&st2, &sid2, &abort).await {
                                    Ok(o) => (
                                        true,
                                        format!(
                                            "compacted {} message(s) -> 1 summary + {} kept",
                                            o.removed, o.kept
                                        ),
                                    ),
                                    Err(e) => (false, format!("compact failed: {e}")),
                                };
                            let _ = st2.events.send(server::ServerEvent::RunFinished {
                                session_id: sid2,
                                ok,
                                message: Some(message),
                            });
                        })
                        .await;
                    }
                    tui::InputAction::Undo => {
                        let st2 = st.clone();
                        let sid2 = sid.clone();
                        tokio::spawn(snapshot_command(st2, sid2, false));
                    }
                    tui::InputAction::Redo => {
                        let st2 = st.clone();
                        let sid2 = sid.clone();
                        tokio::spawn(snapshot_command(st2, sid2, true));
                    }
                    tui::InputAction::Fork => {
                        let forked = st
                            .db
                            .get_session(&sid)
                            .ok()
                            .flatten()
                            .and_then(|s| st.db.fork_session(&s.id, &format!("{} (fork)", s.title)).ok());
                        match forked {
                            Some(new) => {
                                let transcript = st.transcript(&new.id).unwrap_or_default();
                                let mode = st.mode_for(&new.id).as_str().to_string();
                                sid = new.id.clone();
                                let _ = st.events.send(server::ServerEvent::SessionLoaded {
                                    session_id: new.id,
                                    mode,
                                    transcript,
                                });
                            }
                            None => {
                                let _ = st.events.send(server::ServerEvent::AssistantDelta {
                                    session_id: sid.clone(),
                                    text: "[error] fork failed\n".into(),
                                });
                            }
                        }
                    }
                    tui::InputAction::Resume(arg) => {
                        let resolved = opencode_rust::sessions::resolve_session(&st.db, &arg)
                            .and_then(|id| st.transcript(&id).map(|t| (id, t)));
                        match resolved {
                            Ok((id, transcript)) => {
                                let mode = st.mode_for(&id).as_str().to_string();
                                sid = id.clone();
                                let _ = st.events.send(server::ServerEvent::SessionLoaded {
                                    session_id: id,
                                    mode,
                                    transcript,
                                });
                            }
                            Err(e) => {
                                let _ = st.events.send(server::ServerEvent::AssistantDelta {
                                    session_id: sid.clone(),
                                    text: format!("[error] resume: {e:#}\n"),
                                });
                            }
                        }
                    }
                    tui::InputAction::Diff(arg) => {
                        let target = match arg {
                            Some(a) => opencode_rust::sessions::resolve_session(&st.db, &a),
                            None => st
                                .db
                                .get_session(&sid)
                                .ok()
                                .flatten()
                                .and_then(|s| s.parent_id)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("no parent session - pass one: /diff <id|index>")
                                }),
                        };
                        let text = match target.and_then(|t| {
                            opencode_rust::sessions::diff_sessions(&st.db, &sid, &t)
                        }) {
                            Ok(d) => d,
                            Err(e) => format!("[error] diff: {e:#}"),
                        };
                        let _ = st.events.send(server::ServerEvent::AssistantDelta {
                            session_id: sid.clone(),
                            text: format!("{text}\n"),
                        });
                    }
                    tui::InputAction::Skills => {
                        let skills = opencode_rust::skills::load_skills(&st.workspace);
                        let text = if skills.is_empty() {
                            "[skills] none found in .opencode/skills or ~/.config/opencode-rust/skills\n"
                                .to_string()
                        } else {
                            format!(
                                "[skills]\n{}\n",
                                opencode_rust::skills::skill_index(&skills)
                            )
                        };
                        let _ = st.events.send(server::ServerEvent::AssistantDelta {
                            session_id: sid.clone(),
                            text,
                        });
                    }
                    tui::InputAction::Review => {
                        st.kick_review(sid.clone()).await;
                    }
                    tui::InputAction::Debt => {
                        let report = st.debt_report(&sid);
                        let _ = st.events.send(server::ServerEvent::AssistantDelta {
                            session_id: sid.clone(),
                            text: report,
                        });
                    }
                    tui::InputAction::ListSessions => {
                        let sessions = st.session_list();
                        let _ = st.events.send(server::ServerEvent::SessionList { sessions });
                    }
                    tui::InputAction::SwitchSession(id) => {
                        match st.transcript(&id) {
                            Ok(transcript) => {
                                let mode = st.mode_for(&id).as_str().to_string();
                                sid = id.clone();
                                let _ = st.events.send(server::ServerEvent::SessionLoaded {
                                    session_id: id,
                                    mode,
                                    transcript,
                                });
                            }
                            Err(e) => {
                                let _ = st.events.send(server::ServerEvent::RunFinished {
                                    session_id: sid.clone(),
                                    ok: false,
                                    message: Some(format!("session switch failed: {e:#}")),
                                });
                            }
                        }
                    }
                    tui::InputAction::NewSession => match st.db.create_session("new session", None) {
                        Ok(s) => {
                            sid = s.id.clone();
                            let _ = st.events.send(server::ServerEvent::SessionLoaded {
                                session_id: s.id,
                                mode: AgentMode::Build.as_str().to_string(),
                                transcript: Vec::new(),
                            });
                        }
                        Err(e) => {
                            let _ = st.events.send(server::ServerEvent::AssistantDelta {
                                session_id: sid.clone(),
                                text: format!("[error] new session: {e:#}\n"),
                            });
                        }
                    },
                    tui::InputAction::Model(arg) => {
                        let st2 = st.clone();
                        tokio::spawn(async move {
                            let available = match &arg {
                                Some(m) => {
                                    st2.set_model(m).await;
                                    Vec::new()
                                }
                                None => st2.available_models().await,
                            };
                            let current = st2.current_model().await;
                            let _ = st2.events.send(server::ServerEvent::ModelInfo { current, available });
                        });
                    }
                    tui::InputAction::Export(arg) => {
                        let path = match &arg {
                            Some(p) => {
                                let p = std::path::PathBuf::from(p);
                                if p.is_absolute() { p } else { st.workspace.join(p) }
                            }
                            None => st
                                .workspace
                                .join(format!("opencode-rust-transcript-{}.md", sid)),
                        };
                        let md = st.transcript_markdown(&sid);
                        let text = match std::fs::write(&path, md) {
                            Ok(()) => format!("[export] wrote transcript to {}\n", path.display()),
                            Err(e) => format!("[export] failed: {e}\n"),
                        };
                        let _ = st.events.send(server::ServerEvent::AssistantDelta {
                            session_id: sid.clone(),
                            text,
                        });
                    }
                    tui::InputAction::Todos => {
                        let text = format!("{}\n", st.todos_report(&sid));
                        let _ = st.events.send(server::ServerEvent::AssistantDelta {
                            session_id: sid.clone(),
                            text,
                        });
                    }
                    tui::InputAction::Init => {
                        let ws = st.workspace.clone();
                        let text = match opencode_rust::skills::init_agents_md(&ws) {
                            Ok(Some(p)) => format!(
                                "[init] wrote a starter {} — fill in this project's layout, \
                                 commands and conventions\n",
                                p.display()
                            ),
                            Ok(None) => "[init] AGENTS.md already exists — left untouched\n".to_string(),
                            Err(e) => format!("[init] failed: {e:#}\n"),
                        };
                        let _ = st.events.send(server::ServerEvent::AssistantDelta {
                            session_id: sid.clone(),
                            text,
                        });
                    }
                    tui::InputAction::Parallel(task) => {
                        st.kick_parallel(sid.clone(), task).await;
                    }
                    tui::InputAction::Approve { id, always } => {
                        let _ = st.decide(&id, true, always);
                    }
                    tui::InputAction::Deny(id) => {
                        let _ = st.decide(&id, false, false);
                    }
                    tui::InputAction::Edit { path, instruction } => {
                        let st2 = st.clone();
                        let sid2 = sid.clone();
                        tokio::spawn(async move {
                            if st2.mode_for(&sid2) == AgentMode::Plan {
                                let _ = st2.events.send(server::ServerEvent::RunFinished {
                                    session_id: sid2,
                                    ok: false,
                                    message: Some(
                                        "inline /edit is blocked in plan mode (switch with /build)"
                                            .into(),
                                    ),
                                });
                                return;
                            }
                            let abort = Arc::new(AtomicBool::new(false));
                            match opencode_rust::inline_edit::propose(&st2, &path, &instruction, &abort)
                                .await
                            {
                                Ok(p) => {
                                    let _ = st2.events.send(server::ServerEvent::EditPreview {
                                        session_id: sid2,
                                        path: p.path,
                                        diff: p.diff,
                                        old: p.old,
                                        new: p.new,
                                        line: p.line,
                                    });
                                }
                                Err(e) => {
                                    let _ = st2.events.send(server::ServerEvent::RunFinished {
                                        session_id: sid2,
                                        ok: false,
                                        message: Some(format!("edit failed: {e:#}")),
                                    });
                                }
                            }
                        });
                    }
                    tui::InputAction::ApplyEdit { path, old, new, line } => {
                        let st2 = st.clone();
                        let sid2 = sid.clone();
                        let ws = st2.workspace.clone();
                        let path_c = path.clone();
                        // Same contract as an agent run: snapshot before we
                        // mutate, so /undo can revert an accepted inline edit.
                        if let Err(e) = opencode_rust::gitsnap::record_before_run(
                            &st2.workspace,
                            &st2.state_dir,
                            "pre-edit",
                        ) {
                            eprintln!("[snapshot] {e:#}");
                        }
                        tokio::spawn(async move {
                            let res = tokio::task::spawn_blocking(
                                move || -> Result<std::path::PathBuf, anyhow::Error> {
                                    let abs = opencode_rust::inline_edit::resolve_in_workspace(
                                        &ws, &path_c,
                                    )?;
                                    opencode_rust::inline_edit::apply(&abs, &old, &new)?;
                                    Ok(abs)
                                },
                            )
                            .await;
                            let (ok, message) = match res {
                                Ok(Ok(abs)) => {
                                    let _ = st2.events.send(server::ServerEvent::EditApplied {
                                        session_id: sid2.clone(),
                                        path: path.clone(),
                                        line,
                                    });
                                    (
                                        true,
                                        Some(format!(
                                            "applied {path}:{line} ({})",
                                            abs.display()
                                        )),
                                    )
                                }
                                Ok(Err(e)) => (false, Some(format!("edit failed: {e:#}"))),
                                Err(e) => (false, Some(format!("edit failed: {e}"))),
                            };
                            let _ = st2.events.send(server::ServerEvent::RunFinished {
                                session_id: sid2,
                                ok,
                                message,
                            });
                        });
                    }
                }
            }
        });
    }

    // Run the TUI (blocking-ish loop inside async; polls with timeouts).
    let mut app2 = app;
    tui::run(&mut app2, evt_rx, act_tx).await?;
    let _ = addr;
    Ok(())
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
mod cli_args_tests {
    use super::positional_prompt;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn option_values_are_not_the_prompt() {
        let a = args(&["--model", "gpt-4", "--headless", "fix", "the", "bug"]);
        assert_eq!(positional_prompt(&a).unwrap(), Some("fix the bug".to_string()));
        assert_eq!(positional_prompt(&args(&["--port", "4000"])).unwrap(), None);
        assert_eq!(positional_prompt(&args(&[])).unwrap(), None);
    }

    #[test]
    fn double_dash_ends_the_options() {
        let a = args(&["--headless", "--", "--explain", "this"]);
        assert_eq!(positional_prompt(&a).unwrap(), Some("--explain this".to_string()));
    }

    #[test]
    fn unknown_options_are_reported() {
        let err = positional_prompt(&args(&["--headles", "hi"])).unwrap_err();
        assert!(err.contains("--headles"), "{err}");
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
