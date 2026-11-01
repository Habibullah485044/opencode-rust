//! The `edit` tool: exact replacements, with a loose fallback that
//! forgives indentation and line endings.

use super::files::write_atomic;
use super::paths::confine_path;
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

pub(super) fn edit_file(
    workspace: &Path,
    path: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Result<String> {
    // B-01: an empty old_str "matches" between every character; with
    // replace_all it would insert new_str all over the file.
    if old_str.is_empty() {
        bail!(
            "old_str is empty: give the exact text to replace (copy it from the file). \
             To create or fully rewrite a file, use the write tool."
        );
    }
    edit_many(workspace, path, &[(old_str.to_string(), new_str.to_string())], replace_all)
}

/// Apply several edits to one file, in order, all or nothing.
pub(super) fn edit_many(workspace: &Path, path: &str, edits: &[(String, String)], replace_all: bool) -> Result<String> {
    if edits.is_empty() {
        bail!("no edits given");
    }
    if edits.iter().any(|(old, _)| old.is_empty()) {
        bail!(
            "old_str is empty: give the exact text to replace (copy it from the file). \
             To create or fully rewrite a file, use the write tool."
        );
    }
    let abs = confine_path(workspace, path)?;
    let text = std::fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))?;
    let mut new_text = text.clone();
    let mut total = 0;
    let mut loose = 0;
    for (i, (old, new)) in edits.iter().enumerate() {
        let which = if edits.len() > 1 { format!(" (edit {} of {})", i + 1, edits.len()) } else { String::new() };
        let (next, n, fuzzy) = apply_edit(&new_text, old, new, replace_all).map_err(|e| anyhow!("{e} in {path}{which}"))?;
        new_text = next;
        total += n;
        loose += usize::from(fuzzy);
    }
    write_atomic(&abs, new_text.as_bytes()).with_context(|| format!("writing {}", abs.display()))?;
    let lines_before = text.matches('\n').count();
    let lines_after = new_text.matches('\n').count();
    let note = if loose > 0 { " (matched ignoring indentation/line endings)" } else { "" };
    Ok(format!(
        "edited {path}: replaced {total} occurrence(s), line delta {:+}{note}",
        lines_after as i64 - lines_before as i64
    ))
}

/// One replacement in `text`. Exact match first; then a match that ignores
/// line endings and indentation (weak models often get those wrong), with
/// `new` re-indented by the same amount. Returns the new text, how many
/// places changed, and whether the loose match was used. On a miss the
/// error shows the closest lines, so the next try can copy them.
pub(super) fn apply_edit(text: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize, bool)> {
    let (old, new) = match_line_endings(text, old, new);
    if let Some(done) = apply_exact(text, &old, &new, replace_all)? {
        return Ok(done);
    }
    apply_loose(text, &old, &new, replace_all)
}

/// CRLF files: the model writes \n.
fn match_line_endings(text: &str, old: &str, new: &str) -> (String, String) {
    let crlf = text.contains("\r\n");
    if crlf && !old.contains("\r\n") {
        (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
    } else {
        (old.to_string(), new.to_string())
    }
}

/// The exact match. `None` when `old` does not occur in `text`.
fn apply_exact(text: &str, old: &str, new: &str, replace_all: bool) -> Result<Option<(String, usize, bool)>> {
    let matches = text.matches(old).count();
    if matches > 1 && !replace_all {
        bail!("old_str matches {matches} times; make it unique (add a nearby line) or set replace_all");
    }
    if matches >= 1 {
        let out = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        return Ok(Some((out, matches, false)));
    }
    Ok(None)
}

/// The lines of `s` split at `\n`, each without its `\r`.
fn split_lines(s: &str) -> Vec<&str> {
    s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// Leading and trailing blank lines carry no information.
fn trim_blank_edges<'a>(want: &[&'a str]) -> Vec<&'a str> {
    let start = want.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    let end = want.iter().rposition(|l| !l.trim().is_empty()).map_or(start, |e| e + 1);
    want[start..end].to_vec()
}

/// Indentation is spaces and tabs only: `trim_start` also takes NBSP and
/// other multi-byte spaces, and cutting by its byte count could split a
/// character (a panic, and release builds abort).
fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches([' ', '\t']).len()
}

/// The loose match: compare trimmed lines. Each line keeps its own ending
/// (a file with mixed endings is not rewritten to one kind).
fn apply_loose(text: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize, bool)> {
    let has_cr: Vec<bool> = text.split('\n').map(|l| l.ends_with('\r')).collect();
    let lines = split_lines(text);
    let want = trim_blank_edges(&split_lines(old));
    if want.is_empty() {
        bail!("old_str not found");
    }
    let hits: Vec<usize> = (0..=lines.len().saturating_sub(want.len()))
        .filter(|&i| want.iter().enumerate().all(|(k, w)| lines[i + k].trim() == w.trim()))
        .collect();
    if hits.len() > 1 && !replace_all {
        bail!("old_str matches {} places (ignoring indentation); make it unique or set replace_all", hits.len());
    }
    if hits.is_empty() {
        return Err(anyhow!("old_str not found{}", closest_region(&lines, &want)));
    }
    let first = want.iter().find(|l| !l.trim().is_empty()).copied().unwrap_or("");
    let new_lines = split_lines(new);
    let mut out: Vec<String> =
        lines.iter().zip(&has_cr).map(|(l, cr)| if *cr { format!("{l}\r") } else { l.to_string() }).collect();
    // Replace from the last hit back, so earlier indexes stay valid.
    for &i in hits.iter().rev() {
        let fixed = reindent(&new_lines, &lines[i..i + want.len()], first);
        let fixed = end_like_block(fixed, has_cr[i], has_cr[i + want.len() - 1]);
        out.splice(i..i + want.len(), fixed);
    }
    Ok((out.join("\n"), hits.len(), true))
}

/// `new_lines` shifted by how much deeper the matched `block` sits in the
/// file than `first` (the first non-blank line of old_str).
fn reindent(new_lines: &[&str], block: &[&str], first: &str) -> Vec<String> {
    let file_first = block.iter().find(|l| !l.trim().is_empty()).copied().unwrap_or("");
    let file_pad = &file_first[..indent(file_first)];
    let delta = indent(file_first) as i64 - indent(first) as i64;
    new_lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else if delta >= 0 {
                // Same indent character as the file uses.
                let unit = file_pad.chars().next().unwrap_or(' ');
                format!("{}{l}", unit.to_string().repeat(delta as usize))
            } else {
                let cut = (-delta as usize).min(indent(l));
                l[cut..].to_string()
            }
        })
        .collect()
}

/// New lines end like the replaced block: its first line's ending
/// inside, its last line's ending at the end (no CR at a final line
/// that had none).
fn end_like_block(fixed: Vec<String>, inner_cr: bool, last_cr: bool) -> impl Iterator<Item = String> {
    let n = fixed.len();
    fixed.into_iter().enumerate().map(move |(k, l)| {
        if (k + 1 < n && inner_cr) || (k + 1 == n && last_cr) { format!("{l}\r") } else { l }
    })
}

/// ". Closest match at lines N-M ...:" for a failed edit, or "" when
/// nothing is close.
fn closest_region(lines: &[&str], want: &[&str]) -> String {
    let first = want.iter().find(|l| !l.trim().is_empty()).map(|l| l.trim()).unwrap_or("");
    let mut best: Option<(usize, usize)> = None; // (score, start)
    for i in 0..lines.len() {
        let score = want
            .iter()
            .enumerate()
            .filter(|(k, w)| lines.get(i + k).is_some_and(|l| !w.trim().is_empty() && l.trim() == w.trim()))
            .count()
            + usize::from(!first.is_empty() && lines[i].contains(first));
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }
    let Some((_, start)) = best else { return String::new() };
    let end = (start + want.len().max(1)).min(lines.len());
    let shown: Vec<String> = lines[start..end].iter().take(12).enumerate().map(|(k, l)| format!("{:>5}\t{l}", start + k + 1)).collect();
    format!(
        ". Closest match at lines {}-{} (copy old_str from here):\n{}",
        start + 1,
        end,
        shown.join("\n")
    )
}

#[cfg(test)]
mod b01_tests {
    use super::*;

    #[test]
    fn edit_refuses_empty_old_str() {
        let ws = std::env::temp_dir().join(format!("ocrs-b01-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("f.txt"), "abc").unwrap();
        for replace_all in [false, true] {
            let err = edit_file(&ws, "f.txt", "", "X", replace_all).unwrap_err().to_string();
            assert!(err.contains("old_str is empty"), "{err}");
        }
        assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "abc", "file untouched");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn closest_region_points_at_the_nearest_block() {
        let file = ["fn a() {", "    let x = 1;", "    let y = 2;", "}", "fn b() {", "    other();", "}"];
        // Exact region.
        let out = closest_region(&file, &["    let x = 1;", "    let y = 2;"]);
        assert!(out.starts_with(". Closest match at lines 2-3"), "{out}");
        assert!(out.contains("    2\t    let x = 1;\n    3\t    let y = 2;"), "{out}");
        // Only the indentation differs: same lines found.
        let out = closest_region(&file, &["let x = 1;", "\tlet y = 2;"]);
        assert!(out.starts_with(". Closest match at lines 2-3"), "{out}");
        // Nothing close.
        assert_eq!(closest_region(&file, &["nothing like this", "at all"]), "");
        assert_eq!(closest_region(&file, &[]), "");
        // A long old_str shows at most 12 lines of the region.
        let long: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        let long: Vec<&str> = long.iter().map(String::as_str).collect();
        let out = closest_region(&long, &long[4..24]);
        assert!(out.starts_with(". Closest match at lines 5-24"), "{out}");
        assert_eq!(out.lines().count(), 1 + 12, "{out}");
        assert!(out.ends_with("   16\tline 16"), "{out}");
    }
}
