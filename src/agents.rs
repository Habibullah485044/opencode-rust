//! M2: agent layer — Plan/Build mode switch, system-prompt composition
//! (base + AGENTS.md + skills index + mode instructions), and the built-in
//! subagent roster used by the `spawn` tool.

use crate::provider::ToolSpec;
use crate::skills;
use std::path::Path;

/// Plan (read-only: investigate and propose) vs Build (normal tool-using agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    Plan,
    #[default]
    Build,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentMode::Plan => "plan",
            AgentMode::Build => "build",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("plan") {
            Some(AgentMode::Plan)
        } else if s.eq_ignore_ascii_case("build") {
            Some(AgentMode::Build)
        } else {
            None
        }
    }

    /// Tools usable in this mode. `None` = every tool.
    pub fn allowed_tools(self) -> Option<&'static [&'static str]> {
        match self {
            AgentMode::Plan => Some(&["read", "list", "glob", "grep", "webfetch"]),
            AgentMode::Build => None,
        }
    }

    /// May this mode execute the named tool? (Build: everything.)
    pub fn allows_tool(self, name: &str) -> bool {
        match self.allowed_tools() {
            None => true,
            Some(allowed) => allowed.contains(&name),
        }
    }

    /// Mode-specific instructions appended to the system prompt ("" = none).
    pub fn instructions(self) -> &'static str {
        match self {
            AgentMode::Plan => {
                "You are in PLAN MODE: read-only. Investigate with read/list/glob/grep/webfetch \
                 and produce a concrete implementation proposal. Do not modify files, do not run \
                 shell commands, do not spawn subagents. End with a numbered plan."
            }
            AgentMode::Build => "",
        }
    }

    /// Drop tool specs not allowed in this mode (Plan keeps read-only tools).
    pub fn filter_specs(self, specs: Vec<ToolSpec>) -> Vec<ToolSpec> {
        match self.allowed_tools() {
            None => specs,
            Some(allowed) => specs
                .into_iter()
                .filter(|s| allowed.contains(&s.function.name.as_str()))
                .collect(),
        }
    }
}

/// A subagent role: who the child is, what it may use, how far it may go.
/// Built-in roles are below; a project adds or overrides roles with
/// Markdown files (see `load_subagents`).
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentDef {
    pub name: String,
    /// One line the parent model reads to pick the role.
    pub description: String,
    /// The role's system prompt.
    pub system: String,
    /// Allowed tools; `None` = every tool (except spawning).
    pub tools: Option<Vec<String>>,
    /// Model for this role (`None` = the chat's model; read-only built-in
    /// roles use `provider.small_model` when one is set).
    pub model: Option<String>,
    /// Most model calls for one run of this role (`None` = agent.max_steps).
    pub max_steps: Option<usize>,
    /// Run in its own git worktree (`None` = agent.isolate_workers for
    /// `worker`, off otherwise).
    pub isolate: Option<bool>,
    /// Never edits files: may use the cheaper `small_model`.
    pub read_only: bool,
    /// The file this role came from (`None` = built in).
    pub source: Option<std::path::PathBuf>,
}

impl SubagentDef {
    fn builtin(name: &str, description: &str, system: &str, tools: Option<&[&str]>, read_only: bool) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            system: system.into(),
            tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
            model: None,
            max_steps: None,
            isolate: None,
            read_only,
            source: None,
        }
    }
}

const READ_TOOLS: &[&str] = &["read", "list", "glob", "grep", "webfetch"];

pub fn builtin_subagents() -> Vec<SubagentDef> {
    vec![
        SubagentDef::builtin(
            "explore",
            "Read-only: searches and reads code, reports findings with file:line.",
            "You are a read-only exploration subagent. Search and read the workspace, then \
             answer with precise findings (file paths and line references). You cannot modify \
             files — do not try.",
            Some(READ_TOOLS),
            true,
        ),
        SubagentDef::builtin(
            "research",
            "Read-only: reads docs and web pages, reports facts with sources.",
            "You are a research subagent. Find out what the task asks by reading documentation, \
             web pages and the code. Report concrete facts, each with where you found it (URL or \
             file:line). Say clearly what you could not confirm. You cannot modify files.",
            Some(&["read", "list", "glob", "grep", "webfetch", "browser_navigate", "browser_snapshot", "browser_wait"]),
            true,
        ),
        SubagentDef::builtin(
            "plan",
            "Read-only: returns a step-by-step plan with files, risks and checks.",
            "You are a planning subagent. Read the relevant code, then return a numbered plan: \
             each step names the files it changes, what changes, and how to check it worked \
             (a test, a command, something to look at). Flag risks and open questions. Keep it \
             short and concrete. You cannot modify files.",
            Some(READ_TOOLS),
            true,
        ),
        SubagentDef::builtin(
            "worker",
            "Makes one well-defined change with all tools (edits, commands).",
            "You are a focused worker subagent. Complete only the task you were given. Change \
             only the files the task is about. Check your work (build, test or run it) before \
             you finish. End with what you changed and how you checked it.",
            None,
            false,
        ),
        SubagentDef::builtin(
            "review",
            "Checks a change against the task, runs tests, reports problems. No edits.",
            "You are a review subagent. Check the work described in the task: read the changed \
             code, run the tests or the build when you can, and compare the result with what was \
             asked. Also try to break it: bad or corrupt input (including files it reads), \
             a crash or failed write that loses data, too-loose parsing of numbers and \
             arguments, wrong exit codes or messages, temporary files left in the project. \
             Report each real problem with file:line and how to fix it, most serious first. \
             If it is good, say so in one line. Do not edit files.",
            Some(&["read", "list", "glob", "grep", "bash", "check_page"]),
            true,
        ),
    ]
}

pub fn find_subagent(name: &str) -> Option<SubagentDef> {
    builtin_subagents().into_iter().find(|d| d.name == name)
}

/// Every role for `workspace`: the built-in ones, then Markdown role files
/// from the user's config (`<config>/opencode-rust/agents/`), the project's
/// `.claude/agents/` and `.opencode/agents/` — a later file with the same
/// name replaces the earlier role.
pub fn load_subagents(workspace: &Path) -> Vec<SubagentDef> {
    let mut roles = builtin_subagents();
    let user = crate::config::default_config_path().parent().map(|p| p.join("agents"));
    let dirs = user
        .into_iter()
        .chain([workspace.join(".claude").join("agents"), workspace.join(".opencode").join("agents")]);
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
            .collect();
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if let Some(role) = parse_role_file(&path, &text) {
                roles.retain(|r| r.name != role.name);
                roles.push(role);
            }
        }
    }
    roles
}

/// A role file: `---` frontmatter, then the role's system prompt.
///
/// ```text
/// ---
/// name: security
/// description: Looks for security problems in a change
/// tools: read, grep, glob, bash      (or [read, grep]; leave out for all tools)
/// model: anthropic/claude-sonnet-5-5 (optional)
/// max_steps: 20                      (optional; max_turns also works)
/// isolation: worktree                (optional: worktree | none)
/// ---
/// You are a security reviewer…
/// ```
/// `None` when there is no name or no prompt.
pub fn parse_role_file(path: &Path, text: &str) -> Option<SubagentDef> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front, body) = crate::skills::split_frontmatter(text)?;
    let mut role = SubagentDef::builtin("", "", body.trim(), None, false);
    role.source = Some(path.to_path_buf());
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => role.name = value.to_string(),
            "description" => role.description = value.to_string(),
            "tools" => {
                let list: Vec<String> = value
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .map(|t| t.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                if !list.is_empty() && !list.iter().any(|t| t == "*" || t == "all") {
                    role.read_only = !list.iter().any(|t| matches!(t.as_str(), "write" | "edit" | "bash"));
                    role.tools = Some(list);
                }
            }
            "model" if !value.is_empty() && value != "inherit" => role.model = Some(value.to_string()),
            "max_steps" | "max_turns" => role.max_steps = value.parse().ok().filter(|n| *n > 0),
            "isolation" => role.isolate = Some(value.eq_ignore_ascii_case("worktree")),
            _ => {}
        }
    }
    if role.name.is_empty() || role.system.is_empty() {
        return None;
    }
    if role.description.is_empty() {
        role.description = format!("Custom role from {}", path.display());
    }
    Some(role)
}

/// `- name: description` lines for the system prompt.
pub fn subagent_index(defs: &[SubagentDef]) -> String {
    defs.iter()
        .map(|d| format!("- {}: {}", d.name, d.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// P-05: how strongly the Ponytail rules apply in one session. `Full` is
/// the vendored default; `Off` leaves the rules out of the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PonytailLevel {
    Off,
    Lite,
    Full,
    Ultra,
}

impl PonytailLevel {
    /// `off` / `lite` / `full` / `ultra`, any case.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "lite" => Some(Self::Lite),
            "full" => Some(Self::Full),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lite => "lite",
            Self::Full => "full",
            Self::Ultra => "ultra",
        }
    }

    /// The level a session starts with: config `agent.ponytail` on = full.
    pub fn from_config(enabled: bool) -> Self {
        if enabled { Self::Full } else { Self::Off }
    }
}

/// Inputs for [`compose_system`].
pub struct PromptInputs<'a> {
    pub workspace: &'a Path,
    pub mode: AgentMode,
    /// User-configured base prompt (`agent.system_prompt`); replaces the default.
    pub configured: Option<&'a str>,
    /// Contents of the workspace `AGENTS.md`, if present.
    pub agents_md: Option<&'a str>,
    pub skills: &'a [skills::Doc],
    /// Ponytail rules level (P-05): `Off` leaves them out, `Lite`/`Ultra`
    /// add a one-line level note after them.
    pub ponytail: PonytailLevel,
    /// Installed skills that passed vetting — embedded in full ("baked in"),
    /// same idea as the vendored ponytail rules.
    pub baked: &'a [skills::Doc],
}

/// Vendored Ponytail rules — "lazy senior dev" discipline: the smallest
/// change that fully solves the task, reuse over rewrite, never cut safety.
/// MIT License, Copyright (c) DietrichGebert —
/// <https://github.com/DietrichGebert/ponytail>
pub fn ponytail_rules() -> &'static str {
    include_str!("prompts/ponytail.md")
}

/// Vendored ponytail-review prompt: review a change like the senior dev who
/// gets paged when it breaks — including callers OUTSIDE the diff.
/// MIT License, Copyright (c) DietrichGebert.
pub fn ponytail_review_rules() -> &'static str {
    include_str!("prompts/ponytail-review.md")
}

/// P-05: the Ponytail block of a system prompt — empty for `Off`, the
/// vendored rules for `Full`, rules plus one level line otherwise (the
/// vendored "Levels" table says what each level means).
pub fn ponytail_section(level: PonytailLevel) -> String {
    if level == PonytailLevel::Off {
        return String::new();
    }
    let mut out = String::from("\n\n## Ponytail — lazy senior dev discipline\n\n");
    out.push_str(ponytail_prompt_rules());
    if level != PonytailLevel::Full {
        out.push_str(&format!("\n\nPonytail level: {}", level.as_str()));
        if let Some(how) = ponytail_level_rule(level) {
            out.push_str(&format!(" — {how}"));
        }
    }
    out
}

/// The part of the vendored rules the model needs. Sent with every request,
/// so the licence header, the frontmatter, the "how to switch levels" line
/// (the app handles `/ponytail`) and the levels table (only the active
/// level's row is added) stay out.
fn ponytail_prompt_rules() -> &'static str {
    static RULES: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        let text = ponytail_rules();
        let body = text.split_once("\n---\n").and_then(|(_, rest)| rest.split_once("\n---\n"));
        let body = body.map_or(text, |(_, b)| b);
        let body = body.split("\n## Levels").next().unwrap_or(body);
        body.lines()
            .filter(|l| !l.starts_with("# Ponytail") && !l.starts_with("Active for the whole session"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .replace("\n\n\n", "\n\n")
    })
}

/// The levels table row for `level`, e.g. "Build what was asked. …".
fn ponytail_level_rule(level: PonytailLevel) -> Option<String> {
    let key = format!("**{}**", level.as_str());
    ponytail_rules().lines().find(|l| l.contains(&key)).and_then(|row| {
        let cell = row.trim().trim_matches('|').split('|').nth(1)?.trim();
        (!cell.is_empty()).then(|| cell.to_string())
    })
}

/// Compose the system prompt: base + AGENTS.md + skills index + subagent
/// index + mode instructions (binding rules last so they stay salient).
pub fn compose_system(p: PromptInputs<'_>) -> String {
    let base = p.configured.map(str::to_string).unwrap_or_else(|| {
        format!(
            "You are opencode-rust, a coding agent running in {}.\n\
             Use the provided tools to inspect and modify the workspace. Be concise. \
             When done with the user's request, reply with a short summary of what you did.",
            p.workspace.display()
        )
    });
    let mut out = base;

    // Every prompt gets this, configured or not: a small model holding nine
    // tools will cheerfully start grepping the workspace to answer "hi", and
    // that reads as the agent working on its own instead of replying. Stating
    // the rule here rather than only in the default base keeps it in force for
    // users who supply `agent.system_prompt`.
    out.push_str(
        "\n\n## When to act\n\n\
         Reply directly in chat, with no tool calls, when the user greets you, makes \
         small talk, thanks you, or asks something you can answer from what you \
         already know. Tools and file changes are for requests that ask for work. \
         If a request is ambiguous, ask one short clarifying question instead of guessing.\n\n\
         ## Temporary files\n\n\
         Scratch scripts, test data and manual test runs go in `.opencode/scratch/` (git \
         ignores it; it is deleted when the run ends). Run manual checks from inside it, \
         not in the project folder. Leave no temporary files in the project.",
    );

    if let Some(md) = p.agents_md.filter(|s| !s.trim().is_empty()) {
        out.push_str("\n\n## Project instructions (AGENTS.md)\n\n");
        out.push_str(md.trim());
    }
    if !p.skills.is_empty() {
        // "What skills do you have?" is answerable from this list. Saying so
        // stops the model from shelling out to guess at a skills directory
        // (it once ran `ls .openclaw/skills`, a path that exists only in the
        // upstream repo this code was vendored from).
        out.push_str(&format!(
            "\n\n## Skills\n\n\
             These are all of your skills; the list is already complete, so answer \
             questions about what you can do from it instead of running commands to \
             look for skills. Read a skill's file only when you are about to use it.\n\
             Skill files live in `{}/.opencode/skills/`.\n\n",
            p.workspace.display()
        ));
        out.push_str(&skills::skill_index(p.skills));
    }
    // The subagent roles are listed once, in the `spawn` tool description:
    // repeating them here cost every request ~200 tokens.
    out.push_str(&ponytail_section(p.ponytail));
    for s in p.baked {
        out.push_str(&format!("\n\n## Skill: {} — always active\n\n", s.name));
        out.push_str(s.body.trim());
    }
    if let Some(extra) = Some(p.mode.instructions()).filter(|s| !s.is_empty()) {
        out.push_str("\n\n## Mode\n\n");
        out.push_str(extra);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ponytail_level_controls_the_rules() {
        let text = |level| {
            compose_system(PromptInputs {
                workspace: Path::new("/ws"),
                mode: AgentMode::Build,
                configured: None,
                agents_md: None,
                skills: &[],
                ponytail: level,
                baked: &[],
            })
        };
        assert!(!text(PonytailLevel::Off).contains("lazy senior developer"), "off drops the rules");
        let full = text(PonytailLevel::Full);
        assert!(full.contains("lazy senior developer") && !full.contains("Ponytail level:"));
        assert!(text(PonytailLevel::Lite).contains("Ponytail level: lite — Build what was asked."));
        assert!(!full.contains("MIT License") && !full.contains("homepage:") && !full.contains("| Level |"),
            "licence header, frontmatter and the levels table stay out of the prompt");
        assert!(full.contains("Never cut:") && full.contains("## The smallest complete change"));
        assert!(text(PonytailLevel::Ultra).contains("Ponytail level: ultra"));
        assert_eq!(PonytailLevel::parse(" Ultra "), Some(PonytailLevel::Ultra));
        assert_eq!(PonytailLevel::parse("max"), None);
    }

    /// Ponytail is part of the system prompt, not a skill: this repository
    /// lists no ponytail skill, and its rules still reach the prompt with
    /// the skill index (where they would show up as a skill) built from it.
    #[test]
    fn ponytail_is_built_in_not_a_listed_skill() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"));
        let skills = crate::skills::load_skills(ws);
        assert!(
            !skills.iter().any(|s| s.name.starts_with("ponytail")),
            "listed skills: {:?}",
            skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        let text = compose_system(PromptInputs {
            workspace: ws,
            mode: AgentMode::Build,
            configured: None,
            agents_md: None,
            skills: &skills,
            ponytail: PonytailLevel::Full,
            baked: &[],
        });
        assert!(text.contains("lazy senior developer"), "the rules are still active");
        let index = crate::skills::skill_index(&skills);
        assert!(!index.contains("ponytail"), "{index}");
    }

    #[tokio::test]
    async fn session_ponytail_level_reaches_the_system_prompt() {
        let ws = std::env::temp_dir().join(format!("ocrs-ponytail-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let st = crate::server::build_state_ephemeral(crate::config::Config::default(), ws.clone()).await;
        assert_eq!(st.ponytail_level("s1"), PonytailLevel::Full, "config default: on");
        st.set_ponytail_level("s1", PonytailLevel::Ultra);
        assert!(st.composed_system("s1").contains("Ponytail level: ultra"));
        assert!(!st.composed_system("s2").contains("Ponytail level:"), "other sessions keep the default");
        st.set_ponytail_level("s1", PonytailLevel::Off);
        assert!(!st.composed_system("s1").contains("lazy senior developer"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            kind: "function".into(),
            function: crate::provider::ToolFnSpec {
                name: name.into(),
                description: String::new(),
                parameters: json!({"type":"object"}),
            },
        }
    }

    #[test]
    fn mode_parse_and_str() {
        assert_eq!(AgentMode::parse("plan"), Some(AgentMode::Plan));
        assert_eq!(AgentMode::parse("BUILD"), Some(AgentMode::Build));
        assert_eq!(AgentMode::parse("other"), None);
        assert_eq!(AgentMode::Plan.as_str(), "plan");
        assert_eq!(AgentMode::default(), AgentMode::Build);
    }

    #[test]
    fn plan_mode_filters_to_read_only_tools() {
        let specs: Vec<ToolSpec> = ["read", "bash", "write", "grep", "edit"]
            .iter()
            .map(|n| spec(n))
            .collect();
        let kept: Vec<String> = AgentMode::Plan
            .filter_specs(specs)
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert_eq!(kept, vec!["read", "grep"]);
        // Build mode keeps everything.
        let all = AgentMode::Build.filter_specs(vec![spec("bash"), spec("write")]);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn compose_default_prompt_with_all_sections() {
        let skills = vec![skills::Doc {
            name: "deploy".into(),
            description: "Ship it".into(),
            body: "body".into(),
            path: "deploy.md".into(),
        }];
        let text = compose_system(PromptInputs {
            workspace: Path::new("/ws"),
            mode: AgentMode::Plan,
            configured: None,
            agents_md: Some("Always run fmt."),
            skills: &skills,
            ponytail: PonytailLevel::Off,
            baked: &[],
        });
        assert!(text.contains("opencode-rust"), "default base");
        assert!(text.contains("Project instructions (AGENTS.md)"));
        assert!(text.contains("Always run fmt."));
        assert!(text.contains("- deploy: Ship it"));
        assert!(!text.contains("- explore:"), "roles are listed in the spawn tool only");
        assert!(text.contains("PLAN MODE"));
        // Mode instructions are last so they stay salient.
        assert!(text.trim_end().ends_with("numbered plan."));
    }

    #[test]
    fn compose_respects_configured_base_and_build_mode() {
        let text = compose_system(PromptInputs {
            workspace: Path::new("/ws"),
            mode: AgentMode::Build,
            configured: Some("CUSTOM BASE"),
            agents_md: None,
            skills: &[],
            ponytail: PonytailLevel::Full,
            baked: &[],
        });
        assert!(
            text.contains("lazy senior developer"),
            "ponytail rules embedded when enabled"
        );
        assert!(!text.contains("PLAN MODE"));
        assert!(text.starts_with("CUSTOM BASE"));
        assert!(!text.contains("## Skills"));
        // The greeting rule is product behaviour, not part of the default
        // base prompt: replacing the base must not re-enable tool calls for
        // "hi".
        assert!(
            text.contains("## When to act"),
            "act-only-when-asked rule survives a custom system_prompt"
        );
    }

    #[test]
    fn prompt_does_not_leak_upstream_paths_into_the_workspace() {
        // `.openclaw/skills` is a path in the vendored upstream repo. Embedding
        // it made the model try to `ls` it here, where it has never existed.
        let text = compose_system(PromptInputs {
            workspace: Path::new("/ws"),
            mode: AgentMode::Build,
            configured: None,
            agents_md: None,
            skills: &[skills::Doc {
                name: "deploy".into(),
                description: "Ship it".into(),
                body: "body".into(),
                path: "deploy.md".into(),
            }],
            ponytail: PonytailLevel::Full,
            baked: &[],
        });
        assert!(
            !text.contains(".openclaw"),
            "upstream repo path must not look like a local directory"
        );
        assert!(
            text.contains("already complete"),
            "skills list is stated as the whole answer"
        );
        assert!(text.contains("/ws/.opencode/skills"), "real location named");
    }

    #[test]
    fn every_prompt_tells_the_model_not_to_work_on_small_talk() {
        let text = compose_system(PromptInputs {
            workspace: Path::new("/ws"),
            mode: AgentMode::Build,
            configured: None,
            agents_md: None,
            skills: &[],
            ponytail: PonytailLevel::Off,
            baked: &[],
        });
        assert!(text.contains("no tool calls"), "explicit no-tools rule");
        assert!(text.to_lowercase().contains("greets you"), "greeting named");
    }

    #[test]
    fn baked_skills_are_embedded_in_full() {
        let baked = vec![skills::Doc {
            name: "deploy".into(),
            description: "Ship it".into(),
            body: "Always run the smoke test after deploying.".into(),
            path: "deploy.md".into(),
        }];
        let text = compose_system(PromptInputs {
            workspace: Path::new("/ws"),
            mode: AgentMode::Build,
            configured: None,
            agents_md: None,
            skills: &[],
            ponytail: PonytailLevel::Off,
            baked: &baked,
        });
        assert!(
            text.contains("## Skill: deploy — always active"),
            "baked skill announced: {text}"
        );
        assert!(
            text.contains("Always run the smoke test after deploying."),
            "full body embedded, not just the index line"
        );
    }

    #[test]
    fn subagent_roster() {
        let defs = builtin_subagents();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explore", "research", "plan", "worker", "review"]);
        let explore = find_subagent("explore").unwrap();
        let tools = explore.tools.unwrap();
        assert!(tools.iter().any(|t| t == "read"));
        assert!(!tools.iter().any(|t| t == "write" || t == "bash"));
        assert!(explore.read_only);
        assert!(find_subagent("worker").unwrap().tools.is_none());
        assert!(!find_subagent("worker").unwrap().read_only);
        assert!(find_subagent("nope").is_none());
        assert!(subagent_index(&defs).contains("- worker:"));
    }

    #[test]
    fn role_files_add_and_replace_roles() {
        let ws = std::env::temp_dir().join(format!("ocrs-roles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let dir = ws.join(".opencode").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("security.md"),
            "---\nname: security\ndescription: Finds security bugs\ntools: [read, grep]\n\
             model: big/model\nmax_turns: 12\n---\nYou hunt for security bugs.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("worker.md"),
            "---\nname: worker\ndescription: Our worker\nisolation: worktree\n---\nHouse rules.\n",
        )
        .unwrap();
        std::fs::write(dir.join("broken.md"), "no frontmatter here").unwrap();
        let roles = load_subagents(&ws);
        let sec = roles.iter().find(|r| r.name == "security").expect("file role loaded");
        assert_eq!(sec.description, "Finds security bugs");
        assert_eq!(sec.system, "You hunt for security bugs.");
        assert_eq!(sec.tools.as_deref(), Some(&["read".to_string(), "grep".to_string()][..]));
        assert!(sec.read_only);
        assert_eq!(sec.model.as_deref(), Some("big/model"));
        assert_eq!(sec.max_steps, Some(12));
        let worker: Vec<_> = roles.iter().filter(|r| r.name == "worker").collect();
        assert_eq!(worker.len(), 1, "the file replaces the built-in role");
        assert_eq!(worker[0].system, "House rules.");
        assert_eq!(worker[0].isolate, Some(true));
        assert!(worker[0].tools.is_none() && !worker[0].read_only);
        assert!(roles.iter().any(|r| r.name == "explore"), "built-ins stay");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
