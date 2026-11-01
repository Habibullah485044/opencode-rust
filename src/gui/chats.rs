//! The sidebar's chats: opening, switching, naming and stopping them, and
//! rebuilding a transcript from the database.

use super::*;

/// New (B-42): the newest chat, if it has no messages yet (reuse it instead of
/// creating another empty "New chat").
pub(super) fn newest_empty_session(st: &server::AppState) -> Option<crate::db::Session> {
    let newest = st.db.list_sessions().ok()?.into_iter().next()?;
    let empty = st.db.messages(&newest.id).map(|m| m.is_empty()).unwrap_or(false);
    empty.then_some(newest)
}

/// Open a fresh chat in `project` (or reuse the open chat when it is
/// already an empty one in that project — B-42).
pub(super) fn open_new_chat(gs: &mut GuiState, st: &server::AppState, project: Option<String>) {
    let reuse = gs.entries.is_empty()
        && !gs.busy
        && gs.session.as_ref().is_some_and(|id| {
            gs.sessions.iter().find(|c| &c.id == id).map(|c| c.project_id.clone()) == Some(project.clone())
        });
    if reuse {
        return;
    }
    match st.db.create_session_in(NEW_CHAT, None, project.as_deref()) {
        Ok(s) => {
            // B-12: a running chat keeps its live transcript.
            gs.leave_current_chat();
            gs.session = Some(s.id);
            gs.entries.clear();
            gs.busy = false;
            refresh_sessions(gs, st);
        }
        Err(e) => gs.push(EntryKind::Error, format!("new chat failed: {e}")),
    }
}

/// History → Delete: remove chat `id` for good (not while it runs).
pub(super) fn delete_chat(gs: &mut GuiState, st: &server::AppState, id: &str) {
    if gs.running.contains(id) {
        gs.push(EntryKind::System, "Stop the run first, then delete this chat.".to_string());
    } else if let Err(e) = st.db.delete_session(id) {
        gs.push(EntryKind::Error, format!("delete failed: {e}"));
    } else {
        // T-026: drop its in-memory state too, as the HTTP route does.
        st.forget_session(id);
        if gs.is_current(id) {
            gs.session = None;
            let project = gs.default_project.clone();
            open_new_chat(gs, st, project);
        }
    }
    refresh_sessions(gs, st);
}

/// Move chat `id` to `project` (`None` = no project); not while it runs.
pub(super) fn move_chat(gs: &mut GuiState, st: &server::AppState, id: &str, project: Option<&str>) {
    if gs.running.contains(id) {
        gs.push(EntryKind::System, "Stop the run first, then move this chat.".to_string());
    } else if let Err(e) = st.db.set_session_project(id, project) {
        gs.push(EntryKind::Error, format!("move chat failed: {e}"));
    }
    refresh_sessions(gs, st);
}

/// Refresh the sidebar chat list from the database.
pub(super) fn refresh_sessions(gs: &mut GuiState, st: &server::AppState) {
    if let Ok(rows) = st.db.list_session_rows() {
        gs.sessions = rows
            .into_iter()
            .filter(|r| r.session.parent_id.is_none())
            .map(|r| ChatItem {
                id: r.session.id,
                title: r.session.title,
                project_id: r.session.project_id,
                pinned: r.session.pinned,
                archived: r.session.archived,
                empty: !r.has_messages,
                updated_at: r.session.updated_at,
            })
            .collect();
    }
    if let Ok(projects) = st.db.list_projects() {
        gs.projects = projects;
    }
}

/// Switch to an existing session, replaying its transcript into the chat.
///
/// B-12: a chat whose run is still going is shown from its live copy, and
/// the chat we leave keeps its live copy if its run is still going.
pub(super) fn load_session(gs: &mut GuiState, st: &server::AppState, id: &str) {
    let entries = match gs.background.remove(id) {
        Some(live) => live,
        None => match st.db.messages(id) {
            Ok(msgs) => entries_from_messages(&msgs),
            Err(e) => {
                gs.push(EntryKind::Error, format!("load session failed: {e}"));
                return;
            }
        },
    };
    gs.leave_current_chat();
    gs.session = Some(id.to_string());
    if let Ok(t) = st.db.usage_totals(Some(id), None) {
        gs.usage.insert(id.to_string(), t);
    }
    gs.entries = entries;
    gs.busy = gs.running.contains(id);
}

/// Submit the composer and name the chat after its first message. Shared by
/// the CLI prefill and the typed-send path so a prefilled launch names its
/// chat identically (it used to skip this and stay "New chat"). Returns the
/// content to run, or None when there was nothing to send.
pub(super) fn submit_and_name(gs: &mut GuiState, st: &server::AppState) -> Option<String> {
    let content = gs.submit()?;
    if gs.title() == NEW_CHAT {
        let sid = gs.session.clone().unwrap_or_default();
        // F-06: the composer is multi-line; name the chat after line one.
        let first_line = content.lines().next().unwrap_or("").trim();
        let _ = st.db.set_session_title(&sid, &clip(first_line, 40));
        refresh_sessions(gs, st);
    }
    Some(content)
}

/// New (F-06): ask the run of chat `session_id` to stop. Same effect as
/// `POST /api/sessions/{id}/abort`: set the run's abort flag. A run started
/// a moment ago may not be registered yet, so look for up to one second.
/// Returns false when no run showed up.
pub(super) async fn request_stop(st: &server::AppState, session_id: &str) -> bool {
    for _ in 0..10 {
        if let Some(flag) = st.busy.read().await.get(session_id) {
            // T-024: wakes `server::aborted` at once, not on its recheck.
            server::signal_abort(flag);
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Rebuild transcript bubbles from persisted messages (session load).
pub fn entries_from_messages(msgs: &[Message]) -> Vec<(EntryKind, String)> {
    let mut out = Vec::new();
    for m in msgs {
        let base = match m.role.as_str() {
            "user" => EntryKind::User,
            "assistant" => EntryKind::Assistant,
            _ => EntryKind::System,
        };
        for p in &m.parts {
            match p {
                Part::Text { text } => out.push((base.clone(), text.clone())),
                Part::Reasoning { text } => out.push((EntryKind::Thinking, format!("\u{273b} {text}"))),
                Part::ToolCall { name, input, .. } => {
                    // B-42: clipped like results; a `write` input is a whole file.
                    let shown = clip(&input.to_string(), 160);
                    out.push((EntryKind::Tool, format!("\u{23fa} {name}({shown})")));
                }
                Part::ToolResult { output, is_error, .. } => out.push((
                    EntryKind::Tool,
                    format!("  {} {}", if *is_error { "\u{2717}" } else { "\u{2713}" }, clip(output, 160)),
                )),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prefill_names_the_chat_like_typed_sends() {
        let st = server::build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir())
            .await;
        let s = st.db.create_session(NEW_CHAT, None).unwrap();
        let mut g = GuiState::new();
        g.session = Some(s.id.clone());
        refresh_sessions(&mut g, &st);
        // The CLI prefill submits the same way a typed message does…
        g.input = "prefill repro marker one".into();
        assert_eq!(
            submit_and_name(&mut g, &st).as_deref(),
            Some("prefill repro marker one")
        );
        // …and names the chat after it, instead of leaving it "New chat".
        assert_eq!(g.title(), "prefill repro marker one");
        // Later messages never rename an already-named chat.
        g.input = "second message".into();
        submit_and_name(&mut g, &st);
        assert_eq!(g.title(), "prefill repro marker one");
    }

    /// B-12: switching chats keeps a running chat's transcript.
    #[tokio::test]
    async fn switching_chats_keeps_a_running_chat_live() {
        let st = server::build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir())
            .await;
        let a = st.db.create_session("a", None).unwrap();
        let b = st.db.create_session("b", None).unwrap();
        let mut g = GuiState::new();
        g.session = Some(a.id.clone());
        g.input = "long job".into();
        g.submit();
        load_session(&mut g, &st, &b.id);
        assert_eq!(g.session.as_deref(), Some(b.id.as_str()));
        assert!(!g.busy, "chat b is idle");
        load_session(&mut g, &st, &a.id);
        assert!(g.busy, "chat a is still running");
        assert!(g.entries.iter().any(|(_, t)| t.contains("long job")), "live copy restored");
    }

    /// F-06: Stop sets the same abort flag as `POST /api/sessions/{id}/abort`.
    #[tokio::test]
    async fn stop_sets_the_runs_abort_flag() {
        let st = server::build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir())
            .await;
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        st.busy.write().await.insert("ses_run".into(), flag.clone());
        assert!(request_stop(&st, "ses_run").await);
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// T-024: a GUI stop wakes a run waiting in `server::aborted` at once,
    /// well before its 500 ms recheck.
    #[tokio::test]
    async fn stop_wakes_an_aborted_waiter_at_once() {
        let st = server::build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir())
            .await;
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        st.busy.write().await.insert("ses_wake".into(), flag.clone());
        let f2 = flag.clone();
        let waiter = tokio::spawn(async move {
            server::aborted(&f2).await;
            Instant::now()
        });
        tokio::time::sleep(Duration::from_millis(50)).await; // let it start waiting
        let t0 = Instant::now();
        assert!(request_stop(&st, "ses_wake").await);
        let woke = waiter.await.unwrap();
        assert!(woke.duration_since(t0) < Duration::from_millis(250), "{:?}", woke.duration_since(t0));
    }

    /// T-026, T-043: deleting a chat forgets its in-memory state; a running
    /// chat is neither deleted nor moved, and the user is told why.
    #[tokio::test]
    async fn delete_and_move_chat() {
        let st = server::build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir())
            .await;
        let a = st.db.create_session("a", None).unwrap();
        st.set_mode(&a.id, crate::agents::AgentMode::Plan);
        let mut g = GuiState::new();
        g.running.insert(a.id.clone());
        delete_chat(&mut g, &st, &a.id);
        assert!(g.entries.last().unwrap().1.contains("Stop the run first, then delete"));
        move_chat(&mut g, &st, &a.id, None);
        assert!(g.entries.last().unwrap().1.contains("Stop the run first, then move"));
        assert!(st.db.get_session(&a.id).unwrap().is_some(), "a running chat stays");
        g.running.clear();
        delete_chat(&mut g, &st, &a.id);
        assert!(st.db.get_session(&a.id).unwrap().is_none());
        assert_eq!(st.mode_for(&a.id), crate::agents::AgentMode::default(), "forget_session ran");
    }

    /// T-044: a move the database refuses shows an error line.
    #[tokio::test]
    async fn a_failed_move_shows_an_error() {
        let dir = std::env::temp_dir().join(format!("habi-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.db");
        let mut st = server::build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
        Arc::get_mut(&mut st).expect("sole owner").db = crate::db::Db::open(&file).unwrap();
        let s = st.db.create_session("s", None).unwrap();
        rusqlite::Connection::open(&file)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_move BEFORE UPDATE OF project_id ON sessions \
                 BEGIN SELECT RAISE(ABORT, 'moves refused'); END;",
            )
            .unwrap();
        let mut g = GuiState::new();
        move_chat(&mut g, &st, &s.id, Some("p"));
        let (kind, text) = g.entries.last().unwrap();
        assert_eq!(*kind, EntryKind::Error);
        assert!(text.contains("moves refused"), "{text}");
        drop(st);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_from_messages_maps_parts_to_bubbles() {
        let msg = |role: &str, parts: Vec<Part>| Message {
            id: "m".into(),
            session_id: "s".into(),
            role: role.into(),
            parts,
            created_at: 0,
        };
        let msgs = vec![
            msg("user", vec![Part::Text { text: "hi".into() }]),
            msg(
                "assistant",
                vec![
                    Part::Reasoning { text: "thinking…".into() },
                    Part::Text { text: "hello".into() },
                    Part::ToolCall { id: "t1".into(), name: "bash".into(), input: serde_json::json!({"cmd":"ls"}) },
                    Part::ToolResult { tool_id: "t1".into(), output: "file.rs".into(), is_error: false },
                ],
            ),
        ];
        let entries = entries_from_messages(&msgs);
        assert_eq!(entries[0], (EntryKind::User, "hi".into()));
        assert_eq!(entries[1].0, EntryKind::Thinking);
        assert_eq!(entries[2], (EntryKind::Assistant, "hello".into()));
        assert_eq!(entries[3].0, EntryKind::Tool);
        assert!(entries[3].1.contains("bash"));
        assert!(entries[4].1.contains('\u{2713}'));
        // Long tool output is clipped, not dumped whole into a bubble.
        let long = entries_from_messages(&[msg(
            "assistant",
            vec![Part::ToolResult { tool_id: "t".into(), output: "x".repeat(500), is_error: true }],
        )]);
        assert!(long[0].1.chars().count() < 200);
        assert!(long[0].1.contains('\u{2717}'));
    }

    /// B-42: a replayed `write` call does not dump the whole file.
    #[test]
    fn replayed_tool_calls_are_clipped() {
        let input = serde_json::json!({ "path": "a.rs", "content": "x".repeat(5_000) });
        let msgs = vec![Message {
            id: "m".into(),
            session_id: "s".into(),
            role: "assistant".into(),
            parts: vec![Part::ToolCall { id: "t".into(), name: "write".into(), input }],
            created_at: 0,
        }];
        let e = entries_from_messages(&msgs);
        assert!(e[0].1.chars().count() < 200, "{} chars", e[0].1.chars().count());
    }

    /// B-42: start-up reuses the newest chat only while it is empty.
    #[tokio::test]
    async fn startup_reuses_an_empty_newest_chat() {
        let st = server::build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir())
            .await;
        let s = st.db.create_session(NEW_CHAT, None).unwrap();
        assert_eq!(newest_empty_session(&st).map(|x| x.id), Some(s.id.clone()));
        st.db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        assert!(newest_empty_session(&st).is_none(), "a chat with messages is not reused");
    }
}
