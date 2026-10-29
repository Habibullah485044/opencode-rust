//! Check files right after the agent wrote or edited them (`agent.check_edits`).
//!
//! The model only learns that an edit broke the build when something runs the
//! build — often at the very end, after it already said "done". This checks
//! the files one step touched with the language's own checker and hands the
//! errors back before the next step: JSON and TOML are parsed here, Python
//! and JavaScript files are compiled one by one, and Rust and TypeScript
//! projects are checked once per step (cargo check / tsc), keeping only the
//! errors in the touched files. A checker that is not installed, or that runs
//! longer than the time limit, is skipped silently: no errors is no message.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// At most this many error lines go back to the model.
const MAX_LINES: usize = 20;
/// And at most this many characters.
const MAX_CHARS: usize = 3000;

/// The errors in `files` (absolute paths the step wrote or edited), as one
/// report for the model. `None` when every file checked clean.
pub async fn check(workspace: &Path, files: &[PathBuf], limit: Duration) -> Option<String> {
    let mut files: Vec<PathBuf> = files.to_vec();
    files.sort();
    files.dedup();
    let mut errors: Vec<String> = Vec::new();
    let mut rust = Vec::new();
    let mut ts = Vec::new();
    for f in &files {
        let shown = shown_path(workspace, f);
        match ext(f).as_str() {
            "json" => {
                let Ok(text) = std::fs::read_to_string(f) else { continue };
                if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
                    errors.push(format!("{shown}:{}:{}: invalid JSON: {e}", e.line(), e.column()));
                }
            }
            "toml" => {
                let Ok(text) = std::fs::read_to_string(f) else { continue };
                if let Err(e) = text.parse::<toml::Table>() {
                    let msg = e.message().to_string();
                    let line = e.span().map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
                    errors.push(match line {
                        Some(l) => format!("{shown}:{l}: invalid TOML: {msg}"),
                        None => format!("{shown}: invalid TOML: {msg}"),
                    });
                }
            }
            "py" => {
                let out = run(workspace, &["python", "-m", "py_compile"], f, limit).await;
                if let Some((false, text)) = out {
                    errors.extend(last_lines(&text, 4).into_iter().map(|l| format!("{shown}: {l}")));
                }
            }
            "js" | "mjs" | "cjs" => {
                let out = run(workspace, &["node", "--check"], f, limit).await;
                if let Some((false, text)) = out {
                    errors.extend(last_lines(&text, 6).into_iter().map(|l| format!("{shown}: {l}")));
                }
            }
            "rs" => rust.push(f.clone()),
            "ts" | "tsx" | "mts" | "cts" => ts.push(f.clone()),
            _ => {}
        }
    }
    if let Some(dir) = rust.first().and_then(|f| project_dir(workspace, f, "Cargo.toml")) {
        let args = ["cargo", "check", "--quiet", "--message-format", "short"];
        if let Some((_, text)) = run_in(&dir, &args, limit).await {
            errors.extend(pick_errors(&text, &dir, &rust, workspace));
        }
    }
    if let Some(dir) = ts.first().and_then(|f| project_dir(workspace, f, "tsconfig.json")) {
        let tsc = dir.join("node_modules").join(".bin").join(if cfg!(windows) { "tsc.cmd" } else { "tsc" });
        if tsc.is_file() {
            let tsc = tsc.to_string_lossy().to_string();
            if let Some((_, text)) = run_in(&dir, &[&tsc, "--noEmit", "--pretty", "false"], limit).await {
                errors.extend(pick_errors(&text, &dir, &ts, workspace));
            }
        }
    }
    report(errors)
}

/// The message for the model, or `None` for no errors.
fn report(errors: Vec<String>) -> Option<String> {
    if errors.is_empty() {
        return None;
    }
    let total = errors.len();
    let mut body = String::new();
    for e in errors.iter().take(MAX_LINES) {
        if body.len() + e.len() > MAX_CHARS {
            break;
        }
        body.push_str(e);
        body.push('\n');
    }
    let shown = body.lines().count();
    if shown < total {
        body.push_str(&format!("… and {} more\n", total - shown));
    }
    Some(format!(
        "[check] The files you just changed have errors. Fix them before you go on:\n{}",
        body.trim_end()
    ))
}

/// Error lines of a project checker (cargo `--message-format short`:
/// `src/a.rs:3:5: error[E0308]: …`; tsc: `src/a.ts(3,5): error TS2322: …`)
/// that point into one of `files`, shown relative to the workspace.
fn pick_errors(output: &str, dir: &Path, files: &[PathBuf], workspace: &Path) -> Vec<String> {
    let wanted: Vec<String> = files.iter().map(|f| norm(f)).collect();
    output
        .lines()
        .filter(|l| l.contains("error"))
        .filter_map(|l| {
            let end = l.find(['(', ':']).filter(|&i| i > 0)?;
            // A Windows drive letter (`C:\…`) is part of the path, not its end.
            let end = if end == 1 && l[2..].starts_with(['\\', '/']) {
                2 + l[2..].find(['(', ':'])?
            } else {
                end
            };
            let file = &l[..end];
            let full = if Path::new(file).is_absolute() { PathBuf::from(file) } else { dir.join(file) };
            wanted.contains(&norm(&full)).then(|| format!("{}{}", shown_path(workspace, &full), &l[end..]))
        })
        .collect()
}

/// The nearest folder from `file` up to the workspace that holds `marker`.
fn project_dir(workspace: &Path, file: &Path, marker: &str) -> Option<PathBuf> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d.join(marker).is_file() {
            return Some(d.to_path_buf());
        }
        if norm(d) == norm(workspace) {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Run `cmd… file` in the workspace: `(succeeded, stdout+stderr)`.
async fn run(workspace: &Path, cmd: &[&str], file: &Path, limit: Duration) -> Option<(bool, String)> {
    let path = file.to_string_lossy().to_string();
    let mut args: Vec<&str> = cmd.to_vec();
    args.push(&path);
    run_in(workspace, &args, limit).await
}

/// Run a command in `dir`. `None` when it is not installed or ran too long.
async fn run_in(dir: &Path, args: &[&str], limit: Duration) -> Option<(bool, String)> {
    let mut cmd = tokio::process::Command::new(args[0]);
    cmd.args(&args[1..])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash
    let out = tokio::time::timeout(limit, cmd.output()).await.ok()?.ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), text))
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

fn last_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
}

/// A path for comparing: forward slashes, no `./`, lower case on Windows.
fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/").replace("/./", "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// `file` relative to the workspace when it is inside it.
fn shown_path(workspace: &Path, file: &Path) -> String {
    let (w, f) = (norm(workspace), norm(file));
    match f.strip_prefix(&w) {
        Some(rest) if rest.starts_with('/') => rest[1..].to_string(),
        _ => file.to_string_lossy().replace('\\', "/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("ocrs-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn broken_json_and_toml_are_reported_with_their_line() {
        let dir = temp_dir("editcheck-bad");
        let ws = dir.as_path();
        std::fs::write(ws.join("a.json"), "{\n  \"x\": 1,\n}\n").unwrap();
        std::fs::write(ws.join("b.toml"), "[p]\nname = \"x\"\nbad = \n").unwrap();
        std::fs::write(ws.join("ok.json"), "{\"x\": 1}").unwrap();
        let files = vec![ws.join("a.json"), ws.join("b.toml"), ws.join("ok.json")];
        let out = rt().block_on(check(ws, &files, Duration::from_secs(10))).unwrap();
        assert!(out.starts_with("[check]"), "{out}");
        assert!(out.contains("a.json:3:"), "{out}");
        assert!(out.contains("b.toml:3: invalid TOML"), "{out}");
        assert!(!out.contains("ok.json"), "{out}");
    }

    #[test]
    fn clean_files_give_no_message() {
        let dir = temp_dir("editcheck-ok");
        let ws = dir.as_path();
        std::fs::write(ws.join("a.json"), "[1, 2]").unwrap();
        std::fs::write(ws.join("notes.md"), "# hi").unwrap();
        let files = vec![ws.join("a.json"), ws.join("notes.md")];
        assert_eq!(rt().block_on(check(ws, &files, Duration::from_secs(10))), None);
    }

    #[test]
    fn project_errors_are_kept_only_for_the_changed_files() {
        let ws = Path::new("/w");
        let cargo = "src/a.rs:3:5: error[E0308]: mismatched types\n\
                     src/b.rs:9:1: error[E0425]: cannot find value `y`\n\
                     src/a.rs:7:1: warning: unused variable\n\
                     error: could not compile `x` (lib) due to 2 previous errors\n";
        let got = pick_errors(cargo, ws, &[ws.join("src/a.rs")], ws);
        assert_eq!(got, vec!["src/a.rs:3:5: error[E0308]: mismatched types".to_string()]);
        let tsc = "src/app.ts(3,5): error TS2322: Type 'string' is not assignable to type 'number'.\n\
                   src/other.ts(1,1): error TS2304: Cannot find name 'x'.\n";
        let got = pick_errors(tsc, Path::new("/w/web"), &[PathBuf::from("/w/web/src/app.ts")], ws);
        assert_eq!(got, vec!["web/src/app.ts(3,5): error TS2322: Type 'string' is not assignable to type 'number'.".to_string()]);
    }

    #[test]
    fn a_long_report_is_cut() {
        let errs: Vec<String> = (0..50).map(|i| format!("a.rs:{i}:1: error: x")).collect();
        let out = report(errs).unwrap();
        assert!(out.contains("… and 30 more"), "{out}");
    }
}
