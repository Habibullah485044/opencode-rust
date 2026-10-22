//! M2: session commands — resolve user-supplied session references
//! (`/resume`, `/diff`), flatten a transcript to diffable lines, and diff
//! two sessions' histories.

use crate::db::{Db, Message, Part};
use anyhow::{bail, Context, Result};

/// Resolve a session reference: exact id, unique id prefix, or 1-based
/// index into the session list (ordered most-recently-updated first, so
/// `1` = the most recent session).
pub fn resolve_session(db: &Db, arg: &str) -> Result<String> {
    let arg = arg.trim();
    if arg.is_empty() {
        bail!("usage: /resume <id|index> or /diff <id|index>");
    }
    if let Some(s) = db.get_session(arg)? {
        return Ok(s.id);
    }
    let all = db.list_sessions()?;
    let by_prefix: Vec<&String> =
        all.iter().filter(|s| s.id.starts_with(arg)).map(|s| &s.id).collect();
    match by_prefix.len() {
        1 => return Ok(by_prefix[0].clone()),
        0 => {}
        _ => bail!("ambiguous session reference `{arg}`"),
    }
    if let Ok(idx) = arg.parse::<usize>() {
        if idx >= 1 && idx <= all.len() {
            return Ok(all[idx - 1].id.clone());
        }
    }
    bail!("no session matches `{arg}`")
}

fn collapse(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// One diffable line per message: `role: joined text parts`, newlines
/// collapsed and long tool output truncated so diffs stay readable.
pub fn flatten(msgs: &[Message]) -> Vec<String> {
    msgs.iter()
        .map(|m| format!("{}: {}", m.role, flatten_msg(m)))
        .collect()
}

/// The diffable display text for a single message (no role prefix).
pub fn flatten_msg(m: &Message) -> String {
    flatten_msg_with(m, DIFF_LIMITS)
}

/// Per-part character limits for [`flatten_msg_with`].
#[derive(Debug, Clone, Copy)]
pub struct FlattenLimits {
    pub text: usize,
    /// 0 = leave reasoning out.
    pub reasoning: usize,
    pub tool_args: usize,
    pub tool_result: usize,
}

/// The short limits `/diff` uses, so diffs stay readable.
pub const DIFF_LIMITS: FlattenLimits =
    FlattenLimits { text: 400, reasoning: 300, tool_args: 200, tool_result: 300 };

/// [`flatten_msg`] with explicit limits.
pub fn flatten_msg_with(m: &Message, lim: FlattenLimits) -> String {
    let mut parts = Vec::new();
    for p in &m.parts {
        match p {
            Part::Text { text } => parts.push(collapse(text, lim.text)),
            Part::Reasoning { text } => {
                if lim.reasoning > 0 {
                    parts.push(format!("✻ {}", collapse(text, lim.reasoning)));
                }
            }
            Part::ToolCall { name, input, .. } => {
                let args = collapse(&input.to_string(), lim.tool_args);
                parts.push(format!("[call {name} {args}]"));
            }
            Part::ToolResult { output, is_error, .. } => {
                let tag = if *is_error { "err" } else { "ok" };
                parts.push(format!("[result {tag} {}]", collapse(output, lim.tool_result)));
            }
        }
    }
    parts.join(" ")
}

/// LCS line diff (common prefix/suffix trimmed first; falls back to a
/// naive removed/added listing when the LCS table would be too large).
pub fn diff_lines(a: &[String], b: &[String]) -> Vec<String> {
    let mut lo = 0;
    while lo < a.len() && lo < b.len() && a[lo] == b[lo] {
        lo += 1;
    }
    let mut hi_a = a.len();
    let mut hi_b = b.len();
    while hi_a > lo && hi_b > lo && a[hi_a - 1] == b[hi_b - 1] {
        hi_a -= 1;
        hi_b -= 1;
    }
    let (mid_a, mid_b) = (&a[lo..hi_a], &b[lo..hi_b]);
    let mut out: Vec<String> = a[..lo].iter().map(|l| format!(" {l}")).collect();

    let (n, m) = (mid_a.len(), mid_b.len());
    if n == 0 || m == 0 || n * m > 1_000_000 {
        out.extend(mid_a.iter().map(|l| format!("- {l}")));
        out.extend(mid_b.iter().map(|l| format!("+ {l}")));
    } else {
        let mut dp = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i][j] = if mid_a[i] == mid_b[j] {
                    dp[i + 1][j + 1] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0usize, 0usize);
        while i < n && j < m {
            if mid_a[i] == mid_b[j] {
                out.push(format!(" {}", mid_a[i]));
                i += 1;
                j += 1;
            } else if dp[i + 1][j] >= dp[i][j + 1] {
                out.push(format!("- {}", mid_a[i]));
                i += 1;
            } else {
                out.push(format!("+ {}", mid_b[j]));
                j += 1;
            }
        }
        while i < n {
            out.push(format!("- {}", mid_a[i]));
            i += 1;
        }
        while j < m {
            out.push(format!("+ {}", mid_b[j]));
            j += 1;
        }
    }
    out.extend(a[hi_a..].iter().map(|l| format!(" {l}")));
    out
}

/// Diff two sessions' flattened transcripts (`/diff`).
pub fn diff_sessions(db: &Db, a: &str, b: &str) -> Result<String> {
    let ma = db.get_session(a)?.with_context(|| format!("no session `{a}`"))?;
    let mb = db.get_session(b)?.with_context(|| format!("no session `{b}`"))?;
    let ta = flatten(&db.messages(&ma.id)?);
    let tb = flatten(&db.messages(&mb.id)?);
    let mut out = format!(
        "--- {} ({})\n+++ {} ({})\n",
        ma.id,
        ta.len(),
        mb.id,
        tb.len()
    );
    if ta == tb {
        out.push_str(&format!("identical ({} messages)\n", ta.len()));
        return Ok(out);
    }
    for line in diff_lines(&ta, &tb) {
        out.push_str(&line);
        out.push('\n');
        if out.len() > 8000 {
            out.push_str("… [diff truncated]\n");
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Part;

    fn db_with_session(title: &str) -> (Db, crate::db::Session) {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session(title, None).unwrap();
        (db, s)
    }

    #[test]
    fn resolve_by_id_prefix_and_index() {
        let (db, s) = db_with_session("first");
        db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        let other = db.create_session("second", None).unwrap();

        assert_eq!(resolve_session(&db, &s.id).unwrap(), s.id);
        // Prefix (unique).
        let prefix = &s.id[..s.id.len().min(10)];
        assert_eq!(resolve_session(&db, prefix).unwrap(), s.id);
        let all = db.list_sessions().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|x| x.id == other.id));
        assert!(all.iter().any(|x| x.id == s.id));
        // Index: list is newest-first — assert against the real ordering
        // (timestamps can tie within a millisecond).
        assert_eq!(resolve_session(&db, "1").unwrap(), all[0].id);
        assert_eq!(resolve_session(&db, "2").unwrap(), all[1].id);

        assert!(resolve_session(&db, "").is_err());
        assert!(resolve_session(&db, "ses_does_not_exist").is_err());
        assert!(resolve_session(&db, "99").is_err());
    }

    #[test]
    fn flatten_collapses_and_truncates() {
        let (db, s) = db_with_session("f");
        db.add_message(&s.id, "user", &[Part::Text { text: "line1\nline2".into() }]).unwrap();
        db.add_message(
            &s.id,
            "tool",
            &[Part::ToolResult { tool_id: "t".into(), output: "x".repeat(1000), is_error: true }],
        )
        .unwrap();
        let lines = flatten(&db.messages(&s.id).unwrap());
        assert_eq!(lines.len(), 2);
        assert!(!lines[0].contains('\n'));
        assert!(lines[0].ends_with("line1 line2"));
        assert!(lines[1].contains("[result err"));
        assert!(lines[1].len() < 500, "long output truncated");
    }

    #[test]
    fn diff_identical_and_changed() {
        let a: Vec<String> = ["user: hi", "assistant: hello", "user: bye"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let same = diff_lines(&a, &a);
        assert!(same.iter().all(|l| l.starts_with(' ')));
        assert_eq!(same.len(), 3);

        let mut b = a.clone();
        b[1] = "assistant: HELLO".to_string();
        b.push("user: again".to_string());
        let d = diff_lines(&a, &b);
        assert!(d.contains(&"- assistant: hello".to_string()));
        assert!(d.contains(&"+ assistant: HELLO".to_string()));
        assert!(d.contains(&"+ user: again".to_string()));
        assert!(d.contains(&" user: hi".to_string()));
    }

    #[test]
    fn diff_sessions_reports_identical_and_changes() {
        let (db, s) = db_with_session("a");
        db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        let fork = db.fork_session(&s.id, "fork").unwrap();

        let out = diff_sessions(&db, &s.id, &fork.id).unwrap();
        assert!(out.contains("identical"));
        assert!(!out.contains("+ user"));

        db.add_message(&fork.id, "assistant", &[Part::Text { text: "extra".into() }]).unwrap();
        let out = diff_sessions(&db, &s.id, &fork.id).unwrap();
        assert!(out.contains("+ assistant: extra"), "diff was:\n{out}");
        let out = diff_sessions(&db, &fork.id, &s.id).unwrap();
        assert!(out.contains("- assistant: extra"), "diff was:\n{out}");
        assert!(diff_sessions(&db, "ses_nope", &fork.id).is_err());
    }
}
