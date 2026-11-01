//! M8: the desktop app shell (working name "Habi") — a wry/WebView2 window
//! hosting our own UI (`src/gui.html`), with every decision made in Rust.
//!
//! Why not egui/eframe: this machine's Intel HD Graphics 3000 caps at
//! DX10.1 / OpenGL 3.1, so eframe's glow renderer ("extension to create ES
//! context with wgl is not present") and wgpu ("no suitable adapter") both
//! fail — egui cannot run on this GPU at all. WebView2 renders through
//! ANGLE/D3D and works here (M4's preview proved it), and it is the same
//! tech the M9 browser panel needs, so the whole app shares one renderer.
//!
//! Layout: `state` (the model) and `payload` (what the page gets),
//! `events` (server events into chats), `ipc` (page messages) and
//! `dispatch` / `settings` / `terminal` / `panel` (their handlers), `app`
//! (the event loop), `window` / `native` (toolkit and Win32), `bridge` (the
//! agent's browser tools), `nav` (URL rules), `chats` / `project` / `slash`.

use crate::db::{Message, Part};
use crate::server::{self, PermissionMode, ServerEvent};
use crate::tui::EntryKind;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};

mod app;
mod bridge;
mod chats;
mod dispatch;
mod events;
mod ipc;
mod native;
mod nav;
mod panel;
mod payload;
mod project;
mod settings;
mod slash;
mod state;
mod terminal;
mod window;

// The public surface, at the paths it had before the split.
pub use self::chats::*;
pub use self::ipc::*;
pub use self::native::*;
pub use self::nav::*;
pub use self::project::*;
pub use self::slash::*;
pub use self::state::*;
pub use self::terminal::*;
// Shared between the submodules only.
use self::app::*;
use self::bridge::*;
use self::events::*;
use self::panel::*;
use self::window::*;

/// Sidebar title for fresh chats; renamed from the first user message.
const NEW_CHAT: &str = "New chat";

/// Clip text to `max` chars with an ellipsis (chat titles, tool outputs).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}\u{2026}")
    }
}

/// T-057: a diagnostic line to stderr and to core.log: a shortcut launch
/// has no console, so stderr alone is lost.
fn gui_log(msg: &str) {
    eprintln!("{msg}");
    server::core_log(msg);
}

/// Wake-up sent by the SSE forwarder so the event loop drains promptly.
#[derive(Debug, Clone, Copy)]
enum AppEvent {
    Wake,
}

/// Open the desktop app. Blocks until the window closes (mirrors `tui::run`).
pub async fn run(
    state: Arc<server::AppState>,
    addr: std::net::SocketAddr,
    prompt: Option<String>,
    boot_ms: u128,
) -> Result<()> {
    // F-13: one desktop window per state directory (two would write the same
    // database), and a readable message when WebView2 is missing.
    let _instance_lock = match single_instance_lock(&state.state_dir) {
        Ok(lock) => lock,
        Err(msg) => {
            show_fatal_dialog("Habi is already running", &msg);
            anyhow::bail!(msg);
        }
    };
    if let Err(e) = wry::webview_version() {
        let msg = format!(
            "Habi needs the Microsoft Edge WebView2 Runtime, and it was not found ({e}).\n\n\
             Install it from https://developer.microsoft.com/microsoft-edge/webview2/ \
             and start Habi again."
        );
        show_fatal_dialog("WebView2 Runtime missing", &msg);
        anyhow::bail!(msg);
    }
    // B-42: reuse the newest chat while it is still empty.
    let session = match newest_empty_session(&state) {
        Some(s) => s,
        None => state.db.create_session(NEW_CHAT, None)?,
    };
    let mut gs = initial_state(&state, &session);
    tracing_boot(boot_ms, addr, &session.id);
    if let Some(p) = prompt {
        gs.input = p;
    }

    let rt = tokio::runtime::Handle::current();

    let event_loop: EventLoop<AppEvent> =
        tao::event_loop::EventLoopBuilder::<AppEvent>::with_user_event().build();
    let window = build_window(&event_loop)?;
    // Breadcrumbs: a window that dies between these lines used to leave no
    // trace at all (exit code 1, empty stderr). Now the last line printed says
    // how far it got.
    gui_log("[gui] window created");

    // Page -> Rust, plus preview-webview callbacks. One channel means one
    // drain site in the event loop, so state never updates twice per frame.
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiMsg>();
    let webview = build_main_webview(&window, &event_loop, ui_tx.clone())?;
    // The interface zoom (Ctrl+wheel, Ctrl+= / Ctrl+-) from last time.
    let page_zoom_now = load_zoom(&state.state_dir);
    if (page_zoom_now - 1.0).abs() > 0.001 {
        let _ = webview.zoom(page_zoom_now);
    }

    // Goal A: a real second WebView2 for the preview panel. It is a *native
    // child view*, not part of the page: it cannot be painted under HTML and
    // CSS cannot clip it, so we own its bounds and visibility for its whole
    // life (`show_browser`). Built hidden — nothing shows until the user
    // opens the panel and it reports a usable rect.
    gs.browser.home = state.cfg.browser.home_url.clone();
    let browser_enabled = state.cfg.browser.enabled;
    // S-12: shared between the bridge (writes) and the preview IPC handler (reads).
    let bridge_ids: PendingIds = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let browser_webview: Option<wry::WebView> = if browser_enabled {
        match build_preview_webview(&window, &event_loop, &ui_tx, &bridge_ids, state.cfg.browser.allow_remote) {
            Ok(wv) => Some(wv),
            Err(e) => {
                // The chat must still work when the preview cannot be created.
                gui_log(&format!("[gui] preview panel unavailable: {e}"));
                gs.browser.fail(format!("Preview panel unavailable: {e}"));
                None
            }
        }
    } else {
        gui_log("[gui] preview panel disabled in config ([browser] enabled = false)");
        None
    };

    // Goal B: publish a handle so agent tools can drive this webview. The
    // waker is what turns a queued command into an event-loop wake-up.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<crate::browser::Request>();
    if browser_enabled {
        let proxy = event_loop.create_proxy();
        let waker: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
            let _ = proxy.send_event(AppEvent::Wake);
        });
        if let Ok(mut slot) = state.browser.lock() {
            *slot = Some(crate::browser::Handle::new(cmd_tx, waker));
        }
    }

    let evt_rx = forward_server_events(&state, &event_loop);
    if let Some(wv) = &browser_webview {
        preview_smoke(&mut gs, wv, state.cfg.browser.allow_remote);
    }

    // First paint (the page also asks for one via IpcMsg::Ready once loaded),
    // then an optional prefilled prompt from the CLI.
    paint(&webview, &gs);
    if let Some(content) = submit_and_name(&mut gs, &state) {
        gui_log(&format!("[gui] run start (prefill): {}", clip(&content, 80)));
        let sid = gs.session.clone().unwrap_or_default();
        paint(&webview, &gs);
        let st0 = state.clone();
        rt.spawn(async move {
            st0.kick_run(sid, content).await;
        });
    }

    gui_log("[gui] entering the event loop");
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Done>();
    let mut app = App {
        gs,
        st: state.clone(),
        rt,
        window,
        webview,
        browser_webview,
        browser_enabled,
        bridge: Bridge::new(),
        bridge_ids,
        page_zoom_now,
        covered_applied: false,
        evt_rx,
        ui_rx,
        cmd_rx,
        done_tx,
        done_rx,
        proxy: event_loop.create_proxy(),
    };
    event_loop.run(move |event, _, control_flow| app.on_loop_event(event, control_flow))
}

/// The window's state at start-up: the chat to open, where plain chats
/// work, the commands, the Settings page and the autonomy mode.
fn initial_state(state: &server::AppState, session: &crate::db::Session) -> GuiState {
    let mut gs = GuiState::new();
    gs.session = Some(session.id.clone());
    // The folder the app started in is a project; plain new chats go there,
    // unless it is no project at all (see `chat_home`).
    let home = chat_home(&state.workspace);
    if home != state.workspace {
        if let Err(e) = std::fs::create_dir_all(&home) {
            gs.push(EntryKind::Error, format!("could not make {}: {e}", home.display()));
        }
        gs.push(
            EntryKind::System,
            format!(
                "New chats work in {} (Habi was started in {}, which is not a project). \
                 Add your own project folder from the sidebar to work on its code.",
                home.display(),
                state.workspace.display()
            ),
        );
    }
    gs.default_project = state.db.upsert_project(&home).ok().map(|p| p.id);
    if session.project_id.is_none() {
        if let Some(p) = &gs.default_project {
            let _ = state.db.set_session_project(&session.id, Some(p));
            state.forget_workspaces(); // T-149
        }
    }
    gs.editors = find_editors().into_iter().map(|(n, _)| n.to_string()).collect();
    gs.commands = GUI_COMMANDS.iter().map(|(n, a, h)| (format!("{n}{}{a}", if a.is_empty() { "" } else { " " }), h.to_string())).collect();
    for doc in crate::skills::load_commands(&state.workspace) {
        gs.commands.push((doc.name.clone(), doc.description.clone()));
    }
    // Agents write bash; without Git Bash they get cmd.exe and stumble.
    if crate::tools::shell_kind() == crate::permissions::Shell::Cmd {
        gs.push(
            EntryKind::System,
            "Git for Windows is not installed, so agents run commands in cmd.exe and often get them \
             wrong. Install it (then restart Habi): winget install --id Git.Git -e --source winget"
                .to_string(),
        );
    }
    {
        let p = &state.cfg.provider;
        let placeholder = p.base_url == crate::config::SETUP_PLACEHOLDER_URL;
        gs.settings = GuiSettings {
            base_url: if placeholder { String::new() } else { p.base_url.clone() },
            model: p.model.clone(),
            reasoning_effort: p.reasoning_effort.clone().unwrap_or_default(),
            has_key: !p.api_key.is_empty(),
            needs_setup: placeholder || p.model.trim().is_empty(),
            models: Vec::new(),
            models_error: (0, String::new()),
            config_path: crate::config::default_config_path().display().to_string(),
            system_prompt: String::new(),
            prompt_replaces: true,
            temperature: p.temperature,
        };
        let (main, extra) = state.user_prompt.read().map(|p| p.clone()).unwrap_or_default();
        gs.settings.prompt_replaces = extra.is_none();
        gs.settings.system_prompt = main.or(extra).unwrap_or_default();
    }
    gs.approval_timeout_secs = state.cfg.permissions.approval_timeout_secs;
    gs.perm_mode = state.perm_mode();
    refresh_sessions(&mut gs, state);
    gs
}

/// Rust -> page: server events wake the loop, then the loop drains + pushes.
fn forward_server_events(
    state: &server::AppState,
    event_loop: &EventLoop<AppEvent>,
) -> tokio::sync::mpsc::UnboundedReceiver<ServerEvent> {
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<ServerEvent>();
    let proxy = event_loop.create_proxy();
    let mut rx = state.events.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if evt_tx.send(ev).is_err() {
                        break;
                    }
                    let _ = proxy.send_event(AppEvent::Wake);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    evt_rx
}

fn tracing_boot(boot_ms: u128, addr: std::net::SocketAddr, session: &str) {
    gui_log(&format!("[habi] ready in {boot_ms}ms \u{b7} core at http://{addr} \u{b7} session {session}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_respects_char_boundary_and_budget() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("hello world", 5), "hello\u{2026}");
        assert_eq!(clip("\u{1f512}abc", 2), "\u{1f512}a\u{2026}");
    }

    /// T-057: GUI diagnostics (a page error here) reach core.log.
    #[test]
    fn gui_diagnostics_reach_core_log() {
        let marker = format!("[gui] page error: t057-{}-{}", std::process::id(), dispatch::epoch_ms());
        gui_log(&marker);
        let log = std::fs::read_to_string(crate::config::default_state_dir().join("core.log")).unwrap_or_default();
        assert!(log.contains(&marker), "core.log has the line");
    }
}
