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
        for rule in &self.rules {
            if glob_match(&rule.tool, tool) && glob_match(&rule.pattern, detail) {
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

/// The subset of path a detail represents, for rule matching:
/// commands matched verbatim, paths matched workspace-relative with `/`.
pub fn normalize_path_detail(workspace: &std::path::Path, path: &str) -> String {
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) };
    abs.strip_prefix(workspace)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.to_string_lossy().replace('\\', "/"))
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
}
