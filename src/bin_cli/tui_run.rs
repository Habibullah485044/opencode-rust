//! The terminal UI runner: bridges the core's event bus to the TUI and
//! carries out the TUI's actions against the core, like an API client.

use super::shared;
use anyhow::Result;
use opencode_rust::{agents::AgentMode, server, tui};
use std::sync::Arc;

type State = Arc<server::AppState>;

pub(crate) async fn tui_run(
    state: State,
    addr: std::net::SocketAddr,
    prompt: Option<String>,
    boot_ms: u128,
) -> Result<()> {
    // Create initial session.
    let session = state.db.run(|db| db.create_session("tui", None)).await?;
    let mut app = tui::App::new();
    app.session_id = Some(session.id.clone());
    app.push(
        tui::EntryKind::System,
        format!("[boot] {boot_ms}ms · core at http://{addr} · session {}", session.id),
    );

    // New (B-23): subscribe to the event bus BEFORE the first run starts,
    // so its first events (even a fast failure) reach the TUI. The
    // broadcast receiver keeps them until the forwarder below reads them.
    let early_rx = state.events.subscribe();

    // If a prompt was passed on the CLI, pre-fill and send. (kick_run
    // registers the run so a later prompt can abort/replace it.)
    if let Some(p) = prompt {
        app.push(tui::EntryKind::User, p.clone());
        app.busy = true;
        state.kick_run(session.id.clone(), p).await;
    }

    // Bridge: server events → TUI channel; TUI actions → HTTP calls.
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
    let (act_tx, act_rx) = tokio::sync::mpsc::unbounded_channel::<tui::InputAction>();
    spawn_event_forwarder(state.clone(), early_rx, evt_tx);
    spawn_action_handler(state.clone(), session.id.clone(), act_rx);

    // Run the TUI (blocking-ish loop inside async; polls with timeouts).
    let mut app2 = app;
    tui::run(&mut app2, evt_rx, act_tx).await?;
    let _ = addr;
    Ok(())
}

/// SSE forwarder task: core events → the TUI's channel.
fn spawn_event_forwarder(
    st_fwd: State,
    rx: tokio::sync::broadcast::Receiver<server::ServerEvent>,
    evt_tx: tokio::sync::mpsc::UnboundedSender<server::ServerEvent>,
) {
    // New (B-09): after a lag every pending approval is re-sent; T-102 adds
    // a note that streamed text was skipped.
    tokio::spawn(tui::forward_events(rx, evt_tx, move || st_fwd.pending_approvals()));
}

/// Action handler task: talks to the local core exactly like an API client.
/// `sid` follows the session the TUI shows (fork, resume, switch, new).
fn spawn_action_handler(
    st: State,
    mut sid: String,
    mut act_rx: tokio::sync::mpsc::UnboundedReceiver<tui::InputAction>,
) {
    tokio::spawn(async move {
        while let Some(action) = act_rx.recv().await {
            handle_action(&st, &mut sid, action).await;
        }
    });
}

/// Text into the session's transcript, as if the assistant said it.
fn say(st: &State, sid: &str, text: String) {
    let _ = st.events.send(server::ServerEvent::AssistantDelta {
        session_id: sid.to_string(),
        text,
    });
}

/// A system note in the session's transcript.
fn note(st: &State, sid: &str, text: String) {
    let _ = st.events.send(server::ServerEvent::Note {
        session_id: sid.to_string(),
        text,
    });
}

/// T-018: run `f` on a blocking thread — DB and file calls must not hold
/// up the async worker the action handler runs on.
async fn blocking<T: Send + 'static>(
    st: &State,
    f: impl FnOnce(&server::AppState) -> T + Send + 'static,
) -> Result<T> {
    let st = st.clone();
    Ok(tokio::task::spawn_blocking(move || f(&st)).await?)
}

/// Make `id` the TUI's session and show its transcript.
fn show_session(st: &State, sid: &mut String, id: String, transcript: Vec<server::TranscriptLine>) {
    let mode = st.mode_for(&id).as_str().to_string();
    *sid = id.clone();
    let _ = st.events.send(server::ServerEvent::SessionLoaded {
        session_id: id,
        mode,
        transcript,
    });
}

async fn handle_action(st: &State, sid: &mut String, action: tui::InputAction) {
    match action {
        tui::InputAction::Prompt(content) => {
            // Registers the run: a new prompt aborts/replaces a
            // stuck one instead of racing it in parallel.
            st.kick_run(sid.clone(), content).await;
        }
        tui::InputAction::Mission(mission) => {
            shared::start_mission(st, sid.clone(), mission, true).await;
        }
        tui::InputAction::MissionCtl(what) => match what.as_str() {
            "resume" => st.kick_mission(sid.clone(), None).await,
            "stop" => {
                let stopped = st.abort_run(sid).await;
                let text = if stopped { "mission: stopping — /mission resume carries on" } else { "no mission is running" };
                note(st, sid, text.into());
            }
            _ => note(st, sid, server::mission_report(st, sid)),
        },
        tui::InputAction::Turbo(prompt) => {
            // Core is the authority: toggle on bare /turbo, force
            // ON when a prompt comes with it.
            let on = shared::switch_turbo(st, prompt.is_some());
            note(
                st,
                sid,
                if on {
                    "turbo ON — full access granted; I'll keep working on my own".into()
                } else {
                    "turbo off — back to asking before anything risky".into()
                },
            );
            if let Some(p) = prompt {
                st.kick_run(sid.clone(), p).await;
            }
        }
        tui::InputAction::SkillInstall(src) => skill_install(st, sid, src).await,
        tui::InputAction::Team(goal) => shared::kick_team(st, sid.clone(), goal).await,
        tui::InputAction::SetMode(m) => {
            // Core is the authority; the TUI already flipped its display.
            st.set_mode(sid, m);
        }
        tui::InputAction::Compact => compact(st, sid).await,
        tui::InputAction::Undo => {
            tokio::spawn(snapshot_command(st.clone(), sid.clone(), false, false));
        }
        tui::InputAction::UndoForce => {
            tokio::spawn(snapshot_command(st.clone(), sid.clone(), false, true));
        }
        tui::InputAction::Redo => {
            tokio::spawn(snapshot_command(st.clone(), sid.clone(), true, false));
        }
        tui::InputAction::Fork => fork(st, sid).await,
        tui::InputAction::Resume(arg) => {
            // T-018: session lookup + transcript read are DB calls.
            let resolved = blocking(st, move |st| {
                opencode_rust::sessions::resolve_session(&st.db, &arg)
                    .and_then(|id| st.transcript(&id).map(|t| (id, t)))
            })
            .await
            .and_then(|r| r);
            match resolved {
                Ok((id, transcript)) => show_session(st, sid, id, transcript),
                Err(e) => say(st, sid, format!("[error] resume: {e:#}\n")),
            }
        }
        tui::InputAction::Diff(arg) => {
            let me = sid.clone();
            let text = blocking(st, move |st| diff(st, &me, arg))
                .await
                .unwrap_or_else(|e| format!("[error] diff: {e:#}"));
            say(st, sid, format!("{text}\n"));
        }
        tui::InputAction::Skills => {
            let skills = opencode_rust::skills::load_skills(&st.workspace);
            let text = if skills.is_empty() {
                "[skills] none found in .opencode/skills or ~/.config/opencode-rust/skills\n"
                    .to_string()
            } else {
                format!(
                    "[skills]\n{}\n",
                    opencode_rust::skills::skill_menu(&skills)
                )
            };
            say(st, sid, text);
        }
        tui::InputAction::Review => {
            st.kick_review(sid.clone()).await;
        }
        tui::InputAction::Ponytail(level) => {
            // P-05: the next run of this session uses the new level.
            if let Some(l) = level {
                st.set_ponytail_level(sid, l);
            }
            note(
                st,
                sid,
                format!(
                    "ponytail level: {} — choose with /ponytail lite | full | ultra | off",
                    st.ponytail_level(sid).as_str()
                ),
            );
        }
        tui::InputAction::Debt => say(st, sid, st.debt_report(sid)),
        tui::InputAction::ListSessions => {
            let sessions = st.session_list();
            let _ = st.events.send(server::ServerEvent::SessionList { sessions });
        }
        tui::InputAction::SwitchSession(id) => match st.transcript(&id) {
            Ok(transcript) => show_session(st, sid, id, transcript),
            Err(e) => {
                let _ = st.events.send(server::ServerEvent::RunFinished {
                    session_id: sid.clone(),
                    ok: false,
                    message: Some(format!("session switch failed: {e:#}")),
                });
            }
        },
        tui::InputAction::NewSession => match st.db.run(|db| db.create_session("new session", None)).await {
            Ok(s) => {
                *sid = s.id.clone();
                let _ = st.events.send(server::ServerEvent::SessionLoaded {
                    session_id: s.id,
                    mode: AgentMode::Build.as_str().to_string(),
                    transcript: Vec::new(),
                });
            }
            Err(e) => say(st, sid, format!("[error] new session: {e:#}\n")),
        },
        tui::InputAction::Model(arg) => {
            let st2 = st.clone();
            tokio::spawn(async move {
                let available = match &arg {
                    Some(m) => {
                        st2.set_model(m).await;
                        Vec::new()
                    }
                    None => st2.available_models().await,
                };
                let current = st2.current_model().await;
                let _ = st2.events.send(server::ServerEvent::ModelInfo { current, available });
            });
        }
        tui::InputAction::Export(arg) => {
            // New (S-16): inside the workspace only, no silent overwrite.
            // T-018: reads the DB and writes a file, so off the async task.
            let me = sid.clone();
            let res = blocking(st, move |st| st.export_transcript(&me, arg.as_deref()))
                .await
                .and_then(|r| r);
            let text = match res {
                Ok(path) => format!("[export] wrote transcript to {}\n", path.display()),
                Err(e) => format!("[export] failed: {e:#}\n"),
            };
            say(st, sid, text);
        }
        tui::InputAction::Todos => say(st, sid, format!("{}\n", st.todos_report(sid))),
        tui::InputAction::Usage => {
            let me = sid.clone();
            let text = blocking(st, move |st| st.usage_report(&me))
                .await
                .unwrap_or_else(|e| format!("[error] usage: {e:#}"));
            say(st, sid, format!("{text}\n"));
        }
        tui::InputAction::Init => {
            let ws = st.workspace.clone();
            let text = match opencode_rust::skills::init_agents_md(&ws) {
                Ok(Some(p)) => format!(
                    "[init] wrote a starter {} — fill in this project's layout, \
                     commands and conventions\n",
                    p.display()
                ),
                Ok(None) => "[init] AGENTS.md already exists — left untouched\n".to_string(),
                Err(e) => format!("[init] failed: {e:#}\n"),
            };
            say(st, sid, text);
        }
        tui::InputAction::Deep { attempts, task } => {
            st.kick_deep(sid.clone(), task, attempts).await;
        }
        tui::InputAction::Approve { id, always } => {
            let _ = st.decide(&id, true, always);
        }
        tui::InputAction::Deny(id) => {
            let _ = st.decide(&id, false, false);
        }
        tui::InputAction::Edit { path, instruction } => inline_edit(st, sid, path, instruction).await,
        tui::InputAction::ApplyEdit { path, old, new, line } => {
            apply_edit(st, sid, path, old, new, line)
        }
    }
}

/// `/undo` / `/redo`: git snapshot restore off the async runtime (subprocess).
/// `force`: undo even over work done outside Habi since the run.
async fn snapshot_command(st: State, sid: String, redo: bool, force: bool) {
    // The chat's own folder: that is where its runs took their undo points.
    let ws = st.workspace_for(&sid);
    let sd = st.state_dir.clone();
    let res = shared::run_snapshot(ws, sd, redo, force).await;
    let verb = if redo { "redo" } else { "undo" };
    let text = match res {
        Ok(Ok(Some(m))) => format!(
            "[{verb}] restored snapshot {} ({}, {} file(s))\n",
            m.id,
            m.label,
            m.present.len()
        ),
        Ok(Ok(None)) => {
            format!("[{verb}] nothing to {verb} - no snapshot recorded (not a git repository?)\n")
        }
        Ok(Err(e)) => format!("[{verb}] failed: {e:#}\n"),
        Err(e) => format!("[{verb}] failed: {e}\n"),
    };
    let _ = st.events.send(server::ServerEvent::AssistantDelta {
        session_id: sid,
        text,
    });
}

/// `/skill install <src>`. New (B-06): registered in `busy`, so it can be stopped.
async fn skill_install(st: &State, sid: &str, src: String) {
    st.kick_task(sid.to_string(), move |st2, sid2, abort| async move {
        match opencode_rust::server::install_skill_flow(&st2, &src, &abort).await {
            Ok(report) => {
                let _ = st2.events.send(server::ServerEvent::AssistantDelta {
                    session_id: sid2,
                    text: report,
                });
            }
            Err(e) => {
                let _ = st2.events.send(server::ServerEvent::RunFinished {
                    session_id: sid2,
                    ok: false,
                    message: Some(format!("skill install failed: {e:#}")),
                });
            }
        }
    })
    .await;
}

/// `/compact`. New (B-06): registered in `busy`, so it can be stopped.
async fn compact(st: &State, sid: &str) {
    st.kick_task(sid.to_string(), |st2, sid2, abort| async move {
        let (ok, message) =
            match opencode_rust::compact::compact(&st2, &sid2, &abort).await {
                Ok(o) => (
                    true,
                    format!(
                        "compacted {} message(s) -> 1 summary + {} kept",
                        o.removed, o.kept
                    ),
                ),
                Err(e) => (false, format!("compact failed: {e}")),
            };
        let _ = st2.events.send(server::ServerEvent::RunFinished {
            session_id: sid2,
            ok,
            message: Some(message),
        });
    })
    .await;
}

/// `/fork`: branch the session and switch to the copy.
async fn fork(st: &State, sid: &mut String) {
    // A fork copies the whole history: off the async worker.
    let from = sid.clone();
    let forked = st
        .db
        .run(move |db| {
            let Some(s) = db.get_session(&from)? else { return Ok(None) };
            Ok(Some(db.fork_session(&s.id, &format!("{} (fork)", s.title))?))
        })
        .await
        .ok()
        .flatten();
    match forked {
        Some(new) => {
            let transcript = st.transcript(&new.id).unwrap_or_default();
            show_session(st, sid, new.id, transcript);
        }
        None => say(st, sid, "[error] fork failed\n".into()),
    }
}

/// `/diff [id|n]`: this session against its parent or another session.
/// Blocking (DB reads): called through [`blocking`].
fn diff(st: &server::AppState, sid: &str, arg: Option<String>) -> String {
    let target = match arg {
        Some(a) => opencode_rust::sessions::resolve_session(&st.db, &a),
        None => st
            .db
            .get_session(sid)
            .ok()
            .flatten()
            .and_then(|s| s.parent_id)
            .ok_or_else(|| {
                anyhow::anyhow!("no parent session - pass one: /diff <id|index>")
            }),
    };
    match target.and_then(|t| {
        opencode_rust::sessions::diff_sessions(&st.db, sid, &t)
    }) {
        Ok(d) => d,
        Err(e) => format!("[error] diff: {e:#}"),
    }
}

/// `/edit <file> <what>`: propose the edit and show its preview.
/// B-49: registered like /team, so the next message (or an abort)
/// cancels a slow /edit.
async fn inline_edit(st: &State, sid: &str, path: String, instruction: String) {
    st.kick_task(sid.to_string(), move |st2, sid2, abort| async move {
        if st2.mode_for(&sid2) == AgentMode::Plan {
            let _ = st2.events.send(server::ServerEvent::RunFinished {
                session_id: sid2,
                ok: false,
                message: Some(
                    "inline /edit is blocked in plan mode (switch with /build)"
                        .into(),
                ),
            });
            return;
        }
        let result =
            opencode_rust::inline_edit::propose(&st2, &sid2, &path, &instruction, &abort)
                .await;
        // A newer run took the slot: this edit was cancelled,
        // and its events must not end that run's busy state.
        if st2.is_superseded(&sid2, &abort).await {
            return;
        }
        match result {
            Ok(p) => {
                let _ = st2.events.send(server::ServerEvent::EditPreview {
                    session_id: sid2,
                    path: p.path,
                    diff: p.diff,
                    old: p.old,
                    new: p.new,
                    line: p.line,
                });
            }
            Err(e) => {
                let _ = st2.events.send(server::ServerEvent::RunFinished {
                    session_id: sid2,
                    ok: false,
                    message: Some(format!("edit failed: {e:#}")),
                });
            }
        }
    })
    .await;
}

/// The user accepted an edit preview: snapshot, then write it.
fn apply_edit(st: &State, sid: &str, path: String, old: String, new: String, line: usize) {
    let st2 = st.clone();
    let sid2 = sid.to_string();
    let ws = st2.workspace_for(&sid2);
    let path_c = path.clone();
    // Same contract as an agent run: snapshot before we
    // mutate, so /undo can revert an accepted inline edit.
    if let Err(e) = opencode_rust::gitsnap::record_before_run(
        &ws,
        &st2.state_dir,
        "pre-edit",
    ) {
        // New (B-48): stderr would draw over the TUI.
        opencode_rust::server::core_log(&format!("[snapshot] {e:#}"));
    }
    tokio::spawn(async move {
        let res = tokio::task::spawn_blocking(
            move || -> Result<std::path::PathBuf, anyhow::Error> {
                let abs = opencode_rust::inline_edit::resolve_in_workspace(
                    &ws, &path_c,
                )?;
                opencode_rust::inline_edit::apply(&abs, &old, &new)?;
                Ok(abs)
            },
        )
        .await;
        let (ok, message) = match res {
            Ok(Ok(abs)) => {
                let _ = st2.events.send(server::ServerEvent::EditApplied {
                    session_id: sid2.clone(),
                    path: path.clone(),
                    line,
                });
                (
                    true,
                    Some(format!(
                        "applied {path}:{line} ({})",
                        abs.display()
                    )),
                )
            }
            Ok(Err(e)) => (false, Some(format!("edit failed: {e:#}"))),
            Err(e) => (false, Some(format!("edit failed: {e}"))),
        };
        let _ = st2.events.send(server::ServerEvent::RunFinished {
            session_id: sid2,
            ok,
            message,
        });
    });
}
