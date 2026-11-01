//! Command steps that the frontends (headless runner, TUI runner, desktop
//! app) do the same way; only how each reports the outcome differs.

use crate::{agents::AgentMode, gitsnap::Manifest, mission::Mission, server};
use std::path::PathBuf;
use std::sync::Arc;

/// `/undo`, `/undo --force` (`force`) or `/redo` (`redo`): restore a git
/// snapshot off the async runtime (it runs subprocesses).
pub async fn run_snapshot(
    ws: PathBuf,
    sd: PathBuf,
    redo: bool,
    force: bool,
) -> Result<anyhow::Result<Option<Manifest>>, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        if redo {
            crate::gitsnap::redo(&ws, &sd)
        } else if force {
            crate::gitsnap::undo_anyway(&ws, &sd)
        } else {
            crate::gitsnap::undo(&ws, &sd)
        }
    })
    .await
}

/// `/turbo`: a prompt with it forces turbo ON; bare `/turbo` toggles.
/// The core is the authority. Returns whether turbo is now on.
pub fn switch_turbo(st: &server::AppState, with_prompt: bool) -> bool {
    if with_prompt {
        st.set_perm_mode(server::PermissionMode::Turbo);
        true
    } else {
        st.toggle_turbo()
    }
}

/// `/team <goal>`: registered in `busy` (New B-06), so a new prompt or an
/// abort stops it instead of running in parallel. Ends with RunFinished.
pub async fn kick_team(st: &Arc<server::AppState>, sid: String, goal: String) {
    st.kick_task(sid, move |st2, sid2, abort| async move {
        let (ok, message) = match crate::team::run_team(&st2, &sid2, &goal, abort).await {
            Ok(s) => (true, Some(s.lines().last().unwrap_or("team done").to_string())),
            Err(e) => (false, Some(format!("team failed: {e}"))),
        };
        let _ = st2.events.send(server::ServerEvent::RunFinished { session_id: sid2, ok, message });
    })
    .await;
}

/// Start a mission. An explore (read-only) mission reads and reports: one
/// run in plan mode (`announce_plan` tells the chat plan mode is now on).
/// Any other kind runs in mission mode: step by step until the goal is met.
pub async fn start_mission(st: &Arc<server::AppState>, sid: String, mission: Mission, announce_plan: bool) {
    if mission.kind.read_only() {
        st.set_mode(&sid, AgentMode::Plan);
        if announce_plan {
            let _ = st.events.send(server::ServerEvent::Note {
                session_id: sid.clone(),
                text: "explore mission: plan mode (read-only) is on; /build switches back".into(),
            });
        }
        st.kick_run(sid, mission.briefing()).await;
    } else {
        st.kick_mission(sid, Some(mission.goal_for_mission_mode())).await;
    }
}
