use crate::agents::{self, AgentMode, PromptInputs};
use crate::config::Config;
use crate::db::{Db, Message, Part, Session};
use crate::permissions::{normalize_path_detail, Action, Engine};
use crate::provider::{ChatMsg, Client, ToolCallAccumulator, StreamEvent};
use crate::skills;
use crate::todo::TodoStore;
use crate::tools;
use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, patch, post},
    Json, Router,
};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex, RwLock};

mod deep;
mod mission_run;
mod agent_loop;
mod http;
mod prompt;
mod run;
mod session_view;
mod skill_install;
mod tool_exec;
mod verify;
pub use agent_loop::*;
pub use http::*;
pub use prompt::*;
pub use run::*;
pub use skill_install::*;
pub use tool_exec::*;
pub use verify::*;
pub use mission_run::mission_report;

/// How much autonomy the agent has — set by the client (GUI composer toggle,
/// `/turbo` in the TUI/CLI) and read by every permission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PermissionMode {
    /// Config-driven: safe reads run, anything that mutates asks first.
    #[default]
    Default = 0,
    /// Ask before every tool call, even ones the config allows by default.
    Manual = 1,
    /// Full access: no prompts, and open work continues on its own.
    Turbo = 2,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "manual" => Some(Self::Manual),
            "turbo" | "auto" => Some(Self::Turbo),
            _ => None,
        }
    }

    /// Stable wire/UI name (also what the page sends back over IPC).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Manual => "manual",
            Self::Turbo => "turbo",
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Manual,
            2 => Self::Turbo,
            _ => Self::Default,
        }
    }

    /// One transcript line so a mode change is never silent.
    pub fn note(self) -> &'static str {
        match self {
            Self::Default => "mode: default — reads run, writes and commands ask first",
            Self::Manual => "mode: manual — every tool call waits for your approval",
            Self::Turbo => "mode: turbo — full access, no prompts; I'll keep working on my own",
        }
    }
}

/// Events pushed to the TUI over SSE.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    AssistantDelta { session_id: String, text: String },
    /// The last `chars` characters of streamed reply text belong to a reply
    /// that broke off and is being asked again: clients drop them, or the
    /// retry would show after the half reply (partial + full on screen).
    ReplyRetracted { session_id: String, chars: usize },
    /// Token counts of one provider reply (`last`) and the session's running
    /// totals (`session`); `cached` is the part read from the prompt cache.
    Usage { session_id: String, last: crate::provider::Usage, session: crate::db::UsageTotals },
    /// M5: streamed chain-of-thought (`reasoning_content`).
    ReasoningDelta { session_id: String, text: String },
    ToolStarted { session_id: String, call_id: String, tool: String, detail: String },
    ToolFinished { session_id: String, call_id: String, tool: String, ok: bool },
    /// A `bash` run printed a local dev-server URL; the GUI offers it as a chip
    /// and can open it in the preview panel (A-B5).
    DevServer { session_id: String, url: String },
    ApprovalNeeded {
        session_id: String,
        approval_id: String,
        tool: String,
        detail: String,
        /// New (B-22): what the call will change (diff-like text) for
        /// `write`/`edit`; empty for other tools. `detail` stays the key that
        /// permission rules and "always allow" match on.
        #[serde(default)]
        preview: String,
    },
    /// An approval request is no longer pending (answered, timed out,
    /// aborted, or released by turbo). UIs drop the matching modal.
    ApprovalResolved {
        session_id: String,
        approval_id: String,
        allowed: bool,
        /// "user" | "always" | "timeout" | "abort" | "turbo"
        reason: String,
    },
    RunFinished { session_id: String, ok: bool, message: Option<String> },
    SessionCreated { session_id: String, title: String },
    /// M2: a fork/resume switched the client's session — full replay + mode.
    SessionLoaded {
        session_id: String,
        mode: String,
        transcript: Vec<TranscriptLine>,
    },
    /// M3: inline `/edit` produced a diff preview awaiting accept/reject.
    EditPreview {
        session_id: String,
        path: String,
        diff: Vec<String>,
        old: String,
        new: String,
        line: usize,
    },
    /// M3: an inline edit was applied — records a next-edit jump site.
    EditApplied { session_id: String, path: String, line: usize },
    /// M5: periodic endpoint probe — keeps quick tunnels warm and drives the
    /// TUI's `api ok/DOWN` header indicator.
    Heartbeat {
        ok: bool,
        latency_ms: u64,
        /// What the check asked and got, e.g. "GET https://x/v1/models → 404".
        #[serde(default)]
        detail: String,
    },
    /// Laziness Meter: a senior-dev note (reuse hint / overbuild flag / info).
    Note { session_id: String, text: String },
    /// Laziness Meter: live per-session line totals for the status bar.
    Stats { session_id: String, added: u64, deleted: u64, hints: u64 },
    /// M6: `/sessions` — the store's sessions, newest first, for the picker.
    SessionList { sessions: Vec<SessionInfo> },
    /// M6: `/model` — the active model plus what the endpoint advertises.
    ModelInfo { current: String, available: Vec<String> },
    /// M7: live status of a spawned subagent — one card in the agents panel.
    /// `state` is "working", "done" or "failed".
    /// `ask_user`: a question for the user, with answers they can click.
    /// The run ends; the answer arrives as the next user message.
    Question { session_id: String, question: String, options: Vec<String> },
    AgentStatus {
        agent_id: String,
        /// The chat that spawned it.
        #[serde(default)]
        parent_id: String,
        name: String,
        task: String,
        state: String,
        detail: String,
    },
}

/// M6: one row of the `/sessions` picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub updated_at: i64,
}

/// One replayed transcript line for [`ServerEvent::SessionLoaded`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub role: String,
    pub text: String,
}

/// Laziness Meter: per-session run totals (across all runs on the session).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunStats {
    pub added: u64,
    pub deleted: u64,
    pub writes: u64,
    pub edits: u64,
    pub bash_runs: u64,
    pub hints: u64,
}

/// New (B-10): how a pending approval ended. Sent through the approval's
/// channel so the waiting run can report it in `ApprovalResolved`.
#[derive(Debug, Clone, Copy)]
pub struct Resolution {
    pub allowed: bool,
    /// "user" | "always" | "turbo"
    pub reason: &'static str,
}

pub struct ApprovalEntry {
    pub tx: oneshot::Sender<Resolution>,
    pub tool: String,
    pub detail: String,
    /// New (B-09): the `ApprovalNeeded` event that announced this request,
    /// kept so `pending_approvals` can send it again.
    pub request: ServerEvent,
}

pub struct AppState {
    pub cfg: Config,
    pub db: Db,
    pub workspace: PathBuf,
    /// Where snapshots/config state live (default state dir).
    pub state_dir: PathBuf,
    pub engine: Engine,
    pub events: broadcast::Sender<ServerEvent>,
    /// approval_id -> resolver. Core is the authority: TUI is just a client.
    pub approvals: Mutex<HashMap<String, ApprovalEntry>>,
    pub busy: RwLock<HashMap<String, Arc<AtomicBool>>>, // session_id -> abort flag
    /// Laziness Meter: session_id -> run totals.
    pub stats: Mutex<HashMap<String, RunStats>>,
    pub client: AsyncMutex<Client>,
    /// M2: per-session plan/build mode (in-memory; resets on restart).
    pub modes: Mutex<HashMap<String, AgentMode>>,
    /// P-05: per-session ponytail level (in-memory; default from config).
    pub ponytail_levels: Mutex<HashMap<String, agents::PonytailLevel>>,
    /// M5: per-session todo lists, stored in SQLite (F-11).
    pub todo_store: TodoStore,
    /// Runtime autonomy mode (not per-session): the composer selector and
    /// `/turbo` write it; permission checks and auto-continuation read it.
    pub perm_mode: AtomicU8,
    /// M2: connected MCP servers (child processes + cached tool lists).
    pub mcp: AsyncMutex<crate::mcp::Registry>,
    /// Where installed skills live (user config dir) — install and the system
    /// prompt both read through this one base.
    pub skill_base: PathBuf,
    /// Goal B: the GUI's preview webview, when one is running. Headless and
    /// TUI runs have none, and `browser_*` tools say so instead of hanging.
    pub browser: Mutex<Option<crate::browser::Handle>>,
    /// The model's context window as the provider reports it in `/models`
    /// (`context_window`, `context_length`…). 0 = not known.
    pub detected_context: std::sync::atomic::AtomicUsize,
    /// Does the server pass a `system` message on to the model?
    /// [`SYSTEM_UNKNOWN`] = not asked yet, [`SYSTEM_DELIVERS`] = yes,
    /// [`SYSTEM_DROPS`] = no, the role is silently discarded.
    pub system_delivery: std::sync::atomic::AtomicU8,
    /// The user's own prompt from Settings: (replaces Habi's base
    /// instructions, added after them). Starts from the config
    /// (`agent.system_prompt`, `agent.extra_prompt`) and changes live.
    pub user_prompt: std::sync::RwLock<(Option<String>, Option<String>)>,
    /// Chats whose approvals are automatic (a running mission), with how
    /// many grants hold each. Their child chats count too. Per chat, unlike
    /// `perm_mode`: a mission must not auto-approve the user's other chats.
    pub auto_approve: Mutex<HashMap<String, u32>>,
    /// T-011: chat → its project's folder (`None` = no project), so
    /// `workspace_for` asks the database once per chat. Dropped per chat by
    /// `forget_session`, and wholly by `forget_workspaces` when chats move.
    pub workspaces: Mutex<HashMap<String, Option<PathBuf>>>,
    /// T-010: chat → the chat it was started from. A chat's parent never
    /// changes, so the auto-approval walk reads each link from the database
    /// once.
    pub parents: Mutex<HashMap<String, Option<String>>>,
}

/// `system_delivery`: not probed yet — the prompt travels as a user note,
/// which every server passes on, until the probe answers.
pub const SYSTEM_UNKNOWN: u8 = 0;
/// `system_delivery`: the server delivers `system` messages (the normal case).
pub const SYSTEM_DELIVERS: u8 = 1;
/// `system_delivery`: the server drops the role, so the prompt must be sent
/// in a message it does deliver (seen 2026-10-26: a gateway in front of
/// Ollama discarded the role, and with it the whole prompt — mode rules,
/// tool guide, AGENTS.md — while answering cheerfully with none of them).
pub const SYSTEM_DROPS: u8 = 2;

pub type Shared = Arc<AppState>;

/// New (B-09): how many events the bus keeps for a slow client. Every
/// streamed text piece is one event, so 256 was easy to overrun.
pub const EVENT_BUFFER: usize = 4096;

impl AppState {
    /// New (B-09): every approval request still waiting for an answer, as
    /// the `ApprovalNeeded` events that announced them. A client that fell
    /// behind the event bus calls this to show the prompts again.
    pub fn pending_approvals(&self) -> Vec<ServerEvent> {
        self.approvals
            .lock()
            .map(|m| m.values().map(|e| e.request.clone()).collect())
            .unwrap_or_default()
    }
}

pub async fn build_state(cfg: Config, workspace: PathBuf) -> Result<Shared> {
    let state_dir = crate::config::default_state_dir();
    let db = Db::open(&state_dir.join("opencode.db"))?;
    // Projects: the folder the app starts in is one; new chats go there.
    let start = db.upsert_project(&workspace)?;
    db.set_default_project(Some(&start.id));
    let todo_store = TodoStore::open(&state_dir.join("opencode.db"))?;
    let engine = Engine::new(&cfg.permissions);
    let client = Client::from_cfg(&cfg.effective_base_url()?, &cfg.provider)?;
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let mut mcp = crate::mcp::Registry::default();
    mcp.start_all(&cfg.mcp.servers).await;
    let skill_base = skills::skill_base(&state_dir);
    let cfg_prompt = (cfg.agent.system_prompt.clone(), cfg.agent.extra_prompt.clone());
    Ok(Arc::new(AppState {
        engine,
        events,
        approvals: Mutex::new(HashMap::new()),
        busy: RwLock::new(HashMap::new()),
        stats: Mutex::new(HashMap::new()),
        db,
        cfg,
        workspace,
        state_dir,
        modes: Mutex::new(HashMap::new()),
        ponytail_levels: Mutex::new(HashMap::new()),
        client: AsyncMutex::new(client),
        todo_store,
        perm_mode: AtomicU8::new(PermissionMode::Default as u8),
        skill_base,
        mcp: AsyncMutex::new(mcp),
        browser: Mutex::new(None),
        detected_context: std::sync::atomic::AtomicUsize::new(0),
        system_delivery: std::sync::atomic::AtomicU8::new(SYSTEM_UNKNOWN),
        user_prompt: std::sync::RwLock::new((cfg_prompt.0, cfg_prompt.1)),
        auto_approve: Mutex::new(HashMap::new()),
        workspaces: Mutex::new(HashMap::new()),
        parents: Mutex::new(HashMap::new()),
    }))
}

/// Test/ephemeral state with in-memory DB. Uses the config's provider so tests
/// can point it at a local mock server; falls back to a harmless stub on error.
/// Skills resolve against the real user config dir (parallel workers share the
/// user's baked skills); use [`build_state_ephemeral_skills`] to isolate them.
pub async fn build_state_ephemeral(cfg: Config, workspace: PathBuf) -> Shared {
    let base = skills::skill_base(&crate::config::default_state_dir());
    build_state_ephemeral_skills(cfg, workspace, base).await
}

/// Ephemeral state with an explicit skill base — tests install skills without
/// touching the user's real config dir.
pub async fn build_state_ephemeral_skills(
    cfg: Config,
    workspace: PathBuf,
    skill_base: PathBuf,
) -> Shared {
    let db = Db::open_in_memory().expect("in-memory db");
    if let Ok(p) = db.upsert_project(&workspace) {
        db.set_default_project(Some(&p.id));
    }
    build_state_ephemeral_db(cfg, workspace, skill_base, db).await
}

/// Ephemeral state on a database you give (tests that need a file DB).
pub async fn build_state_ephemeral_db(cfg: Config, workspace: PathBuf, skill_base: PathBuf, db: Db) -> Shared {
    let engine = Engine::new(&cfg.permissions);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let client = Client::from_cfg(cfg.provider.base_url.trim().trim_end_matches('/'), &cfg.provider)
        .unwrap_or_else(|_| Client::new("http://127.0.0.1:9", "", "stub", 5).expect("stub"));
    let mut mcp = crate::mcp::Registry::default();
    mcp.start_all(&cfg.mcp.servers).await;
    let cfg_prompt = (cfg.agent.system_prompt.clone(), cfg.agent.extra_prompt.clone());
    Arc::new(AppState {
        engine,
        events,
        approvals: Mutex::new(HashMap::new()),
        busy: RwLock::new(HashMap::new()),
        stats: Mutex::new(HashMap::new()),
        db,
        cfg,
        workspace,
        // New (B-25): unit tests must not write snapshots or worktrees into
        // the user's real state folder.
        state_dir: if cfg!(test) {
            std::env::temp_dir().join(format!("ocrs-test-state-{}", std::process::id()))
        } else {
            crate::config::default_state_dir()
        },
        modes: Mutex::new(HashMap::new()),
        ponytail_levels: Mutex::new(HashMap::new()),
        client: AsyncMutex::new(client),
        todo_store: TodoStore::default(),
        perm_mode: AtomicU8::new(PermissionMode::Default as u8),
        skill_base,
        mcp: AsyncMutex::new(mcp),
        browser: Mutex::new(None),
        detected_context: std::sync::atomic::AtomicUsize::new(0),
        system_delivery: std::sync::atomic::AtomicU8::new(SYSTEM_UNKNOWN),
        user_prompt: std::sync::RwLock::new((cfg_prompt.0, cfg_prompt.1)),
        auto_approve: Mutex::new(HashMap::new()),
        workspaces: Mutex::new(HashMap::new()),
        parents: Mutex::new(HashMap::new()),
    })
}

impl AppState {
    /// Resolve a pending approval. Same logic as POST /api/approvals/{id},
    /// available in-process for the headless path.
    pub fn decide(&self, approval_id: &str, allow: bool, always: bool) -> bool {
        let entry = {
            let mut map = match self.approvals.lock() {
                Ok(m) => m,
                Err(_) => return false,
            };
            map.remove(approval_id)
        };
        let Some(entry) = entry else { return false };
        if allow && always {
            let folder = match &entry.request {
                ServerEvent::ApprovalNeeded { session_id, .. } => self.workspace_for(session_id),
                _ => self.workspace.clone(),
            };
            let ws = crate::db::workspace_key(&folder);
            // T-058: the run is still allowed, but the user must be able to
            // see why the next one asks again.
            if let Err(e) = self.db.add_approval(&ws, &entry.tool, &entry.detail, "session") {
                core_log(&format!(
                    "[core] saving \"always allow\" for {} `{}` in {ws} failed: {e:#}",
                    entry.tool, entry.detail
                ));
            }
        }
        let reason = if allow && always { "always" } else { "user" };
        entry.tx.send(Resolution { allowed: allow, reason }).is_ok()
    }

    /// Plan/build mode for a session (defaults to Build).
    pub fn mode_for(&self, session_id: &str) -> AgentMode {
        self.modes
            .lock()
            .ok()
            .and_then(|m| m.get(session_id).copied())
            .unwrap_or_default()
    }

    pub fn set_mode(&self, session_id: &str, mode: AgentMode) {
        if let Ok(mut m) = self.modes.lock() {
            m.insert(session_id.to_string(), mode);
        }
    }

    /// P-05: this session's ponytail level (default: config `agent.ponytail`).
    pub fn ponytail_level(&self, session_id: &str) -> agents::PonytailLevel {
        self.ponytail_levels
            .lock()
            .ok()
            .and_then(|m| m.get(session_id).copied())
            .unwrap_or_else(|| agents::PonytailLevel::from_config(self.cfg.agent.ponytail))
    }

    /// P-05: `/ponytail <level>` for one session (in-memory, like the mode).
    pub fn set_ponytail_level(&self, session_id: &str, level: agents::PonytailLevel) {
        if let Ok(mut m) = self.ponytail_levels.lock() {
            m.insert(session_id.to_string(), level);
        }
    }

    /// Current autonomy mode.
    pub fn perm_mode(&self) -> PermissionMode {
        PermissionMode::from_u8(self.perm_mode.load(Ordering::SeqCst))
    }

    /// Switch modes. Switching to Turbo is a blanket grant, so anything already
    /// blocked on a prompt is approved immediately — the returned ids let the
    /// client drop its modal instead of waiting out the timeout.
    pub fn set_perm_mode(&self, mode: PermissionMode) -> Vec<String> {
        self.perm_mode.store(mode as u8, Ordering::SeqCst);
        if mode != PermissionMode::Turbo {
            return Vec::new();
        }
        let drained: Vec<(String, ApprovalEntry)> = match self.approvals.lock() {
            Ok(mut map) => map.drain().collect(),
            Err(_) => return Vec::new(),
        };
        let mut ids = Vec::with_capacity(drained.len());
        for (id, entry) in drained {
            let _ = entry.tx.send(Resolution { allowed: true, reason: "turbo" });
            ids.push(id);
        }
        ids
    }

    /// Turbo is the "full access" slice of the mode.
    pub fn turbo(&self) -> bool {
        self.perm_mode() == PermissionMode::Turbo
    }

    /// `/turbo`: on from any mode, off back to the config-driven default.
    pub fn toggle_turbo(&self) -> bool {
        let on = !self.turbo();
        self.set_perm_mode(if on { PermissionMode::Turbo } else { PermissionMode::Default });
        on
    }

    /// The mode chat `session_id` runs in: the user's mode, or Turbo while
    /// the chat (or a chat it was started from) holds an [`AutoApprove`]
    /// grant. Deny rules still apply in every mode.
    pub fn perm_mode_for(&self, session_id: &str) -> PermissionMode {
        let mode = self.perm_mode();
        if mode != PermissionMode::Turbo && self.auto_approved(session_id) {
            return PermissionMode::Turbo;
        }
        mode
    }

    /// Does `session_id` or one of its parent chats hold an auto-approval?
    fn auto_approved(&self, session_id: &str) -> bool {
        let granted: Vec<String> = match self.auto_approve.lock() {
            Ok(m) if !m.is_empty() => m.keys().cloned().collect(),
            _ => return false,
        };
        let mut id = session_id.to_string();
        // Missions nest a few levels (step → subagent); a cycle cannot loop.
        for _ in 0..16 {
            if granted.contains(&id) {
                return true;
            }
            match self.parent_of(&id) {
                Some(parent) => id = parent,
                None => return false,
            }
        }
        false
    }

    /// The chat `session_id` was started from, from the [`AppState::parents`]
    /// cache; a miss asks the database once. An unknown chat or a failed
    /// read is not cached (the chat may not be written yet).
    fn parent_of(&self, session_id: &str) -> Option<String> {
        if let Some(known) = self.parents.lock().ok().and_then(|m| m.get(session_id).cloned()) {
            return known;
        }
        let parent = self.db.get_session(session_id).ok().flatten()?.parent_id;
        if let Ok(mut m) = self.parents.lock() {
            m.insert(session_id.to_string(), parent.clone());
        }
        parent
    }

    /// Automatic approvals for chat `session_id` and its child chats until
    /// the returned guard is dropped (also on an early return or a panic).
    /// Other chats and their waiting approvals are not touched.
    pub fn auto_approve_session(self: &Shared, session_id: &str) -> AutoApprove {
        if let Ok(mut m) = self.auto_approve.lock() {
            *m.entry(session_id.to_string()).or_insert(0) += 1;
        }
        AutoApprove { st: self.clone(), session_id: session_id.to_string() }
    }

    /// A deleted chat's in-memory state: stats, mode, ponytail level, grants
    /// and the per-chat caches, so long-running servers do not keep them.
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut m) = self.stats.lock() {
            m.remove(session_id);
        }
        if let Ok(mut m) = self.modes.lock() {
            m.remove(session_id);
        }
        if let Ok(mut m) = self.ponytail_levels.lock() {
            m.remove(session_id);
        }
        if let Ok(mut m) = self.auto_approve.lock() {
            m.remove(session_id);
        }
        if let Ok(mut m) = self.workspaces.lock() {
            m.remove(session_id);
        }
        if let Ok(mut m) = self.parents.lock() {
            m.remove(session_id);
        }
        if let Some(Ok(mut m)) = FROZEN_LESSONS.get().map(|c| c.lock()) {
            m.remove(session_id);
        }
        if let Some(Ok(mut m)) = COMPACT_FAILED_AT.get().map(|c| c.lock()) {
            m.remove(session_id);
        }
    }
}

/// Lessons per chat, read once (see `composed_system`).
static FROZEN_LESSONS: std::sync::OnceLock<Mutex<HashMap<String, Option<String>>>> = std::sync::OnceLock::new();
/// Message count of the chat's last failed automatic compaction.
static COMPACT_FAILED_AT: std::sync::OnceLock<Mutex<HashMap<String, usize>>> = std::sync::OnceLock::new();

/// A mission's automatic approvals ([`AppState::auto_approve_session`]),
/// taken back when dropped.
pub struct AutoApprove {
    st: Shared,
    session_id: String,
}

impl Drop for AutoApprove {
    fn drop(&mut self) {
        if let Ok(mut m) = self.st.auto_approve.lock() {
            if let Some(n) = m.get_mut(&self.session_id) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&self.session_id);
                }
            }
        }
    }
}

impl AppState {

    /// M5: probe `{base}/models` every 15s and emit `Heartbeat`. Only a 2xx
    /// answer counts as up: Cloudflare error pages (530/1033) and auth
    /// errors (401) arrive fine at the transport level and must show DOWN.
    /// New (B-08): sends the API key like the real client, and uses the same
    /// base URL rule (`effective_base_url`). Also keeps Cloudflare quick
    /// tunnels warm between prompts.
    pub fn start_heartbeat(self: &Shared) {
        let st = self.clone();
        tokio::spawn(async move {
            let Ok(base) = st.cfg.effective_base_url() else {
                return;
            };
            let key = st.cfg.provider.api_key.clone();
            // A connect timeout of its own: reqwest splits it over the
            // host's addresses, so one dead address (seen with Cloudflare
            // tunnels: one of two IPs never answers on some networks) costs
            // 2 s and the next is tried, instead of using up the whole check.
            let http = match reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(4))
                .timeout(std::time::Duration::from_secs(12))
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };
            let mut last_ok = None;
            loop {
                let t0 = std::time::Instant::now();
                // Require 2xx: Cloudflare error pages (530/1030/1033) arrive
                // fine at the transport level and must count as DOWN.
                // F-02: Bearer for OpenAI-style servers, x-api-key for Anthropic.
                let req = crate::provider::with_auth(
                    http.get(format!("{base}/models")),
                    st.cfg.provider.api,
                    &key,
                );
                let (mut ok, got) = match req.send().await {
                    Ok(r) => {
                        let ok = r.status().is_success();
                        let status = r.status().to_string();
                        if ok {
                            let body = r.json::<Value>().await.ok();
                            st.note_context(body.as_ref(), &http, &base, &key).await;
                        }
                        (ok, status)
                    }
                    Err(e) => (false, crate::provider::send_error_text(&e)),
                };
                let mut got = got;
                if ok && st.client.lock().await.key_rejected.load(Ordering::Relaxed) {
                    ok = false;
                    got = format!("{got}, but chat requests get 401/403: the provider rejected the API key");
                }
                let latency_ms = t0.elapsed().as_millis() as u64;
                let detail = format!("GET {base}/models → {got} ({latency_ms} ms)");
                // Say it once per change, so a failing check is never silent.
                // To the log only: this runs while the TUI owns the terminal,
                // and the event's `detail` already shows the reason there.
                if last_ok != Some(ok) {
                    core_log(&format!("[api] {} — {detail}", if ok { "online" } else { "offline" }));
                    last_ok = Some(ok);
                }
                let _ = st.events.send(ServerEvent::Heartbeat { ok, latency_ms, detail });
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });
    }

    /// Learn the context window a `/models` answer reports and store it, so
    /// every limit-aware guard works: fitting tool results, auto-compaction
    /// and the output budget all read it, and all stay off while it is 0.
    ///
    /// Ollama's OpenAI endpoint carries no context metadata at all; its
    /// native `/api/ps` does (`context_length`). Without that fallback the
    /// window is never known on a local Ollama.
    async fn note_context(
        self: &Shared,
        body: Option<&Value>,
        http: &reqwest::Client,
        base: &str,
        key: &str,
    ) -> Option<usize> {
        let model = self.current_model().await;
        let mut known = body.and_then(|b| context_from_models(b, &model));
        if known.is_none()
            && self.detected_context.load(Ordering::Relaxed) == 0
            && matches!(self.cfg.provider.api, crate::config::ProviderApi::OpenAi)
        {
            known = probe_ollama_context(http, base, key, &model).await;
        }
        if let Some(n) = known {
            self.detected_context.store(n, Ordering::Relaxed);
            self.client.lock().await.set_context_limit(n);
        }
        known
    }

    /// One `/models` call at startup.
    ///
    /// The heartbeat is what learns the window, and a headless run never
    /// starts it (nothing displays the health line) — so `context_limit`
    /// stayed 0, every limit-aware guard stayed off, and a chat that grew
    /// past the model's window was silently trimmed by the server. That trim
    /// takes the OLDEST messages, the user's request among them, and chat
    /// templates like Qwen's then fail the request with "No user query found
    /// in messages". Learned once up front, the guards work in every run
    /// (seen 2026-10-26 against a 16384-token model).
    pub fn start_context_probe(self: &Shared) {
        let st = self.clone();
        // In the background of a TUI/GUI run: findings go to the log only.
        tokio::spawn(async move { st.startup_probes(false).await });
    }

    /// The two questions worth asking a server before using it: how big the
    /// model's context window is, and whether it delivers `system` messages.
    /// An interactive run asks in the background; a headless run asks first,
    /// so its very first request is already packed for the server it has.
    /// Headless calls this before its first request; nothing else owns the
    /// terminal then, so the findings are printed too.
    pub async fn run_startup_probes(self: &Shared) {
        self.startup_probes(true).await
    }

    /// The probes; `echo` also prints each finding to stderr (headless only).
    async fn startup_probes(self: &Shared, echo: bool) {
        let Ok(base) = self.cfg.effective_base_url() else { return };
        let key = self.cfg.provider.api_key.clone();
        let Ok(http) = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(4))
            .timeout(std::time::Duration::from_secs(12))
            .build()
        else {
            return;
        };
        let req = crate::provider::with_auth(
            http.get(format!("{base}/models")),
            self.cfg.provider.api,
            &key,
        );
        let body = match req.send().await {
            Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
            _ => None,
        };
        if let Some(n) = self.note_context(body.as_ref(), &http, &base, &key).await {
            probe_note(echo, &format!("[api] model context window: {n} tokens"));
        }
        self.probe_system_delivery(&base, &key, echo).await;
    }

    /// One tiny question: does this server pass a `system` message on to the
    /// model? The answer is stored in [`AppState::system_delivery`] and read
    /// when the prompt is packed into a request.
    ///
    /// Seen 2026-10-26: a gateway in front of Ollama dropped the role without
    /// an error — the prompt, the tool guide and AGENTS.md all stayed behind,
    /// and the model answered cheerfully with none of them. Nothing in a
    /// `/models` answer says so, so the only way to know is to ask.
    ///
    /// `None` = could not tell (offline, timeout, refused). The state then
    /// stays [`SYSTEM_UNKNOWN`] and the prompt keeps travelling as a trailing
    /// user note, which every server passes on.
    async fn probe_system_delivery(&self, base: &str, key: &str, echo: bool) -> Option<()> {
        // Anthropic's protocol has its own top-level `system` field, filled in
        // by the API itself: there is no role in the message list to lose.
        if self.cfg.provider.api != crate::config::ProviderApi::OpenAi {
            self.system_delivery.store(SYSTEM_DELIVERS, Ordering::Relaxed);
            return Some(());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(4))
            .timeout(std::time::Duration::from_secs(45))
            .build()
            .ok()?;
        let body = json!({
            "model": self.current_model().await,
            "messages": [
                {"role": "system", "content": "Reply to every message with exactly the word ARCHITECT."},
                {"role": "user", "content": "Say hello."}
            ],
            "temperature": 0,
            "stream": false
        });
        let req = crate::provider::with_auth(
            http.post(format!("{base}/chat/completions")),
            self.cfg.provider.api,
            key,
        )
        .json(&body);
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        let text = v.pointer("/choices/0/message/content").and_then(Value::as_str)?;
        let delivers = text.to_ascii_uppercase().contains("ARCHITECT");
        self.system_delivery
            .store(if delivers { SYSTEM_DELIVERS } else { SYSTEM_DROPS }, Ordering::Relaxed);
        let found = if delivers {
            "system messages reach the model"
        } else {
            "this server drops system messages — the prompt travels as a user note"
        };
        probe_note(echo, &format!("[api] {found}"));
        Some(())
    }
}

/// T-054: a startup probe's finding, to core.log always and to stderr only
/// when nothing else draws on the terminal (headless).
fn probe_note(echo: bool, line: &str) {
    if echo {
        eprintln!("{line}");
    }
    core_log(line);
}

/// Where [`core_log`] writes: `<state_dir>/core.log`. Unit tests get a
/// temp folder of their own, so they never touch the user's log (T-145).
pub fn core_log_path() -> PathBuf {
    let dir = if cfg!(test) {
        std::env::temp_dir().join(format!("ocrs-test-state-{}", std::process::id()))
    } else {
        crate::config::default_state_dir()
    };
    dir.join("core.log")
}

/// T-055: core.log is moved to core.log.1 (replacing the older one) once it
/// passes this size, so a long-lived install does not grow it forever.
const CORE_LOG_MAX: u64 = 2 * 1024 * 1024;

/// New (B-48): append one line to `<state_dir>/core.log`. Use this instead
/// of `eprintln!` in code that can run while the TUI owns the terminal.
pub fn core_log(msg: &str) {
    // Opened once and kept: this runs on async threads, and reopening the
    // file (plus create_dir_all) on every line was a blocking call each time.
    static LOG: Mutex<Option<(std::fs::File, u64)>> = Mutex::new(None);
    let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    append_log(&mut log, &core_log_path(), CORE_LOG_MAX, &format!("[{stamp}] {msg}"));
}

/// One line to the log at `path` through the kept handle in `slot` (file and
/// its size), opening it when needed and rotating it to `<path>.1` once it
/// has reached `max` bytes.
fn append_log(slot: &mut Option<(std::fs::File, u64)>, path: &std::path::Path, max: u64, line: &str) {
    use std::io::Write;
    if slot.is_none() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        *slot = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(|f| {
                let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                (f, size)
            });
    }
    if slot.as_ref().is_some_and(|(_, size)| *size >= max) {
        // Closed first: Windows cannot rename a file that is open.
        *slot = None;
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(path, &old);
        *slot = std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(|f| (f, 0));
    }
    if let Some((f, size)) = slot.as_mut() {
        // A failed write (file deleted, disk full) reopens next time.
        match writeln!(f, "{line}") {
            Ok(()) => *size += line.len() as u64 + 1,
            Err(_) => *slot = None,
        }
    }
}

tokio::task_local! {
    /// What the model requests inside this scope are for, and the chat they
    /// belong to (one-shot calls have no session of their own): the usage
    /// dashboard's "what eats the tokens". Outside any scope: plain chat.
    pub static USAGE_TAG: (&'static str, Option<String>);
}

fn usage_tag() -> (&'static str, Option<String>) {
    USAGE_TAG.try_with(|t| t.clone()).unwrap_or(("chat", None))
}

/// Run `fut` with its model requests counted as `purpose` (in chat `session`).
pub async fn tagged<F: std::future::Future>(purpose: &'static str, session: &str, fut: F) -> F::Output {
    USAGE_TAG.scope((purpose, Some(session.to_string()).filter(|s| !s.is_empty())), fut).await
}

/// M3: one non-streamed completion with NO tools — the inline-edit engine
/// and the team planner/reviewer use this instead of a full agent loop.
pub async fn one_shot(
    st: &Shared,
    system: &str,
    user: &str,
    abort: &AtomicBool,
    prefer_small: bool,
) -> Result<String> {
    if abort.load(Ordering::SeqCst) {
        anyhow::bail!("aborted");
    }
    let mut msgs = vec![
        ChatMsg {
            role: "system".into(),
            content: Some(system.to_string()),
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMsg {
            role: "user".into(),
            content: Some(user.to_string()),
            tool_calls: None,
            tool_call_id: None,
        },
    ];
    deliver_system_prompt(&mut msgs, st);
    // B-29: copy the client out of the lock; never hold it while waiting.
    let client = st.client.lock().await.clone();
    let model = if prefer_small {
        st.cfg.provider.small_model.clone().unwrap_or_else(|| client.model.clone())
    } else {
        client.model.clone()
    };
    let mut stream = client.stream(&model, &msgs, &[]).await?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        if abort.load(Ordering::SeqCst) {
            anyhow::bail!("aborted");
        }
        match ev? {
            StreamEvent::Text(t) => text.push_str(&t),
            StreamEvent::Usage(u) => st.record_usage("", &model, &u),
            StreamEvent::Done { finish_reason } => {
                // B-03: half an answer must not be used as a whole one.
                if finish_reason.as_deref() == Some("length") {
                    anyhow::bail!(
                        "the model's reply was cut off by its output token limit \
                         (finish_reason = length); the incomplete result was not used"
                    );
                }
                break;
            }
            _ => {}
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod b29_tests {
    use super::*;

    /// A request whose prompt is "slow" waits 1.5 s before the provider
    /// answers. A second, fast request must not wait for it.
    #[tokio::test]
    async fn a_slow_provider_call_does_not_block_other_calls() {
        use axum::{routing::post, Router};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<Value>| async move {
                let user = body["messages"][1]["content"].as_str().unwrap_or("").to_string();
                if user == "slow" {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                }
                let chunk = json!({"choices": [{"delta": {"content": user}, "finish_reason": "stop"}]});
                let payload = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = format!("http://{addr}/v1");
        cfg.provider.model = "mock".into();
        let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;

        let st_slow = st.clone();
        let slow = tokio::spawn(async move {
            one_shot(&st_slow, "sys", "slow", &AtomicBool::new(false), false).await
        });
        // Let the slow call take the lock and send its request first.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let started = std::time::Instant::now();
        let fast = one_shot(&st, "sys", "fast", &AtomicBool::new(false), false).await.unwrap();
        let waited = started.elapsed();
        assert_eq!(fast, "fast");
        assert!(
            waited < std::time::Duration::from_millis(1000),
            "fast call waited {waited:?} for the slow one"
        );
        assert_eq!(slow.await.unwrap().unwrap(), "slow");
    }
}

/// Regression tests for the server review fixes (fix/server).
#[cfg(test)]
mod review_fix_tests {
    use super::*;
    use serde_json::json;

    /// Invisible characters taken out after streaming move the shown mark by
    /// what was taken out of the shown part, not to `min(len)`.
    #[test]
    fn stripping_keeps_the_shown_mark_in_place() {
        // "a\u{200b}b" streamed (5 bytes), then "cd" not yet shown.
        let (t, shown) = strip_streamed("a\u{200b}bcd", 5);
        assert_eq!((t.as_str(), shown), ("abcd", 2), "the rest to send is `cd`, once");
        let (t, shown) = strip_streamed("abc\u{feff}", 3);
        assert_eq!((t.as_str(), shown), ("abc", 3));
        let (t, shown) = strip_streamed("\u{2060}x", 0);
        assert_eq!((t.as_str(), shown), ("x", 0));
    }

    /// A user message typed to look like the app's own is stored guarded,
    /// so it neither counts as a turbo continuation nor hides the request.
    #[test]
    fn user_text_cannot_pass_for_the_apps() {
        for typed in ["[app] do it", "TURBO-CONTINUE: go", "[verify] ok", "  [app] x"] {
            let stored = neutralize_user_text(typed);
            assert!(stored.starts_with(USER_TEXT_GUARD), "{typed}");
            assert!(!APP_MARKERS.iter().any(|m| stored.starts_with(m)), "{typed}");
        }
        assert_eq!(neutralize_user_text("fix the bug"), "fix the bug", "ordinary text is unchanged");
        assert_eq!(rewind_key(&neutralize_user_text("[app] x")), rewind_key("[app] x"), "rewind still finds it");
        let msg = |text: &str| Message {
            id: "m".into(),
            session_id: "s".into(),
            role: "user".into(),
            parts: vec![Part::Text { text: text.into() }],
            created_at: 0,
        };
        let spoofed = vec![msg(&format!("{TURBO_CONTINUE_PREFIX} a")), msg(&neutralize_user_text(&format!("{TURBO_CONTINUE_PREFIX} b")))];
        assert_eq!(turbo_streak_in(&spoofed), 0, "the typed one is the user's and ends the streak");
    }

    /// Only jobs the agent started skip the approval; the user's terminal
    /// commands (`term<n>`) are asked about like any other call.
    #[test]
    fn only_agent_jobs_skip_the_permission_check() {
        assert!(agent_job(&json!({"job": "job3"})));
        assert!(!agent_job(&json!({"job": "term1"})));
        assert!(!agent_job(&json!({"job": "job"})));
        assert!(!agent_job(&json!({"job": "jobx"})));
        assert_eq!(job_detail("bash", &json!({"job": "term2", "stop": true})).as_deref(), Some("stop job term2"));
        assert_eq!(job_detail("bash", &json!({"job": "term2"})).as_deref(), Some("read job term2"));
        assert_eq!(job_detail("bash", &json!({"command": "ls"})), None);
    }

    /// One prompt gets a fixed number of extra passes; the user is told
    /// once when one is skipped.
    #[tokio::test]
    async fn extra_passes_per_prompt_are_capped() {
        let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
        let mut rx = st.events.subscribe();
        let budget = PassBudget::new();
        let shared = budget.clone();
        for _ in 0..MAX_EXTRA_PASSES {
            assert!(shared.take(&st, "s", "a fix round"));
        }
        assert!(!budget.take(&st, "s", "a fix round"), "a continuation shares the prompt's budget");
        assert!(!budget.take(&st, "s", "the design review"));
        assert!(spend(None, &st, "s", "x"), "missions and deep mode have their own limits");
        let mut notes = 0;
        while let Ok(ev) = rx.try_recv() {
            if let ServerEvent::Note { text, .. } = ev {
                assert!(text.contains("skipping a fix round"), "{text}");
                notes += 1;
            }
        }
        assert_eq!(notes, 1, "told once per prompt");
    }

    /// The scratch folder is held per project: another project's run does
    /// not keep it, and the last holder removes it.
    #[test]
    fn scratch_is_held_per_project() {
        let base = std::env::temp_dir().join(format!("ocrs-scratch-pp-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        scratch_hold(&a);
        scratch_hold(&b);
        scratch_hold(&a);
        scratch_let_go(&b);
        assert!(!scratch_dir(&b).exists(), "b's last run ended: gone, though a still runs");
        assert!(!b.join(".opencode").exists(), "the project is left as it was");
        scratch_let_go(&a);
        assert!(scratch_dir(&a).exists(), "a still has a run");
        scratch_let_go(&a);
        assert!(!scratch_dir(&a).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The project's own check command runs only with a bash approval; the
    /// one the user configured runs without asking.
    #[tokio::test]
    async fn auto_verify_asks_like_bash() {
        let mut cfg = crate::config::Config::default();
        cfg.permissions.approval_timeout_secs = 5;
        let st = build_state_ephemeral(cfg.clone(), std::env::temp_dir()).await;
        st.set_perm_mode(PermissionMode::Manual);
        let s = st.db.create_session("v", None).unwrap();
        let none = AtomicBool::new(false);
        let notes = Mutex::new(Vec::new());
        let note = |t: String| notes.lock().unwrap().push(t);
        let mut rx = st.events.subscribe();
        let (st2, sid) = (st.clone(), s.id.clone());
        let denier = tokio::spawn(async move {
            loop {
                if let Ok(ServerEvent::ApprovalNeeded { approval_id, .. }) = rx.recv().await {
                    st2.decide(&approval_id, false, false);
                    return;
                }
            }
        });
        assert!(!verify_permitted(&st, &sid, "cargo check", &note, &none).await, "asked, and denied");
        denier.await.unwrap();
        assert!(notes.lock().unwrap()[0].contains("not approved"));

        cfg.agent.verify_command = Some("make check".into());
        let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
        st.set_perm_mode(PermissionMode::Manual);
        assert!(verify_permitted(&st, &s.id, "make check", &note, &none).await, "the user's own command");
        st.set_perm_mode(PermissionMode::Turbo);
        assert!(verify_permitted(&st, &s.id, "cargo check", &note, &none).await, "turbo needs no prompt");
    }

    /// A run that panics still frees its chat's busy slot.
    #[tokio::test]
    async fn a_panicking_run_frees_its_slot() {
        let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
        st.kick_task("s".into(), |_, _, _| async move { panic!("run blew up") }).await;
        let freed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while st.run_active("s").await {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(freed.is_ok(), "the slot was left busy");
    }

    /// `signal_abort` wakes a waiter at once, not on the next poll.
    #[tokio::test]
    async fn an_abort_wakes_its_waiters_at_once() {
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        let waiter = tokio::spawn(async move { aborted(&f2).await });
        tokio::task::yield_now().await;
        let t = std::time::Instant::now();
        signal_abort(&flag);
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await.unwrap().unwrap();
        assert!(t.elapsed() < std::time::Duration::from_millis(400), "woken, not polled: {:?}", t.elapsed());
    }

    /// A deleted chat's in-memory state goes with it.
    #[tokio::test]
    async fn deleting_a_chat_forgets_its_state() {
        let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
        let s = st.db.create_session("x", None).unwrap();
        st.set_mode(&s.id, AgentMode::Plan);
        let grant = st.auto_approve_session(&s.id);
        std::mem::forget(grant);
        st.forget_session(&s.id);
        assert_eq!(st.mode_for(&s.id), AgentMode::default());
        assert!(st.auto_approve.lock().unwrap().is_empty());
    }

    /// An export cannot write outside the workspace through a linked folder.
    #[test]
    fn export_refuses_a_path_through_a_link() {
        let base = std::env::temp_dir().join(format!("ocrs-export-link-{}", std::process::id()));
        let (ws, outside) = (base.join("ws"), base.join("outside"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, ws.join("docs"));
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, ws.join("docs"));
        assert!(AppState::export_stays_inside(&ws, &ws.join("chat.md")).is_ok());
        if linked.is_ok() {
            let err = AppState::export_stays_inside(&ws, &ws.join("docs").join("chat.md")).unwrap_err();
            assert!(format!("{err}").contains("through a link"), "{err}");
        } // else: no right to make links here (Windows without developer mode)
        let _ = std::fs::remove_dir_all(&base);
    }
}
