//! M8: the desktop app shell (working name "Habi") — a wry/WebView2 window
//! hosting our own UI (`src/gui.html`), with every decision made in Rust.
//!
//! Why not egui/eframe: this machine's Intel HD Graphics 3000 caps at
//! DX10.1 / OpenGL 3.1, so eframe's glow renderer ("extension to create ES
//! context with wgl is not present") and wgpu ("no suitable adapter") both
//! fail — egui cannot run on this GPU at all. WebView2 renders through
//! ANGLE/D3D and works here (M4's preview proved it), and it is the same
//! tech the M9 browser panel needs, so the whole app shares one renderer.

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

const APP_HTML: &str = include_str!("gui.html");
/// Sidebar title for fresh chats; renamed from the first user message.
const NEW_CHAT: &str = "New chat";

/// Testable GUI state: everything the window draws, minus the toolkit.
pub struct GuiState {
    pub entries: Vec<(EntryKind, String)>,
    pub input: String,
    pub busy: bool,
    pub session: Option<String>,
    pub api: Option<(bool, u64)>,
    /// Sidebar chats, newest first (top-level chats only: subagent and
    /// team-worker chats are internal).
    pub sessions: Vec<ChatItem>,
    /// Projects (folders) for the sidebar, by name.
    pub projects: Vec<crate::db::Project>,
    /// Where a plain "New chat" goes when the open chat has no project.
    pub default_project: Option<String>,
    /// New (B-12): chats that have a run in flight right now.
    pub running: std::collections::HashSet<String>,
    /// New (B-12): live transcripts of running chats that are not on screen,
    /// so switching back shows what streamed while they were hidden.
    pub background: HashMap<String, Vec<(EntryKind, String)>>,
    /// New (B-12): approvals waiting behind the one on screen (several chats
    /// can be blocked at once). Shown one at a time, oldest first.
    pub approval_queue: std::collections::VecDeque<PendingApproval>,
    /// New (B-12): another client created or switched a chat; reload the
    /// sidebar list from the database.
    pub sessions_stale: bool,
    /// Preview panel: URL, history flags, geometry. Drawn by the page;
    /// the page itself lives in a separate native child webview.
    pub browser: BrowserView,
    /// Last `http://localhost:<port>` seen in tool output (A-B5).
    pub dev_server: Option<String>,
    /// D1: the tool call waiting on the user right now (`ask` permissions).
    pub approval: Option<PendingApproval>,
    /// `[permissions] approval_timeout_secs` — drives the modal countdown.
    pub approval_timeout_secs: u64,
    /// Token totals per chat (from `ServerEvent::Usage`, or the database
    /// when a chat is opened); the sidebar shows the open chat's cache rate.
    pub usage: HashMap<String, crate::db::UsageTotals>,
    /// Autonomy mode shown by the composer selector. The core owns the real
    /// value; this mirrors it for the page.
    pub perm_mode: PermissionMode,
    /// A4: spawned agents, oldest first (the open chat's are shown).
    pub agents: Vec<AgentCard>,
    /// Code editors found on this machine (for "Open in editor").
    pub editors: Vec<String>,
    /// `/` autocomplete: built-in and project commands (name, help).
    pub commands: Vec<(String, String)>,
    /// `@` autocomplete: the last query and its matching files.
    pub file_matches: (String, Vec<String>),
    /// Next-step buttons after a run: (label, what to send; `@changes`
    /// opens the Changes view).
    pub followups: Vec<(String, String)>,
    /// The last status check, in words (tooltip of the API pill).
    pub api_detail: String,
    /// Terminal panel: shown or not, its folder (`None` = the chat's
    /// project), what was typed there, and the last view sent to the page.
    pub term_open: bool,
    pub term_cwd: Option<std::path::PathBuf>,
    pub term_entries: Vec<TermEntry>,
    pub term_root: std::cell::RefCell<std::path::PathBuf>,
    pub term_last: std::cell::RefCell<String>,
    /// Text to put in the composer (after a rewind), with a counter so the
    /// page applies each one once.
    pub compose: (u64, String),
    /// The project's git branch and how many files changed, if git.
    pub git: Option<(String, usize)>,
    /// Read the git state again (after a run, a chat switch).
    pub git_stale: bool,
    /// The open chat's `ask_user` question and its answer buttons.
    pub question: Option<(String, Vec<String>)>,
    /// What the Settings page shows.
    pub settings: GuiSettings,
    /// Attachments to put into the composer: (counter, markers). The page
    /// inserts each batch once, by counter.
    pub attach: (u64, Vec<String>),
    /// The Changes view waiting to be sent to the page (JSON), once.
    pub changes_out: std::cell::RefCell<Option<String>>,
    /// New (B-37): the entries as the page last received them, and for which
    /// chat, so a paint only sends what changed. `RefCell` because painting
    /// only has `&GuiState`.
    sent: std::cell::RefCell<Vec<(EntryKind, String)>>,
    sent_session: std::cell::RefCell<Option<String>>,
}

/// The Settings page: provider fields (never the key itself).
#[derive(Debug, Clone, Default)]
pub struct GuiSettings {
    pub base_url: String,
    pub model: String,
    pub reasoning_effort: String,
    pub has_key: bool,
    /// No provider yet: the page opens by itself.
    pub needs_setup: bool,
    /// Models the provider lists (after "Load models").
    pub models: Vec<String>,
    pub config_path: String,
}

/// A4: one spawned agent in the agents panel.
#[derive(Debug, Clone)]
pub struct AgentCard {
    pub id: String,
    /// The chat that spawned it.
    pub parent: String,
    pub name: String,
    pub task: String,
    /// queued | working | done | failed
    pub state: String,
    /// Current tool, or the first line of the result.
    pub detail: String,
    pub started: std::time::Instant,
    pub ended: Option<std::time::Instant>,
}

impl AgentCard {
    fn running(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "working")
    }
}

/// Most agent cards kept; the oldest finished ones go first.
const MAX_AGENT_CARDS: usize = 60;

/// One chat in the sidebar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatItem {
    pub id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    /// Nothing was said yet: hidden from the lists (except when open).
    pub empty: bool,
    pub updated_at: i64,
}

impl ChatItem {
    /// A plain, non-empty chat (tests and simple callers).
    pub fn new(id: &str, title: &str) -> Self {
        Self { id: id.into(), title: title.into(), ..Default::default() }
    }
}

/// One `ask` decision the engine is blocked on. The core holds the actual
/// oneshot; this is only what the window needs to draw and answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub id: String,
    /// New (B-12): the chat whose run is blocked on this approval.
    pub session_id: String,
    pub tool: String,
    pub detail: String,
    pub started: std::time::Instant,
}

impl PendingApproval {
    /// Seconds before the engine auto-denies (never negative).
    fn secs_left(&self, timeout_secs: u64) -> u64 {
        timeout_secs.saturating_sub(self.started.elapsed().as_secs())
    }
}

impl Default for GuiState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            busy: false,
            session: None,
            api: None,
            sessions: Vec::new(),
            projects: Vec::new(),
            default_project: None,
            running: std::collections::HashSet::new(),
            background: HashMap::new(),
            approval_queue: std::collections::VecDeque::new(),
            sessions_stale: false,
            browser: BrowserView::default(),
            dev_server: None,
            approval: None,
            approval_timeout_secs: 300,
            usage: HashMap::new(),
            perm_mode: PermissionMode::Default,
            agents: Vec::new(),
            editors: Vec::new(),
            commands: Vec::new(),
            file_matches: (String::new(), Vec::new()),
            followups: Vec::new(),
            api_detail: String::new(),
            term_open: false,
            term_cwd: None,
            term_entries: Vec::new(),
            term_root: std::cell::RefCell::new(std::path::PathBuf::from(".")),
            term_last: std::cell::RefCell::new(String::new()),
            compose: (0, String::new()),
            git: None,
            git_stale: true,
            question: None,
            settings: GuiSettings::default(),
            attach: (0, Vec::new()),
            changes_out: std::cell::RefCell::new(None),
            sent: std::cell::RefCell::new(Vec::new()),
            sent_session: std::cell::RefCell::new(None),
        }
    }
}

/// Panel geometry in CSS pixels. These are *already* logical pixels — `dpi`
/// does the scale-factor conversion, so multiplying by `devicePixelRatio`
/// here would double-scale the child view off the panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl PanelRect {
    /// Fixed size used until the page reports its real layout.
    pub fn fallback() -> Self {
        Self { x: 0.0, y: 0.0, w: 200.0, h: 200.0 }
    }

    /// A rect we can actually show a view in (finite, non-degenerate).
    pub fn is_usable(self) -> bool {
        [self.x, self.y, self.w, self.h].iter().all(|v| v.is_finite()) && self.w >= 1.0 && self.h >= 1.0
    }

    /// The rect multiplied by the page zoom (Ctrl+wheel in the app window
    /// makes one CSS pixel `z` logical pixels).
    pub fn scaled(self, z: f64) -> Self {
        Self { x: self.x * z, y: self.y * z, w: self.w * z, h: self.h * z }
    }

    /// CSS pixels -> wry `Rect`, with no manual DPI math.
    pub fn to_wry(self) -> wry::Rect {
        wry::Rect {
            position: wry::dpi::LogicalPosition::new(self.x, self.y).into(),
            size: wry::dpi::LogicalSize::new(self.w, self.h).into(),
        }
    }
}

/// Everything the browser panel chrome draws, plus the native view's target.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrowserView {
    pub url: String,
    pub title: String,
    pub can_back: bool,
    pub can_forward: bool,
    pub loading: bool,
    /// Last refusal/failure, shown as a card in place of the page.
    pub error: Option<String>,
    /// Whether the native child view should be on screen.
    pub visible: bool,
    pub rect: Option<PanelRect>,
    /// `[browser] home_url` — offered when there is nothing to load.
    pub home: String,
    /// Set while an HTML overlay covers the panel. A native child view always
    /// paints above the page, so it must be hidden, not layered under.
    pub covered: bool,
    /// New (B-42): the user closed the panel. Only an explicit `open()` (the
    /// user or the agent asked for a page) shows it again.
    pub user_closed: bool,
}

impl BrowserView {
    /// Start a navigation: remember the URL, drop the error, reveal the view.
    pub fn open(&mut self, url: &str) {
        self.url = url.to_string();
        self.error = None;
        self.loading = true;
        self.visible = true;
        self.user_closed = false;
    }

    /// Refuse or fail: the native view cannot be composited under HTML, so it
    /// has to go away for the error card to be readable.
    pub fn fail(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
        self.loading = false;
        self.visible = false;
    }

    /// True when a page (not the empty state) is showing.
    pub fn has_page(&self) -> bool {
        !self.url.is_empty() && self.error.is_none()
    }

    /// New (B-11): should the native child view be on screen right now?
    pub fn wants_native_view(&self) -> bool {
        self.visible
            && !self.covered
            && self.error.is_none()
            && matches!(self.rect, Some(r) if r.is_usable())
    }

    fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "url": self.url,
            "title": self.title,
            "can_back": self.can_back,
            "can_forward": self.can_forward,
            "loading": self.loading,
            "error": self.error,
            "visible": self.visible,
            "home": self.home,
        })
    }
}

impl GuiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// New (B-13): drop approvals the core must have timed out already —
    /// their `ApprovalResolved` event may have been lost. `now` is a
    /// parameter so tests can use a fixed clock. Returns true when one went.
    pub fn expire_stale_approval(&mut self, now: Instant) -> bool {
        let grace = Duration::from_secs(self.approval_timeout_secs + 5);
        let stale: Vec<String> = self
            .approval
            .iter()
            .chain(self.approval_queue.iter())
            .filter(|a| now.saturating_duration_since(a.started) > grace)
            .map(|a| a.id.clone())
            .collect();
        for id in &stale {
            self.finish_approval(id);
        }
        if !stale.is_empty() {
            self.push(
                EntryKind::System,
                "An approval got no answer in time and was denied.".to_string(),
            );
        }
        !stale.is_empty()
    }

    /// New (B-12): true when chat `id` is the one on screen.
    pub fn is_current(&self, id: &str) -> bool {
        self.session.as_deref() == Some(id)
    }

    /// New (B-12): sidebar title of chat `id` ("New chat" when unknown).
    pub fn session_title(&self, id: &str) -> &str {
        self.sessions
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.title.as_str())
            .unwrap_or(NEW_CHAT)
    }

    /// New (B-12): approval `id` is answered or gone. Drop it (on screen or
    /// queued) and show the next waiting one, if any.
    pub fn finish_approval(&mut self, id: &str) {
        self.approval_queue.retain(|a| a.id != id);
        if self.approval.as_ref().is_some_and(|a| a.id == id) {
            self.approval = self.approval_queue.pop_front();
        }
        self.browser.covered = self.approval.is_some();
    }

    /// Project of the open chat, else the default project ("New chat" goes
    /// there).
    pub fn current_project(&self) -> Option<String> {
        self.session
            .as_ref()
            .and_then(|id| self.sessions.iter().find(|c| &c.id == id))
            .and_then(|c| c.project_id.clone())
            .or_else(|| self.default_project.clone())
    }

    /// Title of the active session (chat header + window title).
    pub fn title(&self) -> &str {
        self.session
            .as_ref()
            .and_then(|id| {
                self.sessions
                    .iter()
                    .find(|c| &c.id == id)
                    .map(|c| c.title.as_str())
            })
            .unwrap_or(NEW_CHAT)
    }

    /// Streaming kinds merge into the previous entry (one bubble per reply).
    pub fn push(&mut self, kind: EntryKind, text: impl Into<String>) {
        let text = text.into();
        let mergeable = matches!(kind, EntryKind::Assistant | EntryKind::Thinking);
        if mergeable {
            if let Some(last) = self.entries.last_mut() {
                if last.0 == kind {
                    last.1.push_str(&text);
                    return;
                }
            }
        }
        self.entries.push((kind, text));
    }

    /// New (B-12): the chat on screen is being replaced. If its run is still
    /// going, keep its transcript so its events can keep arriving.
    pub fn leave_current_chat(&mut self) {
        let entries = std::mem::take(&mut self.entries);
        if let Some(cur) = self.session.clone() {
            if self.running.contains(&cur) {
                self.background.insert(cur, entries);
            }
        }
    }

    /// New (B-12): the chat a transcript event belongs to. `None` for events
    /// that are not transcript text (run state, approvals, heartbeat).
    fn transcript_session(ev: &ServerEvent) -> Option<String> {
        match ev {
            ServerEvent::AssistantDelta { session_id, .. }
            | ServerEvent::ReasoningDelta { session_id, .. }
            | ServerEvent::ToolStarted { session_id, .. }
            | ServerEvent::ToolFinished { session_id, .. }
            | ServerEvent::Note { session_id, .. }
            | ServerEvent::EditPreview { session_id, .. }
            | ServerEvent::EditApplied { session_id, .. } => Some(session_id.clone()),
            _ => None,
        }
    }

    /// Drain one server event into the chat it belongs to.
    ///
    /// B-12: every chat's events share one bus. Text for a chat that is not
    /// on screen goes to its live copy in `background` (or is dropped), so it
    /// never lands in the open chat.
    pub fn on_event(&mut self, ev: ServerEvent) {
        // Token counts belong to their chat, open or not.
        if let ServerEvent::Usage { session_id, session, .. } = &ev {
            self.usage.insert(session_id.clone(), *session);
            return;
        }
        if let ServerEvent::ModelInfo { available, .. } = &ev {
            self.settings.models = available.clone();
        }
        if let ServerEvent::Question { session_id, question, options } = ev {
            if self.is_current(&session_id) {
                self.push(EntryKind::System, format!("? {question}"));
                self.question = Some((question, options));
            }
            return;
        }
        if let ServerEvent::AgentStatus { agent_id, parent_id, name, task, state, detail } = ev {
            return self.on_agent(agent_id, parent_id, name, task, state, detail);
        }
        // A chat was created or switched by another client: refresh the
        // sidebar, but never take over the chat the user is looking at.
        if matches!(ev, ServerEvent::SessionCreated { .. } | ServerEvent::SessionLoaded { .. }) {
            self.sessions_stale = true;
            return;
        }
        // Run state, approvals and the heartbeat are handled in `apply_event`.
        let Some(sid) = Self::transcript_session(&ev) else {
            return self.apply_event(ev);
        };
        // Streaming output means that chat's run is going.
        if matches!(
            ev,
            ServerEvent::AssistantDelta { .. }
                | ServerEvent::ReasoningDelta { .. }
                | ServerEvent::ToolStarted { .. }
        ) {
            self.running.insert(sid.clone());
            if self.is_current(&sid) {
                self.busy = true;
            }
        }
        if self.is_current(&sid) {
            return self.apply_event(ev);
        }
        // Another chat: append to its live copy while we keep one.
        if let Some(mut live) = self.background.remove(&sid) {
            std::mem::swap(&mut self.entries, &mut live);
            self.apply_event(ev);
            std::mem::swap(&mut self.entries, &mut live);
            self.background.insert(sid, live);
        }
    }

    /// Next-step buttons for the end of a run: no model call, just the usual
    /// next moves (and a retry after a failure).
    fn next_steps(&self, ok: bool) -> Vec<(String, String)> {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        if !ok {
            let last = self
                .entries
                .iter()
                .rev()
                .find(|(k, _)| matches!(k, EntryKind::User))
                .map(|(_, t)| t.trim_start_matches('\u{203a}').trim().to_string());
            let mut v = vec![s("Explain the error", "Explain the error above in plain words and how to fix it.")];
            if let Some(t) = last.filter(|t| !t.is_empty()) {
                v.insert(0, ("Try again".into(), t));
            }
            return v;
        }
        let mut v = vec![s("Show the changes", "@changes"), s("Review the work", "/review")];
        if self.git.as_ref().is_some_and(|(_, n)| *n > 0) {
            v.push(s("Commit it", "Commit all the changes with a short, clear commit message. Do not push."));
        }
        v.push(s("Explain what you did", "Explain briefly what you changed and why, and what I should check."));
        v
    }

    /// A4: upsert the agent's card.
    fn on_agent(&mut self, id: String, parent: String, name: String, task: String, state: String, detail: String) {
        let now = std::time::Instant::now();
        let ended = !matches!(state.as_str(), "queued" | "working");
        match self.agents.iter_mut().find(|a| a.id == id) {
            Some(card) => {
                if ended && card.ended.is_none() {
                    card.ended = Some(now);
                }
                if !ended {
                    // A message from the parent wakes a finished agent.
                    card.ended = None;
                }
                card.state = state;
                card.detail = detail;
            }
            None => {
                self.agents.push(AgentCard {
                    id,
                    parent,
                    name,
                    task,
                    state,
                    detail,
                    started: now,
                    ended: ended.then_some(now),
                });
                if self.agents.len() > MAX_AGENT_CARDS {
                    if let Some(i) = self.agents.iter().position(|a| !a.running()) {
                        self.agents.remove(i);
                    }
                }
            }
        }
    }

    /// Apply one event to the chat on screen (`entries`, `busy`, the modal).
    fn apply_event(&mut self, ev: ServerEvent) {
        match ev {
            ServerEvent::AssistantDelta { text, .. } => self.push(EntryKind::Assistant, text),
            ServerEvent::ReasoningDelta { text, .. } => {
                // First reasoning chunk carries the marker; appends merge bare.
                let last_is_thinking = self
                    .entries
                    .last()
                    .map(|(k, _)| *k == EntryKind::Thinking)
                    .unwrap_or(false);
                if last_is_thinking {
                    self.push(EntryKind::Thinking, text);
                } else {
                    self.push(EntryKind::Thinking, format!("\u{273b} {text}"));
                }
            }
            ServerEvent::ToolStarted { tool, detail, .. } => {
                self.push(EntryKind::Tool, format!("\u{23fa} {tool}({detail})"));
            }
            ServerEvent::ToolFinished { tool, ok, .. } => {
                self.push(EntryKind::Tool, format!("  {} {tool}", if ok { "\u{2713}" } else { "\u{2717}" }));
            }
            ServerEvent::Note { text, .. } => self.push(EntryKind::System, text),
            ServerEvent::RunFinished { session_id, ok, message } => {
                eprintln!("[gui] run finished ok={ok}");
                self.running.remove(&session_id);
                // A finished run cannot still be waiting on an approval — but
                // only ITS approvals end; another chat's run may still wait.
                self.approval_queue.retain(|a| a.session_id != session_id);
                if self.approval.as_ref().is_some_and(|a| a.session_id == session_id) {
                    self.approval = self.approval_queue.pop_front();
                }
                self.browser.covered = self.approval.is_some();
                if !self.is_current(&session_id) {
                    // The database now has the whole reply; the next visit
                    // to that chat replays it from there.
                    self.background.remove(&session_id);
                    return;
                }
                self.busy = false;
                self.git_stale = true;
                self.followups = self.next_steps(ok);
                if let Some(m) = message {
                    self.push(if ok { EntryKind::System } else { EntryKind::Error }, m);
                }
            }
            // SessionCreated / SessionLoaded are handled in `on_event` (B-12).
            ServerEvent::Heartbeat { ok, latency_ms, detail } => {
                self.api = Some((ok, latency_ms));
                self.api_detail = detail;
            }
            ServerEvent::DevServer { url, .. } => self.dev_server = Some(url),
            ServerEvent::ApprovalNeeded { session_id, approval_id, tool, detail, .. } => {
                // The run is blocked until this is answered (or times out).
                // B-12: shown for every chat (a hidden chat's run is blocked
                // too), queued behind one already on screen, and only the
                // chat on screen turns "busy".
                self.running.insert(session_id.clone());
                if self.is_current(&session_id) {
                    self.busy = true;
                }
                let pending = PendingApproval {
                    id: approval_id,
                    session_id,
                    tool,
                    detail,
                    started: std::time::Instant::now(),
                };
                if self.approval.is_some() {
                    self.approval_queue.push_back(pending);
                } else {
                    self.approval = Some(pending);
                }
                // Never leave a native view painted over the modal.
                self.browser.covered = true;
            }
            ServerEvent::EditPreview { path, .. } | ServerEvent::EditApplied { path, .. } => {
                self.push(EntryKind::System, format!("[edit] {path}"));
            }
            // B-13: the core says this approval is over (answered here or by
            // another client, timed out, aborted, or released by turbo).
            ServerEvent::ApprovalResolved { approval_id, reason, .. } => {
                let tool = self
                    .approval
                    .as_ref()
                    .filter(|a| a.id == approval_id && self.is_current(&a.session_id))
                    .map(|a| a.tool.clone());
                self.finish_approval(&approval_id);
                if let Some(tool) = tool {
                    match reason.as_str() {
                        "timeout" => self.push(
                            EntryKind::System,
                            format!("No answer in time — `{tool}` was denied."),
                        ),
                        "abort" => self.push(
                            EntryKind::System,
                            format!("Run stopped — `{tool}` was not run."),
                        ),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Drain one preview-webview callback (navigation, title, load state).
    pub fn on_browser_event(&mut self, ev: BrowserEvt) {
        match ev {
            // Every fresh webview starts on about:blank; that is not a page.
            BrowserEvt::Url(url) if url == "about:blank" => {}
            BrowserEvt::Url(url) => {
                self.browser.url = url;
                self.browser.error = None;
                // B-42: a page that navigates by itself (redirect, reload, hot
                // reload) must not reopen a panel the user closed.
                if !self.browser.user_closed {
                    self.browser.visible = true;
                }
            }
            BrowserEvt::Title(t) => self.browser.title = t,
            BrowserEvt::Loading(loading) => self.browser.loading = loading,
            // The message was already written by `check_navigation`.
            BrowserEvt::Blocked(msg) => self.browser.fail(msg),
            // Requests/responses are matched by id in the event loop, not here.
            BrowserEvt::Eval(_) => {}
        }
    }

    /// Take the input box contents as a sendable message (None = nothing to send).
    pub fn submit(&mut self) -> Option<String> {
        let content = std::mem::take(&mut self.input);
        let content = content.trim().to_string();
        if content.is_empty() {
            return None;
        }
        self.push(EntryKind::User, format!("\u{203a} {content}"));
        self.busy = true;
        // B-12: remember which chat runs (the user may leave it meanwhile).
        if let Some(sid) = self.session.clone() {
            self.running.insert(sid);
        }
        Some(content)
    }

    /// JSON payload for `window.render(...)` — keys are read by gui.html.
    pub fn payload(&self) -> String {
        self.payload_from(0)
    }

    /// New (B-37): the payload for a paint. It sends only the entries that
    /// changed since the last paint; the page keeps the rows it already has.
    pub fn paint_payload(&self) -> String {
        let mut sent = self.sent.borrow_mut();
        // Another chat on screen: the page rebuilds, so send everything.
        if *self.sent_session.borrow() != self.session {
            sent.clear();
            *self.sent_session.borrow_mut() = self.session.clone();
        }
        let from = self
            .entries
            .iter()
            .zip(sent.iter())
            .take_while(|(now, before)| now == before)
            .count();
        sent.truncate(from);
        sent.extend(self.entries[from..].iter().cloned());
        drop(sent);
        self.payload_from(from)
    }

    /// New (B-37): forget what the page has; the next paint sends everything.
    pub fn resync(&self) {
        self.sent.borrow_mut().clear();
        *self.sent_session.borrow_mut() = None;
    }

    /// New (B-37): the payload with `entries[from..]`, plus `entries_from`
    /// and `entries_len` so the page knows where that slice starts.
    fn payload_from(&self, from: usize) -> String {
        let entries: Vec<serde_json::Value> = self
            .entries
            .iter()
            .skip(from)
            .map(|(k, t)| serde_json::json!({ "k": kind_class(k).to_string(), "t": t }))
            .collect();
        // Chats blocked on an approval: shown as "needs you" in the sidebar.
        let waiting: std::collections::HashSet<&str> = self
            .approval
            .iter()
            .chain(self.approval_queue.iter())
            .map(|a| a.session_id.as_str())
            .collect();
        let sessions: Vec<serde_json::Value> = self
            .sessions
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "title": c.title,
                    "active": self.session.as_deref() == Some(c.id.as_str()),
                    "project": c.project_id,
                    "pinned": c.pinned,
                    "archived": c.archived,
                    "empty": c.empty,
                    "updated": c.updated_at,
                    "running": self.running.contains(&c.id),
                    "needs": waiting.contains(c.id.as_str()),
                })
            })
            .collect();
        let agents: Vec<serde_json::Value> = self
            .agents
            .iter()
            .filter(|a| self.session.as_deref() == Some(a.parent.as_str()))
            .map(|a| {
                let secs = a.ended.unwrap_or_else(std::time::Instant::now).duration_since(a.started).as_secs();
                serde_json::json!({
                    "id": a.id,
                    "name": a.name,
                    "task": a.task,
                    "state": a.state,
                    "detail": a.detail,
                    "secs": secs,
                })
            })
            .collect();
        let projects: Vec<serde_json::Value> = self
            .projects
            .iter()
            .map(|p| serde_json::json!({ "id": p.id, "name": p.name, "root": p.root }))
            .collect();
        serde_json::json!({
            "session": self.session.clone().unwrap_or_default(),
            "title": self.title(),
            "busy": self.busy,
            "perm_mode": self.perm_mode.as_str(),
            "api": self.api.map(|(ok, ms)| serde_json::json!([ok, ms])),
            "api_detail": self.api_detail,
            "sessions": sessions,
            "agents": agents,
            "editors": self.editors,
            "commands": self.commands,
            "file_matches": { "query": self.file_matches.0, "items": self.file_matches.1 },
            "followups": self.followups.iter().map(|(l, t)| serde_json::json!({ "label": l, "text": t })).collect::<Vec<_>>(),
            "git": self.git.as_ref().map(|(b, n)| serde_json::json!({ "branch": b, "changed": n })),
            "attach": { "seq": self.attach.0, "items": self.attach.1 },
            "compose": { "seq": self.compose.0, "text": self.compose.1 },
            "question": self.question.as_ref().map(|(q, o)| serde_json::json!({ "text": q, "options": o })),
            "settings": {
                "base_url": self.settings.base_url,
                "model": self.settings.model,
                "reasoning_effort": self.settings.reasoning_effort,
                "has_key": self.settings.has_key,
                "needs_setup": self.settings.needs_setup,
                "models": self.settings.models,
                "config_path": self.settings.config_path,
            },
            "projects": projects,
            "current_project": self.current_project(),
            "entries": entries,
            "entries_from": from,
            "entries_len": self.entries.len(),
            "browser": self.browser.payload(),
            "dev_server": self.dev_server,
            "usage": self.session.as_ref().and_then(|s| self.usage.get(s)),
            "approval": self.approval.as_ref().map(|a| serde_json::json!({
                "id": a.id,
                "tool": a.tool,
                "detail": a.detail,
                "secs_left": a.secs_left(self.approval_timeout_secs),
                // B-12: which chat asks, and how many requests wait behind it.
                "other_chat": !self.is_current(&a.session_id),
                "chat": self.session_title(&a.session_id),
                "queued": self.approval_queue.len(),
            })),
        })
        .to_string()
    }
}

fn kind_class(k: &EntryKind) -> char {
    match k {
        EntryKind::User => 'u',
        EntryKind::Assistant => 'a',
        EntryKind::Thinking => 't',
        EntryKind::Tool => 'w',
        EntryKind::System => 's',
        EntryKind::Error => 'x',
    }
}

/// Clip text to `max` chars with an ellipsis (chat titles, tool outputs).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}\u{2026}")
    }
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

/// A message from the page (`window.ipc.postMessage`).
#[derive(Debug, Clone, PartialEq)]
pub enum IpcMsg {
    Send(String),
    NewSession,
    Load(String),
    /// The page finished loading — trigger the first paint.
    Ready,
    /// New (B-37): the page missed a paint and wants everything again.
    Resync,
    Minimize,
    MaximizeToggle,
    Close,
    /// Navigate the preview panel (as typed; normalized when handled).
    Browse(String),
    NavBack,
    NavFwd,
    NavReload,
    /// Where the page wants the native child view, in CSS pixels.
    /// The panel's rect in CSS pixels and the page's `devicePixelRatio`
    /// (0 when not sent), which tells the page zoom.
    PanelRect(PanelRect, f64),
    /// Show/hide the native child view (collapsed panel, overlays, errors).
    PanelVisible(bool),
    /// The "Open Dev Server" CTA, optionally with an explicit URL.
    OpenDevServer(Option<String>),
    /// D1: answer a pending `ask` gate.
    Approve { id: String, always: bool },
    Deny(String),
    /// Composer mode selector: default / manual / turbo autonomy.
    Mode(PermissionMode),
    /// An uncaught JS error in the page (`window.onerror`).
    PageError(String),
    /// New (F-06): the Stop button — abort the run of the chat on screen.
    Stop,
    /// A4: stop one spawned agent.
    AgentStop(String),
    /// Settings page: save the provider; `api_key` None keeps the saved one.
    SaveSettings { base_url: String, model: String, api_key: Option<String>, effort: String },
    /// Settings page: ask the provider which models it has.
    ListModels,
    /// Write the open chat as Markdown into its project.
    Export,
    /// Attach files picked in a dialog.
    PickFiles,
    /// Open the Changes view (the project's diff).
    LoadChanges,
    /// Make a new git branch in the chat's project and switch to it.
    NewBranch(String),
    /// Terminal panel: shown or hidden.
    TermOpen(bool),
    /// Terminal panel: run a command (or `cd`).
    TermRun(String),
    /// Terminal panel: stop a command (`None` = the running one typed there).
    TermStop(Option<String>),
    /// Terminal panel: forget what was typed and printed.
    TermClear,
    /// Rewind the chat to before this user message (`skip` = later
    /// messages with the same text to pass over).
    Rewind { text: String, skip: usize },
    /// `@` autocomplete: project files matching the text.
    FindFiles(String),
    /// Send line comments from the Changes view to the agent.
    ReviewComments(Vec<(String, u32, String)>),
    /// Attach a pasted or dropped file (`data` is base64).
    SaveAttachment { name: String, data: String },
    /// Open the chat's project folder in a code editor (by name, or the first found).
    OpenInEditor(Option<String>),
    /// Projects sidebar: a new chat inside project `id`.
    NewInProject(String),
    /// Add a project: this folder, or (no path) ask with a folder picker.
    AddProject(Option<String>),
    /// Take a project off the list (its chats stay, without a project).
    RemoveProject(String),
    Pin { id: String, on: bool },
    Archive { id: String, on: bool },
    Rename { id: String, title: String },
    /// Move a chat to a project (`None` = no project).
    MoveChat { id: String, project: Option<String> },
    /// Delete a chat for good (from History).
    DeleteChat(String),
    /// File every chat without a project under project `id`.
    AdoptUnfiled(String),
}

/// Callbacks from the preview child webview itself (not from the page).
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserEvt {
    Url(String),
    Title(String),
    Loading(bool),
    /// A navigation we refused. Carries the message the panel shows
    /// (written by `check_navigation`).
    Blocked(String),
    /// A result frame posted by the injected page runtime (Goal B).
    Eval(String),
}

/// One input to the event loop: either a page verb or a webview callback.
#[derive(Debug, Clone, PartialEq)]
pub enum UiMsg {
    Ipc(IpcMsg),
    Browser(BrowserEvt),
}

/// Parse one IPC frame; `None` = ignore (unknown or malformed).
pub fn parse_ipc(raw: &str) -> Option<IpcMsg> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    match v.get("type")?.as_str()? {
        "send" => {
            let text = v.get("text")?.as_str()?.trim().to_string();
            if text.is_empty() { None } else { Some(IpcMsg::Send(text)) }
        }
        "new" => Some(IpcMsg::NewSession),
        "load" => Some(IpcMsg::Load(v.get("id")?.as_str()?.to_string())),
        "ready" => Some(IpcMsg::Ready),
        "resync" => Some(IpcMsg::Resync),
        "min" => Some(IpcMsg::Minimize),
        "max" => Some(IpcMsg::MaximizeToggle),
        "close" => Some(IpcMsg::Close),
        "browse" => Some(IpcMsg::Browse(v.get("url")?.as_str()?.to_string())),
        "nav_back" => Some(IpcMsg::NavBack),
        "nav_fwd" => Some(IpcMsg::NavFwd),
        "nav_reload" => Some(IpcMsg::NavReload),
        "panel_rect" => {
            let num = |k: &str| v.get(k)?.as_f64();
            let rect = PanelRect { x: num("x")?, y: num("y")?, w: num("w")?, h: num("h")? };
            let dpr = v.get("dpr").and_then(|d| d.as_f64()).unwrap_or(0.0);
            if rect.is_usable() { Some(IpcMsg::PanelRect(rect, dpr)) } else { None }
        }
        "panel_visible" => Some(IpcMsg::PanelVisible(v.get("visible")?.as_bool()?)),
        "open_dev_server" => {
            Some(IpcMsg::OpenDevServer(v.get("url").and_then(|u| u.as_str()).map(str::to_string)))
        }
        "approve" => Some(IpcMsg::Approve {
            id: v.get("id")?.as_str()?.to_string(),
            always: v.get("always").and_then(|a| a.as_bool()).unwrap_or(false),
        }),
        "deny" => Some(IpcMsg::Deny(v.get("id")?.as_str()?.to_string())),
        "mode" => Some(IpcMsg::Mode(PermissionMode::parse(v.get("mode")?.as_str()?)?)),
        "page_error" => Some(IpcMsg::PageError(v.get("msg")?.as_str()?.to_string())),
        "stop" => Some(IpcMsg::Stop),
        "agent_stop" => Some(IpcMsg::AgentStop(v.get("id")?.as_str()?.to_string())),
        "save_settings" => Some(IpcMsg::SaveSettings {
            base_url: v.get("base_url")?.as_str()?.trim().to_string(),
            model: v.get("model")?.as_str()?.trim().to_string(),
            api_key: v.get("api_key").and_then(|k| k.as_str()).map(str::to_string),
            effort: v.get("reasoning_effort").and_then(|e| e.as_str()).unwrap_or("").to_string(),
        }),
        "list_models" => Some(IpcMsg::ListModels),
        "export" => Some(IpcMsg::Export),
        "pick_files" => Some(IpcMsg::PickFiles),
        "load_changes" => Some(IpcMsg::LoadChanges),
        "term_open" => Some(IpcMsg::TermOpen(v.get("open")?.as_bool()?)),
        "term_run" => {
            let c = v.get("command")?.as_str()?.trim().to_string();
            if c.is_empty() { None } else { Some(IpcMsg::TermRun(c)) }
        }
        "term_stop" => Some(IpcMsg::TermStop(v.get("id").and_then(|i| i.as_str()).map(str::to_string))),
        "term_clear" => Some(IpcMsg::TermClear),
        "rewind" => Some(IpcMsg::Rewind {
            text: v.get("text")?.as_str()?.to_string(),
            skip: v.get("skip").and_then(|s| s.as_u64()).unwrap_or(0) as usize,
        }),
        "new_branch" => Some(IpcMsg::NewBranch(v.get("name")?.as_str()?.trim().to_string())),
        "find_files" => Some(IpcMsg::FindFiles(v.get("query").and_then(|q| q.as_str()).unwrap_or("").to_string())),
        "review_comments" => Some(IpcMsg::ReviewComments(
            v.get("comments")?
                .as_array()?
                .iter()
                .filter_map(|c| {
                    Some((
                        c.get("path")?.as_str()?.to_string(),
                        c.get("line")?.as_u64()? as u32,
                        c.get("text")?.as_str()?.to_string(),
                    ))
                })
                .collect(),
        )),
        "save_attachment" => Some(IpcMsg::SaveAttachment {
            name: v.get("name").and_then(|n| n.as_str()).unwrap_or("pasted.png").to_string(),
            data: v.get("data")?.as_str()?.to_string(),
        }),
        "open_in_editor" => Some(IpcMsg::OpenInEditor(v.get("editor").and_then(|e| e.as_str()).map(str::to_string))),
        "new_in_project" => Some(IpcMsg::NewInProject(v.get("project")?.as_str()?.to_string())),
        "add_project" => Some(IpcMsg::AddProject(
            v.get("path").and_then(|p| p.as_str()).map(str::to_string).filter(|p| !p.trim().is_empty()),
        )),
        "remove_project" => Some(IpcMsg::RemoveProject(v.get("id")?.as_str()?.to_string())),
        "pin" => Some(IpcMsg::Pin { id: v.get("id")?.as_str()?.to_string(), on: v.get("on")?.as_bool()? }),
        "archive" => Some(IpcMsg::Archive { id: v.get("id")?.as_str()?.to_string(), on: v.get("on")?.as_bool()? }),
        "rename" => {
            let id = v.get("id")?.as_str()?.to_string();
            let title = v.get("title")?.as_str()?.trim().to_string();
            (!title.is_empty()).then_some(IpcMsg::Rename { id, title })
        }
        "move" => Some(IpcMsg::MoveChat {
            id: v.get("id")?.as_str()?.to_string(),
            project: v.get("project").and_then(|p| p.as_str()).map(str::to_string),
        }),
        "delete" => Some(IpcMsg::DeleteChat(v.get("id")?.as_str()?.to_string())),
        "adopt_unfiled" => Some(IpcMsg::AdoptUnfiled(v.get("project")?.as_str()?.to_string())),
        _ => None,
    }
}

/// New (S-11): the one rule for every page the preview panel opens — typed
/// URLs, the agent's `browser_navigate`, the dev-server button, links,
/// redirects and back/forward. `Err` carries the message the panel shows.
pub fn check_navigation(url: &str, allow_remote: bool) -> Result<(), String> {
    if !is_allowed_scheme(url) {
        return Err(format!("Blocked: `{url}` — the panel only loads http(s) pages."));
    }
    if !allow_remote && !is_local_url(url) {
        return Err(format!(
            "`{url}` is blocked: this panel is dev-server-only ([browser] allow_remote = false)."
        ));
    }
    Ok(())
}

/// Schemes the panel may load. Anything else (`file:`, `javascript:`, `data:`)
/// is refused before it reaches the engine — a browsing surface inside the app
/// must not be a way to read local files or run script against our origin.
pub fn is_allowed_scheme(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower == "about:blank"
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("localhost")
        || lower.starts_with("127.0.0.1")
}

/// Turn what a human typed into a URL we are willing to load.
///
/// `:3000` -> `http://localhost:3000`, `localhost:3000` -> `http://localhost:3000`,
/// bare hosts get `https://`, and refused schemes come back as an error the
/// page shows as a card instead of a silent no-op.
fn normalize_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Enter a URL to open.".into());
    }
    let lower = raw.to_ascii_lowercase();
    // Refuse explicit non-web schemes with a message that says why.
    if let Some(scheme) = lower.split(':').next() {
        if matches!(scheme, "file" | "javascript" | "data" | "vbscript" | "chrome" | "edge") {
            return Err(format!("`{scheme}:` URLs are blocked in the preview panel."));
        }
    }
    if lower == "about:blank" || lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(raw.to_string());
    }
    // Host:port shorthands people actually type.
    if let Some(rest) = raw.strip_prefix(':') {
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) && rest.chars().all(|c| c.is_ascii_digit()) {
            return Ok(format!("http://localhost:{rest}"));
        }
    }
    let first = raw.split(['/', '?', '#']).next().unwrap_or(raw);
    if lower.starts_with("localhost") || lower.starts_with("127.0.0.1") {
        return Ok(format!("http://{raw}"));
    }
    // Bare IPv4 (optionally with a port) is a local server, so plain http.
    let host = first.split(':').next().unwrap_or(first);
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() == 4 && octets.iter().all(|o| !o.is_empty() && o.chars().all(|c| c.is_ascii_digit())) {
        return Ok(format!("http://{raw}"));
    }
    if !host.contains('.') {
        return Err(format!("`{raw}` is not a URL — try `example.com` or `localhost:3000`."));
    }
    Ok(format!("https://{raw}"))
}

/// Push the current state into the page (no-op until the page is loaded).
/// B-37: only the entries that changed since the last paint are sent.
fn paint(webview: &wry::WebView, gs: &GuiState) {
    let _ = webview.evaluate_script(&format!("window.render&&window.render({})", gs.paint_payload()));
    send_term(webview, gs);
    // A Changes view is sent once, apart from the frequent repaints.
    if let Some(json) = gs.changes_out.borrow_mut().take() {
        let _ = webview.evaluate_script(&format!("window.showChanges&&window.showChanges({json})"));
    }
}

/// New (S-18): may the main window go to `uri`? Only our own page: WebView2
/// reports the page loaded from a string as `about:blank` or as a
/// `data:text/html…` URI (the exact form depends on the runtime version).
/// Chromium itself refuses `data:` navigations started by page scripts.
fn main_window_may_navigate(uri: &str) -> bool {
    let u = uri.trim().to_ascii_lowercase();
    u == "about:blank" || u.starts_with("data:text/html")
}

/// New (S-18): navigation handler for the main window. The very first
/// navigation is always our own page (`with_html`), whatever URI the
/// runtime reports for it; after that only `main_window_may_navigate` URIs.
fn main_window_navigation_guard() -> impl Fn(String) -> bool + 'static {
    let first = std::sync::atomic::AtomicBool::new(true);
    move |uri: String| {
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) || main_window_may_navigate(&uri) {
            return true;
        }
        eprintln!("[gui] blocked a navigation of the main window to {uri}");
        false
    }
}

/// Apply the panel's rect and visibility to the native child view.
///
/// Bounds go first: a view shown at a stale rect flashes a grey/white
/// rectangle outside the panel until the next frame. Child views are not
/// clipped by CSS, so "hidden" really has to mean `set_visible(false)`.
fn show_browser(wv: &wry::WebView, view: &BrowserView) {
    if let Some(rect) = view.rect.filter(|r| r.is_usable()) {
        let _ = wv.set_bounds(rect.to_wry());
    }
    let show = view.wants_native_view();
    let _ = wv.set_visible(show);
}

/// Clear the modal for the request it belongs to and unhide the panel.
/// `answered` is false when the engine already timed out / aborted it.
fn settle_approval(gs: &mut GuiState, id: &str, answered: bool) {
    // B-12: also brings up the next queued approval, if any.
    gs.finish_approval(id);
    if !answered {
        gs.push(EntryKind::System, "That approval had already expired.".to_string());
    }
}

/// Mirror the engine's history state into the payload (drives arrow buttons).
fn sync_history(wv: &wry::WebView, view: &mut BrowserView) {
    view.can_back = wv.can_go_back().unwrap_or(false);
    view.can_forward = wv.can_go_forward().unwrap_or(false);
}

/// True for URLs that stay on this machine. Used to enforce the
/// dev-server-only mode (`[browser] allow_remote = false`).
///
/// The URL is parsed with the same rules a browser uses (the `url` crate,
/// re-exported by `reqwest`), so the host we check is the host the preview
/// really connects to. Cutting the text by hand got forms like
/// `http://name:password@host/` wrong.
pub fn is_local_url(url: &str) -> bool {
    let url = url.trim();
    if url.eq_ignore_ascii_case("about:blank") {
        return true;
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    match parsed.host_str() {
        // The parser lower-cases host names, so `LOCALHOST` arrives here too.
        Some("localhost") => true,
        // IP addresses: any loopback address (127.0.0.0/8 or ::1), or the
        // "all interfaces" address 0.0.0.0 that dev servers often print.
        // `host_str()` keeps the brackets around IPv6, so strip them first.
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback() || ip.is_unspecified())
            .unwrap_or(false),
        None => false,
    }
}

/// New (B-42): the newest chat, if it has no messages yet (reuse it instead of
/// creating another empty "New chat").
/// Where plain chats work. The start folder, unless it is no project: Habi's
/// own source (`cargo run` there put a user's mission into this code), the
/// folder the program sits in, the home folder, a system folder or a drive
/// root (a double-clicked app starts in one of those). Then `~/Habi Projects`.
pub fn chat_home(start: &std::path::Path) -> std::path::PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(std::path::PathBuf::from);
    let fallback = home.clone().unwrap_or_else(std::env::temp_dir).join("Habi Projects");
    let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let start_c = canon(start);
    let is_habi_source = std::fs::read_to_string(start.join("Cargo.toml"))
        .is_ok_and(|t| t.contains("name = \"opencode-rust\""));
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(canon));
    let lower = start_c.to_string_lossy().to_lowercase().replace('/', "\\");
    let system = ["\\windows", "\\program files", "\\programdata"].iter().any(|s| lower.contains(s));
    let unsuitable = is_habi_source
        || start_c.parent().is_none()
        || exe_dir.as_deref() == Some(start_c.as_path())
        || home.as_deref().map(canon).as_deref() == Some(start_c.as_path())
        || system;
    if unsuitable { fallback } else { start.to_path_buf() }
}

fn newest_empty_session(st: &server::AppState) -> Option<crate::db::Session> {
    let newest = st.db.list_sessions().ok()?.into_iter().next()?;
    let empty = st.db.messages(&newest.id).map(|m| m.is_empty()).unwrap_or(false);
    empty.then_some(newest)
}

/// Open a fresh chat in `project` (or reuse the open chat when it is
/// already an empty one in that project — B-42).
fn open_new_chat(gs: &mut GuiState, st: &server::AppState, project: Option<String>) {
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

/// Ask for a folder with the system's folder picker (Windows). Elsewhere
/// the page asks for a path and sends it with `add_project`.
#[cfg(windows)]
fn pick_folder(window: &tao::window::Window) -> Option<std::path::PathBuf> {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
    };
    // SAFETY: plain COM calls on the UI thread; the returned string is
    // copied and then freed with CoTaskMemFree as the API requires.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        dialog.Show(Some(HWND(window.hwnd() as _))).ok()?; // Err = cancelled
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(std::path::PathBuf::from)
    }
}

#[cfg(not(windows))]
fn pick_folder(_window: &tao::window::Window) -> Option<std::path::PathBuf> {
    None
}

/// Save the Settings page and build the client it describes. Returns the
/// config path, the new client and whether a key is set.
fn save_settings(
    st: &server::AppState,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    effort: &str,
) -> Result<(std::path::PathBuf, crate::provider::Client, bool)> {
    if base_url.trim().is_empty() || model.trim().is_empty() {
        anyhow::bail!("the provider address and the model are both needed");
    }
    let path = crate::settings::save_provider(base_url, model, effort)?;
    if let Some(k) = api_key.filter(|k| !k.trim().is_empty()) {
        crate::settings::save_api_key(k)?;
    }
    let mut p = st.cfg.provider.clone();
    p.base_url = base_url.trim().to_string();
    p.model = model.trim().to_string();
    p.reasoning_effort = (!effort.trim().is_empty()).then(|| effort.trim().to_string());
    p.api_key = api_key
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .or_else(crate::settings::load_api_key)
        .unwrap_or_else(|| st.cfg.provider.api_key.clone());
    let client = crate::provider::Client::from_cfg(p.base_url.trim_end_matches('/'), &p)?;
    Ok((path, client, !p.api_key.is_empty()))
}

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
    ("mission", "<goal> | status | stop | resume", "Mission mode: works on the goal step by step, on its own, until it is done"),
    ("undo", "", "Put the files back as they were before the last run"),
    ("redo", "", "Re-apply what /undo put back"),
    ("init", "", "Write a starter AGENTS.md for this project"),
    ("export", "[path]", "Save this chat as Markdown in the project"),
];

/// Run `line` when it is one of `GUI_COMMANDS`. False = not one of them:
/// it goes to the agent (project commands are resolved by the core).
fn gui_command(gs: &mut GuiState, st: &Arc<server::AppState>, rt: &tokio::runtime::Handle, line: &str) -> bool {
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
            let report = st.usage_report(&sid);
            say(gs, report);
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
                say(gs, server::mission_report(st, &sid));
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
                let mission = crate::mission::Mission { kind: kind.unwrap_or(crate::mission::MissionKind::Custom), effort, goal };
                if gs.title() == NEW_CHAT {
                    let _ = st.db.set_session_title(&sid, &clip(&mission.goal, 40));
                    refresh_sessions(gs, st);
                }
                let (st2, sid2) = (st.clone(), sid.clone());
                if mission.kind.read_only() {
                    st.set_mode(&sid, crate::agents::AgentMode::Plan);
                    let brief = mission.briefing();
                    rt.spawn(async move { st2.kick_run(sid2, brief).await });
                } else {
                    let goal = mission.goal_for_mission_mode();
                    rt.spawn(async move { st2.kick_mission(sid2, Some(goal)).await });
                }
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
            let res = if name == "undo" {
                crate::gitsnap::undo(&root, &st.state_dir)
            } else {
                crate::gitsnap::redo(&root, &st.state_dir)
            };
            let text = match res {
                Ok(Some(m)) => format!("{} done: files are back as of snapshot {}.", if name == "undo" { "Undo" } else { "Redo" }, m.id),
                Ok(None) => format!("Nothing to {name} (or this project is not a git repository)."),
                Err(e) => format!("{name} failed: {e:#}"),
            };
            say(gs, text);
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

/// Files of the project whose path contains `query` (case-insensitive),
/// shortest paths first; `.gitignore` is respected.
fn find_project_files(root: &std::path::Path, query: &str, max: usize) -> Vec<String> {
    let q = query.to_lowercase().replace('\\', "/");
    let mut hits: Vec<String> = Vec::new();
    let walk = ignore::WalkBuilder::new(root).hidden(true).max_depth(Some(12)).build();
    for entry in walk.flatten().take(20_000) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if q.is_empty() || rel.to_lowercase().contains(&q) {
            hits.push(rel);
        }
    }
    hits.sort_by_key(|p| (p.len(), p.clone()));
    hits.truncate(max);
    hits
}

/// The page zoom from the page's `devicePixelRatio` and the screen's scale
/// (1 when the page sent none), rounded so float noise does not count.
fn page_zoom(dpr: f64, scale_factor: f64) -> f64 {
    if !(dpr.is_finite() && dpr > 0.0 && scale_factor > 0.0) {
        return 1.0;
    }
    ((dpr / scale_factor) * 100.0).round() / 100.0
}

fn load_zoom(state_dir: &std::path::Path) -> f64 {
    std::fs::read_to_string(state_dir.join("zoom.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|z| (0.25..=5.0).contains(z))
        .unwrap_or(1.0)
}

fn save_zoom(state_dir: &std::path::Path, z: f64) {
    let _ = std::fs::write(state_dir.join("zoom.txt"), format!("{z}"));
}

/// One line of work in the terminal panel's own tab.
#[derive(Debug, Clone, PartialEq)]
pub enum TermEntry {
    /// A command run as a background job (its output lives in `jobs`).
    Job(String),
    /// Text the app printed itself (`cd`, a command that could not start).
    Note(String),
}

/// Most output shown per command in the terminal tab, and per agent job.
const TERM_TAIL: usize = 8_000;
const JOB_TAIL: usize = 20_000;

/// The terminal panel's view: the folder, the shell, the user's tab (what
/// was typed and printed) and one tab per background job of the agent.
fn term_view(gs: &GuiState, cwd: &std::path::Path) -> serde_json::Value {
    let jobs = crate::tools::jobs::list(TERM_TAIL.max(JOB_TAIL));
    let tail = |s: &str, max: usize| -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut start = s.len() - max;
        while !s.is_char_boundary(start) {
            start += 1;
        }
        s[start..].to_string()
    };
    let mut text = String::new();
    let mut running = None;
    for e in gs.term_entries.iter().rev().take(30).collect::<Vec<_>>().into_iter().rev() {
        match e {
            TermEntry::Note(t) => text.push_str(t),
            TermEntry::Job(id) => {
                let Some(j) = jobs.iter().find(|j| &j.id == id) else { continue };
                text.push_str(&format!("$ {}\n", j.command));
                text.push_str(&tail(&j.tail, TERM_TAIL));
                if !text.ends_with('\n') && !text.is_empty() {
                    text.push('\n');
                }
                match &j.exit {
                    Some(how) => text.push_str(&format!("[{how}]\n")),
                    None => running = Some(j.id.clone()),
                }
            }
        }
    }
    let agent_jobs: Vec<serde_json::Value> = jobs
        .iter()
        .filter(|j| !j.by_user)
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "title": j.command,
                "running": j.exit.is_none(),
                "exit": j.exit,
                "text": tail(&j.tail, JOB_TAIL),
            })
        })
        .collect();
    let shell = match crate::tools::shell_kind() {
        crate::permissions::Shell::Posix if cfg!(windows) => "Git Bash",
        crate::permissions::Shell::Posix => "sh",
        crate::permissions::Shell::Cmd => "cmd",
    };
    serde_json::json!({
        "cwd": cwd.display().to_string(),
        "shell": shell,
        "text": text,
        "running": running,
        "jobs": agent_jobs,
    })
}

/// Send the terminal view when the panel is open and something changed.
fn send_term(webview: &wry::WebView, gs: &GuiState) {
    if !gs.term_open {
        return;
    }
    let cwd = gs.term_cwd.clone().unwrap_or_else(|| gs.term_root.borrow().clone());
    let json = term_view(gs, &cwd).to_string();
    if *gs.term_last.borrow() != json {
        let _ = webview.evaluate_script(&format!("window.showTerm&&window.showTerm({json})"));
        *gs.term_last.borrow_mut() = json;
    }
}

/// `cd`, `cd <dir>` (and `cd ~`, `/c/...`): the folder to switch to, or why
/// not. `None` = not a plain `cd` (it runs in the shell like any command).
fn cd_target(
    command: &str,
    cwd: &std::path::Path,
    root: &std::path::Path,
) -> Option<std::result::Result<std::path::PathBuf, String>> {
    let rest = command.strip_prefix("cd").filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))?;
    let arg = rest.trim().trim_matches(|c| c == '"' || c == '\'');
    if ["&&", "||", ";", "|"].iter().any(|op| arg.contains(op)) {
        return None;
    }
    let dir = if arg.is_empty() {
        root.to_path_buf()
    } else if arg == "~" || arg.starts_with("~/") {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(std::path::PathBuf::from)?;
        home.join(arg.trim_start_matches('~').trim_start_matches('/'))
    } else {
        let arg = crate::tools::msys_to_windows(arg).unwrap_or_else(|| arg.to_string());
        cwd.join(arg)
    };
    Some(match std::fs::canonicalize(&dir) {
        Ok(d) if d.is_dir() => Ok(strip_verbatim(d)),
        _ => Err(format!("cd: no such folder: {}", dir.display())),
    })
}

/// `\\?\C:\x` (from canonicalize on Windows) -> `C:\x`.
fn strip_verbatim(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => std::path::PathBuf::from(rest),
        _ => p,
    }
}

/// Branch name and number of changed files, when `root` is in a git repo.
fn git_state(root: &std::path::Path) -> Option<(String, usize)> {
    let branch = crate::gitsnap::git(root, &["branch", "--show-current"]).ok()?;
    let branch = branch.trim();
    let branch = if branch.is_empty() { "detached".to_string() } else { branch.to_string() };
    let changed = crate::gitsnap::git(root, &["status", "--porcelain", "--", "."])
        .map(|s| s.lines().filter(|l| !l[3.min(l.len())..].starts_with(".opencode/")).count())
        .unwrap_or(0);
    Some((branch, changed))
}

/// The folder of the open chat's project (the start folder without one).
fn chat_root(gs: &GuiState, st: &server::AppState) -> std::path::PathBuf {
    gs.current_project()
        .and_then(|id| gs.projects.iter().find(|p| p.id == id).map(|p| std::path::PathBuf::from(&p.root)))
        .unwrap_or_else(|| st.workspace.clone())
}

/// Files picked in the Windows open dialog (several allowed).
#[cfg(windows)]
fn pick_files(window: &tao::window::Window) -> Vec<std::path::PathBuf> {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH,
    };
    let mut out = Vec::new();
    // SAFETY: plain COM calls on the UI thread; each returned string is
    // copied and then freed with CoTaskMemFree as the API requires.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(dialog) = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) else {
            return out;
        };
        let Ok(options) = dialog.GetOptions() else { return out };
        if dialog.SetOptions(options | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM).is_err() {
            return out;
        }
        if dialog.Show(Some(HWND(window.hwnd() as _))).is_err() {
            return out; // cancelled
        }
        let Ok(items) = dialog.GetResults() else { return out };
        let n = items.GetCount().unwrap_or(0);
        for i in 0..n {
            let Ok(item) = items.GetItemAt(i) else { continue };
            let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) else { continue };
            if let Ok(p) = name.to_string() {
                out.push(std::path::PathBuf::from(p));
            }
            CoTaskMemFree(Some(name.0 as *const _));
        }
    }
    out
}

#[cfg(not(windows))]
fn pick_files(_window: &tao::window::Window) -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// Closing stops every run: ask first while one is going.
#[cfg(windows)]
fn confirm_close(window: &tao::window::Window, gs: &GuiState) -> bool {
    use tao::platform::windows::WindowExtWindows;
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONWARNING, MB_YESNO};
    if !gs.busy && gs.running.is_empty() {
        return true;
    }
    // SAFETY: a modal message box owned by our own window, on the UI thread.
    let answer = unsafe {
        MessageBoxW(
            Some(HWND(window.hwnd() as _)),
            w!("An agent is still working. Close anyway? Its run will stop."),
            w!("Habi"),
            MB_YESNO | MB_ICONWARNING,
        )
    };
    answer == IDYES
}

#[cfg(not(windows))]
fn confirm_close(_window: &tao::window::Window, _gs: &GuiState) -> bool {
    true
}

/// A Windows notification (a tray balloon, shown as a toast on Windows 10
/// and 11). The tray icon is added on first use and removed by
/// `notify_cleanup` when the app exits.
#[cfg(windows)]
fn notify(window: &tao::window::Window, title: &str, body: &str) {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};
    fn fill(dst: &mut [u16], s: &str) {
        let n = dst.len() - 1;
        for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(n)) {
            *d = c;
        }
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(window.hwnd() as _),
        uID: 1,
        uFlags: NIF_ICON | NIF_TIP | NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    // SAFETY: a stock system icon; no module handle involved.
    nid.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    fill(&mut nid.szTip, "Habi");
    fill(&mut nid.szInfoTitle, title);
    fill(&mut nid.szInfo, body);
    let first = !TRAY_ADDED.swap(true, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: `nid` is fully initialised and outlives the call.
    unsafe {
        let _ = Shell_NotifyIconW(if first { NIM_ADD } else { NIM_MODIFY }, &nid);
    }
}

#[cfg(windows)]
static TRAY_ADDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
fn notify_cleanup(window: &tao::window::Window) {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIM_DELETE, NOTIFYICONDATAW};
    if !TRAY_ADDED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(window.hwnd() as _),
        uID: 1,
        ..Default::default()
    };
    // SAFETY: removes the icon `notify` added for this window.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

#[cfg(not(windows))]
fn notify(_window: &tao::window::Window, _title: &str, _body: &str) {}

#[cfg(not(windows))]
fn notify_cleanup(_window: &tao::window::Window) {}

/// What to say in a notification for an event that waits on the user (or a
/// finished run), when the window is not in front.
fn notice_for(ev: &ServerEvent) -> Option<(&'static str, String)> {
    match ev {
        ServerEvent::ApprovalNeeded { tool, .. } => Some(("Habi needs your OK", format!("Approve or deny: {tool}"))),
        ServerEvent::Question { question, .. } => Some(("Habi has a question", clip(question, 200))),
        ServerEvent::RunFinished { ok, message, .. } => Some((
            if *ok { "Habi finished" } else { "Habi stopped" },
            message.as_deref().map(|m| clip(m, 200)).unwrap_or_else(|| "The run is done.".into()),
        )),
        _ => None,
    }
}

/// Code editors that can open a folder, found on this machine: (name, program).
fn find_editors() -> Vec<(&'static str, std::path::PathBuf)> {
    let mut found = Vec::new();
    let candidates: &[(&str, &[&str])] = &[
        ("VS Code", &["code.cmd", "code"]),
        ("Cursor", &["cursor.cmd", "cursor"]),
        ("Zed", &["zed.exe", "zed"]),
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    for (name, files) in candidates {
        let hit = std::env::split_paths(&path).find_map(|dir| files.iter().map(|f| dir.join(f)).find(|p| p.is_file()));
        if let Some(p) = hit {
            found.push((*name, p));
        }
    }
    found
}

/// Open `folder` in the editor called `name` (or the first one found).
fn open_in_editor(name: Option<&str>, folder: &std::path::Path) -> Result<&'static str, String> {
    let editors = find_editors();
    let (label, program) = editors
        .iter()
        .find(|(n, _)| name.is_none_or(|want| want == *n))
        .or(editors.first())
        .ok_or("no VS Code, Cursor or Zed found on PATH")?;
    let mut cmd = if program.extension().is_some_and(|e| e == "cmd") {
        // A .cmd launcher runs through cmd.exe.
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(program).arg(folder);
        c
    } else {
        let mut c = std::process::Command::new(program);
        c.arg(folder);
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash
    }
    cmd.spawn().map(|_| *label).map_err(|e| format!("could not start {label}: {e}"))
}

/// Refresh the sidebar chat list from the database.
fn refresh_sessions(gs: &mut GuiState, st: &server::AppState) {
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
fn load_session(gs: &mut GuiState, st: &server::AppState, id: &str) {
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

/// New (F-13): hold an exclusive lock on `<state_dir>/gui.lock` while the
/// window is open. The OS drops it when the process ends, even after a crash.
/// Needs Rust 1.89 or newer (`File::try_lock`).
fn single_instance_lock(state_dir: &std::path::Path) -> std::result::Result<std::fs::File, String> {
    let _ = std::fs::create_dir_all(state_dir);
    let path = state_dir.join("gui.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err("Another Habi window is already open. Use that one, or close it first.".to_string())
        }
        Err(std::fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

/// New (F-13): show a fatal error to a user who may have no console window.
pub fn show_fatal_dialog(title: &str, msg: &str) {
    eprintln!("[gui] {title}: {msg}");
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        // SAFETY: a plain Win32 call with valid wide strings that outlive it.
        unsafe {
            MessageBoxW(None, &HSTRING::from(msg), &HSTRING::from(title), MB_OK | MB_ICONERROR);
        }
    }
}

/// Submit the composer and name the chat after its first message. Shared by
/// the CLI prefill and the typed-send path so a prefilled launch names its
/// chat identically (it used to skip this and stay "New chat"). Returns the
/// content to run, or None when there was nothing to send.
fn submit_and_name(gs: &mut GuiState, st: &server::AppState) -> Option<String> {
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
async fn request_stop(st: &server::AppState, session_id: &str) -> bool {
    for _ in 0..10 {
        if let Some(flag) = st.busy.read().await.get(session_id) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
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
            config_path: crate::config::default_config_path().display().to_string(),
        };
    }
    gs.approval_timeout_secs = state.cfg.permissions.approval_timeout_secs;
    gs.perm_mode = state.perm_mode();
    refresh_sessions(&mut gs, &state);
    tracing_boot(boot_ms, addr, &session.id);
    if let Some(p) = prompt {
        gs.input = p;
    }

    let rt = tokio::runtime::Handle::current();

    let event_loop: EventLoop<AppEvent> =
        tao::event_loop::EventLoopBuilder::<AppEvent>::with_user_event().build();
    // No OS decorations: the page draws its own titlebar (drag region +
    // min/max/close buttons over IPC). Start maximized so the composer is
    // never shoved off-screen on small displays.
    let window = WindowBuilder::new()
        .with_title("Habi \u{2014} agent workspace")
        .with_decorations(false)
        .with_maximized(true)
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_min_inner_size(LogicalSize::new(900.0, 560.0))
        .build(&event_loop)
        .map_err(|e| anyhow::anyhow!("creating the app window failed: {e}"))?;
    // Breadcrumbs: a window that dies between these lines used to leave no
    // trace at all (exit code 1, empty stderr). Now the last line printed says
    // how far it got.
    eprintln!("[gui] window created");

    // Page -> Rust, plus preview-webview callbacks. One channel means one
    // drain site in the event loop, so state never updates twice per frame.
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiMsg>();
    let ipc_proxy = event_loop.create_proxy();
    let page_tx = ui_tx.clone();
    let webview = wry::WebViewBuilder::new()
        .with_background_color((7, 10, 18, 255))
        // S-18: DevTools only in debug builds (`cargo run`), never in release.
        .with_devtools(cfg!(debug_assertions))
        .with_initialization_script(
            "window.addEventListener('error', (e) => {\n\
             \x20 try { window.ipc.postMessage(JSON.stringify({ type: 'page_error', msg: e.message })) } catch (_) {}\n\
             \x20});",
        )
        .with_html(APP_HTML)
        // S-18: this window only ever shows our own page; refuse every other
        // navigation, so no link or script can swap the app for a page that
        // would then get our IPC channel.
        .with_navigation_handler(main_window_navigation_guard())
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            if let Some(msg) = parse_ipc(req.body()) {
                let _ = page_tx.send(UiMsg::Ipc(msg));
                let _ = ipc_proxy.send_event(AppEvent::Wake);
            } else {
                eprintln!("[gui] IPC: {}", req.body());
            }
        })
        .build(&window)
        .map_err(|e| {
            anyhow::anyhow!(
                "opening the app window failed: {e}\n\
                 On Windows this needs the Microsoft Edge WebView2 Runtime \
                 (preinstalled on Windows 11; a small slice of Windows 10 \
                 machines need the free 'WebView2 Runtime' download)."
            )
        })?;
    // The interface zoom (Ctrl+wheel, Ctrl+= / Ctrl+-) from last time.
    let mut page_zoom_now = load_zoom(&state.state_dir);
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
        let allow_remote = state.cfg.browser.allow_remote;
        let nav_tx = ui_tx.clone();
        let nav_proxy = event_loop.create_proxy();
        let eval_tx = ui_tx.clone();
        let eval_proxy = event_loop.create_proxy();
        let eval_ids = bridge_ids.clone();
        let title_tx = ui_tx.clone();
        let title_proxy = event_loop.create_proxy();
        let load_tx = ui_tx.clone();
        let load_proxy = event_loop.create_proxy();
        let built = wry::WebViewBuilder::new()
            .with_background_color((7, 10, 18, 255))
            .with_visible(false)
            // S-18: no `with_clipboard(true)`: in wry 0.57 on Windows it lets
            // every page in the panel read the clipboard without asking.
            .with_bounds(PanelRect::fallback().to_wry())
            // Goal B: the page-side primitives (snapshot/click/type/wait) and
            // the channel their results come back on.
            .with_initialization_script(crate::browser::RUNTIME_JS)
            .with_ipc_handler(move |req: wry::http::Request<String>| {
                // S-12: only a reply to a request we are waiting for may pass,
                // each id at most once, and never an oversized frame.
                let body = req.body();
                if body.len() > MAX_EVAL_FRAME_BYTES {
                    return;
                }
                let Some((id, _)) = crate::browser::parse_reply(body) else {
                    return;
                };
                let expected = eval_ids.lock().map(|mut set| set.remove(&id)).unwrap_or(false);
                if !expected {
                    return;
                }
                let _ = eval_tx.send(UiMsg::Browser(BrowserEvt::Eval(body.clone())));
                let _ = eval_proxy.send_event(AppEvent::Wake);
            })
            .with_navigation_handler(move |url: String| {
                // S-11: links, redirects and page scripts follow the same rule
                // as typed URLs, including `allow_remote = false`.
                let verdict = check_navigation(&url, allow_remote);
                let allowed = verdict.is_ok();
                match verdict {
                    Ok(()) => {
                        let _ = nav_tx.send(UiMsg::Browser(BrowserEvt::Url(url)));
                    }
                    Err(msg) => {
                        let _ = nav_tx.send(UiMsg::Browser(BrowserEvt::Blocked(msg)));
                    }
                }
                let _ = nav_proxy.send_event(AppEvent::Wake);
                allowed
            })
            .with_document_title_changed_handler(move |title: String| {
                let _ = title_tx.send(UiMsg::Browser(BrowserEvt::Title(title)));
                let _ = title_proxy.send_event(AppEvent::Wake);
            })
            .with_on_page_load_handler(move |ev: wry::PageLoadEvent, _url: String| {
                let loading = matches!(ev, wry::PageLoadEvent::Started);
                let _ = load_tx.send(UiMsg::Browser(BrowserEvt::Loading(loading)));
                let _ = load_proxy.send_event(AppEvent::Wake);
            })
            .build_as_child(&window);
        match built {
            Ok(wv) => Some(wv),
            Err(e) => {
                // The chat must still work when the preview cannot be created.
                eprintln!("[gui] preview panel unavailable: {e}");
                gs.browser.fail(format!("Preview panel unavailable: {e}"));
                None
            }
        }
    } else {
        eprintln!("[gui] preview panel disabled in config ([browser] enabled = false)");
        None
    };

    // Goal B: publish a handle so agent tools can drive this webview. The
    // waker is what turns a queued command into an event-loop wake-up.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<crate::browser::Request>();
    if browser_enabled {
        let proxy = event_loop.create_proxy();
        let waker: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
            let _ = proxy.send_event(AppEvent::Wake);
        });
        if let Ok(mut slot) = state.browser.lock() {
            *slot = Some(crate::browser::Handle::new(cmd_tx, waker));
        }
    }
    let mut bridge = Bridge::new();

    // Rust -> page: server events wake the loop, then we drain + push.
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel::<ServerEvent>();
    {
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
    }

    // Diagnostic hook: `OPENCODE_RUST_PREVIEW_SMOKE=<url>` opens the panel on
    // that URL at boot and logs every webview callback. It exists because the
    // child view can only be verified by a human clicking the window
    // otherwise — this makes "does a real page actually render?" testable.
    if let (Some(wv), Ok(raw)) = (&browser_webview, std::env::var("OPENCODE_RUST_PREVIEW_SMOKE")) {
        // S-11: the diagnostic hook obeys the same rule as the URL bar.
        let checked = normalize_url(&raw)
            .and_then(|url| check_navigation(&url, state.cfg.browser.allow_remote).map(|()| url));
        match checked {
            Ok(url) => {
                let rect = PanelRect { x: 800.0, y: 80.0, w: 440.0, h: 700.0 };
                gs.browser.rect = Some(rect);
                gs.browser.open(&url);
                let _ = wv.set_bounds(rect.to_wry());
                let _ = wv.set_visible(true);
                let _ = wv.load_url(&url);
                eprintln!("[preview-smoke] loading {url}");
            }
            Err(e) => eprintln!("[preview-smoke] refused: {e}"),
        }
    }

    // First paint (the page also asks for one via IpcMsg::Ready once loaded),
    // then an optional prefilled prompt from the CLI.
    paint(&webview, &gs);
    if let Some(content) = submit_and_name(&mut gs, &state) {
        eprintln!("[gui] run start (prefill): {}", clip(&content, 80));
        let sid = gs.session.clone().unwrap_or_default();
        paint(&webview, &gs);
        let st0 = state.clone();
        rt.spawn(async move {
            st0.kick_run(sid, content).await;
        });
    }

    eprintln!("[gui] entering the event loop");
    let st = state.clone();
    // B-11: the `covered` value the native view got last time.
    let mut covered_applied = false;
    event_loop.run(move |event, _, control_flow| {
        // While a modal countdown is on screen the loop has to wake up on its
        // own; otherwise a heartbeat-less idle window freezes the timer.
        // Never overwrite `Exit`: every callback starts here, so resetting it
        // would strand a windowless process after the user closes the window.
        if !matches!(*control_flow, ControlFlow::Exit) {
            let term_live = gs.term_open && crate::tools::jobs::list(0).iter().any(|j| j.exit.is_none());
            *control_flow = if gs.approval.is_some() || bridge.busy() || term_live {
                ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(500))
            } else {
                ControlFlow::Wait
            };
        }
        let mut changed = false;
        match event {
            Event::UserEvent(AppEvent::Wake) | Event::MainEventsCleared => {
                while let Ok(ev) = evt_rx.try_recv() {
                    // Something waits on the user (or a run ended) while they
                    // look elsewhere: flash the taskbar button (it does not
                    // steal focus) and show a notification.
                    if !window.is_focused() {
                        if let Some((title, body)) = notice_for(&ev) {
                            if !matches!(ev, ServerEvent::RunFinished { .. }) {
                                window.request_user_attention(Some(tao::window::UserAttentionType::Critical));
                            }
                            notify(&window, title, &body);
                        }
                    }
                    gs.on_event(ev);
                    changed = true;
                }
                // B-12: another client added or switched a chat.
                if gs.sessions_stale {
                    gs.sessions_stale = false;
                    refresh_sessions(&mut gs, &st);
                }
                // A running command's new output reaches an open terminal.
                send_term(&webview, &gs);
                if gs.git_stale {
                    gs.git_stale = false;
                    *gs.term_root.borrow_mut() = chat_root(&gs, &st);
                    gs.git = git_state(&chat_root(&gs, &st));
                    changed = true;
                }
                while let Ok(msg) = ui_rx.try_recv() {
                    match msg {
                        UiMsg::Ipc(IpcMsg::Send(text)) if gui_command(&mut gs, &st, &rt, &text) => {
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Send(text)) => {
                            gs.question = None; // answered (or passed over)
                            gs.followups.clear();
                            gs.input = text;
                            if let Some(content) = submit_and_name(&mut gs, &st) {
                                eprintln!("[gui] run start: {}", clip(&content, 80));
                                let sid = gs.session.clone().unwrap_or_default();
                                let st = st.clone();
                                rt.spawn(async move {
                                    st.kick_run(sid, content).await;
                                });
                                changed = true;
                            }
                        }
                        // "New chat" opens a chat in the open chat's project.
                        UiMsg::Ipc(IpcMsg::NewSession) => {
                            let project = gs.current_project();
                            open_new_chat(&mut gs, &st, project);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::NewInProject(project)) => {
                            open_new_chat(&mut gs, &st, Some(project));
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::AddProject(path)) => {
                            let folder = match path {
                                Some(p) => Some(std::path::PathBuf::from(p)),
                                None => pick_folder(&window),
                            };
                            if let Some(folder) = folder {
                                if folder.is_dir() {
                                    match st.db.upsert_project(&folder) {
                                        Ok(p) => open_new_chat(&mut gs, &st, Some(p.id)),
                                        Err(e) => gs.push(EntryKind::Error, format!("add project failed: {e}")),
                                    }
                                } else {
                                    gs.push(EntryKind::Error, format!("not a folder: {}", folder.display()));
                                }
                                changed = true;
                            }
                        }
                        UiMsg::Ipc(IpcMsg::RemoveProject(id)) => {
                            if let Err(e) = st.db.remove_project(&id) {
                                gs.push(EntryKind::Error, format!("remove project failed: {e}"));
                            }
                            refresh_sessions(&mut gs, &st);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Pin { id, on }) => {
                            let _ = st.db.set_session_pinned(&id, on);
                            refresh_sessions(&mut gs, &st);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Archive { id, on }) => {
                            // A chat with a run going stays until the run ends.
                            if !(on && gs.running.contains(&id)) {
                                let _ = st.db.set_session_archived(&id, on);
                                if on && gs.is_current(&id) {
                                    let project = gs.current_project();
                                    open_new_chat(&mut gs, &st, project);
                                }
                            }
                            refresh_sessions(&mut gs, &st);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Rename { id, title }) => {
                            let _ = st.db.set_session_title(&id, &title);
                            refresh_sessions(&mut gs, &st);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::MoveChat { id, project }) => {
                            if !gs.running.contains(&id) {
                                let _ = st.db.set_session_project(&id, project.as_deref());
                            }
                            refresh_sessions(&mut gs, &st);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::AdoptUnfiled(project)) => {
                            if let Err(e) = st.db.adopt_unfiled_sessions(&project) {
                                gs.push(EntryKind::Error, format!("move chats failed: {e}"));
                            }
                            refresh_sessions(&mut gs, &st);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::DeleteChat(id)) => {
                            if !gs.running.contains(&id) {
                                if let Err(e) = st.db.delete_session(&id) {
                                    gs.push(EntryKind::Error, format!("delete failed: {e}"));
                                } else if gs.is_current(&id) {
                                    gs.session = None;
                                    let project = gs.default_project.clone();
                                    open_new_chat(&mut gs, &st, project);
                                }
                            }
                            refresh_sessions(&mut gs, &st);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Load(id)) => {
                            if gs.session.as_deref() != Some(id.as_str()) {
                                gs.question = None;
                                load_session(&mut gs, &st, &id);
                            }
                            changed = true;
                        }
                        // The page just finished loading: give it the state.
                        // B-37: also when the page asks for a full resend.
                        UiMsg::Ipc(IpcMsg::Ready) | UiMsg::Ipc(IpcMsg::Resync) => {
                            gs.resync();
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::PageError(msg)) => {
                            eprintln!("[gui] page error: {msg}");
                        }
                        UiMsg::Ipc(IpcMsg::Minimize) => window.set_minimized(true),
                        UiMsg::Ipc(IpcMsg::MaximizeToggle) => {
                            let maxed = window.is_maximized();
                            window.set_maximized(!maxed);
                        }
                        UiMsg::Ipc(IpcMsg::Close) => {
                            if confirm_close(&window, &gs) {
                                notify_cleanup(&window);
                                *control_flow = ControlFlow::Exit;
                            }
                        }
                        UiMsg::Ipc(IpcMsg::Stop) => {
                            // F-06: the same abort as the HTTP route.
                            if let Some(sid) = gs.session.clone() {
                                gs.push(EntryKind::System, "Stopping…".to_string());
                                let st = st.clone();
                                rt.spawn(async move {
                                    if !request_stop(&st, &sid).await {
                                        eprintln!("[gui] stop: no run found for {sid}");
                                    }
                                });
                                changed = true;
                            }
                        }
                        UiMsg::Ipc(IpcMsg::OpenInEditor(editor)) => {
                            let root = chat_root(&gs, &st);
                            let text = match open_in_editor(editor.as_deref(), &root) {
                                Ok(name) => format!("Opened {} in {name}.", root.display()),
                                Err(e) => format!("Open in editor: {e}"),
                            };
                            gs.push(EntryKind::System, text);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::SaveSettings { base_url, model, api_key, effort }) => {
                            let text = match save_settings(&st, &base_url, &model, api_key.as_deref(), &effort) {
                                Ok((path, client, has_key)) => {
                                    let st2 = st.clone();
                                    rt.spawn(async move {
                                        *st2.client.lock().await = client;
                                    });
                                    gs.settings.base_url = base_url;
                                    gs.settings.model = model;
                                    gs.settings.reasoning_effort = effort;
                                    gs.settings.has_key = has_key;
                                    gs.settings.needs_setup = false;
                                    format!(
                                        "Settings saved to {}. The new model is used from the next message; \
                                         a few options (small model, limits) apply after a restart.",
                                        path.display()
                                    )
                                }
                                Err(e) => format!("Settings not saved: {e:#}"),
                            };
                            gs.push(EntryKind::System, text);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::FindFiles(query)) => {
                            let items = find_project_files(&chat_root(&gs, &st), &query, 12);
                            gs.file_matches = (query, items);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::TermOpen(open)) => {
                            gs.term_open = open;
                            *gs.term_root.borrow_mut() = chat_root(&gs, &st);
                            gs.term_last.borrow_mut().clear(); // send the view again when shown
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::TermRun(command)) => {
                            let cwd = gs.term_cwd.clone().unwrap_or_else(|| chat_root(&gs, &st));
                            if let Some(target) = cd_target(&command, &cwd, &chat_root(&gs, &st)) {
                                let note = match target {
                                    Ok(dir) => {
                                        let text = format!("$ {command}\n{}\n", dir.display());
                                        gs.term_cwd = Some(dir);
                                        text
                                    }
                                    Err(e) => format!("$ {command}\n{e}\n"),
                                };
                                gs.term_entries.push(TermEntry::Note(note));
                            } else {
                                // Jobs read their pipes on tokio tasks.
                                let _guard = rt.enter();
                                match crate::tools::jobs::spawn(&command, &cwd, true) {
                                    Ok(id) => gs.term_entries.push(TermEntry::Job(id)),
                                    Err(e) => gs.term_entries.push(TermEntry::Note(format!("$ {command}\ncould not start: {e:#}\n"))),
                                }
                            }
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::TermStop(id)) => {
                            let running = crate::tools::jobs::list(0);
                            let target = id.or_else(|| {
                                running.iter().rev().find(|j| j.by_user && j.exit.is_none()).map(|j| j.id.clone())
                            });
                            if let Some(id) = target {
                                rt.spawn(async move {
                                    let _ = crate::tools::jobs::stop(&id).await;
                                });
                            }
                        }
                        UiMsg::Ipc(IpcMsg::Rewind { text, skip }) => {
                            let sid = gs.session.clone().unwrap_or_default();
                            let note = if gs.busy || gs.running.contains(&sid) {
                                "Stop the run first, then rewind.".to_string()
                            } else {
                                match server::rewind_chat(&st, &sid, &text, skip) {
                                    Ok(r) => {
                                        load_session(&mut gs, &st, &sid);
                                        let shown = text.trim().trim_start_matches('\u{203a}').trim().to_string();
                                        gs.compose = (gs.compose.0 + 1, shown);
                                        let files = match (r.undone, &r.problem) {
                                            (0, _) => "no file changes to put back".to_string(),
                                            (n, None) => format!("files put back to before it ({n} undo step(s); /redo brings them back)"),
                                            (n, Some(p)) => format!("{n} undo step(s), but: {p}"),
                                        };
                                        format!("Rewound: {} message(s) removed, {files}. Your message is back in the box to edit.", r.messages)
                                    }
                                    Err(e) => format!("Could not rewind: {e:#}"),
                                }
                            };
                            gs.push(EntryKind::System, note);
                            gs.git_stale = true;
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::TermClear) => {
                            gs.term_entries.clear();
                            crate::tools::jobs::clear_user_jobs();
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::NewBranch(name)) => {
                            let root = chat_root(&gs, &st);
                            let valid = !name.is_empty()
                                && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
                                && !name.starts_with('-');
                            let text = if !valid {
                                format!("\"{name}\" is not a usable branch name (letters, digits, - _ / . only)")
                            } else {
                                match crate::gitsnap::git(&root, &["switch", "-c", &name]) {
                                    Ok(_) => format!("Now on a new branch: {name}"),
                                    Err(e) => format!("Could not make the branch: {e:#}"),
                                }
                            };
                            gs.push(EntryKind::System, text);
                            gs.git_stale = true;
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::LoadChanges) => {
                            let root = chat_root(&gs, &st);
                            let json = match crate::changes::collect(&root) {
                                Ok(files) => serde_json::json!({ "files": files, "root": root.display().to_string() }),
                                Err(e) => serde_json::json!({ "error": format!("{e:#}"), "files": [] }),
                            };
                            *gs.changes_out.borrow_mut() = Some(json.to_string());
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::ReviewComments(comments)) => {
                            if !comments.is_empty() {
                                gs.input = crate::changes::comments_message(&comments);
                                gs.question = None;
                                if let Some(content) = submit_and_name(&mut gs, &st) {
                                    let st2 = st.clone();
                                    let sid = gs.session.clone().unwrap_or_default();
                                    rt.spawn(async move {
                                        st2.kick_run(sid, content).await;
                                    });
                                }
                                changed = true;
                            }
                        }
                        UiMsg::Ipc(IpcMsg::PickFiles) => {
                            let root = chat_root(&gs, &st);
                            let mut markers = Vec::new();
                            for file in pick_files(&window) {
                                match crate::attachments::copy_in(&root, &file) {
                                    Ok(p) => markers.push(crate::attachments::marker(&p)),
                                    Err(e) => gs.push(EntryKind::System, format!("Not attached: {e:#}")),
                                }
                            }
                            if !markers.is_empty() {
                                gs.attach = (gs.attach.0 + 1, markers);
                            }
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::SaveAttachment { name, data }) => {
                            use base64::Engine;
                            let root = chat_root(&gs, &st);
                            let saved = base64::engine::general_purpose::STANDARD
                                .decode(data.split_once(',').map_or(data.as_str(), |(_, b)| b))
                                .map_err(anyhow::Error::from)
                                .and_then(|bytes| crate::attachments::save_bytes(&root, &name, &bytes));
                            match saved {
                                Ok(p) => gs.attach = (gs.attach.0 + 1, vec![crate::attachments::marker(&p)]),
                                Err(e) => gs.push(EntryKind::System, format!("Not attached: {e:#}")),
                            }
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Export) => {
                            if let Some(sid) = gs.session.clone() {
                                let text = match st.export_transcript(&sid, None) {
                                    Ok(path) => format!("Chat exported to {}", path.display()),
                                    Err(e) => format!("Export failed: {e:#}"),
                                };
                                gs.push(EntryKind::System, text);
                                changed = true;
                            }
                        }
                        UiMsg::Ipc(IpcMsg::ListModels) => {
                            let st2 = st.clone();
                            rt.spawn(async move {
                                let client = st2.client.lock().await.clone();
                                let available = client.list_models().await.unwrap_or_default();
                                let _ = st2.events.send(ServerEvent::ModelInfo { current: client.model.clone(), available });
                            });
                        }
                        UiMsg::Ipc(IpcMsg::AgentStop(id)) => {
                            crate::subagent::stop(&id);
                        }
                        UiMsg::Ipc(IpcMsg::Approve { id, always }) => {
                            let answered = st.decide(&id, true, always);
                            settle_approval(&mut gs, &id, answered);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Deny(id)) => {
                            let answered = st.decide(&id, false, false);
                            settle_approval(&mut gs, &id, answered);
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Mode(mode)) => {
                            // The core owns the mode; the page just picks it.
                            let granted = st.set_perm_mode(mode);
                            gs.perm_mode = mode;
                            // Switching to turbo answers whatever was blocking,
                            // so no modal (shown or queued) may outlive the grant.
                            for id in &granted {
                                gs.finish_approval(id);
                            }
                            eprintln!(
                                "[gui] permission mode -> {} ({} prompt(s) released)",
                                mode.as_str(),
                                granted.len()
                            );
                            gs.push(EntryKind::System, mode.note());
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::Browse(raw)) => {
                            // S-11: typed URLs pass the same check as links,
                            // redirects, the dev-server button and the agent.
                            let target = normalize_url(&raw).and_then(|url| {
                                check_navigation(&url, st.cfg.browser.allow_remote).map(|()| url)
                            });
                            match target {
                                Ok(url) => match &browser_webview {
                                    Some(wv) => {
                                        gs.browser.open(&url);
                                        show_browser(wv, &gs.browser);
                                        if let Err(e) = wv.load_url(&url) {
                                            gs.browser.fail(format!("navigate failed: {e}"));
                                            show_browser(wv, &gs.browser);
                                        }
                                    }
                                    None => gs.browser.fail(if browser_enabled {
                                        "Preview panel unavailable on this system."
                                    } else {
                                        "Preview panel is disabled in config ([browser] enabled = false)."
                                    }),
                                },
                                Err(e) => {
                                    gs.browser.fail(e);
                                    if let Some(wv) = &browser_webview {
                                        show_browser(wv, &gs.browser);
                                    }
                                }
                            }
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::NavBack) => {
                            if let Some(wv) = &browser_webview {
                                let _ = wv.go_back();
                                sync_history(wv, &mut gs.browser);
                            }
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::NavFwd) => {
                            if let Some(wv) = &browser_webview {
                                let _ = wv.go_forward();
                                sync_history(wv, &mut gs.browser);
                            }
                            changed = true;
                        }
                        UiMsg::Ipc(IpcMsg::NavReload) => {
                            if let Some(wv) = &browser_webview {
                                let _ = wv.reload();
                            }
                            changed = true;
                        }
                        // Geometry/no-paint verbs: the page already knows its
                        // own layout, so these must not trigger a full repaint.
                        UiMsg::Ipc(IpcMsg::PanelRect(rect, dpr)) => {
                            let z = page_zoom(dpr, window.scale_factor());
                            if (z - page_zoom_now).abs() > 0.001 {
                                page_zoom_now = z;
                                save_zoom(&st.state_dir, z);
                            }
                            gs.browser.rect = Some(rect.scaled(z));
                            if let Some(wv) = &browser_webview {
                                show_browser(wv, &gs.browser);
                            }
                        }
                        UiMsg::Ipc(IpcMsg::PanelVisible(visible)) => {
                            gs.browser.visible = visible;
                            gs.browser.user_closed = !visible;
                            if let Some(wv) = &browser_webview {
                                show_browser(wv, &gs.browser);
                            }
                        }
                        UiMsg::Ipc(IpcMsg::OpenDevServer(url)) => {
                            let target = url
                                .or_else(|| gs.dev_server.clone())
                                .or_else(|| {
                                    let home = gs.browser.home.clone();
                                    if home.is_empty() { None } else { Some(home) }
                                })
                                .unwrap_or_else(|| "http://localhost:3000".into());
                            // S-11: the button obeys `allow_remote` too.
                            let checked = normalize_url(&target).and_then(|url| {
                                check_navigation(&url, st.cfg.browser.allow_remote).map(|()| url)
                            });
                            match checked {
                                Ok(u) => {
                                    if let Some(wv) = &browser_webview {
                                        gs.browser.open(&u);
                                        show_browser(wv, &gs.browser);
                                        if let Err(e) = wv.load_url(&u) {
                                            gs.browser.fail(format!("navigate failed: {e}"));
                                            show_browser(wv, &gs.browser);
                                        }
                                    } else {
                                        gs.browser.fail(if browser_enabled {
                                            "Preview panel unavailable on this system."
                                        } else {
                                            "Preview panel is disabled in config ([browser] enabled = false)."
                                        });
                                    }
                                }
                                Err(e) => {
                                    gs.browser.fail(e);
                                    if let Some(wv) = &browser_webview {
                                        show_browser(wv, &gs.browser);
                                    }
                                }
                            }
                            changed = true;
                        }
                        UiMsg::Browser(BrowserEvt::Eval(raw)) => {
                            // Page -> Rust result frame for an agent tool call.
                            bridge.on_eval(&raw);
                        }
                        UiMsg::Browser(ev) => {
                            if std::env::var_os("OPENCODE_RUST_PREVIEW_SMOKE").is_some() {
                                eprintln!("[preview-smoke] {ev:?}");
                            }
                            let loaded = matches!(ev, BrowserEvt::Loading(false));
                            // B-36: track the start of a load, and refusals.
                            if matches!(ev, BrowserEvt::Loading(true)) {
                                bridge.on_load_started();
                            }
                            if let BrowserEvt::Blocked(msg) = &ev {
                                bridge.on_blocked(msg);
                            }
                            gs.on_browser_event(ev);
                            // A pending `browser_navigate` resolves on load.
                            if loaded {
                                bridge.on_loaded(&gs.browser);
                            }
                            if let Some(wv) = &browser_webview {
                                sync_history(wv, &mut gs.browser);
                                show_browser(wv, &gs.browser);
                            }
                            changed = true;
                        }
                    }
                }
                // Goal B: agent tools ask this loop to act on the webview.
                while let Ok(req) = cmd_rx.try_recv() {
                    bridge.start(
                        &browser_webview,
                        req,
                        &mut gs.browser,
                        st.cfg.browser.allow_remote,
                    );
                    changed = true;
                }
                // Requests that never get an answer must not hang the run.
                bridge.expire();
                // S-12: after start/expire/answers, publish which ids may reply.
                bridge.publish_ids(&bridge_ids);
                // B-13: a modal whose timeout passed long ago cannot block the
                // window forever, even if its "resolved" event was lost.
                if gs.expire_stale_approval(Instant::now()) {
                    changed = true;
                }
                // A live countdown forces a repaint on every timed wake.
                if gs.approval.is_some() {
                    changed = true;
                }
                // B-11: the approval modal is HTML, and the native preview
                // view paints above all HTML. Hide the view while a modal is
                // open and bring it back after — before the page repaints.
                let covered = gs.approval.is_some();
                if covered != covered_applied {
                    covered_applied = covered;
                    gs.browser.covered = covered;
                    if let Some(wv) = &browser_webview {
                        show_browser(wv, &gs.browser);
                    }
                }
                if changed {
                    paint(&webview, &gs);
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                eprintln!("[gui] close requested");
                if confirm_close(&window, &gs) {
                    notify_cleanup(&window);
                    *control_flow = ControlFlow::Exit;
                }
            }
            // tao/winit signal a dead message loop this way; without it a
            // closed window can leave the process running invisibly.
            Event::LoopDestroyed => {
                eprintln!("[gui] window closed — exiting");
                notify_cleanup(&window);
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

/// New (S-12): ids of bridge requests still waiting for a page reply. The
/// preview's IPC handler reads it to drop frames nobody asked for.
type PendingIds = Arc<std::sync::Mutex<std::collections::HashSet<u64>>>;

/// New (S-12): biggest reply frame we accept from the preview page. A
/// snapshot is capped far below this by the page runtime.
const MAX_EVAL_FRAME_BYTES: usize = 1 << 20;

/// New (S-12): a request id the page cannot guess.
///
/// Ids travel through JavaScript as plain numbers, which are exact only up
/// to 2^53, so the id is kept below that. `RandomState` is seeded randomly
/// for each process; hashing a counter and the clock with it is enough for
/// a local, short-lived token (it is not a cryptographic secret).
fn unguessable_id(counter: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(counter);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.write_u128(nanos);
    (h.finish() & ((1u64 << 53) - 1)).max(1)
}

/// Goal B: requests from agent tools that are waiting on this webview.
///
/// Every request has a deadline; a page that never answers (a hung script, a
/// destroyed view) fails the tool instead of freezing the run.
struct Bridge {
    /// S-12: only feeds `unguessable_id`; ids themselves are random.
    counter: u64,
    pending: HashMap<u64, (tokio::sync::oneshot::Sender<Result<String, String>>, Instant)>,
    /// A navigation still waiting for its load event.
    nav: Option<NavWait>,
}

/// New (B-36): one `browser_navigate` waiting for its page to load.
struct NavWait {
    reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    deadline: Instant,
    /// Set once the new page starts loading. A "finished" event that comes
    /// before that belongs to the page we are leaving and must not answer.
    started: bool,
}

/// New (B-36): longest `browser_wait` we run. It must stay below the 25 s
/// request deadline in `Bridge::start`, or the deadline fires first.
const MAX_WAIT_MS: u64 = 20_000;

impl Bridge {
    fn new() -> Self {
        Self { counter: 0, pending: HashMap::new(), nav: None }
    }

    fn busy(&self) -> bool {
        !self.pending.is_empty() || self.nav.is_some()
    }

    /// New (S-12): tell the preview's IPC handler which ids may answer now.
    fn publish_ids(&self, ids: &PendingIds) {
        if let Ok(mut set) = ids.lock() {
            set.clear();
            set.extend(self.pending.keys().copied());
        }
    }

    /// Dispatch one request. `None` webview = panel unavailable (config off).
    fn start(
        &mut self,
        wv: &Option<wry::WebView>,
        req: crate::browser::Request,
        view: &mut BrowserView,
        allow_remote: bool,
    ) {
        let Some(wv) = wv else {
            let _ = req.reply.send(Err("the preview panel is not available".into()));
            return;
        };
        // S-12: random ids, so a page cannot answer a request it never got.
        self.counter += 1;
        let mut id = unguessable_id(self.counter);
        while self.pending.contains_key(&id) {
            self.counter += 1;
            id = unguessable_id(self.counter);
        }
        let deadline = Instant::now() + Duration::from_secs(25);
        match req.method.as_str() {
            "navigate" => {
                let raw = req.input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                // S-11: the same rule as every other way of opening a page.
                let checked = normalize_url(raw)
                    .and_then(|url| check_navigation(&url, allow_remote).map(|()| url));
                match checked {
                    Ok(url) => {
                        view.open(&url);
                        show_browser(wv, view);
                        if let Err(e) = wv.load_url(&url) {
                            let _ = req.reply.send(Err(format!("navigate failed: {e}")));
                            return;
                        }
                        // B-36: a newer navigation replaces an older one, so
                        // tell the older caller instead of dropping it.
                        if let Some(old) = self.nav.take() {
                            let _ = old.reply.send(Err("replaced by a newer navigation".into()));
                        }
                        // Resolved by the first Loading(false) after Loading(true).
                        self.nav = Some(NavWait { reply: req.reply, deadline, started: false });
                    }
                    Err(e) => {
                        let _ = req.reply.send(Err(e));
                    }
                }
            }
            "wait" => {
                let selector = req.input.get("selector").and_then(|v| v.as_str());
                let text = req.input.get("text").and_then(|v| v.as_str());
                if selector.is_none() && text.is_none() {
                    // Plain delay: no page interaction, no blocked UI thread.
                    let ms = req
                        .input
                        .get("ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(500)
                        .clamp(1, MAX_WAIT_MS);
                    let js = format!(
                        "setTimeout(function(){{window.ipc.postMessage(JSON.stringify(\
                         {{hab:{id},ok:true,result:{{\"slept_ms\":{ms}}}}}));}}, {ms});"
                    );
                    self.pending.insert(id, (req.reply, deadline));
                    let _ = wv.evaluate_script(&js);
                    return;
                }
                self.pending.insert(id, (req.reply, deadline));
                // B-36: `ms` (when given) is how long to wait for the condition.
                let budget = req
                    .input
                    .get("ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(8_000)
                    .clamp(250, MAX_WAIT_MS);
                let js = crate::browser::wait_script(id, selector, text, budget);
                let _ = wv.evaluate_script(&js);
            }
            "screenshot" => {
                // F-04: WebView2's own capture; other platforms say so.
                #[cfg(windows)]
                capture_screenshot(wv, req.reply);
                #[cfg(not(windows))]
                let _ = req.reply.send(Err(
                    "browser_screenshot needs the Windows desktop app (WebView2)".into(),
                ));
            }
            method => {
                let Some(js) = crate::browser::eval_wrapper(method, &req.input, id) else {
                    let _ = req.reply.send(Err(format!("unknown browser command `{method}`")));
                    return;
                };
                self.pending.insert(id, (req.reply, deadline));
                let _ = wv.evaluate_script(&js);
            }
        }
    }

    /// Answer a page frame. Returns true when it belonged to a live request.
    fn on_eval(&mut self, raw: &str) -> bool {
        let Some((id, res)) = crate::browser::parse_reply(raw) else {
            return false;
        };
        match self.pending.remove(&id) {
            Some((tx, _)) => {
                let _ = tx.send(res);
                true
            }
            None => false,
        }
    }

    /// New (B-36): the panel started loading a page.
    fn on_load_started(&mut self) {
        if let Some(nav) = self.nav.as_mut() {
            nav.started = true;
        }
    }

    /// New (B-36): the panel refused a navigation. A waiting
    /// `browser_navigate` gets the reason now instead of a timeout later.
    fn on_blocked(&mut self, msg: &str) -> bool {
        let Some(nav) = self.nav.take() else {
            return false;
        };
        let _ = nav.reply.send(Err(msg.to_string()));
        true
    }

    /// A page finished loading: release a waiting `browser_navigate`.
    fn on_loaded(&mut self, view: &BrowserView) -> bool {
        // B-36: ignore a "finished" that belongs to the page we are leaving.
        if !self.nav.as_ref().is_some_and(|n| n.started) {
            return false;
        }
        let Some(NavWait { reply: tx, .. }) = self.nav.take() else {
            return false;
        };
        let out = serde_json::json!({
            "ok": true,
            "url": view.url,
            "title": view.title,
        });
        let _ = tx.send(Ok(out.to_string()));
        true
    }

    /// Fail anything past its deadline (called every timed wake).
    fn expire(&mut self) {
        let now = Instant::now();
        let stale: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, (_, d))| *d <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some((tx, _)) = self.pending.remove(&id) {
                let _ = tx.send(Err("the page did not answer in time".into()));
            }
        }
        if self.nav.as_ref().is_some_and(|n| n.deadline <= now) {
            if let Some(nav) = self.nav.take() {
                let _ = nav.reply.send(Err("navigation timed out".into()));
            }
        }
    }
}

/// New (F-04): save a PNG of what the preview shows and answer the tool with
/// its path. Windows only: WebView2's own `CapturePreview` (no DevTools
/// protocol needed).
#[cfg(windows)]
fn capture_screenshot(
    wv: &wry::WebView,
    reply: tokio::sync::oneshot::Sender<Result<String, String>>,
) {
    use std::cell::RefCell;
    use std::rc::Rc;
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::Win32::UI::Shell::SHCreateMemStream;
    use wry::WebViewExtWindows;

    // SAFETY: SHCreateMemStream has no preconditions; it returns an owned
    // in-memory COM stream (None when out of memory).
    let Some(stream) = (unsafe { SHCreateMemStream(None) }) else {
        let _ = reply.send(Err("could not create a memory stream".into()));
        return;
    };
    // The completion callback and the error path below share the reply:
    // whichever runs first sends it.
    let slot = Rc::new(RefCell::new(Some(reply)));
    let slot_cb = slot.clone();
    let stream_cb = stream.clone();
    let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
        let out = result
            .map_err(|e| format!("capture failed: {e}"))
            .and_then(|()| read_stream(&stream_cb))
            .and_then(|png| save_screenshot(&png));
        if let Some(tx) = slot_cb.borrow_mut().take() {
            let _ = tx.send(out);
        }
        Ok(())
    }));
    // SAFETY: the webview, `stream` and `handler` are valid COM objects;
    // WebView2 calls `handler` once, on this (the UI) thread.
    let started = unsafe {
        wv.webview()
            .CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler)
    };
    if let Err(e) = started {
        if let Some(tx) = slot.borrow_mut().take() {
            let _ = tx.send(Err(format!("capture failed: {e}")));
        }
    }
}

/// New (F-04): read a whole COM stream into memory.
#[cfg(windows)]
fn read_stream(stream: &windows::Win32::System::Com::IStream) -> Result<Vec<u8>, String> {
    use windows::Win32::System::Com::{STREAM_SEEK_END, STREAM_SEEK_SET};
    let mut size = 0u64;
    // SAFETY: plain IStream calls on a valid stream; `buf` outlives `Read`.
    unsafe {
        stream
            .Seek(0, STREAM_SEEK_END, Some(&mut size as *mut u64))
            .map_err(|e| format!("reading the capture: {e}"))?;
        stream
            .Seek(0, STREAM_SEEK_SET, None)
            .map_err(|e| format!("reading the capture: {e}"))?;
        let mut buf = vec![0u8; size as usize];
        let mut read = 0u32;
        stream
            .Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read as *mut u32))
            .ok()
            .map_err(|e| format!("reading the capture: {e}"))?;
        buf.truncate(read as usize);
        Ok(buf)
    }
}

/// New (F-04): store a PNG in the temp folder and describe it for the model.
/// Platform-neutral, so it is unit-tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn save_screenshot(png: &[u8]) -> Result<String, String> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || &png[..8] != SIGNATURE {
        return Err("the capture is not a PNG image".into());
    }
    // The IHDR chunk always comes first: width and height, big-endian u32.
    let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    let dir = std::env::temp_dir().join("opencode-rust-screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("shot-{stamp}.png"));
    std::fs::write(&path, png).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "width": width,
        "height": height,
        "bytes": png.len(),
    })
    .to_string())
}

fn tracing_boot(boot_ms: u128, addr: std::net::SocketAddr, session: &str) {
    eprintln!("[habi] ready in {boot_ms}ms \u{b7} core at http://{addr} \u{b7} session {session}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> ServerEvent {
        ServerEvent::AssistantDelta { session_id: "s".into(), text: text.into() }
    }

    #[test]
    fn chats_never_work_in_habis_own_source_or_system_folders() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(chat_home(src).ends_with("Habi Projects"), "Habi's own source is no project");
        let home = std::path::PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap());
        assert!(chat_home(&home).ends_with("Habi Projects"), "the home folder is no project");
        if cfg!(windows) {
            assert!(chat_home(std::path::Path::new("C:\\Windows\\System32")).ends_with("Habi Projects"));
            assert!(chat_home(std::path::Path::new("C:\\")).ends_with("Habi Projects"));
        }
        let project = std::env::temp_dir().join(format!("ocrs-chat-home-{}", std::process::id()));
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(chat_home(&project), project, "a normal folder is the project");
        let _ = std::fs::remove_dir_all(&project);
    }

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

    #[test]
    fn terminal_ipc_messages() {
        assert_eq!(parse_ipc(r#"{"type":"term_open","open":true}"#), Some(IpcMsg::TermOpen(true)));
        assert_eq!(parse_ipc(r#"{"type":"term_run","command":" ls "}"#), Some(IpcMsg::TermRun("ls".into())));
        assert_eq!(parse_ipc(r#"{"type":"term_run","command":"  "}"#), None, "nothing to run");
        assert_eq!(parse_ipc(r#"{"type":"term_stop"}"#), Some(IpcMsg::TermStop(None)));
        assert_eq!(parse_ipc(r#"{"type":"term_stop","id":"job3"}"#), Some(IpcMsg::TermStop(Some("job3".into()))));
        assert_eq!(parse_ipc(r#"{"type":"term_clear"}"#), Some(IpcMsg::TermClear));
    }

    #[test]
    fn terminal_cd_is_handled_by_the_app() {
        let root = std::env::temp_dir().join(format!("ocrs-cd-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let root = strip_verbatim(std::fs::canonicalize(&root).unwrap());
        assert_eq!(cd_target("cd sub", &root, &root), Some(Ok(root.join("sub"))));
        assert_eq!(cd_target("cd \"sub\"", &root, &root), Some(Ok(root.join("sub"))));
        assert_eq!(cd_target("cd", &root.join("sub"), &root), Some(Ok(root.clone())), "bare cd goes to the project");
        assert_eq!(cd_target("cd ..", &root.join("sub"), &root), Some(Ok(root.clone())));
        assert!(matches!(cd_target("cd nope", &root, &root), Some(Err(e)) if e.contains("no such folder")));
        assert_eq!(cd_target("cd sub && ls", &root, &root), None, "a compound line runs in the shell");
        assert_eq!(cd_target("cdk deploy", &root, &root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn terminal_shows_a_command_and_its_output() {
        let dir = std::env::temp_dir();
        let id = crate::tools::jobs::spawn("echo hello-term", &dir, true).unwrap();
        assert!(id.starts_with("term"), "{id}");
        for _ in 0..100 {
            if crate::tools::jobs::list(0).iter().any(|j| j.id == id && j.exit.is_some()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // The pipe readers may finish just after the exit is seen.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut g = GuiState::new();
        g.term_entries.push(TermEntry::Note("$ cd x\nC:\\x\n".into()));
        g.term_entries.push(TermEntry::Job(id.clone()));
        let v = term_view(&g, &dir);
        let text = v["text"].as_str().unwrap();
        assert!(text.starts_with("$ cd x\n"), "{text}");
        assert!(text.contains("$ echo hello-term\nhello-term"), "{text}");
        assert!(text.contains("[exited with code 0]"), "{text}");
        assert!(v["running"].is_null());
        assert!(
            v["jobs"].as_array().unwrap().iter().all(|j| j["id"] != id.as_str()),
            "the user's own commands are not agent job tabs"
        );
    }

    #[test]
    fn notices_for_waiting_events() {
        let q = ServerEvent::Question { session_id: "s".into(), question: "Which colour?".into(), options: vec![] };
        assert_eq!(notice_for(&q), Some(("Habi has a question", "Which colour?".to_string())));
        let done = ServerEvent::RunFinished { session_id: "s".into(), ok: true, message: None };
        assert_eq!(notice_for(&done).map(|n| n.0), Some("Habi finished"));
    }

    #[test]
    fn next_steps_after_a_run() {
        let mut g = GuiState::new();
        g.push(EntryKind::User, "\u{203a} make the button blue");
        let ok: Vec<String> = g.next_steps(true).into_iter().map(|(_, t)| t).collect();
        assert!(ok.contains(&"@changes".to_string()) && ok.contains(&"/review".to_string()));
        assert!(!ok.iter().any(|t| t.starts_with("Commit")), "no git, no commit button");
        g.git = Some(("main".into(), 2));
        assert!(g.next_steps(true).iter().any(|(_, t)| t.starts_with("Commit")));
        let failed = g.next_steps(false);
        assert_eq!(failed[0], ("Try again".to_string(), "make the button blue".to_string()));
    }

    #[test]
    fn submit_sends_and_sets_busy() {
        let mut g = GuiState::new();
        g.input = "  hello  ".into();
        assert_eq!(g.submit().as_deref(), Some("hello"));
        assert!(g.busy);
        assert!(g.entries.iter().any(|(k, t)| *k == EntryKind::User && t.contains("hello")));
        assert_eq!(g.submit(), None, "empty input sends nothing");
    }

    /// F-13: a second window on the same state directory is refused.
    #[test]
    fn only_one_window_per_state_dir() {
        let dir = std::env::temp_dir().join(format!("habi-lock-test-{}", std::process::id()));
        let first = single_instance_lock(&dir).expect("the first window gets the lock");
        assert!(single_instance_lock(&dir).is_err(), "a second window is refused");
        drop(first);
        assert!(single_instance_lock(&dir).is_ok(), "free again after the first closes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assistant_deltas_merge_into_one_bubble() {
        let mut g = GuiState::new();
        g.session = Some("s".into());
        g.on_event(delta("Hel"));
        g.on_event(delta("lo!"));
        let assistant: Vec<_> = g
            .entries
            .iter()
            .filter(|(k, _)| *k == EntryKind::Assistant)
            .collect();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0].1, "Hello!");
    }

    #[test]
    fn reasoning_marker_once_then_merge() {
        let mut g = GuiState::new();
        g.session = Some("s".into());
        g.on_event(ServerEvent::ReasoningDelta { session_id: "s".into(), text: "think".into() });
        g.on_event(ServerEvent::ReasoningDelta { session_id: "s".into(), text: "ing".into() });
        let thinking: Vec<_> = g.entries.iter().filter(|(k, _)| *k == EntryKind::Thinking).collect();
        assert_eq!(thinking.len(), 1);
        assert!(thinking[0].1.starts_with('\u{273b}'), "marker once: {}", thinking[0].1);
        assert!(thinking[0].1.ends_with("thinking"));
    }

    #[test]
    fn run_finished_clears_busy_and_reports() {
        let mut g = GuiState::new();
        g.session = Some("s".into());
        g.busy = true;
        g.on_event(ServerEvent::RunFinished {
            session_id: "s".into(),
            ok: true,
            message: Some("done".into()),
        });
        assert!(!g.busy);
        assert!(g.entries.iter().any(|(k, t)| *k == EntryKind::System && t == "done"));
    }

    fn delta_for(sid: &str, text: &str) -> ServerEvent {
        ServerEvent::AssistantDelta { session_id: sid.into(), text: text.into() }
    }

    /// Built from JSON so the helper works with or without optional fields
    /// (task B-22 adds `preview` to this event).
    fn ask(sid: &str, id: &str) -> ServerEvent {
        serde_json::from_value(serde_json::json!({
            "type": "approval_needed",
            "session_id": sid,
            "approval_id": id,
            "tool": "bash",
            "detail": "ls"
        }))
        .unwrap()
    }

    /// B-12: approvals from several chats queue up and end by id.
    #[test]
    fn approvals_from_any_chat_queue_and_finish_by_id() {
        let mut g = GuiState::new();
        g.session = Some("a".into());
        g.on_event(ask("b", "apr_b"));
        g.on_event(ask("a", "apr_a"));
        assert_eq!(g.approval.as_ref().map(|a| a.id.as_str()), Some("apr_b"), "first come, first shown");
        assert!(g.busy, "chat a is blocked too");
        let v: serde_json::Value = serde_json::from_str(&g.payload()).unwrap();
        assert_eq!(v["approval"]["other_chat"], true);
        assert_eq!(v["approval"]["queued"], 1);
        // Chat b's run ends: its modal goes, chat a's approval comes up.
        g.on_event(ServerEvent::RunFinished { session_id: "b".into(), ok: true, message: None });
        assert_eq!(g.approval.as_ref().map(|a| a.id.as_str()), Some("apr_a"));
        assert!(g.browser.covered);
        g.finish_approval("apr_a");
        assert!(g.approval.is_none());
        assert!(!g.browser.covered);
    }

    /// B-12: one bus carries every chat; text must land in its own chat.
    #[test]
    fn events_from_other_chats_stay_out_of_the_open_chat() {
        let mut g = GuiState::new();
        g.session = Some("a".into());
        g.on_event(delta_for("b", "text from b"));
        assert!(g.entries.is_empty(), "nothing from chat b in chat a");
        assert!(!g.busy, "chat a is not running");
        g.on_event(ServerEvent::RunFinished {
            session_id: "b".into(),
            ok: false,
            message: Some("b failed".into()),
        });
        assert!(g.entries.is_empty());
    }

    /// B-12: a chat that runs while hidden keeps streaming into its live copy.
    #[test]
    fn a_running_chat_keeps_streaming_while_hidden() {
        let mut g = GuiState::new();
        g.session = Some("a".into());
        g.input = "go".into();
        g.submit();
        g.on_event(delta_for("a", "part one "));
        g.leave_current_chat();
        g.session = Some("b".into());
        g.on_event(delta_for("a", "part two"));
        let live = g.background.get("a").expect("live copy kept");
        assert!(live.iter().any(|(_, t)| t == "part one part two"), "{live:?}");
        assert!(g.entries.is_empty());
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

    /// B-12: another client's chat events never take over the open chat.
    #[test]
    fn session_events_from_other_clients_do_not_switch_the_chat() {
        let mut g = GuiState::new();
        g.session = Some("mine".into());
        g.entries.push((EntryKind::User, "hello".into()));
        g.on_event(ServerEvent::SessionCreated { session_id: "theirs".into(), title: "x".into() });
        g.on_event(ServerEvent::SessionLoaded {
            session_id: "theirs".into(),
            mode: "build".into(),
            transcript: vec![server::TranscriptLine { role: "user".into(), text: "hi".into() }],
        });
        assert_eq!(g.session.as_deref(), Some("mine"));
        assert_eq!(g.entries.len(), 1);
        assert!(g.sessions_stale, "the sidebar is refreshed instead");
    }

    #[test]
    fn ipc_send_new_and_window_controls() {
        assert_eq!(parse_ipc(r#"{"type":"send","text":" hi "}"#), Some(IpcMsg::Send("hi".into())));
        assert_eq!(parse_ipc(r#"{"type":"send","text":"   "}"#), None, "blank sends are dropped");
        assert_eq!(parse_ipc(r#"{"type":"new"}"#), Some(IpcMsg::NewSession));
        // Projects sidebar.
        assert_eq!(
            parse_ipc(r#"{"type":"new_in_project","project":"prj_1"}"#),
            Some(IpcMsg::NewInProject("prj_1".into()))
        );
        assert_eq!(parse_ipc(r#"{"type":"add_project"}"#), Some(IpcMsg::AddProject(None)));
        assert_eq!(
            parse_ipc(r#"{"type":"add_project","path":"C:/code/app"}"#),
            Some(IpcMsg::AddProject(Some("C:/code/app".into())))
        );
        assert_eq!(
            parse_ipc(r#"{"type":"pin","id":"s","on":true}"#),
            Some(IpcMsg::Pin { id: "s".into(), on: true })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"rename","id":"s","title":" New name "}"#),
            Some(IpcMsg::Rename { id: "s".into(), title: "New name".into() })
        );
        assert_eq!(parse_ipc(r#"{"type":"rename","id":"s","title":"  "}"#), None, "no blank titles");
        assert_eq!(
            parse_ipc(r#"{"type":"move","id":"s","project":null}"#),
            Some(IpcMsg::MoveChat { id: "s".into(), project: None })
        );
        assert_eq!(parse_ipc(r#"{"type":"delete","id":"s"}"#), Some(IpcMsg::DeleteChat("s".into())));
        assert_eq!(
            parse_ipc(r#"{"type":"adopt_unfiled","project":"p"}"#),
            Some(IpcMsg::AdoptUnfiled("p".into()))
        );
        assert_eq!(parse_ipc(r#"{"type":"load","id":"ses_9"}"#), Some(IpcMsg::Load("ses_9".into())));
        assert_eq!(parse_ipc(r#"{"type":"load"}"#), None, "load without id is malformed");
        assert_eq!(parse_ipc(r#"{"type":"ready"}"#), Some(IpcMsg::Ready));
        assert_eq!(parse_ipc(r#"{"type":"min"}"#), Some(IpcMsg::Minimize));
        assert_eq!(parse_ipc(r#"{"type":"max"}"#), Some(IpcMsg::MaximizeToggle));
        assert_eq!(parse_ipc(r#"{"type":"close"}"#), Some(IpcMsg::Close));
        assert_eq!(parse_ipc(r#"{"type":"wat"}"#), None);
        assert_eq!(parse_ipc("not json"), None);
    }

    #[test]
    fn ipc_mode_selector_parses_the_three_modes() {
        assert_eq!(
            parse_ipc(r#"{"type":"mode","mode":"default"}"#),
            Some(IpcMsg::Mode(PermissionMode::Default))
        );
        assert_eq!(
            parse_ipc(r#"{"type":"mode","mode":"manual"}"#),
            Some(IpcMsg::Mode(PermissionMode::Manual))
        );
        assert_eq!(
            parse_ipc(r#"{"type":"mode","mode":"turbo"}"#),
            Some(IpcMsg::Mode(PermissionMode::Turbo))
        );
        assert_eq!(parse_ipc(r#"{"type":"mode","mode":"yolo"}"#), None, "unknown mode dropped");
        assert_eq!(parse_ipc(r#"{"type":"mode"}"#), None, "mode without value dropped");
    }

    /// F-06: the Stop button's IPC verb.
    #[test]
    fn ipc_parses_stop() {
        assert_eq!(parse_ipc(r#"{"type":"stop"}"#), Some(IpcMsg::Stop));
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

    #[test]
    fn payload_is_valid_json_with_classes() {
        let mut g = GuiState::new();
        g.session = Some("ses_1".into());
        g.busy = true;
        g.api = Some((true, 16));
        g.push(EntryKind::User, "quote \" and \u{1f512}");
        g.push(EntryKind::System, "</script> injection");
        let v: serde_json::Value = serde_json::from_str(&g.payload()).expect("payload parses");
        assert_eq!(v["session"], "ses_1");
        assert_eq!(v["busy"], true);
        assert_eq!(v["api"][1], 16);
        assert_eq!(v["entries"][0]["k"], "u");
        assert_eq!(v["entries"][1]["k"], "s");
        assert!(v["entries"][1]["t"].as_str().unwrap().contains("</script>"));
        assert_eq!(v["perm_mode"], "default", "selector starts in default mode");
        g.perm_mode = PermissionMode::Turbo;
        let v: serde_json::Value = serde_json::from_str(&g.payload()).unwrap();
        assert_eq!(v["perm_mode"], "turbo");
    }

    #[test]
    fn payload_carries_sessions_and_active_title() {
        let mut g = GuiState::new();
        g.sessions = vec![ChatItem::new("ses_2", "Second"), ChatItem::new("ses_1", "First")];
        g.session = Some("ses_1".into());
        let v: serde_json::Value = serde_json::from_str(&g.payload()).unwrap();
        assert_eq!(v["title"], "First", "header shows the active chat's title");
        assert_eq!(v["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(v["sessions"][0]["active"], false);
        assert_eq!(v["sessions"][1]["active"], true);
        // No session selected yet: title falls back to the default.
        let g2 = GuiState::new();
        let v2: serde_json::Value = serde_json::from_str(&g2.payload()).unwrap();
        assert_eq!(v2["title"], NEW_CHAT);
    }

    /// B-37: a streamed token only resends the entry it changed.
    #[test]
    fn paint_sends_only_the_changed_tail() {
        let mut g = GuiState::new();
        g.session = Some("s".into());
        g.push(EntryKind::User, "hi");
        g.push(EntryKind::Assistant, "Hel");
        let first: serde_json::Value = serde_json::from_str(&g.paint_payload()).unwrap();
        assert_eq!(first["entries_from"], 0);
        assert_eq!(first["entries"].as_array().unwrap().len(), 2);
        g.push(EntryKind::Assistant, "lo");
        let next: serde_json::Value = serde_json::from_str(&g.paint_payload()).unwrap();
        assert_eq!(next["entries_from"], 1, "the user bubble is not resent");
        assert_eq!(next["entries_len"], 2);
        assert_eq!(next["entries"][0]["t"], "Hello");
        // A resync (page reload) starts from zero again.
        g.resync();
        let full: serde_json::Value = serde_json::from_str(&g.paint_payload()).unwrap();
        assert_eq!(full["entries_from"], 0);
        assert_eq!(parse_ipc(r#"{"type":"resync"}"#), Some(IpcMsg::Resync));
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

    /// B-42: a page navigating by itself does not reopen a closed panel.
    #[test]
    fn a_closed_panel_stays_closed_when_the_page_navigates() {
        let mut g = GuiState::new();
        g.browser.open("http://localhost:3000");
        // What `IpcMsg::PanelVisible(false)` does when the user closes it:
        g.browser.visible = false;
        g.browser.user_closed = true;
        g.on_browser_event(BrowserEvt::Url("http://localhost:3000/next".into()));
        assert!(!g.browser.visible);
        g.browser.open("http://localhost:3000/agent");
        assert!(g.browser.visible, "an explicit open still shows it");
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

    #[test]
    fn clip_respects_char_boundary_and_budget() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("hello world", 5), "hello\u{2026}");
        assert_eq!(clip("\u{1f512}abc", 2), "\u{1f512}a\u{2026}");
    }

    /// S-18: the main window only ever shows our own page.
    #[test]
    fn main_window_navigation_is_locked() {
        assert!(main_window_may_navigate("about:blank"));
        assert!(main_window_may_navigate("data:text/html;charset=utf-8;base64,PGgxPg=="));
        assert!(!main_window_may_navigate("https://example.com/"));
        assert!(!main_window_may_navigate("http://localhost:3000/"));
        assert!(!main_window_may_navigate("file:///C:/Windows/win.ini"));
        let guard = main_window_navigation_guard();
        assert!(guard("data:text/html,first".to_string()), "the first load is ours");
        assert!(!guard("https://example.com/".to_string()));
        assert!(guard("about:blank".to_string()));
    }

    /// A-F1: everything a person might paste into the URL bar has to turn into
    /// something the engine will actually load.
    #[test]
    fn url_input_normalizes_to_a_loadable_url() {
        assert_eq!(normalize_url("  https://example.com/a?b=1  ").unwrap(), "https://example.com/a?b=1");
        assert_eq!(normalize_url("http://localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(normalize_url(":5173").unwrap(), "http://localhost:5173");
        assert_eq!(normalize_url("localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(normalize_url("localhost").unwrap(), "http://localhost");
        assert_eq!(normalize_url("127.0.0.1:8080/app").unwrap(), "http://127.0.0.1:8080/app");
        assert_eq!(normalize_url("192.168.1.5:8000").unwrap(), "http://192.168.1.5:8000");
        assert_eq!(normalize_url("news.ycombinator.com").unwrap(), "https://news.ycombinator.com");
        assert_eq!(normalize_url("about:blank").unwrap(), "about:blank");
        // Nonsense is reported, not silently dropped.
        assert!(normalize_url("   ").is_err());
        assert!(normalize_url("just-a-word").is_err());
    }

    /// A-B3: `file:` would read local disk, `javascript:` would run script
    /// against whatever origin the panel happens to be on. Both refused.
    #[test]
    fn refused_schemes_are_reported_with_reasons() {
        for bad in ["file:///C:/Windows/win.ini", "javascript:alert(1)", "data:text/html,hi"] {
            let err = normalize_url(bad).expect_err(bad);
            assert!(err.contains("blocked"), "{bad} -> {err}");
        }
        assert!(!is_allowed_scheme("file:///etc/passwd"));
        assert!(!is_allowed_scheme("JavaScript:alert(1)"));
        assert!(is_allowed_scheme("https://example.com"));
        assert!(is_allowed_scheme("about:blank"));
    }

    /// S-11: one rule for typed URLs, links, redirects, the dev-server
    /// button and agent navigation.
    #[test]
    fn check_navigation_applies_scheme_and_allow_remote() {
        assert!(check_navigation("http://localhost:3000/", false).is_ok());
        assert!(check_navigation("about:blank", false).is_ok());
        assert!(check_navigation("https://example.com/", true).is_ok());
        let remote = check_navigation("https://example.com/", false).unwrap_err();
        assert!(remote.contains("allow_remote"), "{remote}");
        let scheme = check_navigation("file:///etc/hosts", true).unwrap_err();
        assert!(scheme.contains("http(s)"), "{scheme}");
    }

    /// A-B1: CSS pixels are logical pixels. `Rect` must not be pre-multiplied
    /// by the scale factor or the child view lands off-panel on a 125% display.
    #[test]
    fn panel_rect_maps_css_pixels_to_logical_units() {
        let r = PanelRect { x: 840.0, y: 38.0, w: 440.0, h: 762.0 };
        assert!(r.is_usable());
        let wr = r.to_wry();
        match wr.position {
            wry::dpi::Position::Logical(p) => {
                assert_eq!(p.x, 840.0);
                assert_eq!(p.y, 38.0);
            }
            other => panic!("expected logical position, got {other:?}"),
        }
        match wr.size {
            wry::dpi::Size::Logical(s) => {
                assert_eq!(s.width, 440.0);
                assert_eq!(s.height, 762.0);
            }
            other => panic!("expected logical size, got {other:?}"),
        }
        // A collapsed panel reports w=0; that is not a rect to place a view at.
        assert!(!PanelRect { x: 0.0, y: 0.0, w: 0.0, h: 400.0 }.is_usable());
        assert!(!PanelRect { x: f64::NAN, y: 0.0, w: 10.0, h: 10.0 }.is_usable());
    }

    /// S-12: ids are random, large, and still exact as JavaScript numbers.
    #[test]
    fn bridge_ids_are_random_and_js_safe() {
        let a = unguessable_id(1);
        let b = unguessable_id(2);
        assert_ne!(a, b);
        for id in [a, b] {
            assert!(id > 0 && id < (1u64 << 53), "{id} must survive a JS number");
        }
        assert!(a > 1_000_000 || b > 1_000_000, "not a small counter");
    }

    /// S-12: the IPC filter sees exactly the ids that are waiting.
    #[test]
    fn bridge_publishes_only_pending_ids() {
        let mut bridge = Bridge::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        bridge.pending.insert(77, (tx, Instant::now() + Duration::from_secs(5)));
        let ids: PendingIds = Arc::new(std::sync::Mutex::new(std::collections::HashSet::from([5])));
        bridge.publish_ids(&ids);
        let set = ids.lock().unwrap();
        assert!(set.contains(&77));
        assert!(!set.contains(&5), "ids that are no longer waiting are removed");
    }

    #[test]
    fn ipc_parses_browser_verbs() {
        assert_eq!(parse_ipc(r#"{"type":"browse","url":":3000"}"#), Some(IpcMsg::Browse(":3000".into())));
        assert_eq!(parse_ipc(r#"{"type":"browse"}"#), None, "browse without a url is malformed");
        assert_eq!(parse_ipc(r#"{"type":"nav_back"}"#), Some(IpcMsg::NavBack));
        assert_eq!(parse_ipc(r#"{"type":"nav_fwd"}"#), Some(IpcMsg::NavFwd));
        assert_eq!(parse_ipc(r#"{"type":"nav_reload"}"#), Some(IpcMsg::NavReload));
        assert_eq!(
            parse_ipc(r#"{"type":"panel_rect","x":840,"y":38,"w":440,"h":762}"#),
            Some(IpcMsg::PanelRect(PanelRect { x: 840.0, y: 38.0, w: 440.0, h: 762.0 }, 0.0))
        );
        assert_eq!(
            parse_ipc(r#"{"type":"panel_rect","x":1,"y":2,"w":3,"h":4,"dpr":1.25}"#),
            Some(IpcMsg::PanelRect(PanelRect { x: 1.0, y: 2.0, w: 3.0, h: 4.0 }, 1.25))
        );
        assert_eq!(page_zoom(1.25, 1.0), 1.25);
        assert_eq!(page_zoom(2.5, 2.0), 1.25, "a 200% screen at 125% page zoom");
        assert_eq!(page_zoom(0.0, 1.5), 1.0, "no dpr sent: no zoom");
        assert_eq!(parse_ipc(r#"{"type":"panel_rect","x":1,"y":2,"w":3}"#), None, "missing h");
        assert_eq!(
            parse_ipc(r#"{"type":"panel_rect","x":0,"y":0,"w":0,"h":0}"#),
            None,
            "degenerate rect would place a view at zero size"
        );
        assert_eq!(parse_ipc(r#"{"type":"panel_rect","x":"a","y":2,"w":3,"h":4}"#), None);
        assert_eq!(parse_ipc(r#"{"type":"panel_visible","visible":false}"#), Some(IpcMsg::PanelVisible(false)));
        assert_eq!(parse_ipc(r#"{"type":"panel_visible"}"#), None);
        assert_eq!(parse_ipc(r#"{"type":"open_dev_server"}"#), Some(IpcMsg::OpenDevServer(None)));
        assert_eq!(
            parse_ipc(r#"{"type":"open_dev_server","url":"http://localhost:5173"}"#),
            Some(IpcMsg::OpenDevServer(Some("http://localhost:5173".into())))
        );
    }

    /// F-04: a capture is saved as a PNG file and described for the model.
    #[test]
    fn screenshots_are_saved_and_described() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        let out: serde_json::Value = serde_json::from_str(&save_screenshot(&png).unwrap()).unwrap();
        assert_eq!(out["width"], 640);
        assert_eq!(out["height"], 480);
        let path = std::path::PathBuf::from(out["path"].as_str().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), png);
        let _ = std::fs::remove_file(path);
        assert!(save_screenshot(b"not a png at all, sorry").is_err());
    }

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
        assert!(!gui_command(&mut gs, &st, &rt, "/deploy staging"), "a project command goes to the core");
        assert!(!gui_command(&mut gs, &st, &rt, "fix the bug"));
        assert_eq!(find_project_files(&dir, "APP", 5), vec!["src/app.js".to_string()]);
        assert_eq!(find_project_files(&dir, "", 1).len(), 1, "capped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The payload is the whole frontend contract: a missing key here means a
    /// control that silently never enables.
    #[test]
    fn payload_carries_browser_state() {
        let mut g = GuiState::new();
        g.browser.open("http://localhost:3000");
        g.browser.title = "Fixture".into();
        g.browser.can_back = true;
        g.dev_server = Some("http://localhost:5173".into());
        let v: serde_json::Value = serde_json::from_str(&g.payload()).unwrap();
        assert_eq!(v["browser"]["url"], "http://localhost:3000");
        assert_eq!(v["browser"]["title"], "Fixture");
        assert_eq!(v["browser"]["can_back"], true);
        assert_eq!(v["browser"]["can_forward"], false);
        assert_eq!(v["browser"]["loading"], true);
        assert_eq!(v["browser"]["error"], serde_json::Value::Null);
        assert_eq!(v["dev_server"], "http://localhost:5173");
    }

    fn waiting_navigation() -> (Bridge, tokio::sync::oneshot::Receiver<Result<String, String>>) {
        let mut bridge = Bridge::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        bridge.nav = Some(NavWait {
            reply: tx,
            deadline: Instant::now() + Duration::from_secs(25),
            started: false,
        });
        (bridge, rx)
    }

    /// B-36: the old page's "finished" must not answer a new navigation.
    #[test]
    fn navigate_ignores_finish_of_the_previous_page() {
        let (mut bridge, mut rx) = waiting_navigation();
        let view = BrowserView { url: "http://localhost:3000/".into(), ..Default::default() };
        assert!(!bridge.on_loaded(&view), "finished before started = the old page");
        assert!(rx.try_recv().is_err(), "still waiting");
        bridge.on_load_started();
        assert!(bridge.on_loaded(&view));
        let out = rx.try_recv().expect("answered").expect("ok");
        assert!(out.contains("localhost:3000"), "{out}");
    }

    /// B-36: a refused navigation answers at once, with the reason.
    #[test]
    fn blocked_navigation_answers_at_once() {
        let (mut bridge, mut rx) = waiting_navigation();
        assert!(bridge.on_blocked("blocked: dev-server-only"));
        let err = rx.try_recv().expect("answered").unwrap_err();
        assert!(err.contains("blocked"), "{err}");
    }

    /// B-36: every wait must end before the 25 s request deadline.
    #[test]
    fn wait_budget_stays_below_the_request_deadline() {
        assert!(MAX_WAIT_MS + 2_000 < 25_000);
    }

    #[test]
    fn browser_events_track_the_view() {
        let mut g = GuiState::new();
        // The fresh webview's about:blank is not a page.
        g.on_browser_event(BrowserEvt::Url("about:blank".into()));
        assert_eq!(g.browser.url, "");
        assert!(!g.browser.has_page());
        g.on_browser_event(BrowserEvt::Url("https://example.com".into()));
        g.on_browser_event(BrowserEvt::Loading(false));
        assert!(g.browser.has_page());
        assert!(!g.browser.loading);
        assert!(g.browser.visible);
        let refusal = check_navigation("file:///etc/passwd", true).unwrap_err();
        g.on_browser_event(BrowserEvt::Blocked(refusal));
        assert!(!g.browser.visible, "a native view over an error card is unreadable");
        assert!(g.browser.error.unwrap().contains("Blocked"));
    }

    /// D1: an `ask` gate must reach the page as a modal — a dropped event is
    /// a silent denial, which is exactly the hole this closes.
    #[test]
    fn approval_reaches_the_modal_and_clears_on_answer() {
        let mut g = GuiState::new();
        g.approval_timeout_secs = 300;
        g.on_event(ServerEvent::ApprovalNeeded {
            session_id: "s".into(),
            approval_id: "apr_1".into(),
            tool: "bash".into(),
            detail: "rm -rf build".into(),
            preview: String::new(),
        });
        let v: serde_json::Value = serde_json::from_str(&g.payload()).unwrap();
        assert_eq!(v["approval"]["id"], "apr_1");
        assert_eq!(v["approval"]["tool"], "bash");
        assert_eq!(v["approval"]["detail"], "rm -rf build");
        assert!(v["approval"]["secs_left"].as_u64().unwrap() <= 300);
        assert!(g.browser.covered, "a native child view would paint over the modal");

        settle_approval(&mut g, "apr_1", true);
        let v: serde_json::Value = serde_json::from_str(&g.payload()).unwrap();
        assert_eq!(v["approval"], serde_json::Value::Null);
        assert!(!g.browser.covered, "the panel should come back after answering");

        // A late answer for a request that is already gone must not clear a
        // newer one.
        g.on_event(ServerEvent::ApprovalNeeded {
            session_id: "s".into(),
            approval_id: "apr_2".into(),
            tool: "write".into(),
            detail: "src/main.rs".into(),
            preview: String::new(),
        });
        settle_approval(&mut g, "apr_1", true);
        assert_eq!(g.approval.as_ref().map(|a| a.id.as_str()), Some("apr_2"));
    }

    /// B-11: while the approval modal is open the native view must be hidden.
    #[test]
    fn approval_modal_hides_the_native_view_until_answered() {
        let mut g = GuiState::new();
        g.browser.open("http://localhost:3000");
        g.browser.rect = Some(PanelRect { x: 800.0, y: 40.0, w: 400.0, h: 600.0 });
        assert!(g.browser.wants_native_view());
        // Built from JSON so the test works with or without optional fields
        // (task B-22 adds `preview` to this event).
        let ask: ServerEvent = serde_json::from_value(serde_json::json!({
            "type": "approval_needed",
            "session_id": "s",
            "approval_id": "apr_1",
            "tool": "bash",
            "detail": "ls"
        }))
        .unwrap();
        g.on_event(ask);
        assert!(!g.browser.wants_native_view(), "the modal must not be painted over");
        settle_approval(&mut g, "apr_1", true);
        assert!(g.browser.wants_native_view(), "the page comes back after answering");
    }

    #[test]
    fn approval_and_denial_parse_from_the_page() {
        assert_eq!(
            parse_ipc(r#"{"type":"approve","id":"apr_1","always":true}"#),
            Some(IpcMsg::Approve { id: "apr_1".into(), always: true })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"approve","id":"apr_1"}"#),
            Some(IpcMsg::Approve { id: "apr_1".into(), always: false })
        );
        assert_eq!(parse_ipc(r#"{"type":"approve"}"#), None, "id is required");
        assert_eq!(
            parse_ipc(r#"{"type":"deny","id":"apr_1"}"#),
            Some(IpcMsg::Deny("apr_1".into()))
        );
    }

    /// Built from JSON so the helper works with or without optional fields
    /// (task B-22 adds `preview` to this event).
    fn approval_for(sid: &str, id: &str) -> ServerEvent {
        serde_json::from_value(serde_json::json!({
            "type": "approval_needed",
            "session_id": sid,
            "approval_id": id,
            "tool": "bash",
            "detail": "ls"
        }))
        .unwrap()
    }

    /// B-13: the core's "resolved" event closes the modal, whoever answered.
    #[test]
    fn approval_resolved_closes_the_modal_and_explains_timeouts() {
        let mut g = GuiState::new();
        g.session = Some("s".into());
        g.on_event(approval_for("s", "apr_1"));
        g.on_event(ServerEvent::ApprovalResolved {
            session_id: "s".into(),
            approval_id: "apr_other".into(),
            allowed: true,
            reason: "user".into(),
        });
        assert!(g.approval.is_some(), "another id leaves the modal alone");
        g.on_event(ServerEvent::ApprovalResolved {
            session_id: "s".into(),
            approval_id: "apr_1".into(),
            allowed: false,
            reason: "timeout".into(),
        });
        assert!(g.approval.is_none());
        assert!(!g.browser.covered);
        assert!(g.entries.iter().any(|(_, t)| t.contains("denied")), "{:?}", g.entries);
    }

    /// B-13: a modal long past its timeout goes away even without an event.
    #[test]
    fn a_long_expired_modal_is_dropped_without_an_event() {
        let mut g = GuiState::new();
        g.approval_timeout_secs = 2;
        g.on_event(approval_for("s", "apr_1"));
        let started = g.approval.as_ref().unwrap().started;
        assert!(!g.expire_stale_approval(started + Duration::from_secs(3)), "still in the grace time");
        assert!(g.expire_stale_approval(started + Duration::from_secs(60)));
        assert!(g.approval.is_none());
    }

    /// A-B4: `allow_remote = false` must keep the panel on this machine.
    #[test]
    fn local_url_detection_covers_dev_servers() {
        for u in [
            "http://localhost:3000",
            "http://127.0.0.1:5173/app",
            "https://localhost",
            "http://0.0.0.0:8080",
            "http://[::1]:8080",
            "about:blank",
        ] {
            assert!(is_local_url(u), "{u} should count as local");
        }
        for u in [
            "https://example.com",
            "http://192.168.1.5:3000",
            "https://localhost.evil.example",
            "file:///etc/passwd",
        ] {
            assert!(!is_local_url(u), "{u} should not count as local");
        }
    }

    /// S-10: the host we check must be the host the browser really uses.
    #[test]
    fn local_url_detection_reads_the_real_host() {
        for u in [
            "http://localhost:x@example.com/",
            "http://localhost@example.com/",
            "http://127.0.0.1:80@example.com/",
            "http://[::1]@example.com/",
            "http://localhost.example.com/",
            "http://example.com/?next=http://localhost:3000",
            "http://example.com/#localhost",
        ] {
            assert!(!is_local_url(u), "{u} is a remote URL");
        }
        // A user name in front of a real local host is still local.
        assert!(is_local_url("http://user:pw@localhost:3000/"));
    }

    /// S-10: IPv6, upper case, the whole loopback range, and junk input.
    #[test]
    fn local_url_detection_handles_ipv6_case_and_junk() {
        for u in [
            "http://[::1]/",
            "http://[0:0:0:0:0:0:0:1]:8080/app",
            "HTTP://LOCALHOST:3000",
            "http://127.0.0.2:5173",
            "  http://localhost:3000  ",
            "ABOUT:BLANK",
        ] {
            assert!(is_local_url(u), "{u} is local");
        }
        for u in ["http://[::2]/", "http://10.0.0.1/", "ftp://localhost/", "localhost:3000", ""] {
            assert!(!is_local_url(u), "{u} is not a local http(s) URL");
        }
    }

}
