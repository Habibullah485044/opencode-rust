//! The open chat's project folder: where plain chats work, its git state,
//! its files (`@` autocomplete) and opening it in a code editor.

use super::*;

/// Where plain chats work. The start folder, unless it is no project: Habi's
/// own source (`cargo run` there put a user's mission into this code), the
/// folder the program sits in, the home folder, a system folder or a drive
/// root (a double-clicked app starts in one of those). Then `~/Habi Projects`.
pub fn chat_home(start: &std::path::Path) -> std::path::PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(std::path::PathBuf::from);
    let fallback = home.clone().unwrap_or_else(std::env::temp_dir).join("Habi Projects");
    let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let start_c = canon(start);
    let is_habi_source = std::fs::read_to_string(start.join("Cargo.toml"))
        .is_ok_and(|t| t.contains("name = \"opencode-rust\""));
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(canon));
    let lower = start_c.to_string_lossy().to_lowercase().replace('/', "\\");
    let system = ["\\windows", "\\program files", "\\programdata"].iter().any(|s| lower.contains(s));
    let unsuitable = is_habi_source
        || start_c.parent().is_none()
        || exe_dir.as_deref() == Some(start_c.as_path())
        || home.as_deref().map(canon).as_deref() == Some(start_c.as_path())
        || system;
    if unsuitable { fallback } else { start.to_path_buf() }
}

/// The folder of the open chat's project (the start folder without one).
pub(super) fn chat_root(gs: &GuiState, st: &server::AppState) -> std::path::PathBuf {
    gs.current_project()
        .and_then(|id| gs.projects.iter().find(|p| p.id == id).map(|p| std::path::PathBuf::from(&p.root)))
        .unwrap_or_else(|| st.workspace.clone())
}

/// Branch name and number of changed files, when `root` is in a git repo.
pub(super) fn git_state(root: &std::path::Path) -> Option<(String, usize)> {
    let branch = crate::gitsnap::git(root, &["branch", "--show-current"]).ok()?;
    let branch = branch.trim();
    let branch = if branch.is_empty() { "detached".to_string() } else { branch.to_string() };
    let changed = crate::gitsnap::git(root, &["status", "--porcelain", "--", "."])
        .map(|s| s.lines().filter(|l| !l[3.min(l.len())..].starts_with(".opencode/")).count())
        .unwrap_or(0);
    Some((branch, changed))
}

/// Files of the project whose path contains `query` (case-insensitive),
/// shortest paths first; `.gitignore` is respected.
pub(super) fn find_project_files(root: &std::path::Path, query: &str, max: usize) -> Vec<String> {
    let q = query.to_lowercase().replace('\\', "/");
    let mut hits: Vec<String> = Vec::new();
    let walk = ignore::WalkBuilder::new(root).hidden(true).max_depth(Some(12)).build();
    for entry in walk.flatten().take(20_000) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if q.is_empty() || rel.to_lowercase().contains(&q) {
            hits.push(rel);
        }
    }
    hits.sort_by_key(|p| (p.len(), p.clone()));
    hits.truncate(max);
    hits
}

/// Code editors that can open a folder, found on this machine: (name, program).
pub(super) fn find_editors() -> Vec<(&'static str, std::path::PathBuf)> {
    let mut found = Vec::new();
    let candidates: &[(&str, &[&str])] = &[
        ("VS Code", &["code.cmd", "code"]),
        ("Cursor", &["cursor.cmd", "cursor"]),
        ("Zed", &["zed.exe", "zed"]),
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    for (name, files) in candidates {
        let hit = std::env::split_paths(&path).find_map(|dir| files.iter().map(|f| dir.join(f)).find(|p| p.is_file()));
        if let Some(p) = hit {
            found.push((*name, p));
        }
    }
    found
}

/// Open `folder` in the editor called `name` (or the first one found).
pub(super) fn open_in_editor(name: Option<&str>, folder: &std::path::Path) -> Result<&'static str, String> {
    let editors = find_editors();
    let (label, program) = editors
        .iter()
        .find(|(n, _)| name.is_none_or(|want| want == *n))
        .or(editors.first())
        .ok_or("no VS Code, Cursor or Zed found on PATH")?;
    let mut cmd = if program.extension().is_some_and(|e| e == "cmd") {
        // A .cmd launcher runs through cmd.exe.
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(program).arg(folder);
        c
    } else {
        let mut c = std::process::Command::new(program);
        c.arg(folder);
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash
    }
    cmd.spawn().map(|_| *label).map_err(|e| format!("could not start {label}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_never_work_in_habis_own_source_or_system_folders() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(chat_home(src).ends_with("Habi Projects"), "Habi's own source is no project");
        let home = std::path::PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap());
        assert!(chat_home(&home).ends_with("Habi Projects"), "the home folder is no project");
        if cfg!(windows) {
            assert!(chat_home(std::path::Path::new("C:\\Windows\\System32")).ends_with("Habi Projects"));
            assert!(chat_home(std::path::Path::new("C:\\")).ends_with("Habi Projects"));
        }
        let project = std::env::temp_dir().join(format!("ocrs-chat-home-{}", std::process::id()));
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(chat_home(&project), project, "a normal folder is the project");
        let _ = std::fs::remove_dir_all(&project);
    }
}
