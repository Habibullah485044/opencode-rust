//! The model's tools: specs and dispatch here; the work in submodules.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

mod edit;
mod files;
mod paths;
mod process;
mod search;
mod shell;
mod web;
#[cfg(windows)]
mod winjob;

pub mod jobs;

pub(crate) use files::write_atomic;
#[cfg(any(feature = "gui", test))] // the desktop terminal and the tests; the core build has no other user
pub(crate) use paths::msys_to_windows;
pub use paths::{confine_path, confine_read_path};
pub(crate) use process::{adopt_into_job, kill_tree};
pub use shell::{scrub_secrets, shell_command, shell_kind, SECRET_ENV_VARS};
pub use web::is_public_ip;

use edit::{edit_file, edit_many};
use files::{list_dir, read_file, read_outline, write_file};
use process::wait_for_abort;
use search::{glob_files_with, grep_files_with};
use shell::{bash, shell_label};
use web::{web_search, webfetch};

#[cfg(test)]
use edit::apply_edit;
#[cfg(test)]
use paths::user_skill_file;
#[cfg(test)]
use search::{glob_files, grep_files, match_glob};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use web::html_to_text;

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
    execute_with(name, args, workspace, abort, &crate::permissions::ReadFilter::default()).await
}

/// [`execute`] with the read rules that `grep` and `glob` apply to each
/// file they find (`Engine::read_filter`): their one approval covers a
/// whole folder, and a denied file must not show up in it.
pub async fn execute_with(
    name: &str,
    args: &Value,
    workspace: &Path,
    abort: &std::sync::atomic::AtomicBool,
    hidden: &crate::permissions::ReadFilter,
) -> Result<String> {
    if abort.load(std::sync::atomic::Ordering::SeqCst) {
        bail!("not run: the run was aborted");
    }
    if matches!(name, "read" | "write" | "edit" | "glob" | "grep" | "list") {
        // File tools block (a big grep walks the whole tree): off the
        // runtime threads, so other chats and the UI keep running.
        let (tool, args, ws, hidden) = (name.to_string(), args.clone(), workspace.to_path_buf(), hidden.clone());
        return tokio::task::spawn_blocking(move || file_tool(&tool, &args, &ws, &hidden))
            .await
            .map_err(|e| anyhow!("{name} failed: {e}"))?;
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
            let timeout = args.get("timeout_secs").and_then(Value::as_u64).unwrap_or(120).clamp(1, 600);
            bash(cmd, workspace, Duration::from_secs(timeout), abort).await
        }
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
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

/// The blocking file tools (run on a blocking thread, see `execute_with`).
fn file_tool(name: &str, args: &Value, workspace: &Path, hidden: &crate::permissions::ReadFilter) -> Result<String> {
    match name {
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
        "glob" => {
            glob_files_with(workspace, arg_str(args, "pattern")?, args.get("path").and_then(Value::as_str), hidden)
        }
        "grep" => grep_files_with(
            workspace,
            arg_str(args, "pattern")?,
            args.get("path").and_then(Value::as_str),
            args.get("case_insensitive").and_then(Value::as_bool).unwrap_or(false),
            hidden,
        ),
        "list" => list_dir(workspace, args.get("path").and_then(Value::as_str).unwrap_or(".")),
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| anyhow!("missing string arg `{key}`"))
}

/// New (B-06): read a child's pipe to the end (empty when there is none).
/// `bash {job}` / `bash {job, stop}`: reading or stopping a background job
/// the agent already started (and was allowed to start). Needs no approval.
pub fn is_job_control(name: &str, args: &Value) -> bool {
    name == "bash" && args.get("job").and_then(Value::as_str).is_some() && args.get("command").is_none()
}

/// Keep the first `head` and the last `tail` bytes of long command output.
/// Output is resent with every later request of the run, and what matters
/// (the error, the test summary) is usually at the end. `tail == 0` keeps
/// only the start.
fn truncate_middle(s: &str, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }
    let mut a = head;
    while !s.is_char_boundary(a) {
        a -= 1;
    }
    if tail == 0 {
        return format!("{}\n… [truncated {} bytes]", &s[..a], s.len() - a);
    }
    let mut b = s.len() - tail;
    while !s.is_char_boundary(b) {
        b += 1;
    }
    format!("{}\n… [{} bytes cut from the middle] …\n{}", &s[..a], b - a, &s[b..])
}

/// The first `max` bytes of `s` (on a character boundary), noting the cut.
fn truncate(s: &str, max: usize) -> String {
    truncate_middle(s, max, 0)
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
    pub(super) static JOB_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()
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
        let (out, n, loose) =
            apply_edit(file, "if ready {\n    go();\n}", "if ready {\n    go();\n    done();\n}", false).unwrap();
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
        let started = execute(
            "bash",
            &json!({"command": "echo ONE$((0+1)); sleep 1; echo TWO$((1+1)); sleep 30", "background": true}),
            dir.path(),
            &no,
        )
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
        assert_eq!(
            std::fs::read_to_string(ws.join("m.txt")).unwrap(),
            "A\nbeta\nG\n",
            "nothing written when one edit fails"
        );
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
        std::fs::write(dir.path().join("app.py"), "import os\n\nclass Cart:\n    def add(self):\n        pass\n")
            .unwrap();
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
        assert!(
            write_file(ws.path(), &skills.join("x.txt").display().to_string(), "no").is_err(),
            "writing there stays refused"
        );
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
        let out =
            bash(line, &std::env::temp_dir(), Duration::from_secs(10), &std::sync::atomic::AtomicBool::new(false))
                .await;
        std::env::remove_var("OPENCODE_RUST_API_KEY");
        let out = out.unwrap();
        assert!(out.contains("CLEAN"), "{out}");
        assert!(!out.contains("LEAKED"), "{out}");
        assert!(!out.contains("s05-test-secret"), "{out}");
    }
}

#[cfg(test)]
mod review_fix_tests {
    use super::*;

    fn ws(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("ocrs-rf-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// `.opencode/skills/<rest>` only reads plain names below a skills
    /// folder: `..`, a root or a drive in `<rest>` led to the config file
    /// (with the API key) or anywhere on disk.
    #[test]
    fn skill_paths_cannot_climb_out_of_the_skills_folder() {
        for bad in [
            ".opencode/skills/../config.toml",
            ".opencode/skills/../Cargo.toml",
            ".opencode/skills/x/../../Cargo.toml",
            ".opencode/skills//etc/passwd",
            ".opencode/skills/C:/Windows/win.ini",
            ".opencode/skills/",
        ] {
            assert!(user_skill_file(bad).is_none(), "{bad}");
        }
        let ws = ws("skill-escape");
        let read = read_file(&ws, ".opencode/skills/../Cargo.toml", 0, 5);
        assert!(read.is_err() || !read.unwrap().contains("[package]"), "Cargo.toml next to the skills folder");
        // A relative path that left the workspace is not tried in a skills folder.
        assert!(confine_read_path(&ws, "../skills/x/SKILL.md").is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Windows: `.env::$DATA` is `.env` under a name no deny rule matches.
    #[cfg(windows)]
    #[test]
    fn data_stream_names_are_refused() {
        let ws = ws("ads");
        std::fs::write(ws.join(".env"), "KEY=1").unwrap();
        for bad in [".env::$DATA", "secret.txt:hidden", "src/a.rs:x"] {
            assert!(confine_path(&ws, bad).unwrap_err().to_string().contains("data stream"), "{bad}");
        }
        assert!(read_file(&ws, ".env::$DATA", 0, 5).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// `grep . .` must not show what `read secrets/*` denies, and glob does
    /// not list it.
    #[test]
    fn grep_and_glob_skip_files_a_read_rule_denies() {
        let ws = ws("filter");
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        std::fs::write(ws.join("secrets/key.pem"), "TOKEN=abc").unwrap();
        std::fs::write(ws.join("app.txt"), "TOKEN=public").unwrap();
        let cfg = crate::config::PermissionsCfg {
            rules: vec![crate::config::RuleCfg {
                tool: "read".into(),
                pattern: "secrets/*".into(),
                action: "deny".into(),
            }],
            ..Default::default()
        };
        let filter = crate::permissions::Engine::new(&cfg).read_filter("grep");
        let out = grep_files_with(&ws, "TOKEN", None, false, &filter).unwrap();
        assert!(out.contains("app.txt") && !out.contains("key.pem") && !out.contains("abc"), "{out}");
        let out = grep_files_with(&ws, "TOKEN", Some("secrets/key.pem"), false, &filter).unwrap();
        assert!(!out.contains("abc"), "naming the file itself: {out}");
        let out = glob_files_with(&ws, "**/*", None, &filter).unwrap();
        assert!(out.contains("app.txt") && !out.contains("key.pem"), "{out}");
        // Without rules everything is found.
        assert!(grep_files(&ws, "TOKEN", None, false).unwrap().contains("key.pem"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A symlink in the workspace to a file outside: grep did read it.
    #[cfg(unix)]
    #[test]
    fn grep_does_not_follow_symlinks_out() {
        let ws = ws("grep-link");
        let other = super::review_fix_tests::ws("grep-link-target");
        std::fs::write(other.join("secret.txt"), "TOKEN=outside").unwrap();
        std::os::unix::fs::symlink(other.join("secret.txt"), ws.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&other, ws.join("dir")).unwrap();
        let out = grep_files(&ws, "TOKEN", None, false).unwrap();
        assert!(!out.contains("outside"), "{out}");
        assert!(!glob_files(&ws, "**/*", None).unwrap().contains("link.txt"));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// Loose edits: an NBSP indentation cut panicked (release aborts), and
    /// a file with mixed line endings got every ending rewritten.
    #[test]
    fn loose_edits_are_char_safe_and_keep_each_line_ending() {
        // delta -1 and a new line that starts with NBSP (2 bytes).
        let (out, _, loose) = apply_edit("a\nb\n", " a", "\u{a0}z", false).unwrap();
        assert!(loose);
        assert_eq!(out, "\u{a0}z\nb\n");
        let (out, _, loose) = apply_edit("a\r\nb\nc\n", "  b", "B", false).unwrap();
        assert!(loose);
        assert_eq!(out, "a\r\nB\nc\n", "only the edited line changes");
        // (Not an exact substring, so the loose path runs.)
        let (out, _, loose) = apply_edit("a\r\n\tb\r\nc", " b\nc", "B\nC\nD", false).unwrap();
        assert!(loose);
        assert_eq!(out, "a\r\nB\r\nC\r\nD", "no CR added at a last line that had none");
    }

    /// Exponential backtracking: `*a*a*…b` against a long run of `a`.
    #[test]
    fn glob_matching_is_not_exponential() {
        let started = std::time::Instant::now();
        let text = "a".repeat(200);
        let pattern = format!("{}b", "*a".repeat(30));
        assert!(!match_glob(&pattern, &text));
        assert!(!match_glob(&format!("{}b", "**/a".repeat(20)), &"a/".repeat(100)));
        assert!(!crate::permissions::glob_match(&pattern, &text));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        // The documented semantics still hold.
        assert!(match_glob("src/**/*.rs", "src/lib.rs"));
        assert!(match_glob("a?c", "abc") && !match_glob("a?c", "a/c"));
        assert!(match_glob("**", "a/b/c"));
    }

    #[test]
    fn teredo_addresses_are_refused() {
        let ip: std::net::IpAddr = "2001:0:4136:e378:8000:63bf:3fff:fdd2".parse().unwrap();
        assert!(!is_public_ip(ip));
        assert!(is_public_ip("2001:4860:4860::8888".parse().unwrap()), "other 2001:: space stays");
    }

    /// `server &` keeps the pipes open after the shell exits: the command
    /// still ends at once (with a note), not after the whole timeout.
    #[tokio::test]
    async fn bash_returns_when_the_shell_exits_despite_a_background_child() {
        if shell_kind() != crate::permissions::Shell::Posix {
            return;
        }
        let dir = ws("bash-bg");
        let abort = std::sync::atomic::AtomicBool::new(false);
        let t0 = std::time::Instant::now();
        let out = bash("sleep 6 & echo started", &dir, Duration::from_secs(30), &abort).await.unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert!(out.contains("started") && out.contains("exit code: 0"), "{out}");
        assert!(out.contains("still runs"), "{out}");
        // A plain command has no note.
        assert!(!bash("echo hi", &dir, Duration::from_secs(10), &abort).await.unwrap().contains("still runs"));
    }

    /// Ended agent jobs are forgotten beyond a limit.
    #[tokio::test]
    async fn ended_jobs_are_pruned() {
        let _turn = super::tests::JOB_TESTS.lock().await;
        let dir = ws("jobs-prune");
        for _ in 0..30 {
            let id = jobs::spawn("exit 0", &dir, false).unwrap();
            for _ in 0..100 {
                if jobs::output(&id).unwrap().contains("exited") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        assert!(jobs::count() <= 22, "{}", jobs::count());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_keeps_the_start_and_says_how_much_was_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate(&"é".repeat(10), 5), "éé\n… [truncated 16 bytes]");
    }
}
