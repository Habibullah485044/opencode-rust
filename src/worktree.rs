//! F-05: git-worktree isolation for `worker` subagents and team workers.
//!
//! With `agent.isolate_workers = true`, each worker gets its own git worktree
//! under the state dir. The worktree starts from a snapshot of the CURRENT
//! files (uncommitted and untracked files included). When the worker is
//! done, its changes are saved as a patch file and described in its summary.
//! Nothing is applied to the real workspace: the parent agent or the user
//! decides. The worktree and its branch are removed afterwards.

use crate::gitsnap::git;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// One worker's private checkout. Dropping it removes the worktree and its
/// branch, unless `keep` is set (the changes could not be saved).
pub struct Worktree {
    /// Root of the real repository. Git commands run here; patch paths are
    /// relative to it.
    root: PathBuf,
    /// The worktree folder, under `<state_dir>/workers/`.
    dir: PathBuf,
    /// Where the workspace sits inside the repo: "" or "sub/dir".
    prefix: String,
    /// The commit the worktree starts from (a snapshot of the current files).
    base: String,
    branch: String,
    name: String,
    keep: bool,
}

/// Run git with a private index file and a fixed identity, so the user's
/// real index is never touched and `commit-tree` works without a git user.
fn git_private_index(cwd: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_AUTHOR_NAME", "opencode-rust")
        .env("GIT_AUTHOR_EMAIL", "opencode-rust@localhost")
        .env("GIT_COMMITTER_NAME", "opencode-rust")
        .env("GIT_COMMITTER_EMAIL", "opencode-rust@localhost")
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Worktree {
    /// Make a worktree for one worker. The error text says why not (shown to
    /// the parent and the user): no git repository, no commit yet, ...
    pub fn create(workspace: &Path, state_dir: &Path, label: &str) -> Result<Self> {
        let inside = git(workspace, &["rev-parse", "--is-inside-work-tree"])
            .map(|s| s.trim() == "true")
            .unwrap_or(false);
        if !inside {
            bail!("the workspace is not a git repository");
        }
        let head = git(workspace, &["rev-parse", "--verify", "HEAD"])
            .map_err(|_| anyhow!("the repository has no commit yet"))?
            .trim()
            .to_string();
        let root = PathBuf::from(git(workspace, &["rev-parse", "--show-toplevel"])?.trim());
        let prefix = git(workspace, &["rev-parse", "--show-prefix"])?
            .trim()
            .trim_end_matches('/')
            .to_string();
        let workers = std::path::absolute(state_dir.join("workers"))?;
        std::fs::create_dir_all(&workers)?;
        // `git add -A` below must never pick up our own worktrees.
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if canon(&workers).starts_with(canon(&root)) {
            bail!("the state dir {} is inside the repository", state_dir.display());
        }
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let name = format!("{label}-{stamp}-{}", COUNTER.fetch_add(1, Ordering::Relaxed));
        let dir = workers.join(&name);
        let index = workers.join(format!("{name}.index"));
        // Snapshot the current files (changes and untracked files, not ignored
        // ones) as a commit, through the private index.
        let snapshot = (|| -> Result<String> {
            git_private_index(&root, &index, &["read-tree", &head])?;
            git_private_index(&root, &index, &["add", "-A"])?;
            let tree = git_private_index(&root, &index, &["write-tree"])?;
            let args = ["commit-tree", "--no-gpg-sign", tree.trim(), "-p", head.as_str(), "-m", "worker base"];
            Ok(git_private_index(&root, &index, &args)?.trim().to_string())
        })();
        let _ = std::fs::remove_file(&index);
        let base = snapshot?;
        let branch = format!("ocrs/{name}");
        let dir_arg = dir.to_string_lossy().to_string();
        let _ = git(&root, &["worktree", "prune"]); // forget worktrees whose folder is gone
        git(&root, &["worktree", "add", "-b", &branch, &dir_arg, &base])?;
        Ok(Self { root, dir, prefix, base, branch, name, keep: false })
    }

    /// The folder the worker works in: the worktree plus the same subfolder
    /// the real workspace has inside the repository.
    pub fn workspace(&self) -> PathBuf {
        if self.prefix.is_empty() {
            self.dir.clone()
        } else {
            self.dir.join(&self.prefix)
        }
    }

    /// Save the worker's changes as a patch and describe them for the parent.
    /// If saving fails, the worktree is kept so no work is lost.
    fn report(&mut self, patch_dir: &Path) -> String {
        match self.save_patch(patch_dir) {
            Ok(text) => text,
            Err(e) => {
                self.keep = true;
                format!(
                    "[isolated worktree] could not save the changes as a patch ({e:#}). \
                     They are kept in {} on branch {}.",
                    self.dir.display(),
                    self.branch
                )
            }
        }
    }

    fn save_patch(&self, patch_dir: &Path) -> Result<String> {
        git(&self.dir, &["add", "-A"])?;
        let patch_dir = std::path::absolute(patch_dir)?;
        std::fs::create_dir_all(&patch_dir)?;
        let patch = patch_dir.join(format!("{}.patch", self.name));
        let output = format!("--output={}", patch.display());
        let mut diff = vec!["diff", "--cached", "--binary", output.as_str(), self.base.as_str()];
        let mut stat = vec!["diff", "--cached", "--stat", "--stat-count=20", self.base.as_str()];
        if !self.prefix.is_empty() {
            // Only changes inside the workspace folder come back.
            diff.extend(["--", self.prefix.as_str()]);
            stat.extend(["--", self.prefix.as_str()]);
        }
        git(&self.dir, &diff)?;
        if std::fs::metadata(&patch).map(|m| m.len()).unwrap_or(0) == 0 {
            let _ = std::fs::remove_file(&patch);
            return Ok("[isolated worktree] no file changes.".to_string());
        }
        let stat = git(&self.dir, &stat)?;
        let (p, r) = (patch.display(), self.root.display());
        Ok(format!(
            "[isolated worktree] changes saved as a patch, NOT applied to the workspace yet.\n\
             Patch: {p}\n\
             To apply: run `git -C \"{r}\" apply --check \"{p}\"`, then the same without --check.\n\
             If it does not apply cleanly, tell the user instead of forcing it.\n{}",
            stat.trim_end()
        ))
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let dir = self.dir.to_string_lossy().to_string();
        let _ = git(&self.root, &["worktree", "remove", "--force", &dir]);
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = git(&self.root, &["worktree", "prune"]);
        let _ = git(&self.root, &["branch", "-D", &self.branch]);
    }
}

/// A worker's isolation: its worktree, or the reason it runs without one.
pub enum Isolation {
    Worktree(Worktree),
    Skipped(String),
}

impl Isolation {
    /// Never fails. When no worktree can be made, the worker runs in the real
    /// workspace as before, and the reason is reported.
    pub fn start(workspace: &Path, state_dir: &Path, label: &str) -> Self {
        match Worktree::create(workspace, state_dir, label) {
            Ok(wt) => Isolation::Worktree(wt),
            Err(e) => Isolation::Skipped(format!("{e:#}")),
        }
    }

    /// The folder for the worker's tools. `None` = the real workspace.
    pub fn workspace(&self) -> Option<PathBuf> {
        match self {
            Isolation::Worktree(wt) => Some(wt.workspace()),
            Isolation::Skipped(_) => None,
        }
    }

    /// Why there is no worktree (`None` when the worker is isolated).
    pub fn skipped(&self) -> Option<&str> {
        match self {
            Isolation::Skipped(reason) => Some(reason.as_str()),
            Isolation::Worktree(_) => None,
        }
    }

    /// Text for the worker's summary: the patch report, or why the worker
    /// was not isolated. Removes the worktree.
    pub fn finish(self, state_dir: &Path) -> String {
        match self {
            Isolation::Worktree(mut wt) => wt.report(&state_dir.join("worker-patches")),
            Isolation::Skipped(reason) => {
                format!("[not isolated: {reason}; this worker changed the workspace directly]")
            }
        }
    }
}

/// System-prompt text for a worker that runs in its own worktree.
pub fn isolation_prompt(ws: &Path) -> String {
    format!(
        "\n\n## Isolated copy\n\nYou work in a private git worktree at {}: a full copy of \
         the project. Use this folder instead of any other workspace path named above. Use \
         relative paths and stay inside this folder. Do not commit. When you finish, your \
         changes are collected as a patch for the main agent.",
        ws.display()
    )
}

/// Tools an isolated worker may use: the built-in ones. MCP servers and the
/// browser act on the real workspace, so they would break the isolation.
pub fn local_tool_names() -> Vec<String> {
    crate::tools::tool_specs().into_iter().map(|s| s.function.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("ocrs-wt-{tag}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    /// A worker starts from the current files, its changes come back as a
    /// patch that applies on the real (dirty) workspace, and nothing is left.
    #[test]
    fn worker_changes_come_back_as_a_patch() {
        let repo = temp("repo");
        let state = temp("state");
        git(&repo, &["init", "-q"]).unwrap();
        git(&repo, &["config", "user.email", "t@t"]).unwrap();
        git(&repo, &["config", "user.name", "t"]).unwrap();
        git(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
        git(&repo, &["config", "core.autocrlf", "false"]).unwrap(); // exact bytes on Windows too
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "init"]).unwrap();
        // Uncommitted work the worker must see.
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        std::fs::write(repo.join("u.txt"), "untracked\n").unwrap();
        let status_before = git(&repo, &["status", "--porcelain"]).unwrap();

        let iso = Isolation::start(&repo, &state, "worker");
        let ws = iso.workspace().expect("a git repo gets a worktree");
        assert_eq!(read(&ws.join("a.txt")), "two\n");
        assert_eq!(read(&ws.join("u.txt")), "untracked\n");
        let status_now = git(&repo, &["status", "--porcelain"]).unwrap();
        assert_eq!(status_now, status_before, "real index and files untouched");

        // The "worker": edit one file, add another.
        std::fs::write(ws.join("a.txt"), "three\n").unwrap();
        std::fs::write(ws.join("new.txt"), "from worker\n").unwrap();
        let report = iso.finish(&state);
        assert!(report.contains("NOT applied"), "{report}");
        assert!(report.contains("2 files changed"), "{report}");
        assert!(!ws.exists(), "worktree folder removed");
        let branches = git(&repo, &["branch", "--list", "ocrs/*"]).unwrap();
        assert!(branches.trim().is_empty(), "branch deleted: {branches}");
        assert_eq!(read(&repo.join("a.txt")), "two\n", "nothing applied yet");

        let patch = report
            .lines()
            .find_map(|l| l.strip_prefix("Patch: "))
            .expect("patch path in the report")
            .to_string();
        git(&repo, &["apply", &patch]).unwrap();
        assert_eq!(read(&repo.join("a.txt")), "three\n");
        assert_eq!(read(&repo.join("new.txt")), "from worker\n");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// No git repository: the worker runs as before, and the summary says so.
    #[test]
    fn non_git_workspace_runs_without_isolation() {
        let dir = temp("plain");
        let state = temp("plain-state");
        let iso = Isolation::start(&dir, &state, "worker");
        assert!(iso.workspace().is_none());
        let reason = iso.skipped().unwrap_or_default().to_string();
        assert!(reason.contains("not a git repository"), "{reason}");
        assert!(iso.finish(&state).contains("not isolated"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }
}
