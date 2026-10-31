//! S-03 path confinement: every path a file tool gets stays inside the
//! workspace (read-only tools may also reach the shared skills folders).

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

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
///
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
        // An absolute path into a skills folder. A relative one that left the
        // workspace (`../x`) is not read relative to the skills folder.
        Err(e) if !is_absolute_raw(raw) => Err(e),
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
pub(super) fn user_skill_file(raw: &str) -> Option<PathBuf> {
    let norm = raw.replace('\\', "/");
    let rest = norm.trim_start_matches("./").strip_prefix(".opencode/skills/")?;
    // Only plain names below the skills folder: `../config.toml` (the API
    // key), `/etc/passwd`, `C:/x` or `x::$DATA` would leave it.
    let rest = Path::new(rest);
    if rest.as_os_str().is_empty()
        || !rest.components().all(|c| matches!(c, std::path::Component::Normal(n) if !is_stream_name(n)))
    {
        return None;
    }
    // A link inside a skills folder may not lead out of it either.
    let inside = |root: &Path, p: PathBuf| -> Option<PathBuf> {
        let real = p.canonicalize().ok()?;
        real.starts_with(root.canonicalize().ok()?).then_some(p)
    };
    let roots = crate::skills::shared_skill_dirs();
    if let Some(direct) = roots.iter().find_map(|r| inside(r, r.join(rest))) {
        return Some(direct);
    }
    let mut found = roots.iter().flat_map(|r| {
        std::fs::read_dir(r)
            .into_iter()
            .flat_map(|d| d.flatten())
            .filter_map(move |e| inside(r, e.path().join(rest)))
    });
    let one = found.next()?;
    found.next().is_none().then_some(one)
}

/// Is `raw` an absolute path (a Git Bash `/c/...` path counts on Windows)?
fn is_absolute_raw(raw: &str) -> bool {
    #[cfg(windows)]
    if let Some(win) = msys_to_windows(raw) {
        return Path::new(&win).is_absolute();
    }
    Path::new(raw).is_absolute()
}

/// Windows: a `:` in a path part names an alternate data stream
/// (`.env::$DATA`, `secret.txt:hidden`), the same file under a name that
/// deny rules do not match. Such names are refused. Elsewhere `:` is an
/// ordinary character.
fn is_stream_name(part: &std::ffi::OsStr) -> bool {
    cfg!(windows) && part.to_string_lossy().contains(':')
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
            std::path::Component::Normal(n) if is_stream_name(n) => {
                bail!("`{raw}` names a data stream (`:` in a file name); use the plain file name")
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
