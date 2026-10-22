//! M2: builtin slash-command classification, shared by the TUI (busy-state
//! decision) and the headless path. Unknown slash lines are NOT commands
//! here — they may be user-defined command templates (`skills::resolve_
//! command`) or plain text for the model.
//!
//! M6 adds [`builtins`] — the one place that knows every command's name,
//! argument shape and one-line purpose. The TUI's command palette, the
//! `/help` overlay and headless `--help` all read from it, so a new command
//! becomes discoverable the moment it is registered here.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Team(String),
    Plan,
    Build,
    Compact,
    Undo,
    Redo,
    Fork,
    Resume(String),
    Diff(Option<String>),
    Skills,
    Quit,
    /// M3: inline edit — one completion, diff preview, accept (no agent loop).
    Edit { path: String, instruction: String },
    /// Ponytail review: senior-dev review of the uncommitted changes,
    /// including callers OUTSIDE the diff.
    Review,
    /// Laziness Meter report: lines added/deleted/reuse hints this session.
    Debt,
    /// Run the same task in isolated git worktrees and compare results.
    Parallel(String),
    /// Assign a mission: `kind` preselected or `None` (open the picker).
    /// The goal is expanded into a well-worded briefing before the run.
    Mission { kind: Option<crate::mission::MissionKind>, goal: String },
    /// Turbo mode: toggle (no prompt) or enable + run `prompt` immediately.
    Turbo { prompt: Option<String> },
    /// Install a skill from a local path or URL: vet it, then bake it in.
    SkillInstall(String),
    /// M6: show the command/key reference overlay.
    Help,
    /// M6: list sessions and switch to one.
    Sessions,
    /// M6: clear the transcript view (session history is untouched).
    Clear,
    /// M6: show this session's todo list.
    Todos,
    /// M6: show the active model (None) or switch to it (Some).
    Model(Option<String>),
    /// M6: write the transcript to markdown (default path when None).
    Export(Option<String>),
    /// M6: start a fresh session.
    New,
    /// M6: create a starter AGENTS.md when the repo has none.
    Init,
}

/// One registered command: how it is typed, what it takes, what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builtin {
    /// Command verb without the slash.
    pub name: &'static str,
    /// Argument shape for the help/palette line ("" = takes none).
    pub args: &'static str,
    /// One-line purpose.
    pub help: &'static str,
}

impl Builtin {
    /// `name args` — how the command appears in a list.
    pub fn usage(&self) -> String {
        if self.args.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.args)
        }
    }
}

/// Every builtin command, alphabetically. This drives the palette, `/help`
/// and the headless help text, so it must stay in sync with [`classify`].
pub fn builtins() -> Vec<Builtin> {
    vec![
        Builtin { name: "build", args: "", help: "Normal build mode (default)" },
        Builtin { name: "clear", args: "", help: "Clear the transcript view (history kept)" },
        Builtin { name: "compact", args: "", help: "Summarize old history, keep the recent tail" },
        Builtin { name: "debt", args: "", help: "Laziness Meter: lines added/deleted/reuse hints" },
        Builtin { name: "diff", args: "[id|n]", help: "Diff against the parent or another session" },
        Builtin { name: "edit", args: "<file> <what>", help: "Inline edit: one completion, diff preview" },
        Builtin { name: "export", args: "[path]", help: "Write the transcript to markdown" },
        Builtin { name: "fork", args: "", help: "Branch this session into a child session" },
        Builtin { name: "help", args: "", help: "Show the command and key reference" },
        Builtin { name: "init", args: "", help: "Create a starter AGENTS.md when missing" },
        Builtin { name: "mission", args: "[kind] <goal>", help: "Run a briefing (Explore/Commit/Merge PR/Custom)" },
        Builtin { name: "model", args: "[id]", help: "Show the active model, or switch to <id>" },
        Builtin { name: "new", args: "", help: "Start a fresh session" },
        Builtin { name: "parallel", args: "<task>", help: "Run one task in two isolated worktrees" },
        Builtin { name: "plan", args: "", help: "Read-only plan mode (no writes)" },
        Builtin { name: "quit", args: "", help: "Exit" },
        Builtin { name: "redo", args: "", help: "Re-apply the last undone snapshot" },
        Builtin { name: "resume", args: "<id|n>", help: "Switch to another session" },
        Builtin { name: "review", args: "", help: "Senior-dev review of the uncommitted changes" },
        Builtin { name: "sessions", args: "", help: "Browse sessions and switch to one" },
        Builtin { name: "skill", args: "install <path|url>", help: "Vet, install and bake in a skill" },
        Builtin { name: "skills", args: "", help: "List loaded skills" },
        Builtin { name: "team", args: "<goal>", help: "Plan → parallel workers → review → synthesis" },
        Builtin { name: "todos", args: "", help: "Show this session's todo list" },
        Builtin { name: "turbo", args: "[prompt]", help: "Full access, no approvals, keeps working" },
        Builtin { name: "undo", args: "", help: "Restore the workspace to the last snapshot" },
    ]
}

/// The `/help` overlay body: every command with its usage and purpose.
pub fn help_text() -> String {
    let mut out = String::from("Commands\n");
    for b in builtins() {
        out.push_str(&format!("  {:<26} {}\n", b.usage(), b.help));
    }
    out.push_str(
        "\nKeys\n  Enter        send the message\n  Ctrl+C       quit\n  Tab          \
         complete a command · jump to the next accepted edit\n  Shift+Tab    jump \
         back\n  Esc          close the palette / overlay\n  ↑ ↓          \
         scroll transcript (or move in a list)\n  F1 / /help   this reference\n\n\
         Anything not starting with a known command is sent to the agent.",
    );
    out
}

/// Parse a builtin command from a typed line. `None` = not a builtin
/// (prompt, user command template, or bare `/verb` we don't know).
pub fn classify(input: &str) -> Option<Command> {
    let rest = input.trim().strip_prefix('/')?;
    let (verb, arg) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    match verb {
        "quit" | "q" if arg.is_empty() => Some(Command::Quit),
        "help" | "?" if arg.is_empty() => Some(Command::Help),
        "sessions" if arg.is_empty() => Some(Command::Sessions),
        "new" if arg.is_empty() => Some(Command::New),
        "clear" if arg.is_empty() => Some(Command::Clear),
        "todos" if arg.is_empty() => Some(Command::Todos),
        "init" if arg.is_empty() => Some(Command::Init),
        "model" | "models" => Some(Command::Model(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        })),
        "export" => Some(Command::Export(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        })),
        "team" if !arg.is_empty() => Some(Command::Team(arg.to_string())),
        "plan" if arg.is_empty() => Some(Command::Plan),
        "build" if arg.is_empty() => Some(Command::Build),
        "compact" if arg.is_empty() => Some(Command::Compact),
        "undo" if arg.is_empty() => Some(Command::Undo),
        "redo" if arg.is_empty() => Some(Command::Redo),
        "fork" if arg.is_empty() => Some(Command::Fork),
        "resume" if !arg.is_empty() => Some(Command::Resume(arg.to_string())),
        "diff" => Some(Command::Diff(if arg.is_empty() {
            None
        } else {
            Some(arg.to_string())
        })),
        "skills" if arg.is_empty() => Some(Command::Skills),
        "review" if arg.is_empty() => Some(Command::Review),
        "debt" if arg.is_empty() => Some(Command::Debt),
        "parallel" if !arg.is_empty() => Some(Command::Parallel(arg.to_string())),
        "mission" if !arg.is_empty() => {
            // `/mission <kind> <goal>` runs with the kind preselected;
            // `/mission <goal>` opens the mission picker with the goal filled.
            let mut it = arg.splitn(2, char::is_whitespace);
            let first = it.next().unwrap_or("").trim();
            let rest = it.next().unwrap_or("").trim();
            match (crate::mission::MissionKind::parse(first), rest) {
                (Some(_), "") => None, // kind without a goal
                (Some(kind), goal) => {
                    Some(Command::Mission { kind: Some(kind), goal: goal.to_string() })
                }
                (None, _) => Some(Command::Mission { kind: None, goal: arg.to_string() }),
            }
        }
        "turbo" => Some(Command::Turbo {
            prompt: if arg.is_empty() { None } else { Some(arg.to_string()) },
        }),
        "skill" => {
            // `/skill install <path|url>` — the only subcommand.
            let src = arg.strip_prefix("install ")?.trim();
            if src.is_empty() {
                return None;
            }
            Some(Command::SkillInstall(src.to_string()))
        }
        "edit" => {
            // `/edit <path> <instruction…>` — both parts required.
            let mut it = arg.splitn(2, char::is_whitespace);
            let path = it.next().unwrap_or("").trim();
            let instruction = it.next().unwrap_or("").trim();
            if path.is_empty() || instruction.is_empty() {
                return None;
            }
            Some(Command::Edit { path: path.to_string(), instruction: instruction.to_string() })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_builtins() {
        assert_eq!(classify("/quit"), Some(Command::Quit));
        assert_eq!(classify("/q"), Some(Command::Quit));
        assert_eq!(classify("/team refactor the build"), Some(Command::Team("refactor the build".into())));
        assert_eq!(classify("/plan"), Some(Command::Plan));
        assert_eq!(classify("  /build  "), Some(Command::Build));
        assert_eq!(classify("/compact"), Some(Command::Compact));
        assert_eq!(classify("/undo"), Some(Command::Undo));
        assert_eq!(classify("/redo"), Some(Command::Redo));
        assert_eq!(classify("/fork"), Some(Command::Fork));
        assert_eq!(classify("/resume ses_123"), Some(Command::Resume("ses_123".into())));
        assert_eq!(classify("/resume 3"), Some(Command::Resume("3".into())));
        assert_eq!(classify("/diff"), Some(Command::Diff(None)));
        assert_eq!(classify("/diff 2"), Some(Command::Diff(Some("2".into()))));
        assert_eq!(classify("/skills"), Some(Command::Skills));
        assert_eq!(classify("/review"), Some(Command::Review));
        assert_eq!(classify("/debt"), Some(Command::Debt));
        assert_eq!(
            classify("/parallel add a date picker"),
            Some(Command::Parallel("add a date picker".into()))
        );
        assert_eq!(
            classify("/mission explore the auth flow"),
            Some(Command::Mission {
                kind: Some(crate::mission::MissionKind::Explore),
                goal: "the auth flow".into()
            })
        );
        assert_eq!(
            classify("/mission custom fix the flaky test"),
            Some(Command::Mission {
                kind: Some(crate::mission::MissionKind::Custom),
                goal: "fix the flaky test".into()
            })
        );
        assert_eq!(
            classify("/mission fix the flaky test"),
            Some(Command::Mission { kind: None, goal: "fix the flaky test".into() }),
            "first word is not a kind → whole arg is the goal, picker decides"
        );
        assert_eq!(
            classify("/mission merge PR #12"),
            Some(Command::Mission {
                kind: Some(crate::mission::MissionKind::MergePr),
                goal: "PR #12".into()
            })
        );
        assert_eq!(classify("/turbo"), Some(Command::Turbo { prompt: None }));
        assert_eq!(
            classify("/turbo fix the build"),
            Some(Command::Turbo { prompt: Some("fix the build".into()) })
        );
        assert_eq!(
            classify("/skill install ./skills/deploy.md"),
            Some(Command::SkillInstall("./skills/deploy.md".into()))
        );
        assert_eq!(
            classify("/edit src/main.rs add logging to startup"),
            Some(Command::Edit {
                path: "src/main.rs".into(),
                instruction: "add logging to startup".into()
            })
        );
        assert_eq!(
            classify("/edit  docs/a b   with   spaces  "),
            Some(Command::Edit { path: "docs/a".into(), instruction: "b   with   spaces".into() })
        );
    }

    #[test]
    fn parses_m6_control_commands() {
        assert_eq!(classify("/help"), Some(Command::Help));
        assert_eq!(classify("/?"), Some(Command::Help));
        assert_eq!(classify("/sessions"), Some(Command::Sessions));
        assert_eq!(classify("/new"), Some(Command::New));
        assert_eq!(classify("/clear"), Some(Command::Clear));
        assert_eq!(classify("/todos"), Some(Command::Todos));
        assert_eq!(classify("/init"), Some(Command::Init));
        assert_eq!(classify("/model"), Some(Command::Model(None)), "bare /model lists");
        assert_eq!(classify("/models"), Some(Command::Model(None)));
        assert_eq!(
            classify("/model gpt-5"),
            Some(Command::Model(Some("gpt-5".into())))
        );
        assert_eq!(classify("/export"), Some(Command::Export(None)));
        assert_eq!(
            classify("/export notes/run.md"),
            Some(Command::Export(Some("notes/run.md".into())))
        );
    }

    #[test]
    fn rejects_bare_or_unknown() {
        assert_eq!(classify("plain text"), None);
        assert_eq!(classify("/"), None);
        assert_eq!(classify("/team"), None, "goal required");
        assert_eq!(classify("/resume"), None, "reference required");
        assert_eq!(classify("/plan extra"), None, "no stray args");
        assert_eq!(classify("/deploy staging"), None, "user command template");
        assert_eq!(classify("/unknown"), None);
        assert_eq!(classify("/edit"), None, "needs path and instruction");
        assert_eq!(classify("/edit only-a-path.md"), None, "needs an instruction");
        assert_eq!(classify("/review now"), None, "no stray args");
        assert_eq!(classify("/parallel"), None, "task required");
        assert_eq!(classify("/mission"), None, "goal required");
        assert_eq!(classify("/mission custom"), None, "kind without goal");
        assert_eq!(classify("/mission explore"), None, "kind without goal");
        assert_eq!(classify("/skill"), None, "needs install <src>");
        assert_eq!(classify("/skill install"), None, "needs a source");
        assert_eq!(classify("/skill list"), None, "only install is builtin");
        assert_eq!(classify("/help now"), None, "no stray args");
        assert_eq!(classify("/sessions 3"), None, "no stray args");
    }

    #[test]
    fn every_builtin_verb_is_classified_or_arg_gated() {
        // The registry and the parser must not drift apart: a registered
        // command must either parse on its own, or parse once given args.
        for b in builtins() {
            let typed = match b.name {
                // Two-part arguments need a real sample, not a single token.
                "edit" => "/edit src/lib.rs add a log line".to_string(),
                "skill" => "/skill install ./skills/x.md".to_string(),
                _ if b.args.is_empty() => format!("/{}", b.name),
                _ => format!("/{} x", b.name),
            };
            assert!(
                classify(&typed).is_some(),
                "`{typed}` is registered but does not classify"
            );
        }
    }

    #[test]
    fn registry_is_alphabetical_and_unique() {
        let names: Vec<&str> = builtins().iter().map(|b| b.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "palette order should be alphabetical");
        let mut dedup = names.clone();
        dedup.dedup();
        assert_eq!(names, dedup, "no duplicate verbs");
        assert!(builtins().iter().all(|b| !b.help.is_empty()), "every command is documented");
    }

    #[test]
    fn help_text_lists_every_command() {
        let t = help_text();
        for b in builtins() {
            assert!(t.contains(b.name), "help omits {}", b.name);
        }
        assert!(t.contains("Ctrl+C"));
        assert!(t.contains("Tab"));
    }
}
