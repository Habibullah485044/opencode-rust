//! M3: Cursor-style inline `/edit` — prompt → ONE completion (no agent
//! loop) → diff preview → accept. Plus the purely-local `Tab` next-edit
//! jump tracker (no model call, per the plan: "cheap, no model call
//! required for the jump").

use crate::server::{one_shot, Shared};
use crate::sessions::diff_lines;
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Inline edit makes the model echo the whole file, so cap it.
pub const MAX_EDIT_BYTES: usize = 100_000;

const SYSTEM: &str = "You are an inline edit engine. You receive the full content of a file \
and an instruction. Respond with the COMPLETE new content of the file only — no prose, \
no explanations, no markdown fences. Make only the changes the instruction asks for.";

/// A ready-to-accept edit: diff preview + the data needed to apply it.
#[derive(Debug, Clone)]
pub struct EditProposal {
    /// Workspace-relative path as the user typed it.
    pub path: String,
    pub abs: PathBuf,
    pub old: String,
    pub new: String,
    /// 1-based first changed line in the new content (the jump target).
    pub line: usize,
    pub diff: Vec<String>,
}

/// Resolve a user-typed path inside the workspace. Inline edit is
/// workspace-scoped: reject absolute paths and `..` escapes.
pub fn resolve_in_workspace(workspace: &Path, path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if p.as_os_str().is_empty() {
        bail!("empty path");
    }
    if p.is_absolute() {
        bail!("inline edit is limited to the workspace (absolute path not allowed)");
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("path may not contain `..`");
    }
    let abs = workspace.join(p);
    if !abs.starts_with(workspace) {
        bail!("path escapes the workspace");
    }
    Ok(abs)
}

/// Ask the model for the new file content, then build the diff preview.
pub async fn propose(
    st: &Shared,
    path: &str,
    instruction: &str,
    abort: &AtomicBool,
) -> Result<EditProposal> {
    if instruction.trim().is_empty() {
        bail!("edit instruction is empty");
    }
    let abs = resolve_in_workspace(&st.workspace, path)?;
    let old = std::fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))?;
    if old.len() > MAX_EDIT_BYTES {
        bail!("file is {} bytes; inline /edit caps at {MAX_EDIT_BYTES} bytes", old.len());
    }
    let user = format!("FILE: {path}\n\n<file>\n{old}\n</file>\n\nINSTRUCTION: {instruction}");
    let raw = one_shot(st, SYSTEM, &user, abort, false).await?;
    let new = strip_fences(&raw);
    if new == old {
        bail!("model returned the file unchanged");
    }
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let diff = diff_lines(
        &old_lines.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &new_lines.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    );
    let line = first_changed_line(&old_lines, &new_lines);
    Ok(EditProposal { path: path.to_string(), abs, old, new, line, diff })
}

/// Apply an accepted preview — but only if the file is still exactly what
/// the preview was generated from (local race guard).
pub fn apply(abs: &Path, expected_old: &str, new: &str) -> Result<()> {
    // Only existing files reach apply (propose reads the file first), so a
    // missing/changed file here means the workspace moved under us.
    let current = std::fs::read_to_string(abs).with_context(|| format!("reading {}", abs.display()))?;
    if current != expected_old {
        bail!("file changed since the preview was generated — re-run /edit");
    }
    std::fs::write(abs, new).with_context(|| format!("writing {}", abs.display()))?;
    Ok(())
}

/// Strip a markdown code fence the model wrapped its answer in, while
/// leaving a raw file that merely *contains* fences alone.
pub fn strip_fences(s: &str) -> String {
    let t = s.trim();
    let Some(first) = t.find("```") else { return s.to_string() };
    // Only treat leading text as preamble if it is short (a file that
    // starts with or contains ``` deep inside keeps its content intact).
    if t[..first].trim().len() > 100 {
        return s.to_string();
    }
    let Some(nl) = t[first..].find('\n') else { return s.to_string() };
    let body_start = first + nl + 1;
    match t[body_start..].rfind("\n```") {
        Some(end) => t[body_start..][..end].to_string(),
        None => s.to_string(),
    }
}

/// 1-based line number of the first difference (the hunk the Tab jump
/// should land on).
fn first_changed_line(old: &[&str], new: &[&str]) -> usize {
    let mut i = 0;
    while i < old.len() && i < new.len() && old[i] == new[i] {
        i += 1;
    }
    i.min(new.len().saturating_sub(1)) + 1
}

// ---------------------------------------------------------------------------
// Next-edit jump: local heuristic, no model call. Sites are kept sorted by
// (path, line) so Tab order is deterministic regardless of apply order.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpSite {
    pub path: String,
    pub line: usize,
}

#[derive(Debug, Default)]
pub struct JumpTracker {
    sites: Vec<JumpSite>,
    idx: Option<usize>,
}

impl JumpTracker {
    /// Insert a jump site (deduped, sorted). The current position keeps
    /// pointing at the same site after an earlier site is inserted.
    pub fn record(&mut self, site: JumpSite) {
        if self.sites.contains(&site) {
            return;
        }
        let key = (site.path.as_str(), site.line);
        let pos = self
            .sites
            .iter()
            .position(|s| (s.path.as_str(), s.line) > key)
            .unwrap_or(self.sites.len());
        self.sites.insert(pos, site);
        if let Some(i) = self.idx {
            if pos <= i {
                self.idx = Some(i + 1);
            }
        }
    }

    /// Cycle forward with wrap-around. `None` when no sites are recorded.
    pub fn next(&mut self) -> Option<JumpSite> {
        if self.sites.is_empty() {
            return None;
        }
        let i = match self.idx {
            None => 0,
            Some(i) if i + 1 >= self.sites.len() => 0,
            Some(i) => i + 1,
        };
        self.idx = Some(i);
        Some(self.sites[i].clone())
    }

    /// Cycle backward with wrap-around (Shift+Tab).
    pub fn prev(&mut self) -> Option<JumpSite> {
        if self.sites.is_empty() {
            return None;
        }
        let i = match self.idx {
            None => self.sites.len() - 1,
            Some(0) => self.sites.len() - 1,
            Some(i) => i - 1,
        };
        self.idx = Some(i);
        Some(self.sites[i].clone())
    }

    pub fn len(&self) -> usize {
        self.sites.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    /// 1-based position of the current site (for "[jump] 2/3 …" display).
    pub fn position(&self) -> Option<usize> {
        self.idx.map(|i| i + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_absolute_and_parent_escapes() {
        let ws = Path::new("/ws");
        assert!(resolve_in_workspace(ws, "src/main.rs").is_ok());
        assert!(resolve_in_workspace(ws, "").is_err());
        assert!(resolve_in_workspace(ws, "..\\evil.rs").is_err());
        assert!(resolve_in_workspace(ws, "a/../b.rs").is_err(), "even contained .. is rejected");
        // Absolute paths: both unix and windows forms fail on either OS
        // (windows drive paths are absolute only on windows, so test that
        // case conditionally).
        assert!(resolve_in_workspace(ws, "/etc/passwd").is_err());
        if Path::new("C:\\x").is_absolute() {
            assert!(resolve_in_workspace(ws, "C:\\Windows\\system.ini").is_err());
        }
    }

    #[test]
    fn resolve_keeps_relative_paths_inside_workspace() {
        let ws = Path::new("/ws");
        let abs = resolve_in_workspace(ws, "nested/dir/file.txt").unwrap();
        assert!(abs.starts_with(ws));
        assert!(abs.ends_with(Path::new("nested/dir/file.txt")) || abs.ends_with(Path::new("nested\\dir\\file.txt")));
    }

    #[test]
    fn strips_model_fence_wrapping() {
        assert_eq!(strip_fences("```rust\nfn a() {}\n```"), "fn a() {}");
        assert_eq!(strip_fences("Sure!\n```rust\nfn a() {}\n```"), "fn a() {}");
        assert_eq!(strip_fences("plain content"), "plain content");
        // A raw file containing fences but with a long preamble stays intact.
        let raw = format!("{}\n```\ncode\n```", "x".repeat(150));
        assert_eq!(strip_fences(&raw), raw);
        // Fence with no closing → return untouched.
        assert_eq!(strip_fences("```rust\nunclosed"), "```rust\nunclosed");
    }

    #[test]
    fn first_changed_line_positions() {
        let old = vec!["a", "b", "c"];
        let new = vec!["a", "B", "c"];
        assert_eq!(first_changed_line(&old, &new), 2);
        let new2 = vec!["a", "b", "c", "d"];
        assert_eq!(first_changed_line(&old, &new2), 4, "append lands on the new line");
        let new3 = vec!["x", "y"];
        assert_eq!(first_changed_line(&old, &new3), 1);
    }

    #[test]
    fn jump_tracker_sorted_deduped_and_cycles() {
        let mut t = JumpTracker::default();
        assert!(t.next().is_none());
        assert!(t.prev().is_none());
        t.record(JumpSite { path: "b.rs".into(), line: 5 });
        t.record(JumpSite { path: "a.rs".into(), line: 10 });
        t.record(JumpSite { path: "a.rs".into(), line: 3 });
        t.record(JumpSite { path: "a.rs".into(), line: 3 }); // duplicate ignored
        assert_eq!(t.len(), 3);
        // Sorted: a.rs:3, a.rs:10, b.rs:5
        assert_eq!(t.next().unwrap(), JumpSite { path: "a.rs".into(), line: 3 });
        assert_eq!(t.position(), Some(1));
        assert_eq!(t.next().unwrap(), JumpSite { path: "a.rs".into(), line: 10 });
        assert_eq!(t.next().unwrap(), JumpSite { path: "b.rs".into(), line: 5 });
        assert_eq!(t.next().unwrap(), JumpSite { path: "a.rs".into(), line: 3 }, "wraps");
        assert_eq!(t.prev().unwrap(), JumpSite { path: "b.rs".into(), line: 5 }, "backwards wraps");
    }

    #[test]
    fn jump_tracker_keeps_current_site_when_earlier_one_arrives() {
        let mut t = JumpTracker::default();
        t.record(JumpSite { path: "m.rs".into(), line: 50 });
        t.record(JumpSite { path: "m.rs".into(), line: 60 });
        assert_eq!(t.next().unwrap().line, 50);
        // An earlier site arrives while we are on #2.
        t.record(JumpSite { path: "a.rs".into(), line: 1 });
        assert_eq!(t.position(), Some(2), "still pointing at m.rs:50");
        assert_eq!(t.next().unwrap().line, 60);
    }

    #[test]
    fn apply_writes_and_guards_races() {
        let dir = std::env::temp_dir().join(format!("ocrs-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, "old").unwrap();

        assert!(apply(&f, "wrong-old", "new").is_err(), "stale preview rejected");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "old", "nothing written on reject");

        apply(&f, "old", "new").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");

        // A file that vanished since the preview is a race, not a create.
        std::fs::remove_file(&f).unwrap();
        assert!(apply(&f, "new", "newer").is_err(), "missing file rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn propose_end_to_end_with_mock_provider() {
        use axum::{routing::post, Router};
        use serde_json::Value;

        let dir = std::env::temp_dir().join(format!("ocrs-edit-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();

        // Mock: returns the file with println! added, fenced (model habit).
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<Value>| async move {
                let is_edit = body["messages"][0]["content"]
                    .as_str()
                    .map(|c| c.contains("inline edit engine"))
                    .unwrap_or(false);
                assert!(is_edit, "inline edit must use the edit system prompt");
                let content = if is_edit {
                    "```rust\nfn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n```"
                } else {
                    "unexpected"
                };
                let payload = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::to_string(&serde_json::json!({
                        "choices": [{"delta": {"content": content}, "finish_reason": null}]
                    }))
                    .unwrap()
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    payload,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = format!("http://{addr}/v1");
        cfg.provider.model = "mock".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let abort = AtomicBool::new(false);

        let p = propose(&st, "app.rs", "log x", &abort).await.unwrap();
        assert_eq!(p.path, "app.rs");
        assert!(p.new.contains("println!"), "fence stripped, new content present");
        assert!(!p.new.contains("```"));
        assert_eq!(p.line, 3, "first changed line is inside main");
        assert!(p.diff.iter().any(|l| l.starts_with('+') && l.contains("println!")));
        assert!(p.diff.iter().any(|l| l.starts_with(' ')), "context lines kept");

        apply(&p.abs, &p.old, &p.new).unwrap();
        assert!(std::fs::read_to_string(dir.join("app.rs")).unwrap().contains("println!"));

        // Missing file → clear error.
        let err = propose(&st, "nope.rs", "x", &abort).await.unwrap_err();
        assert!(err.to_string().contains("reading"));

        // Path escape → rejected before any IO.
        let err = propose(&st, "../outside.rs", "x", &abort).await.unwrap_err();
        assert!(err.to_string().contains(".."), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
