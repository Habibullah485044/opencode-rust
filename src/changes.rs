//! The Changes view: what the work changed in a project, as unified diffs
//! per file (tracked files against HEAD, new files in full), for review in
//! the desktop app. Line comments made there go back to the agent.

use anyhow::{anyhow, Result};
use std::path::Path;

/// One changed file.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FileChange {
    /// Relative to the repository root, `/`-separated.
    pub path: String,
    /// "modified", "added", "deleted", "renamed" or "new".
    pub status: String,
    /// Unified diff text (`+`/`-`/` ` lines, `@@` hunk headers).
    pub diff: String,
}

const MAX_FILES: usize = 60;
const MAX_DIFF_CHARS: usize = 60_000;
const MAX_TOTAL_CHARS: usize = 300_000;

/// The changes in the git repository that holds `root`.
pub fn collect(root: &Path) -> Result<Vec<FileChange>> {
    let git = |args: &[&str]| crate::gitsnap::git(root, args);
    let top = git(&["rev-parse", "--show-toplevel"])
        .map_err(|_| anyhow!("this project is not a git repository, so there is nothing to diff against"))?;
    let top = std::path::PathBuf::from(top.trim());
    let has_head = git(&["rev-parse", "--verify", "-q", "HEAD"]).is_ok();
    let status = git(&["status", "--porcelain=v1", "--untracked-files=all", "--", "."])?;
    let mut out = Vec::new();
    let mut total = 0;
    for line in status.lines() {
        if out.len() >= MAX_FILES || total >= MAX_TOTAL_CHARS || line.len() < 4 {
            break;
        }
        let code = &line[..2];
        let mut path = line[3..].trim().trim_matches('"').to_string();
        if let Some((_, to)) = path.split_once(" -> ") {
            path = to.to_string();
        }
        // Our own folders (scratch, attachments) are not the user's work.
        if path.starts_with(".opencode/") {
            continue;
        }
        let kind = match code {
            "??" => "new",
            c if c.contains('D') => "deleted",
            c if c.contains('R') => "renamed",
            c if c.contains('A') => "added",
            _ => "modified",
        };
        let mut diff = if kind == "new" {
            new_file_diff(&top.join(&path))
        } else {
            let spec = format!(":/{path}");
            let args: Vec<&str> = if has_head {
                vec!["diff", "--no-color", "-U3", "HEAD", "--", &spec]
            } else {
                vec!["diff", "--no-color", "-U3", "--cached", "--", &spec]
            };
            git(&args).unwrap_or_default()
        };
        if diff.chars().count() > MAX_DIFF_CHARS {
            diff = diff.chars().take(MAX_DIFF_CHARS).collect::<String>() + "\n… (diff cut)";
        }
        total += diff.len();
        out.push(FileChange { path, status: kind.to_string(), diff });
    }
    Ok(out)
}

/// A new file shown as all-added lines (text only, first 400 lines).
fn new_file_diff(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else { return String::new() };
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return "(binary file)".to_string();
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = format!("@@ -0,0 +1,{} @@\n", lines.len());
    for l in lines.iter().take(400) {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    if lines.len() > 400 {
        out.push_str(&format!("… ({} more lines)\n", lines.len() - 400));
    }
    out
}

/// The message that hands review comments to the agent.
pub fn comments_message(comments: &[(String, u32, String)]) -> String {
    let mut msg = String::from("Review comments on your changes — address each one:\n");
    for (path, line, text) in comments {
        msg.push_str(&format!("- {path}:{line}: {}\n", text.trim()));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_listed_with_diffs_and_new_files() {
        let dir = std::env::temp_dir().join(format!("ocrs-changes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(collect(&dir).is_err(), "not a repository yet");
        let git = |args: &[&str]| crate::gitsnap::git(&dir, args).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\n").unwrap();
        std::fs::write(dir.join("b.txt"), "fresh\n").unwrap();
        std::fs::create_dir_all(dir.join(".opencode").join("scratch")).unwrap();
        std::fs::write(dir.join(".opencode/scratch/x.txt"), "tmp").unwrap();
        let changes = collect(&dir).unwrap();
        let a = changes.iter().find(|c| c.path == "a.txt").unwrap();
        assert_eq!(a.status, "modified");
        assert!(a.diff.contains("-two") && a.diff.contains("+TWO"), "{}", a.diff);
        let b = changes.iter().find(|c| c.path == "b.txt").unwrap();
        assert_eq!(b.status, "new");
        assert!(b.diff.contains("+fresh"));
        assert!(!changes.iter().any(|c| c.path.starts_with(".opencode")), "our own folders are left out");
        let msg = comments_message(&[("a.txt".into(), 2, "keep it lowercase".into())]);
        assert!(msg.contains("- a.txt:2: keep it lowercase"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
