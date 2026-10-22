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

/// One built-in subagent (Claude's Explore/Scout idea, MiniMax worker idea).
pub struct SubagentDef {
    pub name: &'static str,
    pub description: &'static str,
    /// Subagent system prompt.
    pub system: &'static str,
    /// Restricted tool list; `None` = all tools (except spawn itself).
    pub tools: Option<&'static [&'static str]>,
}

pub fn builtin_subagents() -> Vec<SubagentDef> {
    vec![
        SubagentDef {
            name: "explore",
            description: "Read-only investigator: searches and reads code, reports findings, never modifies anything.",
            system: "You are a read-only exploration subagent. Search and read the workspace, \
                     then answer with precise findings (file paths and line references). You \
                     cannot modify files — do not try.",
            tools: Some(&["read", "list", "glob", "grep", "webfetch"]),
        },
        SubagentDef {
            name: "worker",
            description: "General-purpose subagent: completes an isolated task with the full toolset.",
            system: "You are a focused worker subagent. Complete only the task you were given in \
                     the workspace provided. Use tools as needed. Reply with a 1-2 sentence \
                     summary of the outcome when done.",
            tools: None,
        },
    ]
}

pub fn find_subagent(name: &str) -> Option<SubagentDef> {
    builtin_subagents().into_iter().find(|d| d.name == name)
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
    pub subagents: &'a [SubagentDef],
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
    out.push_str(ponytail_rules().trim());
    if level != PonytailLevel::Full {
        out.push_str(&format!("\n\nPonytail level: {}", level.as_str()));
    }
    out
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
         If a request is ambiguous, ask one short clarifying question instead of guessing.",
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
    if !p.subagents.is_empty() {
        out.push_str("\n\n## Subagents\n\nAvailable subagents (use the spawn tool):\n");
        out.push_str(&subagent_index(p.subagents));
    }
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
                subagents: &[],
                ponytail: level,
                baked: &[],
            })
        };
        assert!(!text(PonytailLevel::Off).contains("lazy senior developer"), "off drops the rules");
        let full = text(PonytailLevel::Full);
        assert!(full.contains("lazy senior developer") && !full.contains("Ponytail level:"));
        assert!(text(PonytailLevel::Lite).contains("Ponytail level: lite"));
        assert!(text(PonytailLevel::Ultra).contains("Ponytail level: ultra"));
        assert_eq!(PonytailLevel::parse(" Ultra "), Some(PonytailLevel::Ultra));
        assert_eq!(PonytailLevel::parse("max"), None);
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
            subagents: &builtin_subagents(),
            ponytail: PonytailLevel::Off,
            baked: &[],
        });
        assert!(text.contains("opencode-rust"), "default base");
        assert!(text.contains("Project instructions (AGENTS.md)"));
        assert!(text.contains("Always run fmt."));
        assert!(text.contains("- deploy: Ship it"));
        assert!(text.contains("- explore:"));
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
            subagents: &[],
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
            subagents: &[],
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
            subagents: &[],
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
            subagents: &[],
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
        assert_eq!(defs.len(), 2);
        let explore = find_subagent("explore").unwrap();
        let tools = explore.tools.unwrap();
        assert!(tools.contains(&"read"));
        assert!(!tools.contains(&"write"));
        assert!(!tools.contains(&"bash"));
        assert!(find_subagent("worker").unwrap().tools.is_none());
        assert!(find_subagent("nope").is_none());
        assert!(subagent_index(&defs).contains("- worker:"));
    }
}
