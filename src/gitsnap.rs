//! M2: git-snapshot undo/redo.
//!
//! Before an agent run mutates anything we record the workspace's *dirty*
//! state (`git status --porcelain -uall`) and the contents of those files
//! into a snapshot dir under the state dir. `/undo` restores the most
//! recent snapshot; the state being replaced is pushed to a redo stack.
//!
//! Design notes:
//! - Only shell out to `git` — no git library, as the plan specifies.
//! - Nothing destructive runs unless the user invokes `/undo`; snapshots
//!   are plain file copies outside the workspace (they never touch refs,
//!   the index history, or the user's commits).
//! - Restore semantics: paths dirty at snapshot time are rewritten from
//!   their stored bytes; paths dirty *now* but clean at snapshot time were
//!   changed by the run, so they go back to their index/HEAD content via a
//!   scoped `git checkout -- <path>`; untracked files created after the
//!   snapshot are removed.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub base_sha: Option<String>,
    /// Paths that existed at snapshot time (contents stored in the dir).
    pub present: Vec<String>,
    /// Paths that must not exist after restore (e.g. pre-snapshot renames).
    pub absent: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StackFile {
    entries: Vec<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run git in `cwd`; Err with stderr when git fails.
pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_repo(workspace: &Path) -> bool {
    git(workspace, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Repository root. `git status --porcelain` reports paths relative to
/// THIS, which differs from the workspace when the workspace is a subdir
/// of the repo (e.g. running the agent inside `repo/opencode-rust/`).
fn repo_root(workspace: &Path) -> PathBuf {
    git(workspace, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|| workspace.to_path_buf())
}

/// One `git status --porcelain=v1 -z -uall` entry.
/// `kind` = XY status chars; `path` = worktree path (new path for renames).
fn status_entries(workspace: &Path) -> Result<Vec<(String, String)>> {
    let raw = git(workspace, &["status", "--porcelain=v1", "-z", "-uall"])?;
    let mut out = Vec::new();
    let mut fields = raw.split('\0');
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let kind = entry[..2].to_string();
        let path = entry[3..].to_string();
        if kind.starts_with('R') || kind.starts_with('C') {
            // Rename/copy: -z puts the OLD path in the next NUL field.
            // Pack them as `new\u{1}old` so callers can split.
            let old = fields.next().unwrap_or_default();
            out.push((kind, format!("{path}\u{1}{old}")));
            continue;
        }
        out.push((kind, path));
    }
    Ok(out)
}

/// One dirty path, fully resolved.
struct DirtyEntry {
    /// `??` for untracked; otherwise the two porcelain status chars.
    kind: String,
    /// Workspace-relative, `/`-separated — the manifest key.
    rel: String,
    /// Repo-root-relative — what `git checkout -- <path>` expects.
    root_rel: String,
    /// Absolute path on disk.
    abs: PathBuf,
    /// For renames/copies: the old path (workspace-relative), if any.
    old_rel: Option<String>,
}

/// Status entries with root-relative git paths resolved onto the
/// filesystem. The state dir (which may live inside the workspace) is
/// excluded, and anything outside the workspace is skipped entirely —
/// undo must never touch files above the workspace.
fn dirty_entries(workspace: &Path, state_dir: &Path) -> Result<Vec<DirtyEntry>> {
    // Canonicalize before comparing: `std::env::temp_dir()` (and some
    // shells) yield 8.3 short paths like `OBSESS~1`, while git prints long
    // names — the same directory under two spellings.
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let workspace_c = canon(workspace);
    let root = canon(&repo_root(workspace));
    let state_dir_c = canon(state_dir);
    let skip = state_dir_c
        .strip_prefix(&root)
        .or_else(|_| state_dir_c.strip_prefix(&workspace_c))
        .ok()
        .map(|p| {
            let mut s = p.to_string_lossy().replace('\\', "/");
            if !s.is_empty() {
                s.push('/');
            }
            s
        });
    let mut out = Vec::new();
    for (kind, raw_path) in status_entries(workspace)? {
        let (path, old) = match raw_path.split_once('\u{1}') {
            Some((p, o)) => (p.to_string(), Some(o.to_string())),
            None => (raw_path, None),
        };
        let state_hit = match &skip {
            Some(p) if !p.is_empty() => path.starts_with(p) || path == p.trim_end_matches('/'),
            _ => false,
        };
        if state_hit {
            continue;
        }
        let abs = root.join(&path);
        let rel = match abs.strip_prefix(&workspace_c) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue, // outside the workspace: not ours to touch
        };
        out.push(DirtyEntry { kind, rel, root_rel: path, abs, old_rel: old });
    }
    Ok(out)
}

/// Snapshot ids must be unique even for calls in the same millisecond.
fn next_snap_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    format!("s{}_{:x}", now_ms(), CTR.fetch_add(1, Ordering::Relaxed))
}

/// workspace-specific snapshot root under the state dir.
fn snap_root(state_dir: &Path, workspace: &Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    workspace.to_string_lossy().hash(&mut h);
    state_dir.join(format!("snaps_{:016x}", h.finish()))
}

fn read_stack(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<StackFile>(&t).ok())
        .map(|s| s.entries)
        .unwrap_or_default()
}

fn write_stack(path: &Path, entries: &[String]) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let sf = StackFile { entries: entries.to_vec() };
    std::fs::write(path, serde_json::to_string_pretty(&sf)?)?;
    Ok(())
}

const MAX_STACK: usize = 20;

fn push_stack(path: &Path, id: &str, root: &Path) -> Result<()> {
    let mut entries = read_stack(path);
    entries.push(id.to_string());
    // Prune the oldest beyond the cap — drop their dirs too.
    while entries.len() > MAX_STACK {
        let old = entries.remove(0);
        let _ = std::fs::remove_dir_all(root.join(&old));
    }
    write_stack(path, &entries)
}

fn pop_stack(path: &Path) -> Option<String> {
    let mut entries = read_stack(path);
    let id = entries.pop()?;
    let _ = write_stack(path, &entries);
    Some(id)
}

/// Record the current dirty state. `Ok(None)` = not a git repo (nothing to do).
pub fn take_snapshot(workspace: &Path, state_dir: &Path, label: &str) -> Result<Option<Manifest>> {
    if !is_repo(workspace) {
        return Ok(None);
    }
    let root = snap_root(state_dir, workspace);
    let id = next_snap_id();
    let dir = root.join(&id);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let base_sha = git(workspace, &["rev-parse", "HEAD"]).ok().map(|s| s.trim().to_string());
    let mut present = Vec::new();
    let mut absent = Vec::new();
    for e in dirty_entries(workspace, state_dir)? {
        if let Some(old) = e.old_rel {
            absent.push(old);
        }
        if e.abs.is_file() {
            let bytes = std::fs::read(&e.abs)
                .with_context(|| format!("reading {} for snapshot", e.abs.display()))?;
            let stored = dir.join(&e.rel);
            if let Some(parent) = stored.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&stored, &bytes)
                .with_context(|| format!("storing {}", e.rel))?;
            present.push(e.rel);
        } else {
            // Staged-add later deleted from worktree, or pure index state:
            // treat as absent (restore removes it if present).
            absent.push(e.rel);
        }
    }
    let manifest = Manifest {
        id: id.clone(),
        label: label.to_string(),
        created_at: now_ms(),
        base_sha,
        present,
        absent,
    };
    std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
    Ok(Some(manifest))
}

fn load_manifest(root: &Path, id: &str) -> Result<Manifest> {
    let text = std::fs::read_to_string(root.join(id).join("manifest.json"))
        .with_context(|| format!("snapshot {id} manifest"))?;
    Ok(serde_json::from_str(&text)?)
}

fn restore_manifest(
    workspace: &Path,
    state_dir: &Path,
    root: &Path,
    m: &Manifest,
) -> Result<()> {
    let dir = root.join(&m.id);
    // 1) Rewrite files that existed at snapshot time.
    for p in &m.present {
        let stored = dir.join(p);
        if !stored.is_file() {
            continue;
        }
        let abs = workspace.join(p);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&stored, &abs)
            .with_context(|| format!("restoring {p}"))?;
    }
    // 2) Paths that must not exist (pre-snapshot renames/deletes).
    for p in &m.absent {
        let abs = workspace.join(p);
        if abs.is_file() {
            let _ = std::fs::remove_file(&abs);
        }
    }
    // 3) Dirty now but clean at snapshot time → the run changed it.
    let snapshot_set: std::collections::HashSet<String> =
        m.present.iter().chain(m.absent.iter()).cloned().collect();
    for e in dirty_entries(workspace, state_dir)? {
        if snapshot_set.contains(&e.rel) {
            continue;
        }
        if e.kind == "??" {
            // File the run created: remove it.
            if e.abs.is_file() {
                let _ = std::fs::remove_file(&e.abs);
            }
        } else {
            // Tracked file the run modified: scoped checkout to index/HEAD
            // content == its clean-at-snapshot content. Explicit path only,
            // repo-root-relative (that's what git expects).
            let _ = git(workspace, &["checkout", "--", &e.root_rel]);
        }
    }
    Ok(())
}

fn stack_paths(root: &Path) -> (PathBuf, PathBuf) {
    (root.join("undo.json"), root.join("redo.json"))
}

/// Record a snapshot before an agent run and push it on the undo stack.
/// `Ok(None)` = not a git repo (nothing recorded).
pub fn record_before_run(
    workspace: &Path,
    state_dir: &Path,
    label: &str,
) -> Result<Option<Manifest>> {
    let Some(m) = take_snapshot(workspace, state_dir, label)? else {
        return Ok(None);
    };
    let root = snap_root(state_dir, workspace);
    let (undo_path, _) = stack_paths(&root);
    push_stack(&undo_path, &m.id, &root)?;
    Ok(Some(m))
}

/// Restore the most recent snapshot (undo). `Ok(None)` = nothing to undo.
pub fn undo(workspace: &Path, state_dir: &Path) -> Result<Option<Manifest>> {
    let root = snap_root(state_dir, workspace);
    let (undo_path, redo_path) = stack_paths(&root);
    let Some(id) = pop_stack(&undo_path) else { return Ok(None) };
    // Capture the state we're about to replace so redo can bring it back.
    if let Some(cur) = take_snapshot(workspace, state_dir, "pre-undo")? {
        push_stack(&redo_path, &cur.id, &root)?;
    }
    let m = load_manifest(&root, &id)?;
    restore_manifest(workspace, state_dir, &root, &m)?;
    Ok(Some(m))
}

/// Re-apply the state an `/undo` replaced. `Ok(None)` = nothing to redo.
pub fn redo(workspace: &Path, state_dir: &Path) -> Result<Option<Manifest>> {
    let root = snap_root(state_dir, workspace);
    let (undo_path, redo_path) = stack_paths(&root);
    let Some(id) = pop_stack(&redo_path) else { return Ok(None) };
    if let Some(cur) = take_snapshot(workspace, state_dir, "pre-redo")? {
        push_stack(&undo_path, &cur.id, &root)?;
    }
    let m = load_manifest(&root, &id)?;
    restore_manifest(workspace, state_dir, &root, &m)?;
    Ok(Some(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRepo {
        dir: PathBuf,
        state: PathBuf,
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            let _ = std::fs::remove_dir_all(&self.state);
        }
    }

    fn test_repo() -> TestRepo {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ocrs-snap-{}-{stamp}", std::process::id()));
        let state = dir.join("_state");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init"]).unwrap();
        git(&dir, &["config", "user.email", "t@t"]).unwrap();
        git(&dir, &["config", "user.name", "t"]).unwrap();
        git(&dir, &["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(dir.join("a.txt"), "original a\n").unwrap();
        std::fs::write(dir.join("b.txt"), "original b\n").unwrap();
        git(&dir, &["add", "-A"]).unwrap();
        git(&dir, &["commit", "-m", "init"]).unwrap();
        TestRepo { dir, state }
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn non_git_workspace_is_noop() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ocrs-nogit-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(take_snapshot(&dir, &dir, "x").unwrap().is_none());
        assert!(undo(&dir, &dir).unwrap().is_none());
        assert!(redo(&dir, &dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_undo_restores_modified_created_deleted() {
        let r = test_repo();
        // Dirty state the run should return to: edit a, delete b.
        std::fs::write(r.dir.join("a.txt"), "edited a\n").unwrap();
        std::fs::remove_file(r.dir.join("b.txt")).unwrap();
        let snap = record_before_run(&r.dir, &r.state, "pre-run")
            .unwrap()
            .expect("snapshot in a git repo");
        assert_eq!(snap.label, "pre-run");
        assert!(snap.present.contains(&"a.txt".to_string()));
        assert!(snap.present.contains(&"b.txt".to_string()) == false, "b.txt was deleted");
        assert!(snap.absent.iter().any(|p| p == "b.txt"));
        assert!(snap.base_sha.is_some());

        // The "agent run": modify a tracked file, create a new file, undo the delete.
        std::fs::write(r.dir.join("a.txt"), "agent edit\n").unwrap();
        std::fs::write(r.dir.join("new.txt"), "agent created\n").unwrap();
        std::fs::write(r.dir.join("b.txt"), "resurrected\n").unwrap();

        let restored = undo(&r.dir, &r.state).unwrap().expect("undo works");
        assert_eq!(restored.id, snap.id);
        assert_eq!(read(&r.dir.join("a.txt")), "edited a\n");
        assert!(!r.dir.join("new.txt").exists(), "agent-created file removed");
        assert!(!r.dir.join("b.txt").exists(), "deleted file stays deleted");

        // Redo: bring back the agent state.
        let _ = redo(&r.dir, &r.state).unwrap().expect("redo works");
        assert_eq!(read(&r.dir.join("a.txt")), "agent edit\n");
        assert_eq!(read(&r.dir.join("new.txt")), "agent created\n");
        assert_eq!(read(&r.dir.join("b.txt")), "resurrected\n");

        // Second undo returns to the pre-undo (edited) state.
        undo(&r.dir, &r.state).unwrap().unwrap();
        assert_eq!(read(&r.dir.join("a.txt")), "edited a\n");
        assert!(!r.dir.join("new.txt").exists());
    }

    #[test]
    fn clean_workspace_snapshots_empty_but_still_undoes() {
        let r = test_repo();
        let snap = record_before_run(&r.dir, &r.state, "clean").unwrap().unwrap();
        assert!(snap.present.is_empty());
        std::fs::write(r.dir.join("c.txt"), "only change\n").unwrap();
        undo(&r.dir, &r.state).unwrap().unwrap();
        assert!(!r.dir.join("c.txt").exists(), "created file rolled back");
    }

    /// Regression: git reports repo-ROOT-relative paths; when the workspace
    /// is a subdir of the repo, snapshots must still capture and restore the
    /// right files — and never touch files OUTSIDE the workspace.
    #[test]
    fn subdir_workspace_resolves_repo_relative_paths() {
        let r = test_repo();
        // Repo has a subdir that will act as the workspace, plus a sibling.
        let ws = r.dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "original a\n").unwrap();
        git(&r.dir, &["add", "-A"]).unwrap();
        git(&r.dir, &["commit", "-m", "ws"]).unwrap();
        // State dir lives INSIDE the workspace — the harder skip case.
        let state = ws.join("_state");

        std::fs::write(ws.join("a.txt"), "edited a\n").unwrap();
        let snap = record_before_run(&ws, &state, "pre-run").unwrap().unwrap();
        assert!(
            snap.present.contains(&"a.txt".to_string()),
            "workspace-relative path captured (got {:?})",
            snap.present
        );
        assert!(
            !snap.present.iter().any(|p| p.starts_with("ws/")),
            "paths are workspace-relative, not repo-relative"
        );

        // The "run" touches the workspace AND a file outside it.
        std::fs::write(ws.join("a.txt"), "agent edit\n").unwrap();
        std::fs::write(ws.join("new.txt"), "agent created\n").unwrap();
        std::fs::write(r.dir.join("outside.txt"), "agent edit outside\n").unwrap();

        undo(&ws, &state).unwrap().expect("undo works");
        assert_eq!(read(&ws.join("a.txt")), "edited a\n", "restored via root-rel fix");
        assert!(!ws.join("new.txt").exists());
        assert_eq!(
            read(&r.dir.join("outside.txt")),
            "agent edit outside\n",
            "files outside the workspace are never touched"
        );

        redo(&ws, &state).unwrap().expect("redo works");
        assert_eq!(read(&ws.join("a.txt")), "agent edit\n");
        assert_eq!(read(&ws.join("new.txt")), "agent created\n");
    }

    #[test]
    fn stacks_are_separate_and_empty_states_noop() {
        let r = test_repo();
        assert!(undo(&r.dir, &r.state).unwrap().is_none());
        assert!(redo(&r.dir, &r.state).unwrap().is_none());
        take_snapshot(&r.dir, &r.state, "one").unwrap();
        // Redo with an empty redo stack is a no-op even after undo-stack use.
        assert!(redo(&r.dir, &r.state).unwrap().is_none());
    }
}
