    use super::*;
    use crate::agents::AgentMode;
    use crate::commands::builtins;
    use crate::server::SessionInfo;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_and_send() {
        let mut app = App::new();
        for c in "hello".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.input, "hello");
        assert_eq!(app.cursor, 5);
        let action = app.on_key(key(KeyCode::Enter));
        assert!(matches!(action, Some(InputAction::Prompt(ref p)) if p == "hello"));
        assert!(app.busy);
        assert!(app.input.is_empty());
    }

    #[test]
    fn backspace_edits() {
        let mut app = App::new();
        for c in "abc".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "ab");
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn approval_keys() {
        let mut app = App::new();
        app.approval = Some(PendingApproval { id: "apr_1".into(), tool: "bash".into(), detail: "rm x".into() });
        let a = app.on_key(key(KeyCode::Char('y')));
        assert!(matches!(a, Some(InputAction::Approve { always: false, .. })));
        assert!(app.approval.is_none());
    }

    #[test]
    fn approval_always_and_deny() {
        let mut app = App::new();
        app.approval = Some(PendingApproval { id: "a".into(), tool: "t".into(), detail: "d".into() });
        let a = app.on_key(key(KeyCode::Char('a')));
        assert!(matches!(a, Some(InputAction::Approve { always: true, .. })));
        app.approval = Some(PendingApproval { id: "b".into(), tool: "t".into(), detail: "d".into() });
        let d = app.on_key(key(KeyCode::Esc));
        assert!(matches!(d, Some(InputAction::Deny(_))));
    }

    #[test]
    fn team_command_detected() {
        let mut app = App::new();
        for c in "/team refactor the build".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let action = app.on_key(key(KeyCode::Enter));
        assert!(matches!(action, Some(InputAction::Team(ref g)) if g == "refactor the build"));
    }

    #[test]
    fn events_update_transcript() {
        let mut app = App::new();
        app.on_event(ServerEvent::AssistantDelta { session_id: "s".into(), text: "Hel".into() });
        app.on_event(ServerEvent::AssistantDelta { session_id: "s".into(), text: "lo".into() });
        assert_eq!(app.entries.last().unwrap().text, "Hello");
        app.on_event(ServerEvent::RunFinished { session_id: "s".into(), ok: true, message: Some("done".into()) });
        assert!(!app.busy);
    }

    #[test]
    fn laziness_meter_tracks_stats_and_commands() {
        let mut app = App::new();
        // Stats events drive the status bar.
        app.on_event(ServerEvent::Stats {
            session_id: "s".into(),
            added: 12,
            deleted: 40,
            hints: 2,
        });
        assert_eq!(app.stats, Some((12, 40, 2)));
        // /review runs the agent; /debt is instant; /parallel runs the agent.
        type_line(&mut app, "/review");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Review)));
        assert!(app.busy, "review is an agent run");
        // B-33: the review run ends; only then is the app idle again.
        app.on_event(ServerEvent::RunFinished { session_id: "s".into(), ok: true, message: None });
        type_line(&mut app, "/debt");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Debt)));
        assert!(!app.busy, "debt is local");
        type_line(&mut app, "/parallel add a date picker");
        assert!(
            matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Parallel(ref t)) if t == "add a date picker")
        );
        assert!(app.busy, "parallel is an agent run");
        // A senior-dev note lands as a system line.
        app.on_event(ServerEvent::Note {
            session_id: "s".into(),
            text: "ponytail reuse hint: `parse_config` already exists".into(),
        });
        assert!(app.entries.last().unwrap().text.contains("ponytail reuse hint"));
    }

    #[test]
    fn mission_picker_expands_into_a_briefing() {
        use crate::mission::{Effort, MissionKind};
        let mut app = App::new();
        // `/mission <goal>` with no kind opens the dialog (Custom checked).
        type_line(&mut app, "/mission fix the flaky test");
        assert!(app.on_key(key(KeyCode::Enter)).is_none(), "dialog intercepts");
        let mp = app.mission_picker.as_ref().expect("picker open");
        assert_eq!(mp.kind_idx, 3, "Custom preselected like Freebuff");
        assert_eq!(mp.effort_idx, 2, "High effort preselected");
        // ↑/↓ move the mission, ←/→ the effort.
        assert!(app.on_key(key(KeyCode::Up)).is_none());
        assert!(app.on_key(key(KeyCode::Up)).is_none());
        assert_eq!(app.mission_picker.as_ref().unwrap().kind_idx, 1, "Commit");
        assert!(app.on_key(key(KeyCode::Right)).is_none());
        assert_eq!(app.mission_picker.as_ref().unwrap().effort_idx, 3, "Xhigh");
        // Enter runs the mission.
        let a = app.on_key(key(KeyCode::Enter));
        let Some(InputAction::Mission(m)) = a else { panic!("expected a mission action") };
        assert_eq!(m.kind, MissionKind::Commit);
        assert_eq!(m.effort, Effort::Xhigh);
        assert_eq!(m.goal, "fix the flaky test");
        let b = m.briefing();
        assert!(b.contains("COMMIT mission") && b.contains("EFFORT Xhigh"), "{b}");
        assert!(app.mission_picker.is_none(), "picker closed after run");
        // Esc cancels cleanly.
        type_line(&mut app, "/mission something");
        app.on_key(key(KeyCode::Enter));
        assert!(app.on_key(key(KeyCode::Esc)).is_none());
        assert!(app.mission_picker.is_none());
    }

    #[test]
    fn turbo_toggles_and_runs_inline() {
        let mut app = App::new();
        assert!(!app.turbo);
        type_line(&mut app, "/turbo");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(a, Some(InputAction::Turbo(None))));
        assert!(app.turbo, "bare /turbo toggles on");
        assert!(!app.busy, "toggle alone is not a run");
        type_line(&mut app, "/turbo");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Turbo(None))));
        assert!(!app.turbo, "toggles off");
        // `/turbo <prompt>` enables + runs immediately.
        type_line(&mut app, "/turbo fix the build");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(a, Some(InputAction::Turbo(Some(ref p))) if p == "fix the build"));
        assert!(app.turbo && app.busy);
    }

    #[test]
    fn skill_install_command_reaches_the_core() {
        let mut app = App::new();
        type_line(&mut app, "/skill install ./skills/ship.md");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(a, Some(InputAction::SkillInstall(ref s)) if s == "./skills/ship.md"));
        assert!(app.busy, "install runs vetting through the model");
    }

    #[test]
    fn reasoning_streams_into_thinking_entry() {
        let mut app = App::new();
        app.on_event(ServerEvent::ReasoningDelta { session_id: "s".into(), text: "let me think".into() });
        app.on_event(ServerEvent::ReasoningDelta { session_id: "s".into(), text: "…more".into() });
        // Two deltas coalesce into one Thinking entry with the ✻ marker.
        let t = app.entries.last().unwrap();
        assert_eq!(t.kind, EntryKind::Thinking);
        assert_eq!(t.text, "✻ let me think…more");
        // Assistant text after thinking starts a NEW entry, not appended to it.
        app.on_event(ServerEvent::AssistantDelta { session_id: "s".into(), text: "answer".into() });
        let a = app.entries.last().unwrap();
        assert_eq!(a.kind, EntryKind::Assistant);
        assert_eq!(a.text, "answer");
    }

    #[test]
    fn heartbeat_drives_api_status() {
        let mut app = App::new();
        assert!(app.api.is_none());
        app.on_event(ServerEvent::Heartbeat { ok: true, latency_ms: 42 });
        assert_eq!(app.api, Some((true, 42)));
        app.on_event(ServerEvent::Heartbeat { ok: false, latency_ms: 8000 });
        assert_eq!(app.api, Some((false, 8000)));
    }

    #[test]
    fn quit_command() {
        let mut app = App::new();
        for c in "/quit".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.should_quit);
    }

    fn type_line(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn mode_switch_commands() {
        let mut app = App::new();
        type_line(&mut app, "/plan");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(a, Some(InputAction::SetMode(m)) if m == AgentMode::Plan));
        assert_eq!(app.mode, AgentMode::Plan);
        assert!(!app.busy, "mode switch is instant, not a run");

        type_line(&mut app, "/build");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(a, Some(InputAction::SetMode(m)) if m == AgentMode::Build));
        assert_eq!(app.mode, AgentMode::Build);
        assert!(!app.busy);
    }

    #[test]
    fn local_commands_do_not_set_busy() {
        let mut app = App::new();
        type_line(&mut app, "/undo");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Undo)));
        assert!(!app.busy);

        type_line(&mut app, "/redo");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Redo)));

        type_line(&mut app, "/fork");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Fork)));

        type_line(&mut app, "/resume 3");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Resume(ref r)) if r == "3"));

        type_line(&mut app, "/diff");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Diff(None))));

        type_line(&mut app, "/skills");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Skills)));
        assert!(!app.busy);
    }

    #[test]
    fn compact_is_a_busy_command() {
        let mut app = App::new();
        type_line(&mut app, "/compact");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Compact)));
        assert!(app.busy);
    }

    #[test]
    fn unknown_slash_goes_to_prompt() {
        let mut app = App::new();
        type_line(&mut app, "/deploy staging");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(a, Some(InputAction::Prompt(ref p)) if p == "/deploy staging"));
        assert!(app.busy, "user commands run through the agent");
    }

    #[test]
    fn bare_builtin_command_shows_usage_not_chat() {
        // Regression: a live session typed "   /resume" (leading spaces and
        // no argument) and it silently went to the model as chat.
        let mut app = App::new();
        type_line(&mut app, "   /resume");
        assert!(app.on_key(key(KeyCode::Enter)).is_none(), "no action, no run");
        assert!(!app.busy);
        let last = app.entries.last().unwrap();
        assert_eq!(last.kind, EntryKind::Error);
        assert!(last.text.starts_with("usage: /resume"), "got: {}", last.text);

        type_line(&mut app, "/team");
        assert!(app.on_key(key(KeyCode::Enter)).is_none());
        assert!(app.entries.last().unwrap().text.starts_with("usage: /team"));
    }

    #[test]
    fn session_loaded_replays_and_switches() {
        let mut app = App::new();
        app.busy = true;
        type_line(&mut app, "old input");
        app.on_event(ServerEvent::SessionLoaded {
            session_id: "ses_x".into(),
            mode: "plan".into(),
            transcript: vec![
                crate::server::TranscriptLine { role: "user".into(), text: "hello".into() },
                crate::server::TranscriptLine { role: "assistant".into(), text: "hi".into() },
            ],
        });
        assert_eq!(app.session_id.as_deref(), Some("ses_x"));
        assert_eq!(app.mode, AgentMode::Plan);
        assert!(!app.busy);
        assert_eq!(app.input, "old input", "input buffer left alone");
        assert!(app
            .entries
            .iter()
            .any(|e| e.kind == EntryKind::User && e.text == "hello"));
        assert!(app.entries.iter().any(|e| e.kind == EntryKind::Assistant && e.text == "hi"));
        assert!(app.entries.last().unwrap().text.contains("switched to session ses_x"));
    }

    // ---- M3: inline /edit + Tab jump ----

    #[test]
    fn edit_command_is_a_busy_action() {
        let mut app = App::new();
        type_line(&mut app, "/edit src/lib.rs rename Foo to Bar");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            a,
            Some(InputAction::Edit { ref path, ref instruction })
                if path == "src/lib.rs" && instruction == "rename Foo to Bar"
        ));
        assert!(app.busy, "a completion is running");
    }

    #[test]
    fn edit_preview_accept_and_reject() {
        let mut app = App::new();
        app.busy = true;
        app.on_event(ServerEvent::EditPreview {
            session_id: "s".into(),
            path: "a.rs".into(),
            diff: vec!["  keep".into(), "-old".into(), "+new".into()],
            old: "old".into(),
            new: "new".into(),
            line: 7,
        });
        assert!(!app.busy, "preview arrival ends the busy state");

        // Any non y/n key is swallowed while the overlay is up.
        let a = app.on_key(key(KeyCode::Char('x')));
        assert!(a.is_none());
        assert!(app.pending_edit.is_some());

        let a = app.on_key(key(KeyCode::Char('y')));
        assert!(matches!(
            a,
            Some(InputAction::ApplyEdit { ref path, ref new, line: 7, .. })
                if path == "a.rs" && new == "new"
        ));
        assert!(app.pending_edit.is_none());

        // Reject path.
        app.pending_edit = Some(PendingEdit {
            path: "b.rs".into(),
            diff: vec![],
            old: "o".into(),
            new: "n".into(),
            line: 1,
        });
        let a = app.on_key(key(KeyCode::Esc));
        assert!(a.is_none());
        assert!(app.pending_edit.is_none());
        assert!(app.entries.last().unwrap().text.contains("rejected"));
    }

    #[test]
    fn tab_jump_cycles_applied_edits() {
        let mut app = App::new();
        app.on_event(ServerEvent::EditApplied { session_id: "s".into(), path: "m.rs".into(), line: 20 });
        app.on_event(ServerEvent::EditApplied { session_id: "s".into(), path: "a.rs".into(), line: 4 });
        // Deduped.
        app.on_event(ServerEvent::EditApplied { session_id: "s".into(), path: "m.rs".into(), line: 20 });
        assert_eq!(app.jumps.len(), 2);

        app.on_key(key(KeyCode::Tab));
        assert!(app.entries.last().unwrap().text.contains("[jump] 1/2 a.rs:4"));
        app.on_key(key(KeyCode::Tab));
        assert!(app.entries.last().unwrap().text.contains("[jump] 2/2 m.rs:20"));
        app.on_key(key(KeyCode::Tab));
        assert!(app.entries.last().unwrap().text.contains("[jump] 1/2 a.rs:4"), "wraps");
        app.on_key(key(KeyCode::BackTab));
        assert!(app.entries.last().unwrap().text.contains("[jump] 2/2 m.rs:20"), "shift-tab wraps back");
    }

    #[test]
    fn tab_without_sites_hints() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Tab));
        assert!(app.entries.last().unwrap().text.contains("no accepted edits"));
    }

    // ---- Team mode: N medium cards fed by the `[team]` text protocol ----

    fn feed_team_run(app: &mut App) {
        for line in [
            "[team] decomposing goal: ship the feature\n",
            "[team] plan: 2 subtask(s)\n",
            "  1. Builder — create the modules\n",
            "  2. Tester — run the test suite\n",
            "[team] worker 0 (Builder) done\n",
            "[team] reviewer: PASS — looks good\n",
            "[team] results for: ship the feature\n",
        ] {
            app.on_event(ServerEvent::AssistantDelta { session_id: "s".into(), text: line.into() });
        }
    }

    #[test]
    fn team_panel_parses_run_team_protocol() {
        let mut app = App::new();
        assert!(app.team.is_none());
        feed_team_run(&mut app);
        let team = app.team.as_ref().expect("panel created by [team] lines");
        assert_eq!(team.goal, "ship the feature");
        assert_eq!(team.agents.len(), 2, "one card per worker");
        assert_eq!(team.agents[0].title, "Builder");
        assert_eq!(team.agents[0].status, TeamStatus::Done);
        assert_eq!(team.agents[1].status, TeamStatus::Working);
        assert_eq!(team.phase, TeamPhase::Done);
        assert!(team.verdict.as_deref().unwrap().starts_with("PASS"));
    }

    #[test]
    fn team_panel_renders_one_card_per_agent() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        feed_team_run(&mut app);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect::<Vec<_>>().join("");
        assert!(text.contains("Agent 1"), "first card header");
        assert!(text.contains("Agent 2"), "second card header");
        assert!(text.contains("Builder"), "worker title on card");
        assert!(text.contains("done"), "finished worker status");
        assert!(text.contains("ship the feature"), "goal in panel title");
        // Transcript underneath still shows the raw lines (activity log).
        assert!(text.contains("[team] reviewer"));
    }

    #[test]
    fn team_panel_hides_on_small_screens_and_clears_on_switch() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        feed_team_run(&mut app);
        // 10 rows: transcript area (10-1-3-1=5) < TEAM_H+6 → no panel.
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect::<Vec<_>>().join("");
        assert!(!text.contains("Agent 1"), "panel hidden, transcript keeps the space");
        // Switching sessions drops the panel (it belongs to the old run).
        app.on_event(ServerEvent::SessionLoaded {
            session_id: "ses_y".into(),
            mode: "build".into(),
            transcript: vec![],
        });
        assert!(app.team.is_none());
    }

    // ---- M6: command palette, /help, /sessions, /model ----

    #[test]
    fn palette_opens_on_slash_and_filters_by_prefix() {
        let mut app = App::new();
        assert!(app.palette.is_none());
        type_line(&mut app, "/");
        assert!(app.palette.is_some(), "a bare slash lists every command");
        assert_eq!(app.palette.as_ref().unwrap().matches.len(), builtins().len());

        type_line(&mut app, "he");
        let p = app.palette.as_ref().expect("prefix filtered");
        let names: Vec<&str> = p.matches.iter().map(|&i| builtins()[i].name).collect();
        assert_eq!(names, vec!["help"], "only commands matching /he");
    }

    #[test]
    fn palette_closes_once_arguments_start() {
        let mut app = App::new();
        type_line(&mut app, "/ed");
        assert!(app.palette.is_some());
        type_line(&mut app, "it src/lib.rs");
        assert!(app.palette.is_none(), "arguments are not command names");
        type_line(&mut app, "x");
        assert!(app.palette.is_none());
    }

    #[test]
    fn palette_tab_completes_and_enter_runs() {
        let mut app = App::new();
        // Tab completes an argument-taking command and leaves room for args.
        type_line(&mut app, "/ex");
        assert!(app.on_key(key(KeyCode::Tab)).is_none());
        assert_eq!(app.input, "/export ");
        assert!(app.palette.is_none());

        // Enter on a zero-arg command sends it in one keystroke.
        let mut app = App::new();
        type_line(&mut app, "/hel");
        let a = app.on_key(key(KeyCode::Enter));
        assert!(a.is_none(), "help opens an overlay, no action leaves the UI");
        assert!(app.help, "Enter ran the completed command");

        // Enter on an argument-taking command completes instead of sending.
        let mut app = App::new();
        type_line(&mut app, "/mod");
        assert!(app.on_key(key(KeyCode::Enter)).is_none());
        assert_eq!(app.input, "/model ", "waits for the model id");
    }

    #[test]
    fn palette_arrows_move_the_selection() {
        let mut app = App::new();
        type_line(&mut app, "/s");
        let n = app.palette.as_ref().unwrap().matches.len();
        assert!(n > 1, "several commands start with s");
        assert!(app.on_key(key(KeyCode::Down)).is_none());
        assert_eq!(app.palette.as_ref().unwrap().sel, 1);
        assert!(app.on_key(key(KeyCode::Up)).is_none());
        assert_eq!(app.palette.as_ref().unwrap().sel, 0);
        assert!(app.on_key(key(KeyCode::Up)).is_none(), "clamps at the top");
        assert_eq!(app.palette.as_ref().unwrap().sel, 0);
        // Esc dismisses the palette but keeps what was typed.
        assert!(app.on_key(key(KeyCode::Esc)).is_none());
        assert!(app.palette.is_none());
        assert_eq!(app.input, "/s");
    }

    #[test]
    fn help_overlay_opens_and_any_key_closes() {
        let mut app = App::new();
        assert!(app.on_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)).is_none());
        assert!(app.help);
        assert!(app.on_key(key(KeyCode::Char('x'))).is_none());
        assert!(!app.help, "any key closes the reference");
        // /help opens it too.
        type_line(&mut app, "/help");
        assert!(app.on_key(key(KeyCode::Enter)).is_none());
        assert!(app.help);
    }

    #[test]
    fn help_overlay_renders_every_command() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        app.help = true;
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect::<Vec<_>>().join("");
        assert!(text.contains("Commands"), "help title section");
        assert!(text.contains("/sessions") && text.contains("/model") && text.contains("/export"));
    }

    #[test]
    fn sessions_overlay_lists_and_switches() {
        let mut app = App::new();
        app.on_event(ServerEvent::SessionList {
            sessions: vec![
                SessionInfo {
                    id: "ses_b".into(),
                    title: "second".into(),
                    parent_id: None,
                    updated_at: 2,
                },
                SessionInfo {
                    id: "ses_a".into(),
                    title: "first".into(),
                    parent_id: Some("ses_b".into()),
                    updated_at: 1,
                },
            ],
        });
        assert!(app.sessions.is_some());
        assert!(!app.busy);
        // Down then Enter picks the second row (the fork).
        assert!(app.on_key(key(KeyCode::Down)).is_none());
        let a = app.on_key(key(KeyCode::Enter));
        assert!(
            matches!(a, Some(InputAction::SwitchSession(ref id)) if id == "ses_a"),
            "switch carries the selected id"
        );
        assert!(app.sessions.is_none(), "overlay closes on switch");
        // Esc closes without an action.
        app.on_event(ServerEvent::SessionList { sessions: vec![] });
        assert!(app.on_key(key(KeyCode::Esc)).is_none());
        assert!(app.sessions.is_none());
    }

    #[test]
    fn model_info_updates_the_header_and_lists_choices() {
        let mut app = App::new();
        assert!(app.model.is_none());
        app.on_event(ServerEvent::ModelInfo {
            current: "gpt-5".into(),
            available: vec!["gpt-5".into(), "kimi-k2".into()],
        });
        assert_eq!(app.model.as_deref(), Some("gpt-5"));
        let text = &app.entries.last().unwrap().text;
        assert!(text.contains("active: gpt-5"), "{text}");
        assert!(text.contains("kimi-k2"), "{text}");
    }

    #[test]
    fn m6_commands_map_to_actions() {
        let mut app = App::new();
        for (line, want) in [
            ("/sessions", "sessions"),
            ("/todos", "todos"),
            ("/new", "new"),
            ("/init", "init"),
        ] {
            type_line(&mut app, line);
            let a = app.on_key(key(KeyCode::Enter));
            let got = match a {
                Some(InputAction::ListSessions) => "sessions",
                Some(InputAction::Todos) => "todos",
                Some(InputAction::NewSession) => "new",
                Some(InputAction::Init) => "init",
                _ => "?",
            };
            assert_eq!(got, want, "{line}");
            assert!(!app.busy, "{line} is local, not an agent run");
        }
        type_line(&mut app, "/model gpt-5");
        assert!(matches!(
            app.on_key(key(KeyCode::Enter)),
            Some(InputAction::Model(Some(ref m))) if m == "gpt-5"
        ));
        type_line(&mut app, "/export out/run.md");
        assert!(matches!(
            app.on_key(key(KeyCode::Enter)),
            Some(InputAction::Export(Some(ref p))) if p == "out/run.md"
        ));
    }

    #[test]
    fn clear_command_drops_the_view_but_keeps_the_session() {
        let mut app = App::new();
        app.session_id = Some("ses_keep".into());
        app.on_event(ServerEvent::AssistantDelta {
            session_id: "ses_keep".into(),
            text: "some answer".into(),
        });
        type_line(&mut app, "/clear");
        assert!(app.on_key(key(KeyCode::Enter)).is_none());
        assert_eq!(app.entries.len(), 1, "only the confirmation line remains");
        assert!(app.entries[0].text.contains("view cleared"));
        assert_eq!(app.session_id.as_deref(), Some("ses_keep"), "history is kept");
    }

    #[test]
    fn palette_renders_above_the_input() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        type_line(&mut app, "/export");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect::<Vec<_>>().join("");
        assert!(text.contains("commands"), "palette block title");
        assert!(text.contains("/export"), "completed usage shown");
    }

    // ---- M7: spawned-agents panel — one medium card per agent ----

    fn agent_ev(id: &str, name: &str, state: &str, detail: &str) -> ServerEvent {
        ServerEvent::AgentStatus {
            agent_id: id.into(),
            name: name.into(),
            task: "inspect the tree".into(),
            state: state.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn agents_panel_upserts_and_renders_one_card_per_agent() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        app.on_event(agent_ev("ses_a", "explore", "working", "starting…"));
        app.on_event(agent_ev("ses_b", "worker", "working", "bash(cargo test)"));
        assert_eq!(app.agents.len(), 2, "one card per spawned agent");
        app.on_event(agent_ev("ses_a", "explore", "working", "read(src/main.rs)"));
        assert_eq!(app.agents.len(), 2, "updates upsert, not append");
        assert_eq!(app.agents[0].detail, "read(src/main.rs)");

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect::<Vec<_>>().join("");
        assert!(text.contains("agents"), "panel title");
        assert!(text.contains("explore"), "first card name");
        assert!(text.contains("worker"), "second card name");
        assert!(text.contains("inspect the tree"), "task body on the cards");

        app.on_event(agent_ev("ses_a", "explore", "done", "3 files read"));
        assert_eq!(app.agents[0].state, TeamStatus::Done);
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("done"), "finished card shows its status");
    }

    #[test]
    fn agents_panel_clears_on_session_switch() {
        let mut app = App::new();
        app.on_event(agent_ev("ses_a", "explore", "working", "starting…"));
        assert_eq!(app.agents.len(), 1);
        app.on_event(ServerEvent::SessionLoaded {
            session_id: "ses_b".into(),
            mode: "build".into(),
            transcript: vec![],
        });
        assert!(app.agents.is_empty(), "panel belongs to the old session");
    }

    #[test]
    fn busy_header_shows_a_spinner_frame() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        app.busy = true;
        app.tick = 1;
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("◓ working"), "tick picks the spinner frame");
    }

    #[test]
    fn transcript_marks_user_lines_and_counts_scrollback() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        app.push(EntryKind::User, "hello there");
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("› hello there"), "user marker in the gutter");
        assert!(text.contains("transcript"), "block title");

        for i in 0..40 {
            app.push(EntryKind::Assistant, format!("line {i}"));
        }
        app.scroll = 3;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("↑"), "scrollback count in the title");
        assert!(text.contains("earlier"), "scrollback label: {text}");
    }

    #[test]
    fn header_status_cluster_is_right_aligned() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        app.model = Some("gpt-5".into());
        app.api = Some((true, 9));
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row: String = buf.content()[..100].iter().map(|c| c.symbol()).collect();
        assert!(row.contains("model gpt-5"), "{row}");
        assert!(row.trim_end().ends_with("api ok 9ms"), "right-aligned: {row}");
    }

    #[test]
    fn terminal_restore_runs_once_per_take() {
        use std::sync::atomic::Ordering;
        super::TERMINAL_TAKEN.store(true, Ordering::SeqCst);
        assert!(super::release_terminal(), "first restore does the work");
        assert!(!super::release_terminal(), "second restore is a no-op");
        assert!(!super::TERMINAL_TAKEN.load(Ordering::SeqCst));
    }

    #[test]
    fn transcript_shows_the_newest_line_when_lines_wrap() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        for i in 0..30 {
            let long = "lorem ipsum dolor sit amet ".repeat(8);
            app.push(EntryKind::Assistant, format!("para {i}: {long}"));
        }
        app.push(EntryKind::Assistant, "FINAL-LINE");
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String =
            terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("FINAL-LINE"), "newest line visible at the bottom");
        assert!(text.contains("earlier"), "scrollback counted in rows");
        // ↑ stops at the real top, so ↓ works again right away.
        for _ in 0..10_000 {
            app.on_key(key(KeyCode::Up));
        }
        assert_eq!(app.scroll, app.scroll_max.get());
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String =
            terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("para 0:"), "top of the transcript reachable");
        assert!(!text.contains("FINAL-LINE"));
    }

    // ---- B-15: approval overlay ----

    fn approval_ev(id: &str, detail: &str) -> ServerEvent {
        ServerEvent::ApprovalNeeded {
            session_id: "s".into(),
            approval_id: id.into(),
            tool: "bash".into(),
            detail: detail.into(),
            preview: String::new(), // field added by B-22
        }
    }

    /// Pretend the grace period of the approval on screen is over.
    fn skip_grace(app: &mut App) {
        app.approval_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(5));
    }

    #[test]
    fn approvals_queue_and_ignore_unsafe_keys() {
        let mut app = App::new();
        app.on_event(approval_ev("apr_1", "ls"));
        app.on_event(approval_ev("apr_2", "pwd"));
        assert_eq!(app.approval.as_ref().unwrap().id, "apr_1", "first stays on screen");
        assert!(app.on_key(key(KeyCode::Char('y'))).is_none(), "grace period");
        skip_grace(&mut app);
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert!(app.on_key(ctrl_a).is_none(), "Ctrl+A is not 'always'");
        let a = app.on_key(key(KeyCode::Char('n')));
        assert!(matches!(a, Some(InputAction::Deny(ref id)) if id == "apr_1"));
        assert_eq!(app.approval.as_ref().unwrap().id, "apr_2", "next one shown");
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "Ctrl+C quits even with an approval open");
    }

    #[test]
    fn approval_resolved_elsewhere_closes_the_overlay() {
        let mut app = App::new();
        app.on_event(approval_ev("apr_1", "ls"));
        app.on_event(approval_ev("apr_2", "pwd"));
        let resolved = |id: &str, reason: &str| ServerEvent::ApprovalResolved {
            session_id: "s".into(),
            approval_id: id.into(),
            allowed: false,
            reason: reason.into(),
        };
        app.on_event(resolved("apr_2", "timeout"));
        assert!(app.approval_queue.is_empty(), "queued request dropped");
        app.on_event(resolved("apr_1", "turbo"));
        assert!(app.approval.is_none(), "overlay closed");
        assert!(app.entries.last().unwrap().text.contains("turbo"));
    }

    #[test]
    fn approval_overlay_shows_everything_and_sits_on_top() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        app.help = true; // another overlay is open
        let long = format!("echo start; {} echo END-OF-COMMAND", "x ".repeat(150));
        app.on_event(approval_ev("apr_1", &format!("{long}\nsecond-line\u{1b}[0m")));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String =
            terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Tool approval required"), "not hidden by help");
        assert!(!text.contains("help — any key to close"), "help stays below");
        assert!(text.contains("END-OF-COMMAND"), "no 200-character cut");
        assert!(text.contains("second-line\\u{1b}[0m"), "own row, control char shown");
        assert!(text.contains("[y] allow once"), "keys always visible");
    }

    // ---- B-33: busy flag ----

    #[test]
    fn local_commands_do_not_clear_busy_during_a_run() {
        let mut app = App::new();
        type_line(&mut app, "do the thing");
        app.on_key(key(KeyCode::Enter));
        assert!(app.busy);
        type_line(&mut app, "/debt");
        app.on_key(key(KeyCode::Enter));
        assert!(app.busy, "/debt is local; the run is still going");
        app.on_event(ServerEvent::ModelInfo { current: "m".into(), available: vec![] });
        app.on_event(ServerEvent::SessionList { sessions: vec![] });
        assert!(app.busy, "answers to local commands do not end the run");
        app.on_event(ServerEvent::RunFinished { session_id: "s".into(), ok: true, message: None });
        assert!(!app.busy);
    }

    #[test]
    fn competing_run_commands_wait_while_busy() {
        let mut app = App::new();
        app.busy = true;
        type_line(&mut app, "/team ship it");
        assert!(app.on_key(key(KeyCode::Enter)).is_none(), "refused while busy");
        assert_eq!(app.input, "/team ship it", "typed line kept for later");
        assert_eq!(app.entries.last().unwrap().kind, EntryKind::Error);
        // A plain prompt still goes through (the server replaces the run).
        app.input.clear();
        app.cursor = 0;
        type_line(&mut app, "actually, do this instead");
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Prompt(_))));
    }

    #[test]
    fn mission_from_the_picker_marks_busy() {
        let mut app = App::new();
        type_line(&mut app, "/mission fix the flaky test");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.on_key(key(KeyCode::Enter)), Some(InputAction::Mission(_))));
        assert!(app.busy);
    }

    // ---- B-34: team panel end state ----

    #[test]
    fn team_panel_reaches_done_when_the_run_finishes() {
        let mut app = App::new();
        type_line(&mut app, "/team ship the feature");
        app.on_key(key(KeyCode::Enter));
        for line in [
            "[team] decomposing goal: ship the feature\n",
            "[team] plan: 2 subtask(s)\n",
            "  1. Builder — create the modules\n",
            "  2. Tester — run the test suite\n",
            "[team] worker 0 (Builder) done\n",
            "[team] worker 1 (Tester) failed: tool (bash) crashed\n",
            "[team] reviewer: VERDICT: PASS\n",
        ] {
            app.on_event(ServerEvent::AssistantDelta { session_id: "s".into(), text: line.into() });
        }
        let team = app.team.as_ref().unwrap();
        assert_eq!(team.phase, TeamPhase::Reviewing);
        assert_eq!(team.agents[1].status, TeamStatus::Failed, "\") \" inside the error text");
        app.on_event(ServerEvent::RunFinished {
            session_id: "s".into(),
            ok: true,
            message: Some("team done".into()),
        });
        assert_eq!(app.team.as_ref().unwrap().phase, TeamPhase::Done);
    }

    #[test]
    fn synthesis_reviewer_line_does_not_reopen_a_done_team() {
        let mut app = App::new();
        feed_team_run(&mut app); // ends with `[team] results for:`
        app.on_event(ServerEvent::AssistantDelta {
            session_id: "s".into(),
            text: "[team] reviewer: PASS — looks good\n".into(),
        });
        assert_eq!(app.team.as_ref().unwrap().phase, TeamPhase::Done);
    }

    #[test]
    fn plain_text_mentioning_team_does_not_open_the_panel() {
        let mut app = App::new();
        app.on_event(ServerEvent::AssistantDelta {
            session_id: "s".into(),
            text: "lines start with [team] in team mode\n".into(),
        });
        assert!(app.team.is_none());
    }

    // ---- B-35: input keys and paste ----

    #[test]
    fn ctrl_and_alt_letters_do_not_type_but_altgr_does() {
        let mut app = App::new();
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(app.input.is_empty());
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        app.on_key(KeyEvent::new(KeyCode::Char('@'), altgr));
        assert_eq!(app.input, "@", "AltGr characters still type");
    }

    #[test]
    fn paste_keeps_line_breaks_in_one_message() {
        let mut app = App::new();
        app.on_paste("line one\r\nline two");
        assert_eq!(app.input, "line one\nline two");
        assert_eq!(app.cursor, 17);
        let a = app.on_key(key(KeyCode::Enter));
        assert!(matches!(a, Some(InputAction::Prompt(ref p)) if p == "line one\nline two"));
    }

    #[test]
    fn input_view_scrolls_and_counts_wide_characters() {
        // The cursor at the end of a long line: the tail is shown.
        assert_eq!(input_view("abcdefghij", 10, 5), ("ghij".to_string(), 4));
        // Wide characters take two columns each.
        assert_eq!(input_view("日本語", 3, 10), ("日本語".to_string(), 6));
        // A line break is visible.
        assert_eq!(input_view("a\nb", 3, 10), ("a⏎b".to_string(), 3));
    }

    #[test]
    fn wrap_n_measures_wide_characters_in_columns() {
        // Each word is 6 columns wide: two of them do not fit in 7.
        assert_eq!(wrap_n("日本語 日本語", 7, 2), vec!["日本語", "日本語"]);
    }

    // ---- B-41: TUI polish ----

    #[test]
    fn wrap_n_marks_a_cut_off_last_word() {
        assert_eq!(wrap_n("aaa bbb ccc", 3, 2), vec!["aaa", "bbb…"]);
    }

    #[test]
    fn help_scrolls_with_arrows_and_closes_on_other_keys() {
        let mut app = App::new();
        app.on_key(key(KeyCode::F(1)));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert!(app.help, "arrows scroll instead of closing");
        assert_eq!(app.help_scroll, 2);
        app.on_key(key(KeyCode::Char('x')));
        assert!(!app.help, "any other key closes");
        app.on_key(key(KeyCode::F(1)));
        assert_eq!(app.help_scroll, 0, "reopening starts at the top");
    }

    #[test]
    fn selected_session_is_always_drawn() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        let items = (1..=30)
            .map(|i| SessionInfo {
                id: format!("ses_{i:02}"),
                title: format!("chat {i}"),
                parent_id: None,
                updated_at: 0,
            })
            .collect();
        app.sessions = Some(SessionsOverlay { items, sel: 29 });
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String =
            terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("ses_30"), "the selected 30th session is on screen");
        assert!(!text.contains("ses_01"), "the list scrolled to reach it");
    }

    #[test]
    fn long_edit_preview_scrolls_to_its_end_and_keeps_the_hint() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        app.on_event(ServerEvent::EditPreview {
            session_id: "s".into(),
            path: "big.txt".into(),
            diff: (1..=100).map(|i| format!("+line {i:03}")).collect(),
            old: String::new(),
            new: String::new(),
            line: 1,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        for _ in 0..12 {
            app.on_key(key(KeyCode::PageDown));
        }
        assert!(app.pending_edit.is_some(), "scrolling does not answer the preview");
        assert_eq!(app.edit_scroll, app.edit_scroll_max.get(), "↓ stops at the end");
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String =
            terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("line 100"), "the end of the diff is reachable");
        assert!(text.contains("[y] apply"), "the key hint stays on screen");
    }

    #[test]
    fn user_lines_have_one_marker() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new();
        type_line(&mut app, "hello");
        app.on_key(key(KeyCode::Enter));
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String =
            terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("› hello"), "{text}");
        assert!(!text.contains("> hello"), "no second marker");
    }

    #[test]
    fn a_finished_agent_card_stops_its_clock() {
        let mut app = App::new();
        app.on_event(agent_ev("ses_a", "explore", "working", "starting…"));
        assert!(app.agents[0].finished.is_none());
        app.on_event(agent_ev("ses_a", "explore", "done", "ok"));
        let first = app.agents[0].finished.expect("set when it finishes");
        app.on_event(agent_ev("ses_a", "explore", "done", "ok again"));
        assert_eq!(app.agents[0].finished, Some(first), "only the first end counts");
        type_line(&mut app, "/clear");
        app.on_key(key(KeyCode::Enter));
        assert!(app.agents.is_empty(), "/clear drops old cards");
    }
