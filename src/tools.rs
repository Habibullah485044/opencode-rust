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
            "Run a shell command in the workspace. Prefer specialized tools (grep/glob) over raw commands.",
            json!({"type":"object","properties":{"command":{"type":"string"},"timeout_secs":{"type":"integer"}},"required":["command"]}),
        ),
        spec(
            "read",
            "Read a text file. Returns numbered lines. For directories, use list instead.",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}),
        ),
        spec(
            "write",
            "Create or overwrite a file with the given content.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        ),
        spec(
            "edit",
            "Replace old_str with new_str in a file. old_str must be non-empty, copied exactly from the file, and match exactly once unless replace_all.",
            json!({"type":"object","properties":{"path":{"type":"string"},"old_str":{"type":"string"},"new_str":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_str","new_str"]}),
        ),
        spec(
            "glob",
            "Find files by glob pattern (respects .gitignore). Pattern like **/*.rs",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]}),
        ),
        spec(
            "grep",
            "Regex search file contents (respects .gitignore). Returns path:line matches.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"case_insensitive":{"type":"boolean"}},"required":["pattern"]}),
        ),
        spec(
            "webfetch",
            "Fetch a URL and return its text content (html stripped).",
            json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        ),
        spec(
            "list",
            "List directory entries (one level).",
            json!({"type":"object","properties":{"path":{"type":"string"}}}),
        ),
        spec(
            "todo",
            "Manage a per-session todo list: add, list, complete, or remove tasks. Use this to track work across a long session.",
            json!({"type":"object","properties":{"op":{"type":"string","enum":["add","list","complete","remove"],"description":"Operation"},"content":{"type":"string","description":"Task content (required for add)"},"id":{"type":"string","description":"Todo id (required for complete/remove)"}},"required":["op"]}),
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
pub async fn execute(name: &str, args: &Value, workspace: &Path) -> Result<String> {
    match name {
        "bash" => {
            let cmd = arg_str(args, "command")?;
            let timeout = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 600);
            bash(cmd, workspace, Duration::from_secs(timeout)).await
        }
        "read" => read_file(
            workspace,
            arg_str(args, "path")?,
            args.get("offset").and_then(Value::as_i64).unwrap_or(0),
            args.get("limit").and_then(Value::as_i64).unwrap_or(2000),
        ),
        "write" => write_file(workspace, arg_str(args, "path")?, arg_str(args, "content")?),
        "edit" => edit_file(
            workspace,
            arg_str(args, "path")?,
            arg_str(args, "old_str")?,
            arg_str(args, "new_str")?,
            args.get("replace_all").and_then(Value::as_bool).unwrap_or(false),
        ),
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
        "webfetch" => webfetch(arg_str(args, "url")?).await,
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

async fn bash(command: &str, workspace: &Path, timeout: Duration) -> Result<String> {
    #[cfg(windows)]
    let mut child = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };
    #[cfg(not(windows))]
    let mut child = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    child
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // S-05: the shell never sees our provider key or API token.
    scrub_secrets(&mut child);
    let child = child.spawn().context("spawning shell")?;
    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
    match result {
        Ok(Ok(out)) => {
            let mut s = String::new();
            let code = out.status.code().unwrap_or(-1);
            s.push_str(&format!("exit code: {code}\n"));
            if !out.stdout.is_empty() {
                s.push_str("--- stdout ---\n");
                s.push_str(&truncate(&String::from_utf8_lossy(&out.stdout), 60_000));
            }
            if !out.stderr.is_empty() {
                s.push_str("\n--- stderr ---\n");
                s.push_str(&truncate(&String::from_utf8_lossy(&out.stderr), 20_000));
            }
            Ok(s)
        }
        Ok(Err(e)) => Err(anyhow!("command failed to run: {e}")),
        Err(_) => Err(anyhow!("command timed out after {}s", timeout.as_secs())),
    }
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

fn resolve(workspace: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) }
}

fn read_file(workspace: &Path, path: &str, offset: i64, limit: i64) -> Result<String> {
    let abs = resolve(workspace, path);
    if abs.is_dir() {
        bail!("{path} is a directory — use the list tool to see its contents");
    }
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))?;
    if text.bytes().any(|b| b == 0) {
        return Ok(format!("(binary file: {path})"));
    }
    let limit = limit.clamp(1, 10_000) as usize;
    let offset = offset.max(0) as usize;
    let lines: Vec<&str> = text.lines().collect();
    let slice: Vec<String> = lines
        .iter()
        .skip(offset)
        .take(limit)
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{}", offset + i + 1, l))
        .collect();
    let mut out = slice.join("\n");
    if offset + limit < lines.len() {
        out.push_str(&format!("\n… [{} more lines]", lines.len() - offset - limit));
    }
    Ok(out)
}

fn write_file(workspace: &Path, path: &str, content: &str) -> Result<String> {
    let abs = resolve(workspace, path);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(&abs).ok();
    std::fs::write(&abs, content).with_context(|| format!("writing {}", abs.display()))?;
    let bytes = content.len();
    Ok(match existing {
        Some(old) => {
            let delta = content.len() as i64 - old.len() as i64;
            format!("wrote {path} ({bytes} bytes, {delta:+} bytes vs previous)")
        }
        None => format!("created {path} ({bytes} bytes)"),
    })
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
    let abs = resolve(workspace, path);
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))?;
    let matches = text.matches(old_str).count();
    if matches == 0 {
        return Err(anyhow!("old_str not found in {path}"));
    }
    if matches > 1 && !replace_all {
        return Err(anyhow!(
            "old_str matches {matches} times in {path}; make it unique or set replace_all"
        ));
    }
    let new_text = if replace_all {
        text.replace(old_str, new_str)
    } else {
        text.replacen(old_str, new_str, 1)
    };
    std::fs::write(&abs, &new_text).with_context(|| format!("writing {}", abs.display()))?;
    let lines_before = text.matches('\n').count();
    let lines_after = new_text.matches('\n').count();
    Ok(format!(
        "edited {path}: replaced {matches} occurrence(s), line delta {:+}",
        lines_after as i64 - lines_before as i64
    ))
}

fn glob_files(workspace: &Path, pattern: &str, sub: Option<&str>) -> Result<String> {
    use ignore::WalkBuilder;
    let root = resolve(workspace, sub.unwrap_or("."));
    let mut matches = Vec::new();
    // require_git(false): honor .gitignore even in non-git workspaces.
    for entry in WalkBuilder::new(&root).hidden(false).require_git(false).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if match_glob(pattern, &rel_str) {
            matches.push(rel_str);
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
                        // `**/x` matches `x` or `any/x`
                        return m(&rest[1..], t) || (0..t.len()).any(|i| m(p, &t[i + 1..]));
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
    let root = resolve(workspace, sub.unwrap_or("."));
    let mut out: Vec<String> = Vec::new();
    // require_git(false): honor .gitignore even in non-git workspaces.
    for entry in WalkBuilder::new(&root).hidden(false).require_git(false).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Skip big files
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if meta.len() > 2_000_000 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        if text.bytes().any(|b| b == 0) {
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
    if out.is_empty() {
        Ok(format!("no matches for `{pattern}`"))
    } else {
        Ok(out.join("\n"))
    }
}

async fn webfetch(url: &str) -> Result<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(anyhow!("only http(s) urls are allowed"));
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("opencode-rust/0.1")
        .build()?;
    let resp = http.get(url).send().await.with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("{url} returned {status}"));
    }
    let body = resp.text().await?;
    let text = html_to_text(&body);
    Ok(truncate(&text, 40_000))
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
            if let Some(p) = html[i..].find('>') {
                out.push(' ');
                i += p + 1;
                continue;
            }
        }
        let ch_len = utf8_len(bytes[i]);
        let end = (i + ch_len).min(html.len());
        out.push_str(&html[i..end]);
        i = end;
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
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
    let abs = resolve(workspace, path);
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
        #[cfg(windows)]
        let out = bash("echo hi", dir.path(), Duration::from_secs(10)).await.unwrap();
        #[cfg(not(windows))]
        let out = bash("echo hi", dir.path(), Duration::from_secs(10)).await.unwrap();
        assert!(out.contains("hi"));
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
        )
        .await;
        std::env::remove_var("OPENCODE_RUST_API_KEY");
        let out = out.unwrap();
        assert!(out.contains("key=unset"), "{out}");
        assert!(!out.contains("s05-test-secret"), "{out}");
    }

    /// Same check on Windows: `cmd /C` must not see the key either.
    #[cfg(windows)]
    #[tokio::test]
    async fn bash_child_does_not_see_the_api_key() {
        std::env::set_var("OPENCODE_RUST_API_KEY", "s05-test-secret");
        let out = bash(
            "if defined OPENCODE_RUST_API_KEY (echo LEAKED) else (echo CLEAN)",
            &std::env::temp_dir(),
            Duration::from_secs(10),
        )
        .await;
        std::env::remove_var("OPENCODE_RUST_API_KEY");
        let out = out.unwrap();
        assert!(out.contains("CLEAN"), "{out}");
        assert!(!out.contains("LEAKED"), "{out}");
        assert!(!out.contains("s05-test-secret"), "{out}");
    }
}
