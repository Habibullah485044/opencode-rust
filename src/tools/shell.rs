//! The shell behind the `bash` tool: which shell runs commands, keeping
//! our secrets out of it, and running one command to its end.

use super::process::{adopt_into_job, kill_tree, wait_for_abort};
use super::truncate_middle;
use anyhow::{anyhow, Context, Result};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Environment variables that hold opencode-rust's own secrets (S-05).
/// Child processes (bash, MCP servers, git) never get them.
pub const SECRET_ENV_VARS: [&str; 2] = ["OPENCODE_RUST_API_KEY", "OPENCODE_RUST_API_TOKEN"];

/// Remove our secrets from a child process's environment (S-05).
pub fn scrub_secrets(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    for key in SECRET_ENV_VARS {
        cmd.env_remove(key);
    }
    cmd
}

/// Git Bash on Windows, when it is installed: models write POSIX shell
/// (`ls`, `rm -f`, `;`, `$?`, quotes), and `cmd.exe` reads all of that
/// differently. `OPENCODE_RUST_SHELL=cmd` keeps `cmd.exe`; a path to a
/// `bash.exe` picks that one.
#[cfg(windows)]
fn git_bash() -> Option<&'static Path> {
    static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let mut tries = Vec::new();
            match std::env::var("OPENCODE_RUST_SHELL") {
                Ok(v) if v.eq_ignore_ascii_case("cmd") => return None,
                Ok(v) if !v.trim().is_empty() => tries.push(PathBuf::from(v.trim())),
                _ => {}
            }
            for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
                if let Ok(dir) = std::env::var(var) {
                    tries.push(Path::new(&dir).join(r"Git\bin\bash.exe"));
                }
            }
            if let Ok(dir) = std::env::var("LOCALAPPDATA") {
                tries.push(Path::new(&dir).join(r"Programs\Git\bin\bash.exe"));
            }
            // `git` on PATH lives in `<Git>\cmd` or `<Git>\bin`. Never the
            // `bash.exe` in System32: that is WSL, a different file system.
            for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
                if dir.join("git.exe").is_file() {
                    if let Some(git) = dir.parent() {
                        tries.push(git.join(r"bin\bash.exe"));
                    }
                }
            }
            tries.into_iter().find(|p| p.is_file())
        })
        .as_deref()
}

/// The shell that runs `bash` tool commands on this machine.
pub fn shell_kind() -> crate::permissions::Shell {
    #[cfg(windows)]
    if git_bash().is_none() {
        return crate::permissions::Shell::Cmd;
    }
    crate::permissions::Shell::Posix
}

/// Shell name for the `bash` tool description, so the model writes the
/// right syntax.
pub(super) fn shell_label() -> &'static str {
    match shell_kind() {
        crate::permissions::Shell::Posix if cfg!(windows) => {
            "bash (Git Bash: POSIX syntax and tools)"
        }
        crate::permissions::Shell::Posix => "sh",
        crate::permissions::Shell::Cmd => {
            "Windows cmd.exe (cmd syntax: dir, type, del, `&&` between commands — not ls/rm/`;`)"
        }
    }
}

/// A command that runs `line` in the shell `shell_kind` names.
pub fn shell_command(line: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        if let Some(bash) = git_bash() {
            let mut c = tokio::process::Command::new(bash);
            c.arg("-c").arg(line);
            return c;
        }
        // `cmd /C` does its own parsing: pass the line as typed. `arg` would
        // add `\"` escapes that cmd does not understand.
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/S").arg("/C").raw_arg(format!("\"{line}\""));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(line);
        c
    }
}

pub(super) async fn bash(
    command: &str,
    workspace: &Path,
    timeout: Duration,
    abort: &std::sync::atomic::AtomicBool,
) -> Result<String> {
    let mut child = shell_command(command);
    child
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // S-05: the shell never sees our provider key or API token.
    scrub_secrets(&mut child);
    // New (B-06): on Linux/macOS the shell leads its own process group, so
    // abort and timeout can stop everything it started, not just the shell.
    #[cfg(unix)]
    child.process_group(0);
    let mut child = child.spawn().context("spawning shell")?;
    adopt_into_job(&child);
    let pid = child.id();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    // Read both pipes while waiting for the shell. Once the shell has exited,
    // wait only a moment for the pipes: `server &` leaves a child holding
    // them open, and the command would otherwise "time out" after it is done.
    let work = async move {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut still_open = false;
        let status = {
            let reading = async {
                tokio::join!(read_pipe_into(&mut stdout, &mut out), read_pipe_into(&mut stderr, &mut err));
            };
            tokio::pin!(reading);
            let mut read_all = false;
            let status = loop {
                tokio::select! {
                    _ = &mut reading, if !read_all => read_all = true,
                    // `Child::wait` is cancel safe.
                    s = child.wait() => break s,
                }
            };
            if !read_all && tokio::time::timeout(PIPE_GRACE, &mut reading).await.is_err() {
                still_open = true;
            }
            status
        };
        (status, out, err, still_open)
    };
    tokio::pin!(work);
    let stopped = tokio::select! {
        (status, out, err, still_open) = &mut work => {
            let status = status.map_err(|e| anyhow!("command failed to run: {e}"))?;
            let mut s = format!("exit code: {}\n", status.code().unwrap_or(-1));
            if still_open {
                s.push_str(
                    "(something it started in the background still runs; its later output is not \
                     shown. Start servers with bash background:true to read their output.)\n",
                );
            }
            if !out.is_empty() {
                s.push_str("--- stdout ---\n");
                s.push_str(&truncate_middle(&String::from_utf8_lossy(&out), 3_000, 9_000));
            }
            if !err.is_empty() {
                s.push_str("\n--- stderr ---\n");
                s.push_str(&truncate_middle(&String::from_utf8_lossy(&err), 2_000, 6_000));
            }
            return Ok(s);
        }
        _ = tokio::time::sleep(timeout) => format!("command timed out after {}s", timeout.as_secs()),
        _ = wait_for_abort(abort) => "command aborted".to_string(),
    };
    // Abort or timeout: kill the whole process tree while the shell still
    // exists (`work` keeps it alive until this function returns).
    kill_tree(pid).await;
    Err(anyhow!("{stopped} (process tree killed)"))
}

/// How long `bash` still reads the pipes after the shell itself exited.
const PIPE_GRACE: Duration = Duration::from_millis(500);

/// Read a child's pipe to the end into `buf`, chunk by chunk, so what was
/// read is kept when the read is given up (see `PIPE_GRACE`).
async fn read_pipe_into<R: tokio::io::AsyncRead + Unpin>(pipe: &mut Option<R>, buf: &mut Vec<u8>) {
    let Some(p) = pipe.as_mut() else { return };
    let mut chunk = [0u8; 8192];
    while let Ok(n) = tokio::io::AsyncReadExt::read(p, &mut chunk).await {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
