//! The window's model: `GuiState` and the small types it is made of. It
//! holds everything the page draws and never touches the toolkit, so it is
//! tested without a window.

use super::*;

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
    /// The token usage dashboard waiting to be sent (JSON), once, and the
    /// range it shows (days; `None` = all time) for a refresh.
    pub usage_out: std::cell::RefCell<Option<String>>,
    pub usage_days: Option<i64>,
    /// New (B-37): the entries as the page last received them, and for which
    /// chat, so a paint only sends what changed. `RefCell` because painting
    /// only has `&GuiState`.
    pub(super) sent: std::cell::RefCell<Vec<(EntryKind, String)>>,
    pub(super) sent_session: std::cell::RefCell<Option<String>>,
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
    /// T-052: why the last "Load models" got none, and a counter so the
    /// page tells a new answer from the one it already showed.
    pub models_error: (u64, String),
    pub config_path: String,
    /// The user's own prompt, and whether it replaces Habi's base
    /// instructions (else it is added after them).
    pub system_prompt: String,
    pub prompt_replaces: bool,
    /// `provider.temperature`; `None` = the model's default (nothing sent).
    pub temperature: Option<f32>,
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
    pub(super) fn running(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "working")
    }
}

/// Most agent cards kept; the oldest finished ones go first.
pub(super) const MAX_AGENT_CARDS: usize = 60;

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
    pub(super) fn secs_left(&self, timeout_secs: u64) -> u64 {
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
            usage_out: std::cell::RefCell::new(None),
            usage_days: Some(7),
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
        self.visible && !self.covered && self.error.is_none() && matches!(self.rect, Some(r) if r.is_usable())
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
            self.push(EntryKind::System, "An approval got no answer in time and was denied.".to_string());
        }
        !stale.is_empty()
    }

    /// New (B-12): true when chat `id` is the one on screen.
    pub fn is_current(&self, id: &str) -> bool {
        self.session.as_deref() == Some(id)
    }

    /// New (B-12): sidebar title of chat `id` ("New chat" when unknown).
    pub fn session_title(&self, id: &str) -> &str {
        self.sessions.iter().find(|c| c.id == id).map(|c| c.title.as_str()).unwrap_or(NEW_CHAT)
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
            .and_then(|id| self.sessions.iter().find(|c| &c.id == id).map(|c| c.title.as_str()))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_sends_and_sets_busy() {
        let mut g = GuiState::new();
        g.input = "  hello  ".into();
        assert_eq!(g.submit().as_deref(), Some("hello"));
        assert!(g.busy);
        assert!(g.entries.iter().any(|(k, t)| *k == EntryKind::User && t.contains("hello")));
        assert_eq!(g.submit(), None, "empty input sends nothing");
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
}
