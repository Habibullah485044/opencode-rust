use anyhow::{Context, Result};
use bin_cli::{headless::headless_run, tui_run::tui_run};
use opencode_rust::{config::Config, server};
use std::io::Write;
use std::time::Instant;

mod bin_cli;

fn main() -> Result<()> {
    install_crash_log();
    let res = run();
    if let Err(e) = &res {
        // `main -> Result` already prints `Error: ...`; this line only makes
        // sure the reason survives when the window was started without a
        // console (double-printing it made every failure look like two bugs).
        write_crash_log(&format!("fatal: {e:#}"));
        // The desktop app started from a shortcut or Explorer has no console
        // to print to: without a dialog it just "does not open".
        #[cfg(feature = "gui")]
        if std::env::args().any(|a| a == "--gui") {
            let msg = format!("{e:#}");
            // The window itself already showed its own dialogs (another
            // window open, WebView2 missing).
            if !msg.contains("already open") && !msg.contains("WebView2") {
                let hint = if msg.contains("no provider configured") {
                    "\n\nCreate the file %USERPROFILE%\\.config\\opencode-rust\\config.toml with a [provider] \
                     section (base_url, model, api_key) — see config.example.toml — so the app works \
                     however it is started."
                } else {
                    ""
                };
                opencode_rust::gui::show_fatal_dialog("Habi could not start", &format!("{msg}{hint}"));
            }
        }
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

    // A bad option value stops here, before the config is even read: it
    // used to be dropped silently (`--port abc` ran on a random port).
    let overrides = cli_overrides(&args).unwrap_or_else(|msg| usage_error(&msg));
    // Positional prompt: every word that is not an option or an option's
    // value, joined with spaces. New (B-07): see `positional_prompt`.
    let prompt: Option<String> = positional_prompt(&args).unwrap_or_else(|msg| usage_error(&msg));

    let workspace = std::env::current_dir().context("determining cwd")?;
    let mut cfg = Config::load(&workspace)?;

    // --headless: run the agent once from CLI args, no TUI.
    let headless = args.iter().any(|a| a == "--headless");
    // --gui: the desktop app (wry/WebView2 window) instead of the terminal UI.
    let gui = args.iter().any(|a| a == "--gui");
    // --port N / --model / --base-url quick overrides.
    if let Some(p) = overrides.port {
        cfg.server.port = p;
    }
    if let Some(m) = overrides.model {
        cfg.provider.model = m;
    }
    if let Some(u) = overrides.base_url {
        cfg.provider.base_url = u;
    }

    // The desktop app without a provider opens its Settings page instead of
    // failing: started from a shortcut there is nobody to read an error.
    if gui && cfg.effective_base_url().is_err() {
        cfg.provider.base_url = opencode_rust::config::SETUP_PLACEHOLDER_URL.to_string();
    }

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
        // The window the model reports drives fitting the chat to it and
        // compacting it, and whether the server keeps `system` messages decides
        // where the prompt is packed. A headless run has no heartbeat to learn
        // either and its FIRST request already needs both, so ask before it
        // starts; an interactive run asks in the background (a duplicate of the
        // heartbeat's first check otherwise).
        if headless {
            state.run_startup_probes().await;
        } else {
            state.start_context_probe();
        }
        let boot_ms = t0.elapsed().as_millis();
        // New (S-01): say where the HTTP API token is (never the token).
        eprintln!("[api] {}", server::api_token_note(&state.state_dir));

        let result = if headless {
            headless_run(state, addr, prompt, boot_ms).await
        } else if gui {
            #[cfg(feature = "gui")]
            {
                opencode_rust::gui::run(state, addr, prompt, boot_ms).await
            }
            #[cfg(not(feature = "gui"))]
            {
                Err(anyhow::anyhow!("this build has no GUI support; rebuild with: cargo build --features gui"))
            }
        } else {
            tui_run(state, addr, prompt, boot_ms).await
        };
        opencode_rust::tools::jobs::stop_all().await;
        result
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

/// A command-line mistake: say what and exit with code 2.
fn usage_error(msg: &str) -> ! {
    eprintln!("{msg}\nRun `opencode-rust --help` for the list of options.");
    std::process::exit(2);
}

/// Config overrides from the command line.
#[derive(Debug, Default, PartialEq)]
struct CliOverrides {
    port: Option<u16>,
    model: Option<String>,
    base_url: Option<String>,
}

/// T-031: `--port`, `--model`, `--base-url` (and `--timeout`, read later by
/// the headless runner), checked. Err = an option with no value or a value
/// that does not parse, e.g. "invalid value for --port: abc".
fn cli_overrides(args: &[String]) -> Result<CliOverrides, String> {
    let port = option_value(args, "--port")?
        .map(|v| v.parse::<u16>().map_err(|_| format!("invalid value for --port: {v}")))
        .transpose()?;
    if let Some(v) = option_value(args, "--timeout")? {
        v.trim().parse::<u64>().map_err(|_| format!("invalid value for --timeout: {v}"))?;
    }
    Ok(CliOverrides {
        port,
        model: option_value(args, "--model")?.map(str::to_string),
        base_url: option_value(args, "--base-url")?.map(str::to_string),
    })
}

/// The value after `flag` (options end at `--`). Err = the option is the
/// last word or is followed by another option.
fn option_value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>, String> {
    let opts = args.iter().position(|a| a == "--").map_or(args, |end| &args[..end]);
    let Some(i) = opts.iter().position(|a| a == flag) else { return Ok(None) };
    match opts.get(i + 1) {
        Some(v) if !v.starts_with("--") && !v.is_empty() => Ok(Some(v)),
        _ => Err(format!("missing value for {flag}")),
    }
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
  /export [--force] [path]  Transcript to markdown, inside the workspace
  /todos           Show this session's todo list
  /usage           Token usage and prompt-cache savings
  /init            Create a starter AGENTS.md
  /team <goal>     Team mode: plan → parallel workers → synthesis
  /plan /build     Read-only plan mode vs normal build mode
  /mission [kind] <goal>  Mission mode: plan steps, work on them alone until done
  /mission status|stop|resume  Show, stop or carry on the chat's mission
  /edit <file> <what>     Inline edit: one completion, diff preview
  /compact         Summarize old history, keep the recent tail
  /fork            Branch the current session into a child session
  /resume <id|n>   Switch to another session (id, prefix, or index)
  /diff [id|n]     Diff this session against its parent or another one
  /undo /redo      Restore the workspace to a pre-run git snapshot
  /review /debt    Senior-dev review of the diff / line-debt report
  /deep [n] <task> Try the task n times (default 3), keep the best checked one
  /parallel <task> Same as /deep 2 <task>
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

#[cfg(test)]
mod cli_args_tests {
    use super::{cli_overrides, positional_prompt, CliOverrides};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bad_option_values_are_errors() {
        let ok = cli_overrides(&args(&["--port", "4000", "--model", "m", "--base-url", "http://x/v1", "hi"])).unwrap();
        assert_eq!(
            ok,
            CliOverrides { port: Some(4000), model: Some("m".into()), base_url: Some("http://x/v1".into()) }
        );
        assert_eq!(cli_overrides(&args(&["hi"])).unwrap(), CliOverrides::default());
        let err = |a: &[&str]| cli_overrides(&args(a)).unwrap_err();
        assert_eq!(err(&["--port", "abc"]), "invalid value for --port: abc");
        assert_eq!(err(&["--port", "70000"]), "invalid value for --port: 70000");
        assert_eq!(err(&["--timeout", "soon"]), "invalid value for --timeout: soon");
        assert_eq!(err(&["--model"]), "missing value for --model");
        assert_eq!(err(&["--base-url", "--headless"]), "missing value for --base-url");
        assert_eq!(err(&["--port", "--", "hi"]), "missing value for --port");
        // After `--` it is prompt text, not an option.
        assert_eq!(cli_overrides(&args(&["--", "--port", "abc"])).unwrap(), CliOverrides::default());
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
