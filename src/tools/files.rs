//! The `read`, `write` and `list` tools, and the atomic file write the
//! edit tool shares.

use super::paths::{confine_path, confine_read_path};
use anyhow::{bail, Context, Result};
use std::path::Path;

/// `read` with `outline: true` (see `repomap::outline_text`).
pub(super) fn read_outline(workspace: &Path, path: &str) -> Result<String> {
    let abs = confine_read_path(workspace, path)?;
    if abs.is_dir() {
        bail!("{path} is a directory — use the list tool to see its contents");
    }
    let size = std::fs::metadata(&abs).with_context(|| format!("reading {}", abs.display()))?.len();
    if size > READ_MAX_BYTES {
        bail!("{path} is {size} bytes, too big to outline (limit {READ_MAX_BYTES}). Use grep.");
    }
    let bytes = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
    Ok(crate::repomap::outline_text(path, &String::from_utf8_lossy(&bytes), &abs))
}

pub(super) fn read_file(workspace: &Path, path: &str, offset: i64, limit: i64) -> Result<String> {
    let abs = confine_read_path(workspace, path)?;
    if abs.is_dir() {
        bail!("{path} is a directory — use the list tool to see its contents");
    }
    // B-30: refuse huge files before loading them.
    let size = std::fs::metadata(&abs).with_context(|| format!("reading {}", abs.display()))?.len();
    if size > READ_MAX_BYTES {
        bail!(
            "{path} is {size} bytes, too big for the read tool (limit {READ_MAX_BYTES}). \
             Use grep to find the part you need, or bash with head/tail."
        );
    }
    let bytes = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
    // B-30: a NUL byte near the start means binary. Other text is shown even
    // when it is not valid UTF-8 (bad bytes become the � character).
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Ok(format!("(binary file: {path}, {size} bytes)"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let note = if matches!(text, std::borrow::Cow::Owned(_)) {
        "\n… [note: this file is not valid UTF-8; bad bytes are shown as �. The edit tool cannot change it.]"
    } else {
        ""
    };
    let limit = limit.clamp(1, 10_000) as usize;
    // B-30: `offset` is the first line shown, 1-based like the printed
    // numbers; 0 and 1 both mean "from the top".
    let skip = (offset.max(1) - 1) as usize;
    let lines: Vec<&str> = text.lines().collect();
    // A total cap too: 500 long lines (minified code) were one huge result
    // that pushed a whole chat past the model's context.
    let mut out = String::new();
    let mut shown = 0;
    for (i, l) in lines.iter().skip(skip).take(limit).enumerate() {
        let row = format!("{:>6}\t{}", skip + i + 1, cap_line(l));
        if !out.is_empty() && out.len() + row.len() > READ_MAX_TOTAL_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&row);
        shown += 1;
    }
    if shown < limit.min(lines.len().saturating_sub(skip)) {
        out.push_str(&format!(
            "\n… [output cut at {READ_MAX_TOTAL_CHARS} characters; continue with offset {}, or grep for what you need]",
            skip + shown + 1
        ));
    } else if skip + limit < lines.len() {
        out.push_str(&format!(
            "\n… [{} more lines; continue with offset {}]",
            lines.len() - skip - limit,
            skip + limit + 1
        ));
    }
    out.push_str(note);
    Ok(out)
}

/// Largest file the read tool loads, in bytes (B-30).
const READ_MAX_BYTES: u64 = 20_000_000;
/// Longest line the read tool shows, in characters (B-30).
const READ_MAX_LINE_CHARS: usize = 2000;
/// Most characters one read returns (~10K tokens).
const READ_MAX_TOTAL_CHARS: usize = 40_000;

/// Cut one very long line, for example in a minified file (B-30).
fn cap_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.chars().count() <= READ_MAX_LINE_CHARS {
        std::borrow::Cow::Borrowed(line)
    } else {
        let head: String = line.chars().take(READ_MAX_LINE_CHARS).collect();
        std::borrow::Cow::Owned(format!("{head}… [line cut at {READ_MAX_LINE_CHARS} characters]"))
    }
}

pub(super) fn write_file(workspace: &Path, path: &str, content: &str) -> Result<String> {
    let abs = confine_path(workspace, path)?;
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // B-31: the old size from metadata (works for any file, reads nothing).
    let old_len = std::fs::metadata(&abs).ok().filter(|m| m.is_file()).map(|m| m.len());
    write_atomic(&abs, content.as_bytes()).with_context(|| format!("writing {}", abs.display()))?;
    let bytes = content.len();
    Ok(match old_len {
        Some(old) => {
            let delta = bytes as i64 - old as i64;
            format!("wrote {path} ({bytes} bytes, {delta:+} bytes vs previous)")
        }
        None => format!("created {path} ({bytes} bytes)"),
    })
}

/// Write a file so it is never left half written (B-31): write a
/// temporary file in the same folder, then rename it over the target.
/// Keeps the old permissions, refuses read-only files, and falls back to
/// a plain write when the rename is refused (Windows, file held open).
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // A keep-best fix round can put this file back if the round makes things worse.
    crate::keepbest::before_write(path);
    let old_perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    if old_perms.as_ref().map(|p| p.readonly()).unwrap_or(false) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the file is read-only"));
    }
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.ocrs-{}-{n}.tmp", std::process::id()));
    // A write that fails partway (disk full, quota) must not leave the
    // temporary file behind.
    if let Err(e) = std::fs::write(&tmp, data) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(perms) = old_perms {
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(path, data)
        }
    }
}

pub(super) fn list_dir(workspace: &Path, path: &str) -> Result<String> {
    let abs = confine_read_path(workspace, path)?;
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
    // A folder with 100k files would flood the context.
    let more = lines.len().saturating_sub(LIST_MAX_ENTRIES);
    lines.truncate(LIST_MAX_ENTRIES);
    if more > 0 {
        lines.push(format!("… and {more} more (use glob to narrow)"));
    }
    Ok(lines.join("\n"))
}

/// Most entries one `list` call shows.
const LIST_MAX_ENTRIES: usize = 1000;

#[cfg(test)]
mod list_tests {
    use super::*;

    fn ws(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-list-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn folders_first_then_sorted_files_with_sizes() {
        let ws = ws("order");
        std::fs::create_dir_all(ws.join("zdir")).unwrap();
        std::fs::create_dir_all(ws.join("adir")).unwrap();
        std::fs::write(ws.join("b.txt"), "12345").unwrap();
        std::fs::write(ws.join("a.txt"), "").unwrap();
        assert_eq!(list_dir(&ws, ".").unwrap(), "adir/\nzdir/\na.txt (0 B)\nb.txt (5 B)");
        assert_eq!(list_dir(&ws, "adir").unwrap(), "(empty directory)");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_huge_folder_is_capped() {
        let ws = ws("cap");
        for i in 0..1100 {
            std::fs::write(ws.join(format!("f{i:04}.txt")), "").unwrap();
        }
        let out = list_dir(&ws, ".").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1001);
        assert_eq!(lines[0], "f0000.txt (0 B)");
        assert_eq!(lines[999], "f0999.txt (0 B)");
        assert_eq!(lines[1000], "… and 100 more (use glob to narrow)");
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
mod b30_tests {
    use super::*;
    use std::path::PathBuf;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b30-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn offset_is_the_first_line_shown() {
        let ws = ws("offset");
        std::fs::write(ws.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let out = read_file(&ws, "f.txt", 2, 2).unwrap();
        assert!(out.starts_with("     2\ttwo"), "{out}");
        assert!(out.contains("     3\tthree"), "{out}");
        assert!(out.contains("continue with offset 4"), "{out}");
        assert_eq!(read_file(&ws, "f.txt", 0, 1).unwrap(), read_file(&ws, "f.txt", 1, 1).unwrap());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn binary_and_non_utf8_files() {
        let ws = ws("bin");
        std::fs::write(ws.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        assert!(read_file(&ws, "bin.dat", 0, 10).unwrap().starts_with("(binary file"));
        std::fs::write(ws.join("latin1.txt"), b"caf\xe9\n").unwrap();
        let out = read_file(&ws, "latin1.txt", 0, 10).unwrap();
        assert!(out.contains("caf\u{FFFD}"), "{out}");
        assert!(out.contains("not valid UTF-8"), "{out}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn very_long_lines_are_cut() {
        let ws = ws("long");
        std::fs::write(ws.join("min.js"), "x".repeat(10_000)).unwrap();
        let out = read_file(&ws, "min.js", 0, 10).unwrap();
        assert!(out.contains("line cut at 2000 characters"), "{}", out.len());
        assert!(out.len() < 3000, "{}", out.len());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cap_line_counts_chars_not_bytes() {
        let exact = "x".repeat(READ_MAX_LINE_CHARS);
        assert!(matches!(cap_line(&exact), std::borrow::Cow::Borrowed(_)));
        let over = "x".repeat(READ_MAX_LINE_CHARS + 1);
        let cut = cap_line(&over);
        assert_eq!(cut, format!("{exact}… [line cut at {READ_MAX_LINE_CHARS} characters]"));
        // 2000 three-byte characters are 6000 bytes but not too long.
        let wide = "é€".repeat(READ_MAX_LINE_CHARS / 2);
        assert_eq!(cap_line(&wide), wide.as_str());
        let wide_over = format!("{wide}€");
        let cut = cap_line(&wide_over);
        assert!(cut.starts_with(&wide) && cut.ends_with("characters]"), "{cut}");
        assert_eq!(cut.chars().filter(|c| *c == '€').count(), READ_MAX_LINE_CHARS / 2);
    }
}

#[cfg(test)]
mod b31_tests {
    use super::*;
    use crate::tools::edit::edit_file;
    use std::path::PathBuf;

    fn ws(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-b31-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn overwriting_a_non_utf8_file_says_wrote() {
        let ws = ws("msg");
        std::fs::write(ws.join("bin.dat"), [0xffu8, 0xfe, 0x00]).unwrap();
        let msg = write_file(&ws, "bin.dat", "text").unwrap();
        assert!(msg.starts_with("wrote"), "{msg}");
        assert!(msg.contains("+1 bytes vs previous"), "{msg}");
        assert!(write_file(&ws, "new.txt", "abc").unwrap().starts_with("created"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn writes_leave_no_temp_files() {
        let ws = ws("tmp");
        write_file(&ws, "a.txt", "one\n").unwrap();
        edit_file(&ws, "a.txt", "one", "two", false).unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "two\n");
        let leftovers: Vec<String> = std::fs::read_dir(&ws)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".ocrs-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn leftovers(ws: &Path) -> Vec<String> {
        std::fs::read_dir(ws)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".ocrs-") && n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn write_atomic_replaces_refuses_read_only_and_cleans_up() {
        let ws = ws("atomic");
        let f = ws.join("a.txt");
        write_atomic(&f, b"first").unwrap();
        write_atomic(&f, b"second, longer").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"second, longer");
        let mut perms = std::fs::metadata(&f).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&f, perms.clone()).unwrap();
        let err = write_atomic(&f, b"third").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&f).unwrap(), b"second, longer");
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&f, perms).unwrap();
        // A failed temp write (here: the folder is gone) leaves nothing.
        assert!(write_atomic(&ws.join("missing").join("b.txt"), b"x").is_err());
        assert!(leftovers(&ws).is_empty(), "{:?}", leftovers(&ws));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let ws = ws("atomic-mode");
        let f = ws.join("key");
        std::fs::write(&f, "old").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&f, b"new").unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o640);
        assert!(leftovers(&ws).is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn write_keeps_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let ws = ws("mode");
        std::fs::write(ws.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(ws.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        write_file(&ws, "run.sh", "#!/bin/sh\necho hi\n").unwrap();
        let mode = std::fs::metadata(ws.join("run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let _ = std::fs::remove_dir_all(&ws);
    }
}
