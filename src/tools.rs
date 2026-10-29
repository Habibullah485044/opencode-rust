use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Tool specs in OpenAI function-calling format.
pub fn tool_specs() -> Vec<crate::provider::ToolSpec> {
    use crate::provider::{ToolFnSpec, ToolSpec};
    fn spec(name: &str, description: &str, params: Value) -> ToolSpec {
        ToolSpec {
            kind: "function".into(),
            function: ToolFnSpec { name: name.into(), description: description.into(), parameters: params },
        }
    }
    vec![
        spec(
            "bash",
            &format!(
                "Run a shell command with {}. Prefer the file tools. background:true \
                 for servers (gives a job id); {{job}} reads its output, {{job, stop:true}} stops it.",
                shell_label()
            ),
            json!({"type":"object","properties":{"command":{"type":"string"},"timeout_secs":{"type":"integer"},"background":{"type":"boolean"},"job":{"type":"string"},"stop":{"type":"boolean"}}}),
        ),
        spec(
            "read",
            "Read a file's numbered lines (offset: first line; limit: 500). outline:true = its functions/types with lines.",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"outline":{"type":"boolean"}},"required":["path"]}),
        ),
        spec(
            "write",
            "Create or overwrite a file with the given content.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        ),
        spec(
            "edit",
            "Replace old_str with new_str in a file (copy old_str from the file; it must match once unless replace_all). edits: several {old_str,new_str} at once.",
            json!({"type":"object","properties":{"path":{"type":"string"},"old_str":{"type":"string"},"new_str":{"type":"string"},"replace_all":{"type":"boolean"},"edits":{"type":"array","items":{"type":"object"}}},"required":["path"]}),
        ),
        spec(
            "glob",
            "Find files by glob, e.g. **/*.rs (respects .gitignore).",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]}),
        ),
        spec(
            "grep",
            "Regex search in files (respects .gitignore); gives path:line matches.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"case_insensitive":{"type":"boolean"}},"required":["pattern"]}),
        ),
        spec(
            "webfetch",
            "Fetch a URL as text, or search the web with `query`.",
            json!({"type":"object","properties":{"url":{"type":"string"},"query":{"type":"string"}}}),
        ),
        spec(
            "list",
            "List a folder (one level).",
            json!({"type":"object","properties":{"path":{"type":"string"}}}),
        ),
        spec(
            "todo",
            "This chat's todo list for long work: add (content), complete/remove (id).",
            json!({"type":"object","properties":{"op":{"type":"string","enum":["add","list","complete","remove"]},"content":{"type":"string"},"id":{"type":"string"}},"required":["op"]}),
        ),
        spec(
            "ask_user",
            "Ask the user one question with up to 6 answers to click; ends your turn.",
            json!({"type":"object","properties":{"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}},"required":["question"]}),
        ),
        spec(
            "check_page",
            "Open a page (a file, or http://localhost:PORT/ of a running app) at phone and \
             desktop width; lists what is broken, fix it all. expect_text: names it must show. \
             look: true also shows you screenshots, to judge the design.",
            json!({"type":"object","properties":{"path":{"type":"string"},"expect_text":{"type":"array","items":{"type":"string"}},"look":{"type":"boolean"}}}),
        ),
    ]
}

/// All tool specs the model may call: filesystem/shell plus Goal B's browser.
pub fn all_specs() -> Vec<crate::provider::ToolSpec> {
    let mut specs = tool_specs();
    specs.extend(crate::browser::specs());
    specs
}

/// Execute a tool by name. `workspace` is the cwd for relative paths.
/// Permission checks happen *before* this in server.rs; this is the raw executor.
/// New (B-06): `abort` is the run's stop flag. Nothing starts once it is
/// set; `bash` then kills its whole process tree and `webfetch` gives up.
pub async fn execute(
    name: &str,
    args: &Value,
    workspace: &Path,
    abort: &std::sync::atomic::AtomicBool,
) -> Result<String> {
    if abort.load(std::sync::atomic::Ordering::SeqCst) {
        bail!("not run: the run was aborted");
    }
    match name {
        "bash" if is_job_control(name, args) => {
            let id = arg_str(args, "job")?;
            if args.get("stop").and_then(Value::as_bool).unwrap_or(false) {
                jobs::stop(id).await
            } else {
                jobs::output(id)
            }
        }
        "bash" if args.get("background").and_then(Value::as_bool).unwrap_or(false) => {
            jobs::start(arg_str(args, "command")?, workspace)
        }
        "bash" => {
            let cmd = arg_str(args, "command")?;
            let timeout = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 600);
            bash(cmd, workspace, Duration::from_secs(timeout), abort).await
        }
        "read" if args.get("outline").and_then(Value::as_bool).unwrap_or(false) => {
            read_outline(workspace, arg_str(args, "path")?)
        }
        "read" => read_file(
            workspace,
            arg_str(args, "path")?,
            args.get("offset").and_then(Value::as_i64).unwrap_or(0),
            args.get("limit").and_then(Value::as_i64).unwrap_or(500),
        ),
        "write" => write_file(workspace, arg_str(args, "path")?, arg_str(args, "content")?),
        "edit" => {
            let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
            match args.get("edits").and_then(Value::as_array).filter(|a| !a.is_empty()) {
                Some(list) => {
                    let edits: Vec<(String, String)> = list
                        .iter()
                        .map(|e| {
                            let field = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
                            Ok((
                                field("old_str").ok_or_else(|| anyhow!("each edit needs old_str"))?,
                                field("new_str").ok_or_else(|| anyhow!("each edit needs new_str"))?,
                            ))
                        })
                        .collect::<Result<_>>()?;
                    edit_many(workspace, arg_str(args, "path")?, &edits, replace_all)
                }
                None => edit_file(
                    workspace,
                    arg_str(args, "path")?,
                    arg_str(args, "old_str")?,
                    arg_str(args, "new_str")?,
                    replace_all,
                ),
            }
        }
        "glob" => glob_files(
            workspace,
            arg_str(args, "pattern")?,
            args.get("path").and_then(Value::as_str),
        ),
        "grep" => grep_files(
            workspace,
            arg_str(args, "pattern")?,
            args.get("path").and_then(Value::as_str),
            args.get("case_insensitive").and_then(Value::as_bool).unwrap_or(false),
        ),
        "webfetch" => {
            if let Some(q) = args.get("query").and_then(Value::as_str).filter(|q| !q.trim().is_empty()) {
                return tokio::select! {
                    r = web_search(q.trim()) => r,
                    _ = wait_for_abort(abort) => Err(anyhow!("aborted while searching")),
                };
            }
            let url = arg_str(args, "url")?;
            // New (B-06): stop waiting for a slow page when the run is aborted.
            tokio::select! {
                r = webfetch(url) => r,
                _ = wait_for_abort(abort) => Err(anyhow!("aborted while fetching {url}")),
            }
        }
        "list" => list_dir(workspace, args.get("path").and_then(Value::as_str).unwrap_or(".")),
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string arg `{key}`"))
}

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
fn shell_label() -> &'static str {
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

async fn bash(
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
    // Read both pipes to the end, then reap the shell (like wait_with_output).
    let work = async move {
        let (out, err) = tokio::join!(read_pipe(&mut stdout), read_pipe(&mut stderr));
        (child.wait().await, out, err)
    };
    tokio::pin!(work);
    let stopped = tokio::select! {
        (status, out, err) = &mut work => {
            let status = status.map_err(|e| anyhow!("command failed to run: {e}"))?;
            let mut s = format!("exit code: {}\n", status.code().unwrap_or(-1));
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

/// New (B-06): read a child's pipe to the end (empty when there is none).
/// `bash {job}` / `bash {job, stop}`: reading or stopping a background job
/// the agent already started (and was allowed to start). Needs no approval.
pub fn is_job_control(name: &str, args: &Value) -> bool {
    name == "bash" && args.get("job").and_then(Value::as_str).is_some() && args.get("command").is_none()
}

/// Background commands (dev servers, watchers): `bash {command, background:
/// true}` starts one and returns its id; `bash {job}` returns the output
/// since the last read and whether it still runs; `bash {job, stop: true}`
/// stops it and everything it started.
pub mod jobs {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    /// Most output kept per job (older output is dropped).
    const KEEP: usize = 1_000_000;

    struct Job {
        command: String,
        /// Started from the desktop terminal panel (not by the agent).
        by_user: bool,
        cwd: PathBuf,
        pid: Option<u32>,
        out: Arc<Mutex<Vec<u8>>>,
        /// Bytes dropped from the front of `out` so far.
        dropped: Arc<Mutex<usize>>,
        read_to: usize,
        exit: Arc<Mutex<Option<String>>>,
    }

    fn jobs() -> &'static Mutex<HashMap<String, Job>> {
        static J: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
        J.get_or_init(Default::default)
    }

    pub fn start(command: &str, workspace: &Path) -> Result<String> {
        let id = spawn(command, workspace, false)?;
        Ok(format!(
            "started {id} in the background: `{command}`. Read its output with bash {{\"job\": \"{id}\"}}; \
             stop it with bash {{\"job\": \"{id}\", \"stop\": true}}."
        ))
    }

    /// Start `command` in `workspace` and return its id. `by_user` marks a
    /// command typed in the desktop terminal panel.
    pub fn spawn(command: &str, workspace: &Path, by_user: bool) -> Result<String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut cmd = shell_command(command);
        cmd.current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        scrub_secrets(&mut cmd);
        // Its own process group, so `stop` (kill_tree) reaches what it started.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("starting `{command}`"))?;
        adopt_into_job(&child);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = if by_user { format!("term{n}") } else { format!("job{n}") };
        let out = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(Mutex::new(0usize));
        let exit = Arc::new(Mutex::new(None));
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                     child.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)]
            .into_iter()
            .flatten()
        {
            let (out, dropped) = (out.clone(), dropped.clone());
            tokio::spawn(async move {
                let mut pipe = pipe;
                let mut buf = [0u8; 4096];
                while let Ok(n) = tokio::io::AsyncReadExt::read(&mut pipe, &mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
                    o.extend_from_slice(&buf[..n]);
                    if o.len() > KEEP {
                        let cut = o.len() - KEEP;
                        o.drain(..cut);
                        *dropped.lock().unwrap_or_else(|e| e.into_inner()) += cut;
                    }
                }
            });
        }
        let pid = child.id();
        let exit_slot = exit.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            *exit_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(match status {
                Ok(s) => format!("exited with code {}", s.code().unwrap_or(-1)),
                Err(e) => format!("failed: {e}"),
            });
        });
        jobs().lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.clone(),
            Job {
                command: command.to_string(),
                by_user,
                cwd: workspace.to_path_buf(),
                pid,
                out,
                dropped,
                read_to: 0,
                exit,
            },
        );
        Ok(id)
    }

    /// One job as the terminal panel shows it.
    #[derive(Debug, Clone, PartialEq)]
    pub struct JobView {
        pub id: String,
        pub command: String,
        pub by_user: bool,
        pub cwd: PathBuf,
        /// `None` while it runs, else how it ended.
        pub exit: Option<String>,
        /// The end of its output (lossy UTF-8, whole lines).
        pub tail: String,
        /// All output bytes so far (to tell when something new came in).
        pub total: usize,
    }

    /// Every job, oldest first, with the last `max` bytes of its output.
    /// The agent's read position (`output`) is not moved.
    pub fn list(max: usize) -> Vec<JobView> {
        let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<JobView> = map
            .iter()
            .map(|(id, j)| {
                let o = j.out.lock().unwrap_or_else(|e| e.into_inner());
                let dropped = *j.dropped.lock().unwrap_or_else(|e| e.into_inner());
                let mut start = o.len().saturating_sub(max);
                // Start on a line, so the panel never shows half a line.
                if start > 0 {
                    if let Some(nl) = o[start..].iter().position(|&b| b == b'\n') {
                        start += nl + 1;
                    }
                }
                JobView {
                    id: id.clone(),
                    command: j.command.clone(),
                    by_user: j.by_user,
                    cwd: j.cwd.clone(),
                    exit: j.exit.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                    tail: String::from_utf8_lossy(&o[start..]).into_owned(),
                    total: dropped + o.len(),
                }
            })
            .collect();
        let num = |id: &str| id.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u64>().unwrap_or(0);
        v.sort_by_key(|j| num(&j.id));
        v
    }

    /// Forget the ended commands the user typed (the terminal's Clear).
    pub fn clear_user_jobs() {
        let mut map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, j| !(j.by_user && j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_some()));
    }

    pub fn output(id: &str) -> Result<String> {
        let mut map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        let job = map.get_mut(id).ok_or_else(|| anyhow!("no background job `{id}` (ids look like job1)"))?;
        let o = job.out.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = *job.dropped.lock().unwrap_or_else(|e| e.into_inner());
        let start = job.read_to.saturating_sub(dropped).min(o.len());
        let new = String::from_utf8_lossy(&o[start..]).into_owned();
        job.read_to = dropped + o.len();
        drop(o);
        let state = job.exit.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| "still running".into());
        let body = if new.trim().is_empty() { "(no new output)".to_string() } else { truncate_middle(&new, 3_000, 9_000) };
        Ok(format!("{id} `{}` — {state}\n{body}", job.command))
    }

    pub async fn stop(id: &str) -> Result<String> {
        let pid = {
            let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
            let job = map.get(id).ok_or_else(|| anyhow!("no background job `{id}`"))?;
            if job.exit.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                return Ok(format!("{id} had already ended"));
            }
            job.pid
        };
        kill_tree(pid).await;
        Ok(format!("stopped {id}"))
    }

    /// Stop every job still running, with everything it started. Called on
    /// exit: dropping a job only kills its shell, so a dev server it started
    /// (`python -m http.server`) kept running after Habi closed.
    pub async fn stop_all() {
        let pids: Vec<Option<u32>> = {
            let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
            map.values()
                .filter(|j| j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_none())
                .map(|j| j.pid)
                .collect()
        };
        for pid in pids {
            kill_tree(pid).await;
        }
    }

    /// [`stop_all`] for code that cannot wait on the runtime (the GUI's
    /// event loop runs inside it and never returns).
    pub fn stop_all_now() {
        let pids: Vec<u32> = {
            let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
            map.values()
                .filter(|j| j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_none())
                .filter_map(|j| j.pid)
                .collect()
        };
        for pid in pids {
            #[cfg(windows)]
            winjob::kill(pid);
            #[cfg(windows)]
            let mut cmd = {
                let mut c = std::process::Command::new("taskkill");
                c.args(["/T", "/F", "/PID", &pid.to_string()]);
                c
            };
            #[cfg(not(windows))]
            let mut cmd = {
                let mut c = std::process::Command::new("kill");
                c.args(["-s", "KILL", "--", &format!("-{pid}")]);
                c
            };
            let _ = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
}

async fn read_pipe<R: tokio::io::AsyncRead + Unpin>(pipe: &mut Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(p) = pipe.as_mut() {
        let _ = tokio::io::AsyncReadExt::read_to_end(p, &mut buf).await;
    }
    buf
}

/// Windows: every shell Habi starts runs in a Job Object. Its descendants
/// join the job by themselves, so killing the job ends all of them, where
/// `taskkill /T` (which follows parent ids) missed Git Bash's forked
/// children on a busy machine and a dev server outlived Habi. The jobs close
/// when Habi exits for any reason (window closed, crash), and the system
/// then ends what is left in them (`KILL_ON_JOB_CLOSE`).
#[cfg(windows)]
mod winjob {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct Job(HANDLE);
    // SAFETY: a job handle is a kernel object handle; it may be used and
    // closed from any thread.
    unsafe impl Send for Job {}

    impl Job {
        fn active(&self) -> u32 {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: `info` is the struct this information class fills.
            let ok = unsafe {
                QueryInformationJobObject(
                    Some(self.0),
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut std::ffi::c_void,
                    std::mem::size_of_val(&info) as u32,
                    None,
                )
            };
            if ok.is_ok() { info.ActiveProcesses } else { 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn jobs() -> &'static Mutex<HashMap<u32, Job>> {
        static J: OnceLock<Mutex<HashMap<u32, Job>>> = OnceLock::new();
        J.get_or_init(Default::default)
    }

    /// Put the process `pid` (just started, handle `process`) in a new job.
    pub fn adopt(pid: u32, process: std::os::windows::io::RawHandle) {
        // SAFETY: plain Win32 calls with valid arguments; the info struct is
        // the one the information class expects.
        let job = unsafe {
            let Ok(h) = CreateJobObjectW(None, windows::core::PCWSTR::null()) else { return };
            let job = Job(h);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&info) as u32,
            );
            if set.is_err() || AssignProcessToJobObject(job.0, HANDLE(process)).is_err() {
                return;
            }
            job
        };
        let mut map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        // Jobs whose processes have all ended are not needed any more.
        map.retain(|_, j| j.active() > 0);
        map.insert(pid, job);
    }

    /// End everything in the job of `pid`. False when it has none.
    pub fn kill(pid: u32) -> bool {
        let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&pid) {
            // SAFETY: a live job handle from `adopt`.
            Some(job) => unsafe { TerminateJobObject(job.0, 1).is_ok() },
            None => false,
        }
    }
}

/// Put a just-started shell in its own Job Object (Windows; see `winjob`).
fn adopt_into_job(child: &tokio::process::Child) {
    #[cfg(windows)]
    if let (Some(pid), Some(handle)) = (child.id(), child.raw_handle()) {
        winjob::adopt(pid, handle);
    }
    #[cfg(not(windows))]
    let _ = child;
}

/// New (B-06): kill a shell and everything it started. Linux/macOS: signal
/// the shell's process group (see `bash`). Windows: end its Job Object, and
/// taskkill walks the tree too (a shell started before its job existed).
async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(windows)]
    winjob::kill(pid);
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("kill");
        c.args(["-s", "KILL", "--", &format!("-{pid}")]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = cmd.status().await;
}

/// New (B-06): resolves once `abort` is set (checked every 100 ms).
async fn wait_for_abort(abort: &std::sync::atomic::AtomicBool) {
    while !abort.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Keep the first `head` and the last `tail` bytes of long command output.
/// Output is resent with every later request of the run, and what matters
/// (the error, the test summary) is usually at the end.
fn truncate_middle(s: &str, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }
    let mut a = head;
    while !s.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = s.len() - tail;
    while !s.is_char_boundary(b) {
        b += 1;
    }
    format!("{}\n… [{} bytes cut from the middle] …\n{}", &s[..a], b - a, &s[b..])
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… [truncated {} bytes]", &s[..cut], s.len() - max)
    }
}

/// `/c/Users/x` (Git Bash / MSYS form) → `C:/Users/x`. `None` for any
/// other path.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn msys_to_windows(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix('/')?;
    let mut chars = rest.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let after = chars.as_str();
    if !(after.is_empty() || after.starts_with('/')) {
        return None;
    }
    Some(format!("{}:{}", drive.to_ascii_uppercase(), if after.is_empty() { "/" } else { after }))
}

/// S-03: turn a path the model gave into an absolute path that is guaranteed
/// to be inside the workspace, or an error the model can act on.
/// 1. Relative paths are joined onto the workspace.
/// 2. `.` and `..` are applied as text first, so `src/../../x` cannot climb out.
/// 3. The part that exists on disk is resolved with `canonicalize` (follows
///    symlinks). A symlink leading outside, or a broken symlink, is refused.
/// 4. The result keeps the workspace prefix as `workspace` spells it (no
///    `\\?\` prefix on Windows).
/// Like [`confine_path`], for tools that only read (`read`, `list`): also
/// allows the user's skills folder (`~/.config/opencode-rust/skills`), so a
/// skill installed for every project can be read with its reference files.
/// Nothing outside the workspace can be written this way.
pub fn confine_read_path(workspace: &Path, raw: &str) -> Result<PathBuf> {
    match confine_path(workspace, raw) {
        // Seen live: models read an installed skill as `.opencode/skills/<name>/SKILL.md`
        // (or drop the name: `.opencode/skills/references/x.md`). When the
        // project has no such file, the user's skills folder answers.
        Ok(p) if !p.exists() => Ok(user_skill_file(raw).unwrap_or(p)),
        Ok(p) => Ok(p),
        Err(e) => {
            for skills in crate::skills::shared_skill_dirs() {
                let Ok(root) = skills.canonicalize() else { continue };
                if let Ok(p) = confine_path(&root, raw) {
                    if p.starts_with(&root) {
                        return Ok(p);
                    }
                }
            }
            Err(e)
        }
    }
}

/// `.opencode/skills/<rest>` in a shared skills folder (the user's, the ones
/// Habi ships): `<rest>` itself, or (a path without the skill's name) the
/// one skill that has `<rest>`.
fn user_skill_file(raw: &str) -> Option<PathBuf> {
    let norm = raw.replace('\\', "/");
    let rest = norm.trim_start_matches("./").strip_prefix(".opencode/skills/")?;
    let roots = crate::skills::shared_skill_dirs();
    if let Some(direct) = roots.iter().map(|r| r.join(rest)).find(|p| p.exists()) {
        return Some(direct);
    }
    let mut found = roots
        .iter()
        .filter_map(|r| std::fs::read_dir(r).ok())
        .flat_map(|d| d.flatten().map(|e| e.path().join(rest)))
        .filter(|p| p.exists());
    let one = found.next()?;
    found.next().is_none().then_some(one)
}

pub fn confine_path(workspace: &Path, raw: &str) -> Result<PathBuf> {
    let outside = || {
        anyhow!(
            "`{raw}` is outside the workspace ({}). File tools only work inside the \
             workspace: use a path relative to it, for example `src/main.rs`.",
            workspace.display()
        )
    };
    // On Windows both `/` and `\` separate folders. Use `\` everywhere so a
    // `\\?\` path cannot hide a `/` inside one path part. A Git Bash path
    // (`/c/Users/...`, what the bash tool shows) means `C:\Users\...`.
    #[cfg(windows)]
    let raw_owned = msys_to_windows(raw).unwrap_or_else(|| raw.to_string()).replace('/', "\\");
    #[cfg(windows)]
    let raw: &str = &raw_owned;
    let root = workspace
        .canonicalize()
        .with_context(|| format!("workspace {} is not accessible", workspace.display()))?;
    let base = std::path::absolute(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let given = Path::new(raw);
    let joined = if given.is_absolute() { given.to_path_buf() } else { base.join(given) };
    // Step 2: apply `.` and `..` without touching the disk.
    let mut norm = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !norm.pop() {
                    return Err(outside());
                }
            }
            other => norm.push(other.as_os_str()),
        }
    }
    // Step 3: find the longest part that exists (a symlink counts as
    // existing, even a broken one) and resolve it.
    let mut existing = norm.clone();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        match existing.file_name() {
            Some(name) => {
                missing.push(name.to_os_string());
                existing.pop();
            }
            None => break,
        }
    }
    let mut real = existing
        .canonicalize()
        .map_err(|_| anyhow!("cannot resolve `{raw}` (broken symlink or unreadable folder)"))?;
    for name in missing.iter().rev() {
        real.push(name);
    }
    // Step 4: the resolved path must still be inside the workspace.
    let inside = real.strip_prefix(&root).map_err(|_| outside())?;
    Ok(workspace.join(inside))
}

/// `read` with `outline: true` (see `repomap::outline_text`).
fn read_outline(workspace: &Path, path: &str) -> Result<String> {
    let abs = confine_read_path(workspace, path)?;
    if abs.is_dir() {
        bail!("{path} is a directory — use the list tool to see its contents");
    }
    let size = std::fs::metadata(&abs).with_context(|| format!("reading {}", abs.display()))?.len();
    if size > READ_MAX_BYTES {
        bail!("{path} is {size} bytes, too big to outline (limit {READ_MAX_BYTES}). Use grep.");
    }
    let bytes = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
    Ok(crate::repomap::outline_text(path, &String::from_utf8_lossy(&bytes), &abs))
}

fn read_file(workspace: &Path, path: &str, offset: i64, limit: i64) -> Result<String> {
    let abs = confine_read_path(workspace, path)?;
    if abs.is_dir() {
        bail!("{path} is a directory — use the list tool to see its contents");
    }
    // B-30: refuse huge files before loading them.
    let size = std::fs::metadata(&abs).with_context(|| format!("reading {}", abs.display()))?.len();
    if size > READ_MAX_BYTES {
        bail!(
            "{path} is {size} bytes, too big for the read tool (limit {READ_MAX_BYTES}). \
             Use grep to find the part you need, or bash with head/tail."
        );
    }
    let bytes = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
    // B-30: a NUL byte near the start means binary. Other text is shown even
    // when it is not valid UTF-8 (bad bytes become the � character).
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Ok(format!("(binary file: {path}, {size} bytes)"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let note = if matches!(text, std::borrow::Cow::Owned(_)) {
        "\n… [note: this file is not valid UTF-8; bad bytes are shown as �. The edit tool cannot change it.]"
    } else {
        ""
    };
    let limit = limit.clamp(1, 10_000) as usize;
    // B-30: `offset` is the first line shown, 1-based like the printed
    // numbers; 0 and 1 both mean "from the top".
    let skip = (offset.max(1) - 1) as usize;
    let lines: Vec<&str> = text.lines().collect();
    // A total cap too: 500 long lines (minified code) were one huge result
    // that pushed a whole chat past the model's context.
    let mut out = String::new();
    let mut shown = 0;
    for (i, l) in lines.iter().skip(skip).take(limit).enumerate() {
        let row = format!("{:>6}\t{}", skip + i + 1, cap_line(l));
        if !out.is_empty() && out.len() + row.len() > READ_MAX_TOTAL_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&row);
        shown += 1;
    }
    if shown < limit.min(lines.len().saturating_sub(skip)) {
        out.push_str(&format!(
            "\n… [output cut at {READ_MAX_TOTAL_CHARS} characters; continue with offset {}, or grep for what you need]",
            skip + shown + 1
        ));
    } else if skip + limit < lines.len() {
        out.push_str(&format!(
            "\n… [{} more lines; continue with offset {}]",
            lines.len() - skip - limit,
            skip + limit + 1
        ));
    }
    out.push_str(note);
    Ok(out)
}

/// Largest file the read tool loads, in bytes (B-30).
const READ_MAX_BYTES: u64 = 20_000_000;
/// Longest line the read tool shows, in characters (B-30).
const READ_MAX_LINE_CHARS: usize = 2000;
/// Most characters one read returns (~10K tokens).
const READ_MAX_TOTAL_CHARS: usize = 40_000;

/// Cut one very long line, for example in a minified file (B-30).
fn cap_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.chars().count() <= READ_MAX_LINE_CHARS {
        std::borrow::Cow::Borrowed(line)
    } else {
        let head: String = line.chars().take(READ_MAX_LINE_CHARS).collect();
        std::borrow::Cow::Owned(format!("{head}… [line cut at {READ_MAX_LINE_CHARS} characters]"))
    }
}

fn write_file(workspace: &Path, path: &str, content: &str) -> Result<String> {
    let abs = confine_path(workspace, path)?;
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // B-31: the old size from metadata (works for any file, reads nothing).
    let old_len = std::fs::metadata(&abs).ok().filter(|m| m.is_file()).map(|m| m.len());
    write_atomic(&abs, content.as_bytes()).with_context(|| format!("writing {}", abs.display()))?;
    let bytes = content.len();
    Ok(match old_len {
        Some(old) => {
            let delta = bytes as i64 - old as i64;
            format!("wrote {path} ({bytes} bytes, {delta:+} bytes vs previous)")
        }
        None => format!("created {path} ({bytes} bytes)"),
    })
}

/// Write a file so it is never left half written (B-31): write a
/// temporary file in the same folder, then rename it over the target.
/// Keeps the old permissions, refuses read-only files, and falls back to
/// a plain write when the rename is refused (Windows, file held open).
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // A keep-best fix round can put this file back if the round makes things worse.
    crate::keepbest::before_write(path);
    let old_perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    if old_perms.as_ref().map(|p| p.readonly()).unwrap_or(false) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the file is read-only"));
    }
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.ocrs-{}-{n}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    if let Some(perms) = old_perms {
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(path, data)
        }
    }
}

fn edit_file(
    workspace: &Path,
    path: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Result<String> {
    // B-01: an empty old_str "matches" between every character; with
    // replace_all it would insert new_str all over the file.
    if old_str.is_empty() {
        bail!(
            "old_str is empty: give the exact text to replace (copy it from the file). \
             To create or fully rewrite a file, use the write tool."
        );
    }
    edit_many(workspace, path, &[(old_str.to_string(), new_str.to_string())], replace_all)
}

/// Apply several edits to one file, in order, all or nothing.
fn edit_many(workspace: &Path, path: &str, edits: &[(String, String)], replace_all: bool) -> Result<String> {
    if edits.is_empty() {
        bail!("no edits given");
    }
    if edits.iter().any(|(old, _)| old.is_empty()) {
        bail!(
            "old_str is empty: give the exact text to replace (copy it from the file). \
             To create or fully rewrite a file, use the write tool."
        );
    }
    let abs = confine_path(workspace, path)?;
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))?;
    let mut new_text = text.clone();
    let mut total = 0;
    let mut loose = 0;
    for (i, (old, new)) in edits.iter().enumerate() {
        let which = if edits.len() > 1 { format!(" (edit {} of {})", i + 1, edits.len()) } else { String::new() };
        let (next, n, fuzzy) = apply_edit(&new_text, old, new, replace_all).map_err(|e| anyhow!("{e} in {path}{which}"))?;
        new_text = next;
        total += n;
        loose += usize::from(fuzzy);
    }
    write_atomic(&abs, new_text.as_bytes()).with_context(|| format!("writing {}", abs.display()))?;
    let lines_before = text.matches('\n').count();
    let lines_after = new_text.matches('\n').count();
    let note = if loose > 0 { " (matched ignoring indentation/line endings)" } else { "" };
    Ok(format!(
        "edited {path}: replaced {total} occurrence(s), line delta {:+}{note}",
        lines_after as i64 - lines_before as i64
    ))
}

/// One replacement in `text`. Exact match first; then a match that ignores
/// line endings and indentation (weak models often get those wrong), with
/// `new` re-indented by the same amount. Returns the new text, how many
/// places changed, and whether the loose match was used. On a miss the
/// error shows the closest lines, so the next try can copy them.
fn apply_edit(text: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize, bool)> {
    // CRLF files: the model writes \n.
    let crlf = text.contains("\r\n");
    let (old, new) = if crlf && !old.contains("\r\n") {
        (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
    } else {
        (old.to_string(), new.to_string())
    };
    let matches = text.matches(old.as_str()).count();
    if matches > 1 && !replace_all {
        bail!("old_str matches {matches} times; make it unique (add a nearby line) or set replace_all");
    }
    if matches >= 1 {
        let out = if replace_all { text.replace(old.as_str(), &new) } else { text.replacen(old.as_str(), &new, 1) };
        return Ok((out, matches, false));
    }
    // Loose: compare trimmed lines.
    let eol = if crlf { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let want: Vec<&str> = old.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let want: Vec<&str> = {
        // Leading and trailing blank lines carry no information.
        let start = want.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
        let end = want.iter().rposition(|l| !l.trim().is_empty()).map_or(start, |e| e + 1);
        want[start..end].to_vec()
    };
    if want.is_empty() {
        bail!("old_str not found");
    }
    let hits: Vec<usize> = (0..=lines.len().saturating_sub(want.len()))
        .filter(|&i| want.iter().enumerate().all(|(k, w)| lines[i + k].trim() == w.trim()))
        .collect();
    if hits.len() > 1 && !replace_all {
        bail!("old_str matches {} places (ignoring indentation); make it unique or set replace_all", hits.len());
    }
    if hits.is_empty() {
        return Err(anyhow!("old_str not found{}", closest_region(&lines, &want)));
    }
    let indent = |l: &str| l.len() - l.trim_start().len();
    let first = want.iter().find(|l| !l.trim().is_empty()).copied().unwrap_or("");
    let new_lines: Vec<&str> = new.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    // Replace from the last hit back, so earlier indexes stay valid.
    for &i in hits.iter().rev() {
        let file_first = lines[i..i + want.len()].iter().find(|l| !l.trim().is_empty()).copied().unwrap_or("");
        let file_pad = &file_first[..indent(file_first)];
        let delta = indent(file_first) as i64 - indent(first) as i64;
        let fixed: Vec<String> = new_lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else if delta >= 0 {
                    // Same indent character as the file uses.
                    let unit = file_pad.chars().next().unwrap_or(' ');
                    format!("{}{l}", unit.to_string().repeat(delta as usize))
                } else {
                    let cut = (-delta as usize).min(indent(l));
                    l[cut..].to_string()
                }
            })
            .collect();
        out.splice(i..i + want.len(), fixed);
    }
    Ok((out.join(eol), hits.len(), true))
}

/// " — closest match at lines N-M: ..." for a failed edit, or "" when
/// nothing is close.
fn closest_region(lines: &[&str], want: &[&str]) -> String {
    let first = want.iter().find(|l| !l.trim().is_empty()).map(|l| l.trim()).unwrap_or("");
    let mut best: Option<(usize, usize)> = None; // (score, start)
    for i in 0..lines.len() {
        let score = want
            .iter()
            .enumerate()
            .filter(|(k, w)| lines.get(i + k).is_some_and(|l| !w.trim().is_empty() && l.trim() == w.trim()))
            .count()
            + usize::from(!first.is_empty() && lines[i].contains(first));
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }
    let Some((_, start)) = best else { return String::new() };
    let end = (start + want.len().max(1)).min(lines.len());
    let shown: Vec<String> = lines[start..end].iter().take(12).enumerate().map(|(k, l)| format!("{:>5}\t{l}", start + k + 1)).collect();
    format!(
        ". Closest match at lines {}-{} (copy old_str from here):\n{}",
        start + 1,
        end,
        shown.join("\n")
    )
}

fn glob_files(workspace: &Path, pattern: &str, sub: Option<&str>) -> Result<String> {
    use ignore::WalkBuilder;
    let root = confine_path(workspace, sub.unwrap_or("."))?;
    let mut matches = Vec::new();
    // B-32: `{a,b}` alternatives become separate patterns.
    let patterns = expand_braces(pattern);
    // require_git(false): honor .gitignore even in non-git workspaces.
    // B-32: never walk into `.git`.
    for entry in WalkBuilder::new(&root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Match relative to the folder that was searched ...
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if patterns.iter().any(|p| match_glob(p, &rel_str)) {
            // ... but print relative to the workspace, like grep (B-32), so
            // the path works directly with read and edit.
            let shown = path.strip_prefix(workspace).unwrap_or(path);
            matches.push(shown.to_string_lossy().replace('\\', "/"));
            if matches.len() >= 500 {
                break;
            }
        }
    }
    if matches.is_empty() {
        return Ok(format!("no files matched `{pattern}`"));
    }
    Ok(matches.join("\n"))
}

/// Expand `{a,b}` groups into separate glob patterns (B-32). Groups do not
/// nest; a `{` without a closing `}` is kept as plain text.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else { return vec![pattern.to_string()] };
    let Some(len) = pattern[open..].find('}') else { return vec![pattern.to_string()] };
    let close = open + len;
    let (head, body, tail) = (&pattern[..open], &pattern[open + 1..close], &pattern[close + 1..]);
    body.split(',').flat_map(|alt| expand_braces(&format!("{head}{alt}{tail}"))).collect()
}

/// Glob with `*`, `?`, `**` (cross-separator) support.
fn match_glob(pattern: &str, text: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => {
                if p.len() > 1 && p[1] == b'*' {
                    // `**` — match across separators, also collapse `**/`
                    let rest = &p[2..];
                    if rest.first() == Some(&b'/') {
                        // `**/x` matches `x` or `any/x`. B-32: only restart
                        // right after a `/`, so `**/x` does not match `foox`.
                        return m(&rest[1..], t)
                            || (0..t.len()).any(|i| t[i] == b'/' && m(&rest[1..], &t[i + 1..]));
                    }
                    (0..=t.len()).any(|i| m(rest, &t[i..]))
                } else {
                    // single `*` — does not cross path separators
                    for i in 0..=t.len() {
                        if m(&p[1..], &t[i..]) {
                            return true;
                        }
                        if t.get(i) == Some(&b'/') {
                            break;
                        }
                    }
                    false
                }
            }
            Some(b'?') => !t.is_empty() && t[0] != b'/' && m(&p[1..], &t[1..]),
            Some(c) => !t.is_empty() && t[0] == *c && m(&p[1..], &t[1..]),
        }
    }
    m(pattern.as_bytes(), text.as_bytes())
}

fn grep_files(
    workspace: &Path,
    pattern: &str,
    sub: Option<&str>,
    case_insensitive: bool,
) -> Result<String> {
    use ignore::WalkBuilder;
    let re = {
        let mut b = regex::RegexBuilder::new(pattern);
        b.case_insensitive(case_insensitive);
        b.build().with_context(|| format!("invalid regex `{pattern}`"))?
    };
    let root = confine_path(workspace, sub.unwrap_or("."))?;
    let mut out: Vec<String> = Vec::new();
    // require_git(false): honor .gitignore even in non-git workspaces.
    // B-32: count files that cannot be searched, and never walk into `.git`.
    let mut skipped = 0usize;
    for entry in WalkBuilder::new(&root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Skip big files
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if meta.len() > 2_000_000 {
            skipped += 1;
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            skipped += 1; // binary or not UTF-8 (Latin-1, UTF-16, ...)
            continue;
        };
        if text.bytes().any(|b| b == 0) {
            skipped += 1;
            continue;
        }
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let rel = path.strip_prefix(workspace).unwrap_or(path);
                out.push(format!("{}:{}:{}", rel.display().to_string().replace('\\', "/"), i + 1, truncate(line.trim_end(), 300)));
                if out.len() >= 200 {
                    out.push("… [match limit 200 reached]".into());
                    return Ok(out.join("\n"));
                }
            }
        }
    }
    let note = if skipped > 0 {
        format!("\n… [{skipped} file(s) not searched: larger than 2 MB, binary, or not UTF-8]")
    } else {
        String::new()
    };
    if out.is_empty() {
        Ok(format!("no matches for `{pattern}`{note}"))
    } else {
        Ok(format!("{}{note}", out.join("\n")))
    }
}

/// S-06: true only for normal public internet addresses. Loopback, private,
/// link-local (cloud metadata), shared/CGNAT, multicast, broadcast,
/// documentation, benchmarking, reserved and unspecified addresses are not.
/// IPv6 forms that wrap an IPv4 address are judged by that IPv4 address.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d and ::a.b.c.d
            if let Some(v4) = v6.to_ipv4() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| {
                IpAddr::V4(Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8))
            };
            // 64:ff9b::/96 (NAT64) and 2002::/16 (6to4) carry an IPv4 address.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                return is_public_ip(embedded(s[6], s[7]));
            }
            if s[0] == 0x2002 {
                return is_public_ip(embedded(s[1], s[2]));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xffc0) == 0xfec0
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

/// webfetch reads at most this many bytes of a response body (S-06).
const WEBFETCH_MAX_BYTES: usize = 2_000_000;
/// webfetch follows at most this many redirects (S-06).
const WEBFETCH_MAX_REDIRECTS: usize = 5;

async fn webfetch(url: &str) -> Result<String> {
    fetch_checked(url, is_public_ip).await
}

/// S-06: GET `url` from addresses that pass `allowed` only. Checks every
/// hop, pins each request to the checked addresses, caps the body.
/// Returns the body and whether it was cut.
async fn fetch_body(url: &str, allowed: fn(std::net::IpAddr) -> bool) -> Result<(Vec<u8>, bool)> {
    let mut current = reqwest::Url::parse(url).with_context(|| format!("invalid url `{url}`"))?;
    for _ in 0..=WEBFETCH_MAX_REDIRECTS {
        if !matches!(current.scheme(), "http" | "https") {
            bail!("only http(s) urls are allowed");
        }
        let host = current
            .host_str()
            .ok_or_else(|| anyhow!("url `{current}` has no host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = current.port_or_known_default().unwrap_or(80);
        let addrs: Vec<std::net::SocketAddr> = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => vec![std::net::SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host.as_str(), port))
                .await
                .with_context(|| format!("cannot resolve {host}"))?
                .collect(),
        };
        if addrs.is_empty() {
            bail!("{host} did not resolve to any address");
        }
        if let Some(bad) = addrs.iter().find(|a| !allowed(a.ip())) {
            bail!(
                "refused: {host} points to {}, a local or private network address. \
                 webfetch only reads public internet pages.",
                bad.ip()
            );
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent("opencode-rust/0.1")
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&host, &addrs)
            .build()?;
        let mut resp = http
            .get(current.clone())
            .send()
            .await
            .with_context(|| format!("fetching {current}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let next = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("{current} returned {status} without a Location header"))?;
            current = current
                .join(next)
                .with_context(|| format!("bad redirect target `{next}`"))?;
            continue;
        }
        if !status.is_success() {
            bail!("{current} returned {status}");
        }
        let mut body: Vec<u8> = Vec::new();
        let mut cut = false;
        while let Some(chunk) = resp.chunk().await? {
            let room = WEBFETCH_MAX_BYTES - body.len();
            if chunk.len() >= room {
                body.extend_from_slice(&chunk[..room]);
                cut = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        return Ok((body, cut));
    }
    bail!("too many redirects (more than {WEBFETCH_MAX_REDIRECTS}) starting at {url}")
}

/// S-06 fetch as page text (html stripped, capped).
async fn fetch_checked(url: &str, allowed: fn(std::net::IpAddr) -> bool) -> Result<String> {
    let (body, cut) = fetch_body(url, allowed).await?;
    let mut text = truncate(&html_to_text(&String::from_utf8_lossy(&body)), 20_000);
    if cut {
        text.push_str(&format!(
            "\n… [page is larger than {WEBFETCH_MAX_BYTES} bytes; only the start was read]"
        ));
    }
    Ok(text)
}

/// Web search without an API key: DuckDuckGo's HTML results page, read
/// through the same checked fetch as webfetch. Up to 8 results.
async fn web_search(query: &str) -> Result<String> {
    let mut url = reqwest::Url::parse("https://html.duckduckgo.com/html/")?;
    url.query_pairs_mut().append_pair("q", query);
    let (body, _) = fetch_body(url.as_str(), is_public_ip).await?;
    let results = parse_search_results(&String::from_utf8_lossy(&body), 8);
    if results.is_empty() {
        return Ok(format!("no results for \"{query}\""));
    }
    Ok(results
        .iter()
        .enumerate()
        .map(|(i, (title, link, snippet))| format!("{}. {title}\n   {link}\n   {snippet}", i + 1))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// (title, url, snippet) from a DuckDuckGo HTML results page.
fn parse_search_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for block in html.split("class=\"result__a\"").skip(1) {
        if out.len() >= max {
            break;
        }
        let href = block.split("href=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
        let href = href.replace("&amp;", "&");
        // Result links go through a redirect: the target is in `uddg`.
        let link = reqwest::Url::parse(&format!("https:{}", href.trim_start_matches("https:")))
            .ok()
            .and_then(|u| u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned()))
            .unwrap_or(href);
        let title = block.split_once('>').and_then(|(_, s)| s.split("</a>").next()).unwrap_or("");
        let snippet = block
            .split("class=\"result__snippet\"")
            .nth(1)
            .and_then(|s| s.split_once('>'))
            .and_then(|(_, s)| s.split("</a>").next())
            .unwrap_or("");
        // Inline tags (<b> around the matched words) go without a space.
        let clean = |s: &str| {
            let mut plain = String::new();
            let mut in_tag = false;
            for c in s.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => plain.push(c),
                    _ => {}
                }
            }
            html_to_text(&plain).split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let (title, snippet) = (clean(title), clean(snippet));
        if !title.is_empty() && link.starts_with("http") && !out.iter().any(|(_, l, _)| *l == link) {
            out.push((title, link, snippet.chars().take(240).collect()));
        }
    }
    out
}

/// Very small HTML→text: drop script/style, collapse tags to spaces, unescape basics.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < html.len() {
        if lower[i..].starts_with("<script") || lower[i..].starts_with("<style") {
            // skip to closing tag
            let close = lower[i..].find('>').map(|p| i + p + 1).unwrap_or(html.len());
            let end_tag = lower[close..].find("</script>").or_else(|| lower[close..].find("</style>"));
            i = match end_tag {
                Some(p) => close + p + if lower[close + p..].starts_with("</script") { 9 } else { 8 },
                None => html.len(),
            };
            continue;
        }
        if bytes[i] == b'<' {
            match html[i..].find('>') {
                Some(p) => {
                    out.push(' ');
                    i += p + 1;
                    continue;
                }
                // B-43: no `>` anywhere after this point, so no more tags.
                None => {
                    out.push_str(&html[i..]);
                    break;
                }
            }
        }
        let ch_len = utf8_len(bytes[i]);
        let end = (i + ch_len).min(html.len());
        out.push_str(&html[i..end]);
        i = end;
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        // B-43: `&amp;` last, so `&amp;lt;` stays the text `&lt;`.
        .replace("&amp;", "&")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn list_dir(workspace: &Path, path: &str) -> Result<String> {
    let abs = confine_read_path(workspace, path)?;
    let rd = std::fs::read_dir(&abs).with_context(|| format!("listing {}", abs.display()))?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            dirs.push(name);
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(format!("{name} ({size} B)"));
        }
    }
    let mut lines = Vec::new();
    dirs.sort();
    files.sort();
    lines.extend(dirs.into_iter().map(|d| format!("{d}/")));
    lines.extend(files);
    if lines.is_empty() {
        return Ok("(empty directory)".into());
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile_dir::TempDir;

    /// B-06: an abort stops a long command quickly.
    #[tokio::test]
    async fn bash_abort_stops_a_long_command() {
        let dir = TempDir::new("bash-abort");
        let cmd = if cfg!(windows) { "ping -n 30 127.0.0.1" } else { "sleep 30" };
        let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let t0 = std::time::Instant::now();
        let err = bash(cmd, dir.path(), Duration::from_secs(60), &abort).await.unwrap_err();
        assert!(err.to_string().contains("aborted"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(10), "abort must not wait for the command");
    }

    /// B-06: a timeout kills the whole process tree, not only the shell.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_timeout_kills_background_children() {
        let dir = TempDir::new("bash-tree");
        let abort = std::sync::atomic::AtomicBool::new(false);
        let cmd = "(sleep 2; echo late > marker.txt) & sleep 30";
        let err = bash(cmd, dir.path(), Duration::from_secs(1), &abort).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!dir.path().join("marker.txt").exists(), "the background child must die too");
    }

    /// Tests that start background jobs run one at a time (see `stop_all`).
    static JOB_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A dev server the agent started in the background must not outlive
    /// Habi: `stop_all` (run on exit) ends it and what it started.
    #[tokio::test]
    async fn stop_all_ends_background_jobs_and_their_children() {
        if shell_kind() != crate::permissions::Shell::Posix {
            return;
        }
        // `stop_all` stops every job, also another test's: take turns.
        let _turn = JOB_TESTS.lock().await;
        let dir = TempDir::new("jobs-stop-all");
        let started = jobs::start("(sleep 8; echo late > marker.txt) & echo UP; sleep 30", dir.path()).unwrap();
        let id = started.split_whitespace().nth(1).unwrap().to_string();
        // Under a full test run the shell can take seconds to start: stop it
        // once its child exists, and always well before the child's 8 s are
        // up (a marker written before the stop proves nothing either way).
        let mut up = String::new();
        for _ in 0..60 {
            up.push_str(&jobs::output(&id).unwrap());
            if up.contains("UP") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        jobs::stop_all().await;
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert!(!dir.path().join("marker.txt").exists(), "the job's child must die too");
    }

    /// B-06: nothing starts once the run is aborted.
    #[tokio::test]
    async fn execute_refuses_after_abort() {
        let dir = TempDir::new("exec-abort");
        let abort = std::sync::atomic::AtomicBool::new(true);
        let err = execute("list", &json!({}), dir.path(), &abort).await.unwrap_err();
        assert!(err.to_string().contains("aborted"), "{err}");
    }

    // Minimal temp dir helper to avoid extra deps.
    mod tempfile_dir {
        use std::path::PathBuf;
        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "ocrs-test-{tag}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .subsec_nanos()
                ));
                std::fs::create_dir_all(&p).unwrap();
                TempDir(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn glob_double_star() {
        assert!(match_glob("**/*.rs", "src/main.rs"));
        assert!(match_glob("**/*.rs", "main.rs"));
        assert!(!match_glob("**/*.rs", "src/main.py"));
        assert!(match_glob("src/*.rs", "src/lib.rs"));
        assert!(!match_glob("src/*.rs", "src/deep/lib.rs"));
        assert!(match_glob("src/**/*.rs", "src/deep/lib.rs"));
    }

    /// Weak models get indentation and line endings wrong: the edit still
    /// lands, re-indented to fit, and a miss shows the closest lines.
    #[test]
    fn edits_forgive_indentation_and_line_endings() {
        let file = "fn main() {\r\n    if ready {\r\n        go();\r\n    }\r\n}\r\n";
        // Indented two levels too little, LF instead of CRLF.
        let (out, n, loose) = apply_edit(file, "if ready {\n    go();\n}", "if ready {\n    go();\n    done();\n}", false).unwrap();
        assert!(loose && n == 1);
        assert_eq!(out, "fn main() {\r\n    if ready {\r\n        go();\r\n        done();\r\n    }\r\n}\r\n");
        // Exact matches still win and say nothing about looseness.
        let (out, _, loose) = apply_edit("a b c", "b", "x", false).unwrap();
        assert!(!loose && out == "a x c");
        // A miss names the closest region with line numbers.
        let err = apply_edit("one\ntwo\nthree\n", "two\nfour", "x", false).unwrap_err().to_string();
        assert!(err.contains("Closest match at lines 2-3") && err.contains("    2\ttwo"), "{err}");
        // Ambiguous loose matches are refused.
        assert!(apply_edit("  x\n y\n", "x\n", "z", false).is_ok());
        assert!(apply_edit(" x\n  x\n", "x", "z", false).unwrap_err().to_string().contains("2 times"));
    }

    /// A background job keeps running; its output is read in pieces, and
    /// stop ends it.
    #[tokio::test]
    async fn background_jobs_start_report_and_stop() {
        if shell_kind() != crate::permissions::Shell::Posix {
            return;
        }
        let _turn = JOB_TESTS.lock().await;
        let dir = TempDir::new("jobs");
        let no = std::sync::atomic::AtomicBool::new(false);
        let started = execute("bash", &json!({"command": "echo ONE$((0+1)); sleep 1; echo TWO$((1+1)); sleep 30", "background": true}), dir.path(), &no)
            .await
            .unwrap();
        let id = started.split_whitespace().nth(1).unwrap().to_string();
        assert!(id.starts_with("job"), "{started}");
        assert!(is_job_control("bash", &json!({"job": id})));
        assert!(!is_job_control("bash", &json!({"job": id, "command": "rm -rf x"})), "a command always needs approval");
        // The shell may take a while to start: wait for each line.
        async fn read_until(id: &str, ws: &Path, want: &str) -> String {
            let no = std::sync::atomic::AtomicBool::new(false);
            let mut all = String::new();
            for _ in 0..100 {
                let out = execute("bash", &json!({"job": id}), ws, &no).await.unwrap();
                all.push_str(&out);
                if all.contains(want) {
                    return all;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            all
        }
        let out = read_until(&id, dir.path(), "ONE1").await;
        assert!(out.contains("still running") && out.contains("ONE1") && !out.contains("TWO2"), "{out}");
        let out = read_until(&id, dir.path(), "TWO2").await;
        assert!(out.contains("TWO2") && !out.contains("ONE1"), "only new output: {out}");
        let out = execute("bash", &json!({"job": id, "stop": true}), dir.path(), &no).await.unwrap();
        assert!(out.contains("stopped"), "{out}");
        tokio::time::sleep(Duration::from_millis(800)).await;
        let out = execute("bash", &json!({"job": id}), dir.path(), &no).await.unwrap();
        assert!(out.contains("exited"), "{out}");
        assert!(execute("bash", &json!({"job": "job999"}), dir.path(), &no).await.is_err());
    }

    #[tokio::test]
    async fn several_edits_apply_together_or_not_at_all() {
        let dir = TempDir::new("multi-edit");
        let ws = dir.path();
        write_file(ws, "m.txt", "alpha\nbeta\ngamma\n").unwrap();
        let args = json!({"path": "m.txt", "edits": [{"old_str": "alpha", "new_str": "A"}, {"old_str": "gamma", "new_str": "G"}]});
        let out = execute("edit", &args, ws, &std::sync::atomic::AtomicBool::new(false)).await.unwrap();
        assert!(out.contains("replaced 2"), "{out}");
        assert_eq!(std::fs::read_to_string(ws.join("m.txt")).unwrap(), "A\nbeta\nG\n");
        let bad = json!({"path": "m.txt", "edits": [{"old_str": "beta", "new_str": "B"}, {"old_str": "nope", "new_str": "x"}]});
        let err = execute("edit", &bad, ws, &std::sync::atomic::AtomicBool::new(false)).await.unwrap_err().to_string();
        assert!(err.contains("edit 2 of 2"), "{err}");
        assert_eq!(std::fs::read_to_string(ws.join("m.txt")).unwrap(), "A\nbeta\nG\n", "nothing written when one edit fails");
    }

    #[tokio::test]
    async fn write_read_edit_roundtrip() {
        let dir = TempDir::new("rw");
        let ws = dir.path();
        let r = write_file(ws, "a/b.txt", "hello\nworld").unwrap();
        assert!(r.contains("created"));
        let r = read_file(ws, "a/b.txt", 0, 100).unwrap();
        assert!(r.contains("hello"));
        assert!(r.contains("1\t"));
        let r = edit_file(ws, "a/b.txt", "world", "rust", false).unwrap();
        assert!(r.contains("edited"));
        let r = read_file(ws, "a/b.txt", 0, 100).unwrap();
        assert!(r.contains("rust"));
        // unique-match failure
        let _ = write_file(ws, "dup.txt", "x x").unwrap();
        assert!(edit_file(ws, "dup.txt", "x", "y", false).is_err());
        assert!(edit_file(ws, "dup.txt", "x", "y", true).unwrap().contains("replaced 2"));
    }

    #[tokio::test]
    async fn grep_respects_gitignore() {
        let dir = TempDir::new("grep");
        let ws = dir.path();
        write_file(ws, "keep.rs", "fn main() {}").unwrap();
        write_file(ws, ".gitignore", "skip/\n").unwrap();
        write_file(ws, "skip/secret.rs", "fn main() {}").unwrap();
        let r = grep_files(ws, "fn main", None, false).unwrap();
        assert!(r.contains("keep.rs"));
        assert!(!r.contains("secret.rs"));
    }

    #[tokio::test]
    async fn bash_runs() {
        let dir = TempDir::new("bash");
        let abort = std::sync::atomic::AtomicBool::new(false);
        let out = bash("echo hi", dir.path(), Duration::from_secs(10), &abort).await.unwrap();
        assert!(out.contains("hi"));
    }

    #[test]
    fn read_outline_lists_parts_with_lines() {
        let dir = TempDir::new("read-outline");
        std::fs::write(dir.path().join("app.py"), "import os\n\nclass Cart:\n    def add(self):\n        pass\n").unwrap();
        let out = read_outline(dir.path(), "app.py").unwrap();
        assert!(out.contains("     3  class Cart") && out.contains("     4  def add"), "{out}");
        assert!(read_outline(dir.path(), ".").unwrap_err().to_string().contains("list"));
    }

    /// Read-only tools may read the user's skills folder (a skill installed
    /// for every project, with its reference files), nothing else outside.
    #[test]
    fn the_user_skills_folder_is_readable_but_nothing_else_outside() {
        let ws = TempDir::new("skills-read");
        let Some(skills) = crate::skills::user_config_dir().map(|d| d.join("skills")) else { return };
        let probe = skills.join("zz-probe-skill");
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join("SKILL.md"), "---\nname: zz\n---\nhello skill\n").unwrap();
        std::fs::create_dir_all(probe.join("references")).unwrap();
        std::fs::write(probe.join("references/zz-probe-only.md"), "probe reference\n").unwrap();
        let path = probe.join("SKILL.md").display().to_string();
        let out = read_file(ws.path(), &path, 0, 10);
        // The project-relative spellings models use for an installed skill.
        let as_project = read_file(ws.path(), ".opencode/skills/zz-probe-skill/SKILL.md", 0, 10);
        let no_name = read_file(ws.path(), ".opencode/skills/references/zz-probe-only.md", 0, 10);
        let _ = std::fs::remove_dir_all(&probe);
        assert!(out.unwrap().contains("hello skill"));
        assert!(as_project.unwrap().contains("hello skill"));
        assert!(no_name.unwrap().contains("probe reference"));
        assert!(read_file(ws.path(), "C:/Windows/win.ini", 0, 5).is_err() || cfg!(not(windows)));
        assert!(write_file(ws.path(), &skills.join("x.txt").display().to_string(), "no").is_err(), "writing there stays refused");
    }

    #[test]
    fn read_on_directory_points_to_list() {
        let dir = TempDir::new("read-dir");
        let err = read_file(dir.path(), ".", 0, 10).unwrap_err();
        assert!(err.to_string().contains("list"), "got: {err}");
    }

    #[test]
    fn html_strips_scripts() {
        let t = html_to_text("<html><script>alert(1)</script><p>Hi &amp; bye</p></html>");
        assert!(!t.contains("alert"));
        assert!(t.contains("Hi & bye"));
    }
}

#[cfg(test)]
mod b01_tests {
    use super::*;

    #[test]
    fn edit_refuses_empty_old_str() {
        let ws = std::env::temp_dir().join(format!("ocrs-b01-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("f.txt"), "abc").unwrap();
        for replace_all in [false, true] {
            let err = edit_file(&ws, "f.txt", "", "X", replace_all).unwrap_err().to_string();
            assert!(err.contains("old_str is empty"), "{err}");
        }
        assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "abc", "file untouched");
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
mod s03_confine_tests {
    use super::*;

    pub fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("ocrs-s03-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn confine_path_keeps_inside_paths() {
        let ws = temp_dir("inside");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let p = confine_path(&ws, "src/../src/main.rs").unwrap();
        assert!(p.starts_with(&ws) && p.ends_with("src/main.rs"), "{}", p.display());
        // A file that does not exist yet (write creates it) is fine.
        assert!(confine_path(&ws, "new/dir/file.txt").unwrap().starts_with(&ws));
        // An absolute path inside the workspace is fine.
        let abs = ws.join("src").join("lib.rs");
        assert!(confine_path(&ws, abs.to_str().unwrap()).is_ok());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn confine_path_rejects_escapes() {
        let ws = temp_dir("escape");
        let other = temp_dir("other");
        for bad in ["..", "../x.txt", "src/../../x.txt"] {
            let err = confine_path(&ws, bad).unwrap_err().to_string();
            assert!(err.contains("outside the workspace"), "{bad}: {err}");
        }
        assert!(confine_path(&ws, other.join("x.txt").to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[cfg(unix)]
    #[test]
    fn confine_path_rejects_symlinks_that_leave() {
        let ws = temp_dir("link");
        let other = temp_dir("target");
        std::fs::write(other.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(&other, ws.join("out")).unwrap();
        std::os::unix::fs::symlink(other.join("missing.txt"), ws.join("dangling")).unwrap();
        assert!(confine_path(&ws, "out/secret.txt").is_err());
        assert!(confine_path(&ws, "out/new.txt").is_err());
        assert!(confine_path(&ws, "dangling").is_err());
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }
}

#[cfg(test)]
mod s03_tools_tests {
    use super::*;

    #[test]
    fn file_tools_refuse_outside_paths() {
        let ws = super::s03_confine_tests::temp_dir("tools");
        assert!(read_file(&ws, "../x.txt", 0, 10).is_err());
        assert!(write_file(&ws, "../s03-must-not-exist.txt", "x").is_err());
        assert!(!ws.parent().unwrap().join("s03-must-not-exist.txt").exists());
        assert!(edit_file(&ws, "../x.txt", "a", "b", false).is_err());
        assert!(list_dir(&ws, "..").is_err());
        assert!(glob_files(&ws, "*", Some("..")).is_err());
        assert!(grep_files(&ws, "x", Some(".."), false).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
mod s06_ip_tests {
    use super::*;

    #[test]
    fn only_public_addresses_pass() {
        let ip = |s: &str| -> std::net::IpAddr { s.parse().unwrap() };
        for bad in [
            "127.0.0.1", "10.1.2.3", "172.16.5.4", "192.168.1.1", "169.254.169.254",
            "100.64.0.1", "0.0.0.0", "255.255.255.255", "::1", "::", "fe80::1", "fd00::1",
            "::ffff:10.0.0.1", "64:ff9b::a00:1", "2002:c0a8:101::1",
        ] {
            assert!(!is_public_ip(ip(bad)), "{bad} must be refused");
        }
        for good in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "64:ff9b::808:808"] {
            assert!(is_public_ip(ip(good)), "{good} must be allowed");
        }
    }
}

#[cfg(test)]
mod s06_fetch_tests {
    use super::*;
    use axum::http::{header, StatusCode};
    use axum::{routing::get, Router};

    fn loopback_only(ip: std::net::IpAddr) -> bool {
        ip.is_loopback()
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn webfetch_refuses_local_hosts_and_parses_search_results() {
        let page = r#"<div class="result"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&amp;rut=x">Tokio <b>docs</b></a>
<a class="result__snippet" href="x">An async runtime for <b>Rust</b>.</a></div>
<div class="result"><a class="result__a" href="https://example.com/b">Second</a></div>"#;
        let r = parse_search_results(page, 8);
        assert_eq!(r[0], ("Tokio docs".into(), "https://docs.rs/tokio".into(), "An async runtime for Rust.".into()));
        assert_eq!(r[1].1, "https://example.com/b");
        assert_eq!(parse_search_results(page, 1).len(), 1);
        for url in ["http://127.0.0.1:9/", "http://localhost:9/", "http://[::1]:9/", "http://10.0.0.1/"] {
            let err = webfetch(url).await.unwrap_err().to_string();
            assert!(err.contains("refused"), "{url}: {err}");
        }
    }

    #[tokio::test]
    async fn every_redirect_is_checked_and_loops_stop() {
        let app = Router::new()
            .route("/start", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "http://10.255.255.1/x")], "") }))
            .route("/loop", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop")], "") }));
        let base = serve(app).await;
        let err = fetch_checked(&format!("{base}/start"), loopback_only).await.unwrap_err().to_string();
        assert!(err.contains("10.255.255.1"), "{err}");
        let err = fetch_checked(&format!("{base}/loop"), loopback_only).await.unwrap_err().to_string();
        assert!(err.contains("too many redirects"), "{err}");
    }

    #[tokio::test]
    async fn big_bodies_are_cut() {
        let app = Router::new().route("/big", get(|| async { "a".repeat(3_000_000) }));
        let base = serve(app).await;
        let out = fetch_checked(&format!("{base}/big"), loopback_only).await.unwrap();
        assert!(out.contains("only the start was read"), "size note missing");
    }
}

#[cfg(test)]
mod s05_tests {
    use super::*;

    #[test]
    fn scrub_secrets_removes_our_keys() {
        let mut cmd = tokio::process::Command::new("git");
        scrub_secrets(&mut cmd);
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for key in SECRET_ENV_VARS {
            assert!(removed.iter().any(|k| k == key), "{key} not removed: {removed:?}");
        }
    }

    /// End to end on Unix: the shell started by the bash tool cannot see
    /// the key even when opencode-rust itself has it.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_child_does_not_see_the_api_key() {
        std::env::set_var("OPENCODE_RUST_API_KEY", "s05-test-secret");
        let out = bash(
            "echo \"key=${OPENCODE_RUST_API_KEY:-unset}\"",
            &std::env::temp_dir(),
            Duration::from_secs(10),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .await;
        std::env::remove_var("OPENCODE_RUST_API_KEY");
        let out = out.unwrap();
        assert!(out.contains("key=unset"), "{out}");
        assert!(!out.contains("s05-test-secret"), "{out}");
    }

    #[test]
    fn git_bash_paths_become_windows_paths() {
        assert_eq!(msys_to_windows("/c/Users/me/x.js").as_deref(), Some("C:/Users/me/x.js"));
        assert_eq!(msys_to_windows("/d").as_deref(), Some("D:/"));
        assert_eq!(msys_to_windows("/src/main.rs"), None, "a longer first part is not a drive");
        assert_eq!(msys_to_windows("src/main.rs"), None);
    }

    /// The model reads a file by the Git Bash path the shell showed it.
    #[cfg(windows)]
    #[test]
    fn file_tools_accept_git_bash_paths_inside_the_workspace() {
        let ws = std::env::temp_dir().join(format!("ocrs-msys-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let dir = ws.as_path();
        std::fs::write(dir.join("script.js"), "x").unwrap();
        let abs = std::path::absolute(dir).unwrap().join("script.js");
        let s = abs.to_string_lossy().replace('\\', "/");
        let (drive, rest) = s.split_once(':').unwrap();
        let msys = format!("/{}{}", drive.to_ascii_lowercase(), rest);
        let got = confine_path(dir, &msys).expect("accepted");
        assert!(got.ends_with("script.js"), "{got:?}");
        assert!(confine_path(dir, "/c/Windows/win.ini").is_err(), "outside stays refused");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Long output keeps its start and its end (where errors and test
    /// summaries are) and says how much was cut.
    #[test]
    fn long_output_keeps_head_and_tail() {
        let s = format!("START{}END", "é".repeat(10_000));
        let out = truncate_middle(&s, 100, 200);
        assert!(out.starts_with("START") && out.ends_with("END"), "{out}");
        assert!(out.contains("bytes cut from the middle") && out.len() < 400, "{}", out.len());
        assert_eq!(truncate_middle("short", 100, 200), "short");
    }

    /// Quotes reach the shell as typed: no `\"` escapes are added (they
    /// broke `python -c "…"` and quoted paths under `cmd`).
    #[tokio::test]
    async fn bash_passes_quotes_as_typed() {
        let out = bash(
            r#"echo "two words""#,
            &std::env::temp_dir(),
            Duration::from_secs(10),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .await
        .unwrap();
        assert!(out.contains("two words") && !out.contains('\\'), "{out}");
    }

    /// Same check on Windows (Git Bash or `cmd /C`): no key either.
    #[cfg(windows)]
    #[tokio::test]
    async fn bash_child_does_not_see_the_api_key() {
        std::env::set_var("OPENCODE_RUST_API_KEY", "s05-test-secret");
        let line = match shell_kind() {
            crate::permissions::Shell::Posix => {
                r#"if [ -n "$OPENCODE_RUST_API_KEY" ]; then echo LEAKED; else echo CLEAN; fi"#
            }
            crate::permissions::Shell::Cmd => "if defined OPENCODE_RUST_API_KEY (echo LEAKED) else (echo CLEAN)",
        };
        let out = bash(
            line,
            &std::env::temp_dir(),
            Duration::from_secs(10),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .await;
        std::env::remove_var("OPENCODE_RUST_API_KEY");
        let out = out.unwrap();
        assert!(out.contains("CLEAN"), "{out}");
        assert!(!out.contains("LEAKED"), "{out}");
        assert!(!out.contains("s05-test-secret"), "{out}");
    }
}

#[cfg(test)]
mod b30_tests {
    use super::*;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b30-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn offset_is_the_first_line_shown() {
        let ws = ws("offset");
        std::fs::write(ws.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let out = read_file(&ws, "f.txt", 2, 2).unwrap();
        assert!(out.starts_with("     2\ttwo"), "{out}");
        assert!(out.contains("     3\tthree"), "{out}");
        assert!(out.contains("continue with offset 4"), "{out}");
        assert_eq!(read_file(&ws, "f.txt", 0, 1).unwrap(), read_file(&ws, "f.txt", 1, 1).unwrap());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn binary_and_non_utf8_files() {
        let ws = ws("bin");
        std::fs::write(ws.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        assert!(read_file(&ws, "bin.dat", 0, 10).unwrap().starts_with("(binary file"));
        std::fs::write(ws.join("latin1.txt"), b"caf\xe9\n").unwrap();
        let out = read_file(&ws, "latin1.txt", 0, 10).unwrap();
        assert!(out.contains("caf\u{FFFD}"), "{out}");
        assert!(out.contains("not valid UTF-8"), "{out}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn very_long_lines_are_cut() {
        let ws = ws("long");
        std::fs::write(ws.join("min.js"), "x".repeat(10_000)).unwrap();
        let out = read_file(&ws, "min.js", 0, 10).unwrap();
        assert!(out.contains("line cut at 2000 characters"), "{}", out.len());
        assert!(out.len() < 3000, "{}", out.len());
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
mod b31_tests {
    use super::*;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b31-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn overwriting_a_non_utf8_file_says_wrote() {
        let ws = ws("msg");
        std::fs::write(ws.join("bin.dat"), [0xffu8, 0xfe, 0x00]).unwrap();
        let msg = write_file(&ws, "bin.dat", "text").unwrap();
        assert!(msg.starts_with("wrote"), "{msg}");
        assert!(msg.contains("+1 bytes vs previous"), "{msg}");
        assert!(write_file(&ws, "new.txt", "abc").unwrap().starts_with("created"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn writes_leave_no_temp_files() {
        let ws = ws("tmp");
        write_file(&ws, "a.txt", "one\n").unwrap();
        edit_file(&ws, "a.txt", "one", "two", false).unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "two\n");
        let leftovers: Vec<String> = std::fs::read_dir(&ws)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".ocrs-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn write_keeps_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let ws = ws("mode");
        std::fs::write(ws.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(ws.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        write_file(&ws, "run.sh", "#!/bin/sh\necho hi\n").unwrap();
        let mode = std::fs::metadata(ws.join("run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
mod b32_glob_tests {
    use super::*;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b32-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("src")).unwrap();
        p
    }

    #[test]
    fn glob_prints_workspace_paths_and_expands_braces() {
        let ws = ws("glob");
        for f in ["src/a.rs", "src/b.toml", "src/c.txt"] {
            std::fs::write(ws.join(f), "x").unwrap();
        }
        let out = glob_files(&ws, "*.{rs,toml}", Some("src")).unwrap();
        assert!(out.contains("src/a.rs") && out.contains("src/b.toml"), "{out}");
        assert!(!out.contains("c.txt"), "{out}");
        assert_eq!(expand_braces("x.{a,b}.{c,d}").len(), 4);
        assert_eq!(expand_braces("no{brace"), vec!["no{brace"]);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn double_star_slash_matches_whole_names_only() {
        assert!(match_glob("**/x.rs", "a/x.rs"));
        assert!(match_glob("**/x.rs", "x.rs"));
        assert!(!match_glob("**/x.rs", "foox.rs"));
        assert!(!match_glob("**/x.rs", "a/foox.rs"));
    }
}

#[cfg(test)]
mod b32_grep_tests {
    use super::*;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b32-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("src")).unwrap();
        p
    }

    #[test]
    fn git_folder_is_skipped_and_unsearchable_files_are_reported() {
        let ws = ws("grep");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git").join("config"), "token").unwrap();
        std::fs::write(ws.join("a.txt"), "token").unwrap();
        std::fs::write(ws.join("latin1.txt"), b"token caf\xe9").unwrap();
        let out = grep_files(&ws, "token", None, false).unwrap();
        assert!(out.contains("a.txt:1:"), "{out}");
        assert!(!out.contains(".git"), "{out}");
        assert!(out.contains("1 file(s) not searched"), "{out}");
        assert!(glob_files(&ws, "**/config", None).unwrap().contains("no files matched"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
mod b43_tests {
    use super::*;

    #[test]
    fn entities_are_decoded_once_and_stray_brackets_are_fast() {
        assert_eq!(html_to_text("<p>&amp;lt;b&amp;gt; &lt;i&gt;</p>"), "&lt;b&gt; <i>");
        let started = std::time::Instant::now();
        let text = html_to_text(&"a<".repeat(100_000));
        assert_eq!(text.len(), 200_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
