use crate::config::{PermissionsCfg, RuleCfg};
use serde::{Deserialize, Serialize};

/// Decision for a prospective tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Action::Allow),
            "ask" => Some(Action::Ask),
            "deny" => Some(Action::Deny),
            _ => None,
        }
    }
}

/// What the engine decided + which rule produced it (rule pattern becomes the
/// approval key, like OpenCode's "always allow matching pattern").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub key: String,
}

pub struct Engine {
    defaults: Vec<(String, Action)>,
    rules: Vec<RuleCfg>,
}

impl Engine {
    pub fn new(cfg: &PermissionsCfg) -> Self {
        let mut defaults = Vec::new();
        for (tool, action) in &cfg.defaults {
            if let Some(a) = Action::parse(action) {
                defaults.push((tool.clone(), a));
            }
        }
        Self { defaults, rules: cfg.rules.clone() }
    }

    /// Evaluate for (tool, detail) where detail is the command for bash,
    /// the (workspace-relative) path for file tools, the url for webfetch.
    /// `remembered` = session-scope approvals from the DB.
    pub fn evaluate(
        &self,
        tool: &str,
        detail: &str,
        remembered: impl Fn(&str, &str) -> bool,
    ) -> Decision {
        // 1. Ordered rules; first match wins.
        // Windows file paths ignore letter case, so file rules do too (S-04).
        let fold_case = cfg!(windows) && is_path_tool(tool);
        for rule in &self.rules {
            if glob_match(&rule.tool, tool) && detail_matches(&rule.pattern, detail, fold_case) {
                if let Some(a) = Action::parse(&rule.action) {
                    // A rule that says "ask" can be pre-satisfied by a remembered approval.
                    let action = if a == Action::Ask && remembered(&rule.tool, &rule.pattern) {
                        Action::Allow
                    } else {
                        a
                    };
                    return Decision { action, key: format!("{}:{}", rule.tool, rule.pattern) };
                }
            }
        }
        // 2. Defaults per tool.
        let default = self
            .defaults
            .iter()
            .find(|(t, _)| glob_match(t, tool))
            .map(|(_, a)| *a)
            .unwrap_or(Action::Ask); // unknown tool → ask, never silently allow
        // 3. A remembered session approval downgrades ask → allow.
        let action = if default == Action::Ask && remembered(tool, detail) {
            Action::Allow
        } else {
            default
        };
        Decision { action, key: format!("{tool}:{detail}") }
    }
}

/// Minimal glob: `*` matches any run of chars (incl. `/` for simplicity), `?` one char.
/// Anchored full-string match. Good enough for commands/paths; not a regex.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => {
                // try every split
                for i in 0..=t.len() {
                    if m(&p[1..], &t[i..]) {
                        return true;
                    }
                }
                false
            }
            Some(b'?') => !t.is_empty() && m(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && m(&p[1..], &t[1..]),
        }
    }
    m(pattern.as_bytes(), text.as_bytes())
}

/// Tools whose permission detail is a file path (see `tool_detail` in server.rs).
pub fn is_path_tool(tool: &str) -> bool {
    matches!(tool, "read" | "write" | "edit" | "list" | "glob" | "grep")
}

/// Rule pattern vs. detail. `fold_case` compares without letter case
/// (Windows file paths: `Secrets\a.txt` and `secrets\a.txt` are one file).
fn detail_matches(pattern: &str, detail: &str, fold_case: bool) -> bool {
    if fold_case {
        glob_match(&pattern.to_lowercase(), &detail.to_lowercase())
    } else {
        glob_match(pattern, detail)
    }
}

/// Start of a path detail that lies OUTSIDE the workspace, for example
/// `outside:/etc/hosts`. Workspace rules (`src/*`, `.env`) never match it;
/// a rule must name it on purpose (`outside:/tmp/*`).
pub const OUTSIDE_PREFIX: &str = "outside:";

/// The path detail used for rule matching and for "always allow":
/// - `.` and `..` are resolved, so `src/../.env` becomes `.env`;
/// - symlinks are followed for the part of the path that exists, so a link
///   inside the workspace that points elsewhere counts as outside;
/// - inside the workspace: relative with `/` (`src/main.rs`; `.` is the
///   workspace itself);
/// - outside the workspace: [`OUTSIDE_PREFIX`] + the absolute path;
/// - on Windows the result is lower-case (paths there ignore case).
pub fn normalize_path_detail(workspace: &std::path::Path, path: &str) -> String {
    normalize_path_detail_with(workspace, path, cfg!(windows))
}

/// [`normalize_path_detail`] with explicit case folding, so tests can check
/// the Windows behaviour on every platform.
pub fn normalize_path_detail_with(workspace: &std::path::Path, path: &str, fold_case: bool) -> String {
    let p = std::path::Path::new(path.trim());
    let joined = if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) };
    let ws = resolve_for_rules(workspace);
    let abs = resolve_for_rules(&joined);
    let out = match abs.strip_prefix(&ws) {
        Ok(rel) => {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel.is_empty() { ".".to_string() } else { rel }
        }
        Err(_) => format!("{OUTSIDE_PREFIX}{}", display_abs(&abs)),
    };
    if fold_case { out.to_lowercase() } else { out }
}

/// Resolve a path the way the file system will: canonical when it exists;
/// otherwise `.`/`..` are removed by hand, the longest existing parent is
/// canonicalized, and the missing tail is appended as written.
fn resolve_for_rules(p: &std::path::Path) -> std::path::PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let clean = lexical_normalize(p);
    let mut existing = clean.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(mut c) = std::fs::canonicalize(existing) {
            for part in tail.iter().rev() {
                c.push(part);
            }
            return c;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                existing = parent;
            }
            _ => return clean,
        }
    }
}

/// Remove `.` and `..` without touching the disk. `..` never climbs above
/// the root of an absolute path.
fn lexical_normalize(p: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// An absolute path for display: `/` separators, no Windows `\\?\` prefix.
fn display_abs(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = s.strip_prefix("//?/") {
        rest.to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionsCfg;

    fn deny_all() -> impl Fn(&str, &str) -> bool {
        |_, _| false
    }

    #[test]
    fn defaults_allow_read_ask_bash() {
        let e = Engine::new(&PermissionsCfg::default());
        assert_eq!(e.evaluate("read", "src/main.rs", deny_all()).action, Action::Allow);
        assert_eq!(e.evaluate("bash", "ls -la", deny_all()).action, Action::Ask);
    }

    #[test]
    fn rule_allow_exact_command() {
        let cfg = PermissionsCfg {
            rules: vec![RuleCfg {
                tool: "bash".into(),
                pattern: "git status".into(),
                action: "allow".into(),
            }],
            ..Default::default()
        };
        let e = Engine::new(&cfg);
        assert_eq!(e.evaluate("bash", "git status", deny_all()).action, Action::Allow);
        assert_eq!(e.evaluate("bash", "git push", deny_all()).action, Action::Ask);
    }

    #[test]
    fn rule_deny_wildcard() {
        let cfg = PermissionsCfg {
            rules: vec![RuleCfg {
                tool: "bash".into(),
                pattern: "rm *".into(),
                action: "deny".into(),
            }],
            ..Default::default()
        };
        let e = Engine::new(&cfg);
        assert_eq!(e.evaluate("bash", "rm -rf build", deny_all()).action, Action::Deny);
    }

    #[test]
    fn remembered_ask_becomes_allow() {
        let e = Engine::new(&PermissionsCfg::default());
        let yes = |_: &str, d: &str| d == "cargo build";
        assert_eq!(e.evaluate("bash", "cargo build", yes).action, Action::Allow);
        assert_eq!(e.evaluate("bash", "cargo test", yes).action, Action::Ask);
    }

    #[test]
    fn unknown_tool_asks() {
        let e = Engine::new(&PermissionsCfg::default());
        assert_eq!(e.evaluate("nuclear_launch", "x", deny_all()).action, Action::Ask);
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(glob_match("c?t", "cat"));
        assert!(!glob_match("c?t", "cart"));
    }

    fn tmp_ws(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ocrs-perm-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(d.join("src")).unwrap();
        d
    }

    #[test]
    fn path_detail_removes_dot_segments() {
        let ws = tmp_ws("dots");
        assert_eq!(normalize_path_detail_with(&ws, "src/../.env", false), ".env");
        assert_eq!(normalize_path_detail_with(&ws, "./.env", false), ".env");
        assert_eq!(normalize_path_detail_with(&ws, "src/./new.rs", false), "src/new.rs");
        assert_eq!(normalize_path_detail_with(&ws, "", false), ".");
        let inside = ws.join("src").join("lib.rs");
        assert_eq!(normalize_path_detail_with(&ws, &inside.to_string_lossy(), false), "src/lib.rs");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn deny_rule_sees_through_dot_segments() {
        let ws = tmp_ws("deny");
        let cfg = PermissionsCfg {
            rules: vec![RuleCfg { tool: "read".into(), pattern: ".env".into(), action: "deny".into() }],
            ..Default::default()
        };
        let e = Engine::new(&cfg);
        for raw in [".env", "./.env", "src/../.env"] {
            let d = normalize_path_detail_with(&ws, raw, false);
            assert_eq!(e.evaluate("read", &d, deny_all()).action, Action::Deny, "{raw} -> {d}");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn workspace_rules_do_not_match_outside_paths() {
        let ws = tmp_ws("outside");
        let d = normalize_path_detail_with(&ws, "../notes.txt", false);
        assert!(d.starts_with(OUTSIDE_PREFIX), "{d}");
        assert!(!d.contains(".."), "{d}");
        let cfg = PermissionsCfg {
            rules: vec![RuleCfg { tool: "write".into(), pattern: "notes.txt".into(), action: "allow".into() }],
            ..Default::default()
        };
        let e = Engine::new(&cfg);
        let inside = normalize_path_detail_with(&ws, "src/../notes.txt", false);
        assert_eq!(e.evaluate("write", &inside, deny_all()).action, Action::Allow);
        assert_eq!(e.evaluate("write", &d, deny_all()).action, Action::Ask);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_out_of_the_workspace_counts_as_outside() {
        let ws = tmp_ws("link");
        let target = tmp_ws("link-target");
        std::os::unix::fs::symlink(&target, ws.join("link")).unwrap();
        let d = normalize_path_detail_with(&ws, "link/secret.txt", false);
        assert!(d.starts_with(OUTSIDE_PREFIX), "{d}");
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn windows_case_folding() {
        let ws = tmp_ws("case");
        assert_eq!(normalize_path_detail_with(&ws, "Src/Key.PEM", true), "src/key.pem");
        assert!(detail_matches("Src/*.pem", "src/key.pem", true));
        assert!(!detail_matches("Src/*.pem", "src/key.pem", false));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
