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
    let mut cmd = Command::new("git");
    // S-05: like `gitsnap::git`, hooks never see our provider key or token.
    for key in crate::tools::SECRET_ENV_VARS {
        cmd.env_remove(key);
    }
    let out = cmd
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

    /// `git diff --cached <args> <base>`, limited to the workspace folder.
    fn diff_from_base(&self, args: &[&str]) -> Result<String> {
        let mut all = vec!["diff", "--cached"];
        all.extend_from_slice(args);
        all.push(self.base.as_str());
        if !self.prefix.is_empty() {
            // Only changes inside the workspace folder count.
            all.extend(["--", self.prefix.as_str()]);
        }
        git(&self.dir, &all)
    }

    /// What changed so far: the changed files (full paths in the worktree)
    /// and the number of lines added plus removed.
    pub fn changes(&self) -> Result<(Vec<PathBuf>, usize)> {
        git(&self.dir, &["add", "-A"])?;
        let mut files = Vec::new();
        let mut lines = 0;
        for row in self.diff_from_base(&["--numstat", "--no-renames"])?.lines() {
            let mut cols = row.splitn(3, '\t');
            let (Some(add), Some(del), Some(path)) = (cols.next(), cols.next(), cols.next()) else {
                continue;
            };
            // Binary files show "-": count them as one line.
            lines += add.parse::<usize>().unwrap_or(1) + del.parse::<usize>().unwrap_or(0);
            files.push(self.dir.join(path));
        }
        Ok((files, lines))
    }

    /// Write the changes as a patch file in `patch_dir`. `None` = no changes.
    pub fn write_patch(&self, patch_dir: &Path) -> Result<Option<PathBuf>> {
        git(&self.dir, &["add", "-A"])?;
        let patch_dir = std::path::absolute(patch_dir)?;
        std::fs::create_dir_all(&patch_dir)?;
        let patch = patch_dir.join(format!("{}.patch", self.name));
        let output = format!("--output={}", patch.display());
        self.diff_from_base(&["--binary", output.as_str()])?;
        if std::fs::metadata(&patch).map(|m| m.len()).unwrap_or(0) == 0 {
            let _ = std::fs::remove_file(&patch);
            return Ok(None);
        }
        Ok(Some(patch))
    }

    /// Apply a patch from `write_patch` to the real repository's files.
    pub fn apply_to_repo(&self, patch: &Path) -> Result<()> {
        let p = patch.to_string_lossy().to_string();
        git(&self.root, &["apply", "--whitespace=nowarn", &p]).map(|_| ())
    }

    fn save_patch(&self, patch_dir: &Path) -> Result<String> {
        let Some(patch) = self.write_patch(patch_dir)? else {
            return Ok("[isolated worktree] no file changes.".to_string());
        };
        let stat = self.diff_from_base(&["--stat", "--stat-count=20"])?;
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
/// Dropped without [`Isolation::finish`] or [`Isolation::discard`] (an early
/// error return, a panic), the worktree's changes are still saved as a
/// patch, so a failed worker's work is never lost with the worktree.
pub struct Isolation {
    state: IsoState,
    patch_dir: PathBuf,
}

enum IsoState {
    Worktree(Worktree),
    Skipped(String),
    /// Finished or discarded: nothing left to do.
    Done,
}

impl Isolation {
    /// Never fails. When no worktree can be made, the worker runs in the real
    /// workspace as before, and the reason is reported. Runs git: call it
    /// from blocking code (`spawn_blocking` in async code).
    pub fn start(workspace: &Path, state_dir: &Path, label: &str) -> Self {
        let state = match Worktree::create(workspace, state_dir, label) {
            Ok(wt) => IsoState::Worktree(wt),
            Err(e) => IsoState::Skipped(format!("{e:#}")),
        };
        Self { state, patch_dir: state_dir.join("worker-patches") }
    }

    /// [`Isolation::start`] on a blocking thread, for async callers.
    pub async fn start_async(workspace: PathBuf, state_dir: PathBuf, label: String) -> Self {
        let sd = state_dir.clone();
        tokio::task::spawn_blocking(move || Self::start(&workspace, &state_dir, &label))
            .await
            .unwrap_or_else(|e| Self {
                state: IsoState::Skipped(format!("worktree setup crashed: {e}")),
                patch_dir: sd.join("worker-patches"),
            })
    }

    /// The folder for the worker's tools. `None` = the real workspace.
    pub fn workspace(&self) -> Option<PathBuf> {
        match &self.state {
            IsoState::Worktree(wt) => Some(wt.workspace()),
            _ => None,
        }
    }

    /// Why there is no worktree (`None` when the worker is isolated).
    pub fn skipped(&self) -> Option<&str> {
        match &self.state {
            IsoState::Skipped(reason) => Some(reason.as_str()),
            _ => None,
        }
    }

    /// Text for the worker's summary: the patch report, or why the worker
    /// was not isolated. Removes the worktree. Runs git (see `start`).
    pub fn finish(mut self) -> String {
        match std::mem::replace(&mut self.state, IsoState::Done) {
            IsoState::Worktree(mut wt) => wt.report(&self.patch_dir),
            IsoState::Skipped(reason) => {
                format!("[not isolated: {reason}; this worker changed the workspace directly]")
            }
            IsoState::Done => String::new(),
        }
    }

    /// [`Isolation::finish`] on a blocking thread, for async callers.
    pub async fn finish_async(self) -> String {
        tokio::task::spawn_blocking(move || self.finish())
            .await
            .unwrap_or_else(|e| format!("[isolated worktree] collecting the changes crashed: {e}"))
    }

    /// Remove the worktree WITHOUT saving its changes (the worker was
    /// stopped, so its half-done work is dropped on purpose).
    pub fn discard(mut self) {
        self.state = IsoState::Done; // the Worktree's own Drop removes it
    }

    /// [`Isolation::discard`] on a blocking thread, for async callers.
    pub async fn discard_async(self) {
        let _ = tokio::task::spawn_blocking(move || self.discard()).await;
    }
}

impl Drop for Isolation {
    fn drop(&mut self) {
        let IsoState::Worktree(wt) = std::mem::replace(&mut self.state, IsoState::Done) else {
            return;
        };
        let patch_dir = self.patch_dir.clone();
        // T-022: saving the patch and removing the worktree run git several
        // times. Dropped in async code (an early `?`), that would hold up the
        // tokio worker, so the work goes to a blocking thread.
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(move || save_dropped(wt, &patch_dir));
            }
            Err(_) => save_dropped(wt, &patch_dir),
        }
    }
}

/// Save a dropped worker's changes as a patch; the worktree's own Drop then
/// removes it.
fn save_dropped(mut wt: Worktree, patch_dir: &Path) {
    #[cfg(test)]
    tests::note_cleanup(patch_dir);
    let _ = wt.report(patch_dir);
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

/// The tools an isolated role keeps: its own list narrowed to the built-in
/// ones (`None` = every built-in tool). Isolation only ever takes tools
/// away: a `[read, grep]` role in a worktree must not gain bash or write.
pub fn isolated_tools(role_tools: Option<&[String]>) -> Vec<String> {
    let local = local_tool_names();
    match role_tools {
        Some(own) => own.iter().filter(|t| local.contains(t)).cloned().collect(),
        None => local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Which thread saved each dropped worker (by patch folder).
    static CLEANUPS: Mutex<Vec<(PathBuf, std::thread::ThreadId)>> = Mutex::new(Vec::new());

    pub(super) fn note_cleanup(patch_dir: &Path) {
        let mut v = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
        v.push((patch_dir.to_path_buf(), std::thread::current().id()));
    }

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
        let report = iso.finish();
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

    #[test]
    fn isolation_narrows_a_roles_tools_and_never_adds_any() {
        let own = vec!["read".to_string(), "grep".to_string(), "mcp_thing".to_string()];
        assert_eq!(isolated_tools(Some(&own)), ["read", "grep"], "no bash/write gained, MCP dropped");
        let all = isolated_tools(None);
        assert!(all.iter().any(|t| t == "bash") && all.iter().any(|t| t == "read"));
    }

    /// A worker that ends without `finish` (an error returned early) still
    /// gets its changes saved as a patch; `discard` saves nothing. Either
    /// way the worktree and its branch are gone.
    #[test]
    fn dropped_isolation_saves_a_patch_and_discard_does_not() {
        let repo = temp("drop-repo");
        let state = temp("drop-state");
        git(&repo, &["init", "-q"]).unwrap();
        git(&repo, &["config", "user.email", "t@t"]).unwrap();
        git(&repo, &["config", "user.name", "t"]).unwrap();
        git(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "init"]).unwrap();
        let patches = |state: &Path| {
            std::fs::read_dir(state.join("worker-patches")).map(|d| d.count()).unwrap_or(0)
        };

        let iso = Isolation::start(&repo, &state, "worker");
        let ws = iso.workspace().unwrap();
        std::fs::write(ws.join("a.txt"), "worker edit\n").unwrap();
        drop(iso);
        assert_eq!(patches(&state), 1, "the dropped worker's change is saved");
        assert!(!ws.exists(), "worktree removed after saving");

        let iso = Isolation::start(&repo, &state, "worker");
        let ws = iso.workspace().unwrap();
        std::fs::write(ws.join("a.txt"), "stopped edit\n").unwrap();
        iso.discard();
        assert_eq!(patches(&state), 1, "a discarded worker saves nothing");
        assert!(!ws.exists());
        let branches = git(&repo, &["branch", "--list", "ocrs/*"]).unwrap();
        assert!(branches.trim().is_empty(), "branches deleted: {branches}");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// T-022: dropped on a tokio thread, the git work runs on a blocking
    /// thread, not inline; the patch is still saved and the worktree removed.
    #[tokio::test(flavor = "current_thread")]
    async fn drop_in_a_runtime_saves_on_a_blocking_thread() {
        let repo = temp("rt-drop-repo");
        let state = temp("rt-drop-state");
        git(&repo, &["init", "-q"]).unwrap();
        git(&repo, &["config", "user.email", "t@t"]).unwrap();
        git(&repo, &["config", "user.name", "t"]).unwrap();
        git(&repo, &["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one
").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "init"]).unwrap();
        let iso = Isolation::start(&repo, &state, "worker");
        let ws = iso.workspace().unwrap();
        std::fs::write(ws.join("a.txt"), "worker edit
").unwrap();
        let patch_dir = iso.patch_dir.clone();

        drop(iso);
        let me = std::thread::current().id();
        let saved_by = |dir: &Path| {
            let v = CLEANUPS.lock().unwrap_or_else(|e| e.into_inner());
            v.iter().find(|(d, _)| d == dir).map(|(_, t)| *t)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while ws.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let thread = saved_by(&patch_dir).expect("the dropped worker was saved");
        assert_ne!(thread, me, "git ran inline on the runtime thread");
        assert!(!ws.exists(), "worktree removed");
        let n = std::fs::read_dir(&patch_dir).map(|d| d.count()).unwrap_or(0);
        assert_eq!(n, 1, "the dropped worker's change is saved");
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
        assert!(iso.finish().contains("not isolated"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }
}
