//! The `glob` and `grep` tools: walks that honour .gitignore, skip `.git`
//! and symlinks, and hide what a read rule denies.

use super::paths::confine_path;
use super::truncate;
use anyhow::{Context, Result};
use std::path::Path;

#[cfg(test)]
pub(super) fn glob_files(workspace: &Path, pattern: &str, sub: Option<&str>) -> Result<String> {
    glob_files_with(workspace, pattern, sub, &crate::permissions::ReadFilter::default())
}

/// A walked entry that is a regular file. Symlinks are skipped: `is_file`
/// and reading would follow them out of the workspace.
fn is_plain_file(entry: &ignore::DirEntry) -> bool {
    entry.file_type().is_some_and(|t| t.is_file())
}

pub(super) fn glob_files_with(
    workspace: &Path,
    pattern: &str,
    sub: Option<&str>,
    hidden: &crate::permissions::ReadFilter,
) -> Result<String> {
    use ignore::WalkBuilder;
    let root = confine_path(workspace, sub.unwrap_or("."))?;
    let mut matches = Vec::new();
    // B-32: `{a,b}` alternatives become separate patterns.
    let patterns = expand_braces(pattern);
    // require_git(false): honor .gitignore even in non-git workspaces.
    // B-32: never walk into `.git`.
    for entry in WalkBuilder::new(&root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        let path = entry.path();
        if !is_plain_file(&entry) || hidden.hides(workspace, path) {
            continue;
        }
        // Match relative to the folder that was searched ...
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if patterns.iter().any(|p| match_glob(p, &rel_str)) {
            // ... but print relative to the workspace, like grep (B-32), so
            // the path works directly with read and edit.
            let shown = path.strip_prefix(workspace).unwrap_or(path);
            matches.push(shown.to_string_lossy().replace('\\', "/"));
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

/// Expand `{a,b}` groups into separate glob patterns (B-32). Groups do not
/// nest; a `{` without a closing `}` is kept as plain text.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else { return vec![pattern.to_string()] };
    let Some(len) = pattern[open..].find('}') else { return vec![pattern.to_string()] };
    let close = open + len;
    let (head, body, tail) = (&pattern[..open], &pattern[open + 1..close], &pattern[close + 1..]);
    body.split(',').flat_map(|alt| expand_braces(&format!("{head}{alt}{tail}"))).collect()
}

/// Glob with `*`, `?` (not across `/`), `**` and `**/` (across folders;
/// B-32: `**/x` does not match `foox`). The permission matcher in path mode.
pub(super) fn match_glob(pattern: &str, text: &str) -> bool {
    crate::permissions::wildcard_match(pattern, text, true)
}

#[cfg(test)]
pub(super) fn grep_files(workspace: &Path, pattern: &str, sub: Option<&str>, case_insensitive: bool) -> Result<String> {
    grep_files_with(workspace, pattern, sub, case_insensitive, &crate::permissions::ReadFilter::default())
}

pub(super) fn grep_files_with(
    workspace: &Path,
    pattern: &str,
    sub: Option<&str>,
    case_insensitive: bool,
    hidden: &crate::permissions::ReadFilter,
) -> Result<String> {
    use ignore::WalkBuilder;
    let re = {
        let mut b = regex::RegexBuilder::new(pattern);
        b.case_insensitive(case_insensitive);
        b.build().with_context(|| format!("invalid regex `{pattern}`"))?
    };
    let root = confine_path(workspace, sub.unwrap_or("."))?;
    let mut out: Vec<String> = Vec::new();
    // require_git(false): honor .gitignore even in non-git workspaces.
    // B-32: count files that cannot be searched, and never walk into `.git`.
    let mut skipped = 0usize;
    for entry in WalkBuilder::new(&root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        let path = entry.path();
        if !is_plain_file(&entry) || hidden.hides(workspace, path) {
            continue;
        }
        // Skip big files
        let Ok(meta) = std::fs::symlink_metadata(path) else { continue };
        if meta.len() > 2_000_000 {
            skipped += 1;
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            skipped += 1; // binary or not UTF-8 (Latin-1, UTF-16, ...)
            continue;
        };
        if text.bytes().any(|b| b == 0) {
            skipped += 1;
            continue;
        }
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let rel = path.strip_prefix(workspace).unwrap_or(path);
                out.push(format!(
                    "{}:{}:{}",
                    rel.display().to_string().replace('\\', "/"),
                    i + 1,
                    truncate(line.trim_end(), 300)
                ));
                if out.len() >= 200 {
                    out.push("… [match limit 200 reached]".into());
                    return Ok(out.join("\n"));
                }
            }
        }
    }
    let note = if skipped > 0 {
        format!("\n… [{skipped} file(s) not searched: larger than 2 MB, binary, or not UTF-8]")
    } else {
        String::new()
    };
    if out.is_empty() {
        Ok(format!("no matches for `{pattern}`{note}"))
    } else {
        Ok(format!("{}{note}", out.join("\n")))
    }
}

#[cfg(test)]
mod b32_glob_tests {
    use super::*;
    use std::path::PathBuf;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b32-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("src")).unwrap();
        p
    }

    #[test]
    fn glob_prints_workspace_paths_and_expands_braces() {
        let ws = ws("glob");
        for f in ["src/a.rs", "src/b.toml", "src/c.txt"] {
            std::fs::write(ws.join(f), "x").unwrap();
        }
        let out = glob_files(&ws, "*.{rs,toml}", Some("src")).unwrap();
        assert!(out.contains("src/a.rs") && out.contains("src/b.toml"), "{out}");
        assert!(!out.contains("c.txt"), "{out}");
        assert_eq!(expand_braces("x.{a,b}.{c,d}").len(), 4);
        assert_eq!(expand_braces("no{brace"), vec!["no{brace"]);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn double_star_slash_matches_whole_names_only() {
        assert!(match_glob("**/x.rs", "a/x.rs"));
        assert!(match_glob("**/x.rs", "x.rs"));
        assert!(!match_glob("**/x.rs", "foox.rs"));
        assert!(!match_glob("**/x.rs", "a/foox.rs"));
    }
}

#[cfg(test)]
mod b32_grep_tests {
    use super::*;
    use std::path::PathBuf;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b32-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("src")).unwrap();
        p
    }

    #[test]
    fn git_folder_is_skipped_and_unsearchable_files_are_reported() {
        let ws = ws("grep");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git").join("config"), "token").unwrap();
        std::fs::write(ws.join("a.txt"), "token").unwrap();
        std::fs::write(ws.join("latin1.txt"), b"token caf\xe9").unwrap();
        let out = grep_files(&ws, "token", None, false).unwrap();
        assert!(out.contains("a.txt:1:"), "{out}");
        assert!(!out.contains(".git"), "{out}");
        assert!(out.contains("1 file(s) not searched"), "{out}");
        assert!(glob_files(&ws, "**/config", None).unwrap().contains("no files matched"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
