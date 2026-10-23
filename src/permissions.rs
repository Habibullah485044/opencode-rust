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
    /// "allow" | "ask" | "deny" — letter case and surrounding spaces do not
    /// matter. Anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" => Some(Action::Allow),
            "ask" => Some(Action::Ask),
            "deny" => Some(Action::Deny),
            _ => None,
        }
    }

    /// The canonical config word for this action.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Ask => "ask",
            Action::Deny => "deny",
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

/// Sort key for default entries: (exact name?, number of literal characters).
fn default_specificity(pattern: &str) -> (bool, usize) {
    let literal = pattern.chars().filter(|c| *c != '*' && *c != '?').count();
    (literal == pattern.chars().count(), literal)
}

impl Engine {
    pub fn new(cfg: &PermissionsCfg) -> Self {
        // An unknown action word never drops a default or a rule silently:
        // it becomes "ask". (`Config::load` already rejects such configs.)
        let mut defaults: Vec<(String, Action)> = cfg
            .defaults
            .iter()
            .map(|(tool, action)| (tool.clone(), Action::parse(action).unwrap_or(Action::Ask)))
            .collect();
        // `cfg.defaults` is a HashMap, whose order changes from run to run.
        // Sort so the most specific entry is found first: exact tool names,
        // then patterns with more literal characters, then by name.
        defaults.sort_by(|(a, _), (b, _)| {
            default_specificity(b).cmp(&default_specificity(a)).then_with(|| a.cmp(b))
        });
        let rules = cfg
            .rules
            .iter()
            .map(|r| RuleCfg {
                tool: r.tool.clone(),
                pattern: r.pattern.clone(),
                action: Action::parse(&r.action).unwrap_or(Action::Ask).as_str().to_string(),
            })
            .collect();
        Self { defaults, rules }
    }

    /// Evaluate for (tool, detail) where detail is the command for bash,
    /// the (workspace-relative) path for file tools, the url for webfetch.
    /// `remembered` = "always allow" approvals from the DB.
    ///
    /// For `bash`, a chained command (`a && b | c`) is split into its parts
    /// and every part must pass: a denied part denies the whole command, a
    /// part that is not allowed makes it ask. Syntax the splitter cannot
    /// follow, and output redirected into a file, also ask unless the user
    /// approved this exact command before.
    pub fn evaluate(
        &self,
        tool: &str,
        detail: &str,
        remembered: impl Fn(&str, &str) -> bool,
    ) -> Decision {
        if tool != "bash" {
            return self.evaluate_single(tool, detail, &remembered);
        }
        let ask_whole = Decision { action: Action::Ask, key: format!("{tool}:{detail}") };
        let allow_whole = Decision { action: Action::Allow, key: format!("{tool}:{detail}") };
        let Some(parts) = split_command(detail, Shell::current()) else {
            // We cannot see every command inside: a deny rule on the whole
            // line still wins; otherwise ask (an exact earlier approval allows).
            let whole = self.evaluate_single(tool, detail, &remembered);
            if whole.action == Action::Deny {
                return whole;
            }
            return if remembered(tool, detail) { allow_whole } else { ask_whole };
        };
        if parts.len() <= 1 {
            let d = self.evaluate_single(tool, detail, &remembered);
            let writes = parts.first().is_some_and(|p| p.writes_file);
            if d.action == Action::Allow && writes && !remembered(tool, detail) {
                return ask_whole;
            }
            return d;
        }
        let mut all_allowed = true;
        for p in &parts {
            let d = self.evaluate_single(tool, &p.text, &remembered);
            match d.action {
                Action::Deny => return d,
                Action::Allow if !p.writes_file => {}
                _ => all_allowed = false,
            }
        }
        if all_allowed || remembered(tool, detail) {
            allow_whole
        } else {
            ask_whole
        }
    }

    /// One detail against the rules, then the defaults (the old `evaluate`).
    fn evaluate_single(
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

/// The shell that runs `bash` tool commands: `sh -c` on Unix, Git Bash or
/// `cmd /C` on Windows (see `tools::shell_command`). They split command lines differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Posix,
    Cmd,
}

impl Shell {
    /// The shell the `bash` tool uses on this machine (Git Bash counts as
    /// POSIX on Windows).
    pub fn current() -> Self {
        crate::tools::shell_kind()
    }
}

/// One simple command cut out of a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPart {
    /// The command as typed (trimmed, quotes kept).
    pub text: String,
    /// It sends output into a file (`> f`, `>> f`, `&> f`).
    pub writes_file: bool,
}

/// Words that start compound commands (loops, `if`, negation): their parts
/// cannot be judged one by one, so the whole line asks.
const POSIX_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "case", "esac", "for", "select", "while", "until",
    "do", "done", "function", "time", "coproc", "!",
];
const CMD_KEYWORDS: &[&str] = &["if", "for"];

/// Split a command line at the shell's control operators, ignoring
/// operators inside quotes. `None` means the line uses syntax this splitter
/// does not follow (command substitution, groups, here-documents, loops,
/// `cmd` variables, unbalanced quotes): the caller must ask.
pub fn split_command(line: &str, shell: Shell) -> Option<Vec<CommandPart>> {
    let chars: Vec<char> = line.chars().collect();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut writes = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            } else if q == '"' && shell == Shell::Posix {
                match c {
                    '\\' => {
                        if let Some(n) = next {
                            cur.push(n);
                            i += 1;
                        }
                    }
                    '`' => return None,
                    '$' if next == Some('(') => return None,
                    _ => {}
                }
            }
            i += 1;
            continue;
        }
        match (shell, c) {
            (_, '"') | (Shell::Posix, '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (Shell::Posix, '\\') | (Shell::Cmd, '^') => {
                // Escape: the next character is literal. A trailing escape
                // continues the line, which we do not follow.
                let n = next?;
                cur.push(c);
                cur.push(n);
                i += 1;
            }
            (_, '(') | (_, ')') => return None,
            (Shell::Posix, '`') | (Shell::Posix, '{') | (Shell::Posix, '}') => return None,
            (Shell::Cmd, '%') => return None,
            (Shell::Posix, '$') if matches!(next, Some('(') | Some('\'')) => return None,
            (_, '<') if next == Some('<') => return None,
            (Shell::Posix, ';') | (_, '\n') | (_, '\r') => end_part(&mut parts, &mut cur, &mut writes),
            (_, '&') => {
                let prev = cur.chars().last();
                if next == Some('&') {
                    end_part(&mut parts, &mut cur, &mut writes);
                    i += 1;
                } else if matches!(prev, Some('>') | Some('<')) || next == Some('>') {
                    cur.push(c); // part of a redirection: `2>&1`, `<&3`, `&> file`
                } else {
                    end_part(&mut parts, &mut cur, &mut writes); // background, or cmd's `&`
                }
            }
            (_, '|') => {
                end_part(&mut parts, &mut cur, &mut writes);
                if next == Some('|') || (shell == Shell::Posix && next == Some('&')) {
                    i += 1;
                }
            }
            (_, '>') => {
                if shell == Shell::Posix && next == Some('|') {
                    return None; // `>|` (force overwrite) is rare: ask
                }
                cur.push(c);
                if next == Some('>') {
                    cur.push('>');
                    i += 1;
                }
                if !redirect_is_harmless(&chars, i + 1, shell) {
                    writes = true;
                }
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    end_part(&mut parts, &mut cur, &mut writes);
    let keywords = match shell {
        Shell::Posix => POSIX_KEYWORDS,
        Shell::Cmd => CMD_KEYWORDS,
    };
    for p in &parts {
        let first = p.text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        if keywords.contains(&first.as_str()) {
            return None;
        }
    }
    Some(parts)
}

/// Finish the current part (empty parts are dropped).
fn end_part(parts: &mut Vec<CommandPart>, cur: &mut String, writes: &mut bool) {
    let text = cur.trim();
    if !text.is_empty() {
        parts.push(CommandPart { text: text.to_string(), writes_file: *writes });
    }
    cur.clear();
    *writes = false;
}

/// Called right after `>` or `>>`: true when nothing on disk is written,
/// i.e. the output goes to a descriptor (`>&2`) or to the null device.
fn redirect_is_harmless(chars: &[char], mut j: usize, shell: Shell) -> bool {
    if chars.get(j) == Some(&'&') {
        return matches!(chars.get(j + 1), Some(d) if d.is_ascii_digit() || *d == '-');
    }
    while chars.get(j).is_some_and(|c| *c == ' ' || *c == '\t') {
        j += 1;
    }
    let target: String = chars[j.min(chars.len())..]
        .iter()
        .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | '<' | '>'))
        .collect();
    match shell {
        Shell::Posix => target == "/dev/null",
        Shell::Cmd => target.eq_ignore_ascii_case("nul"),
    }
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

    fn bash_rules(rules: &[(&str, &str)]) -> Engine {
        let cfg = PermissionsCfg {
            rules: rules
                .iter()
                .map(|(pattern, action)| RuleCfg {
                    tool: "bash".into(),
                    pattern: (*pattern).into(),
                    action: (*action).into(),
                })
                .collect(),
            ..Default::default()
        };
        Engine::new(&cfg)
    }

    fn texts(line: &str, shell: Shell) -> Option<Vec<String>> {
        split_command(line, shell).map(|v| v.into_iter().map(|p| p.text).collect())
    }

    #[test]
    fn chained_command_needs_every_part_allowed() {
        let e = bash_rules(&[("git *", "allow")]);
        assert_eq!(e.evaluate("bash", "git status", deny_all()).action, Action::Allow);
        assert_eq!(e.evaluate("bash", "git status && git log", deny_all()).action, Action::Allow);
        assert_eq!(e.evaluate("bash", "git status && rm -rf build", deny_all()).action, Action::Ask);
        assert_eq!(e.evaluate("bash", "git log | sh", deny_all()).action, Action::Ask);
    }

    #[test]
    fn any_denied_part_denies_the_whole_command() {
        let e = bash_rules(&[("git *", "allow"), ("rm *", "deny")]);
        assert_eq!(e.evaluate("bash", "git status && rm -rf build", deny_all()).action, Action::Deny);
    }

    #[test]
    fn unparseable_commands_ask() {
        let e = bash_rules(&[("*rm -rf*", "deny"), ("*", "allow")]);
        for cmd in ["echo $(whoami)", "(cd x && ls)", "for f in *; do echo $f; done", "echo \"open"] {
            assert_eq!(e.evaluate("bash", cmd, deny_all()).action, Action::Ask, "{cmd}");
        }
        assert_eq!(e.evaluate("bash", "(rm -rf x)", deny_all()).action, Action::Deny);
        // An earlier "always allow" for exactly this command still counts.
        let yes = |_: &str, d: &str| d == "echo $(whoami)";
        assert_eq!(e.evaluate("bash", "echo $(whoami)", yes).action, Action::Allow);
    }

    #[test]
    fn allowed_command_that_writes_a_file_asks() {
        let e = bash_rules(&[("git *", "allow")]);
        assert_eq!(e.evaluate("bash", "git log > notes.txt", deny_all()).action, Action::Ask);
        assert_eq!(e.evaluate("bash", "git log 2>&1", deny_all()).action, Action::Allow);
    }

    #[test]
    fn posix_split_respects_quotes_and_escapes() {
        assert_eq!(texts("a && b || c; d | e & f", Shell::Posix).unwrap(), ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(texts("echo 'a && b' \"c; d\"", Shell::Posix).unwrap(), ["echo 'a && b' \"c; d\""]);
        assert_eq!(texts("echo a\\;b", Shell::Posix).unwrap(), ["echo a\\;b"]);
        assert_eq!(texts("cargo test 2>&1 | tail -5", Shell::Posix).unwrap(), ["cargo test 2>&1", "tail -5"]);
        assert!(texts("echo `id`", Shell::Posix).is_none());
        assert!(texts("echo \"$(id)\"", Shell::Posix).is_none());
        assert!(texts("cat <<EOF", Shell::Posix).is_none());
        assert!(texts("{ ls; }", Shell::Posix).is_none());
    }

    #[test]
    fn cmd_split_uses_cmd_rules() {
        // cmd has no single quotes and no `;` separator; `^` escapes.
        assert_eq!(texts("echo 'a & b'", Shell::Cmd).unwrap(), ["echo 'a", "b'"]);
        assert_eq!(texts("echo \"a & b\"", Shell::Cmd).unwrap(), ["echo \"a & b\""]);
        assert_eq!(texts("echo a ^& b", Shell::Cmd).unwrap(), ["echo a ^& b"]);
        assert_eq!(texts("dir; del x", Shell::Cmd).unwrap(), ["dir; del x"]);
        assert!(texts("echo %PATH%", Shell::Cmd).is_none());
        assert!(texts("if exist a del b", Shell::Cmd).is_none());
    }

    #[test]
    fn redirect_into_a_file_is_noticed() {
        let w = |line: &str, shell: Shell| split_command(line, shell).unwrap()[0].writes_file;
        assert!(w("git log > notes.txt", Shell::Posix));
        assert!(w("git log >> notes.txt", Shell::Posix));
        assert!(w("git log &> out.txt", Shell::Posix));
        assert!(!w("git log > /dev/null 2>&1", Shell::Posix));
        assert!(!w("git log 2>&1", Shell::Posix));
        assert!(!w("dir > NUL", Shell::Cmd));
        assert!(w("dir > out.txt", Shell::Cmd));
    }

    #[test]
    fn wildcard_default_never_beats_an_exact_name() {
        // Every new HashMap has a new order; the answer must not change.
        for _ in 0..20 {
            let mut cfg = PermissionsCfg::default();
            cfg.defaults.insert("*".into(), "allow".into());
            cfg.defaults.insert("bash".into(), "ask".into());
            let e = Engine::new(&cfg);
            assert_eq!(e.evaluate("bash", "ls", deny_all()).action, Action::Ask);
            assert_eq!(e.evaluate("some_mcp_tool", "x", deny_all()).action, Action::Allow);
        }
    }

    #[test]
    fn bad_rule_action_asks_instead_of_being_skipped() {
        let rule = |action: &str| PermissionsCfg {
            rules: vec![RuleCfg {
                tool: "read".into(),
                pattern: "secrets/*".into(),
                action: action.into(),
            }],
            ..Default::default()
        };
        // Before this fix the bad rule was skipped and the read default
        // ("allow") won.
        let e = Engine::new(&rule("denny"));
        assert_eq!(e.evaluate("read", "secrets/key.pem", deny_all()).action, Action::Ask);
        // Letter case and spaces do not matter.
        let e = Engine::new(&rule(" Deny "));
        assert_eq!(e.evaluate("read", "secrets/key.pem", deny_all()).action, Action::Deny);
    }
}
