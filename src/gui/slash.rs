//! Built-in `/` commands the desktop app runs itself.

use super::*;

/// Built-in commands the desktop app runs itself (name, arguments, help),
/// also offered by the composer's `/` autocomplete.
pub const GUI_COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "List these commands"),
    ("new", "", "Start a new chat in this project"),
    ("clear", "", "Clear the view (the chat's history is kept)"),
    ("plan", "", "Plan mode: read-only, the agent proposes a plan"),
    ("build", "", "Build mode: the agent changes files (default)"),
    ("model", "[id]", "Show the model, or switch to another one"),
    ("usage", "", "Tokens used, and how much came from the cache"),
    ("compact", "", "Summarize old messages to make room"),
    ("review", "", "Review the uncommitted changes"),
    ("deep", "[n] <task>", "Try the task n times (default 3) in separate copies, keep the best checked one"),
    (
        "mission",
        "<goal> | status | stop | resume",
        "Mission mode: works on the goal step by step, on its own, until it is done",
    ),
    ("undo", "", "Put the files back as they were before the last run"),
    ("redo", "", "Re-apply what /undo put back"),
    ("init", "", "Write a starter AGENTS.md for this project"),
    ("export", "[path]", "Save this chat as Markdown in the project"),
];

/// Run `line` when it is one of `GUI_COMMANDS`. False = not one of them:
/// it goes to the agent (project commands are resolved by the core).
pub(super) fn gui_command(
    gs: &mut GuiState,
    st: &Arc<server::AppState>,
    rt: &tokio::runtime::Handle,
    line: &str,
) -> bool {
    let Some(rest) = line.trim().strip_prefix('/') else { return false };
    let (name, arg) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(n, a)| (n, a.trim()));
    if !GUI_COMMANDS.iter().any(|(n, _, _)| *n == name) {
        return false;
    }
    let sid = gs.session.clone().unwrap_or_default();
    let root = chat_root(gs, st);
    let say = |gs: &mut GuiState, text: String| gs.push(EntryKind::System, text);
    gs.push(EntryKind::User, format!("\u{203a} /{rest}"));
    match name {
        "help" => {
            let list: Vec<String> = GUI_COMMANDS
                .iter()
                .map(|(n, a, h)| format!("/{n}{}{a} — {h}", if a.is_empty() { "" } else { " " }))
                .collect();
            say(gs, format!("Commands:\n{}\nProject commands from .opencode/commands work too.", list.join("\n")));
        }
        "new" => {
            let project = gs.current_project();
            open_new_chat(gs, st, project);
        }
        "clear" => {
            gs.entries.clear();
            say(gs, "View cleared — the chat's history is kept.".into());
        }
        "plan" | "build" => {
            let mode = if name == "plan" { crate::agents::AgentMode::Plan } else { crate::agents::AgentMode::Build };
            st.set_mode(&sid, mode);
            say(gs, format!("{} mode for this chat.", if name == "plan" { "Plan" } else { "Build" }));
        }
        "model" if arg.is_empty() => {
            let current = gs.settings.model.clone();
            say(gs, format!("Model: {current}. Switch with /model <id>, or open Settings (Ctrl+,)."));
        }
        "model" => {
            let (st2, model) = (st.clone(), arg.to_string());
            rt.spawn(async move { st2.set_model(&model).await });
            gs.settings.model = arg.to_string();
            say(gs, format!("Model switched to {arg} (until restart; Settings keeps it)."));
        }
        "usage" => {
            // T-070: the report builds the whole system prompt (repo map,
            // skills) and runs three queries — not on the GUI thread. It
            // comes back as a note, like /compact's.
            let (st2, sid2) = (st.clone(), sid.clone());
            rt.spawn_blocking(move || {
                let text = st2.usage_report(&sid2);
                let _ = st2.events.send(ServerEvent::Note { session_id: sid2, text });
            });
        }
        "compact" => {
            let st2 = st.clone();
            let sid2 = sid.clone();
            rt.spawn(async move {
                let abort = std::sync::atomic::AtomicBool::new(false);
                let text = match crate::compact::compact(&st2, &sid2, &abort).await {
                    Ok(o) => format!("Compacted: {} old message(s) summarized, {} kept.", o.removed, o.kept),
                    Err(e) => format!("Not compacted: {e:#}"),
                };
                let _ = st2.events.send(ServerEvent::Note { session_id: sid2, text });
            });
            say(gs, "Compacting…".into());
        }
        "review" => {
            let st2 = st.clone();
            let sid2 = sid.clone();
            rt.spawn(async move { st2.kick_review(sid2).await });
            gs.busy = true;
        }
        "mission" => match crate::commands::classify(line) {
            Some(crate::commands::Command::MissionCtl(what)) if what == "status" => {
                // T-147: read off this thread; it comes back as a note.
                let (st2, sid2) = (st.clone(), sid.clone());
                rt.spawn(async move {
                    let text = server::mission_report(&st2, &sid2).await;
                    let _ = st2.events.send(ServerEvent::Note { session_id: sid2, text });
                });
            }
            Some(crate::commands::Command::MissionCtl(what)) if what == "stop" => {
                let (st2, sid2) = (st.clone(), sid.clone());
                rt.spawn(async move { st2.abort_run(&sid2).await });
                say(gs, "Stopping the mission — /mission resume carries on.".into());
            }
            Some(crate::commands::Command::MissionCtl(_)) => {
                let (st2, sid2) = (st.clone(), sid.clone());
                rt.spawn(async move { st2.kick_mission(sid2, None).await });
                gs.busy = true;
            }
            Some(crate::commands::Command::Mission { kind, goal }) => {
                let (effort, goal) = crate::mission::split_effort(&goal);
                let mission =
                    crate::mission::Mission { kind: kind.unwrap_or(crate::mission::MissionKind::Custom), effort, goal };
                if gs.title() == NEW_CHAT {
                    let _ = st.db.set_session_title(&sid, &clip(&mission.goal, 40));
                    refresh_sessions(gs, st);
                }
                // T-143: started the way the TUI and headless runs start it.
                let (st2, sid2) = (st.clone(), sid.clone());
                rt.spawn(async move { crate::frontend::start_mission(&st2, sid2, mission, false).await });
                gs.busy = true;
            }
            _ => say(gs, "Usage: /mission <goal>, or /mission status | stop | resume.".into()),
        },
        "deep" => match crate::commands::classify(line) {
            Some(crate::commands::Command::Deep { attempts, task }) => {
                if gs.title() == NEW_CHAT {
                    let _ = st.db.set_session_title(&sid, &clip(&task, 40));
                    refresh_sessions(gs, st);
                }
                let st2 = st.clone();
                let sid2 = sid.clone();
                rt.spawn(async move { st2.kick_deep(sid2, task, attempts).await });
                gs.busy = true;
            }
            _ => say(gs, "Usage: /deep [n] <task>, with n from 1 to 5 (default 3).".into()),
        },
        "undo" | "redo" => {
            // T-143: the shared snapshot step (git runs off this thread);
            // the outcome comes back as a note.
            let (redo, force) = (name == "redo", name == "undo" && arg == "force");
            let (st2, sid2, name) = (st.clone(), sid.clone(), name.to_string());
            rt.spawn(async move {
                let text = match crate::frontend::run_snapshot(root, st2.state_dir.clone(), redo, force).await {
                    Ok(Ok(Some(m))) => {
                        format!("{} done: files are back as of snapshot {}.", if redo { "Redo" } else { "Undo" }, m.id)
                    }
                    Ok(Ok(None)) => format!("Nothing to {name} (or this project is not a git repository)."),
                    Ok(Err(e)) => format!("{name} failed: {e:#}"),
                    Err(e) => format!("{name} failed: {e}"),
                };
                let _ = st2.events.send(ServerEvent::Note { session_id: sid2, text });
            });
        }
        "init" => {
            let text = match crate::skills::init_agents_md(&root) {
                Ok(Some(p)) => format!("Wrote {} — edit it to tell agents how this project works.", p.display()),
                Ok(None) => "This project already has an AGENTS.md.".into(),
                Err(e) => format!("init failed: {e:#}"),
            };
            say(gs, text);
        }
        "export" => {
            let text = match st.export_transcript(&sid, (!arg.is_empty()).then_some(arg)) {
                Ok(path) => format!("Chat exported to {}", path.display()),
                Err(e) => format!("Export failed: {e:#}"),
            };
            say(gs, text);
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built-in commands run in the app; anything else goes to the agent;
    /// `@` finds project files.
    #[tokio::test]
    async fn desktop_commands_run_here_and_others_go_to_the_agent() {
        let dir = std::env::temp_dir().join(format!("habi-cmds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("app.js"), "x").unwrap();
        std::fs::write(dir.join("readme.md"), "x").unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = "http://127.0.0.1:9/v1".into();
        let st = server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("c", None).unwrap();
        let mut gs = GuiState { session: Some(s.id.clone()), ..Default::default() };
        let rt = tokio::runtime::Handle::current();
        assert!(gui_command(&mut gs, &st, &rt, "/help"));
        assert!(gs.entries.last().unwrap().1.contains("/export [path]"));
        assert!(gui_command(&mut gs, &st, &rt, "/init"));
        assert!(dir.join("AGENTS.md").is_file(), "/init wrote the starter file");
        assert!(gui_command(&mut gs, &st, &rt, "/plan"));
        assert_eq!(st.mode_for(&s.id), crate::agents::AgentMode::Plan);
        assert!(gui_command(&mut gs, &st, &rt, "/clear"));
        assert_eq!(gs.entries.len(), 1, "only the 'cleared' note is left");
        // T-070: /usage is built off this thread and comes back as a note.
        let mut events = st.events.subscribe();
        assert!(gui_command(&mut gs, &st, &rt, "/usage"));
        let note = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(ServerEvent::Note { session_id, text }) = events.recv().await {
                    break (session_id, text);
                }
            }
        })
        .await
        .expect("the usage note arrived");
        assert_eq!(note.0, s.id);
        assert!(!note.1.is_empty());
        // T-147: so is /mission status.
        assert!(gui_command(&mut gs, &st, &rt, "/mission status"));
        let note = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(ServerEvent::Note { text, .. }) = events.recv().await {
                    break text;
                }
            }
        })
        .await
        .expect("the mission note arrived");
        assert!(note.contains("This chat has no mission"), "{note}");
        // T-143: /undo runs the shared snapshot step; no snapshot here.
        assert!(gui_command(&mut gs, &st, &rt, "/undo"));
        let note = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(ServerEvent::Note { text, .. }) = events.recv().await {
                    break text;
                }
            }
        })
        .await
        .expect("the undo note arrived");
        assert!(note.starts_with("Nothing to undo") || note.starts_with("undo failed"), "{note}");
        assert!(!gui_command(&mut gs, &st, &rt, "/deploy staging"), "a project command goes to the core");
        assert!(!gui_command(&mut gs, &st, &rt, "fix the bug"));
        assert_eq!(find_project_files(&dir, "APP", 5), vec!["src/app.js".to_string()]);
        assert_eq!(find_project_files(&dir, "", 1).len(), 1, "capped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
