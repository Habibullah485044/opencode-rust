//! TUI state: transcript/session types plus all input and event handling.
//! [`App`] owns the state; `on_key` turns keys into [`InputAction`]s and
//! `on_event` folds server events into the state.

use crate::agents::AgentMode;
use crate::commands::{builtins, classify, Command};
use crate::inline_edit::{JumpSite, JumpTracker};
use crate::server::{ServerEvent, SessionInfo};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod keys;
/// A line in the transcript.
#[derive(Clone)]
pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntryKind {
    User,
    Assistant,
    /// Streamed chain-of-thought — dim + italic so it reads as asides.
    Thinking,
    Tool,
    System,
    Error,
}

/// Pending approval shown as overlay (Freebuff's out-of-process dialog idea,
/// adapted: the TUI is dumb — the core is the authority; this just renders).
pub struct PendingApproval {
    pub id: String,
    pub tool: String,
    pub detail: String,
}

/// M3: an inline `/edit` diff waiting for accept/reject.
pub struct PendingEdit {
    pub path: String,
    pub diff: Vec<String>,
    pub old: String,
    pub new: String,
    pub line: usize,
}

/// Mission picker overlay (Freebuff's Missions dialog): choose the mission
/// kind and how much effort it deserves, then it runs as a well-worded brief.
pub struct MissionPicker {
    pub goal: String,
    pub kind_idx: usize,
    pub effort_idx: usize,
}

/// M6: the command palette. Open while a bare command word is being typed;
/// `matches` are indices into [`crate::commands::builtins`].
#[derive(Clone, Debug)]
pub struct Palette {
    pub matches: Vec<usize>,
    pub sel: usize,
}

/// M6: the `/sessions` browser overlay.
#[derive(Clone, Debug)]
pub struct SessionsOverlay {
    pub items: Vec<SessionInfo>,
    pub sel: usize,
}

/// Builtin indices whose verb starts with the command text being typed.
/// Empty unless the input is a bare command word with no arguments yet, so
/// the palette never competes with argument text.
pub fn palette_matches(input: &str) -> Vec<usize> {
    let Some(rest) = input.strip_prefix('/') else { return Vec::new() };
    if rest.contains(char::is_whitespace) {
        return Vec::new();
    }
    let q = rest.to_ascii_lowercase();
    builtins()
        .iter()
        .enumerate()
        .filter(|(_, b)| b.name.starts_with(&q))
        .map(|(i, _)| i)
        .collect()
}

/// Team-mode panel: one medium card per worker, fed by the `[team]` text
/// protocol the core already streams (`run_team` in team.rs). No new event
/// types — the TUI stays dumb and just parses what the core says.
#[derive(Clone, Debug, PartialEq)]
pub enum TeamStatus {
    Working,
    Done,
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum TeamPhase {
    #[default]
    Decomposing,
    Working,
    Reviewing,
    Done,
}

impl TeamPhase {
    pub fn label(&self) -> &'static str {
        match self {
            TeamPhase::Decomposing => "planning",
            TeamPhase::Working => "working",
            TeamPhase::Reviewing => "reviewing",
            TeamPhase::Done => "done",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TeamAgent {
    pub title: String,
    pub task: String,
    pub status: TeamStatus,
}

#[derive(Clone, Debug, Default)]
pub struct TeamState {
    pub goal: String,
    pub phase: TeamPhase,
    pub agents: Vec<TeamAgent>,
    pub verdict: Option<String>,
    /// No more plan-item lines once a worker reports or review starts.
    sealed: bool,
    /// Partial line held across streamed delta chunks.
    pending: String,
}

impl TeamState {
    /// Feed raw streamed text; only complete `\n`-terminated lines parse.
    pub fn feed(&mut self, text: &str) {
        self.pending.push_str(text);
        let mut rest = std::mem::take(&mut self.pending);
        while let Some(nl) = rest.find('\n') {
            let line = rest[..nl].to_string();
            rest = rest[nl + 1..].to_string();
            self.feed_line(line.trim());
        }
        self.pending = rest;
    }

    /// B-34: the team run is over (`RunFinished`). Show "done" even if no
    /// `results for:` line was streamed; on failure, workers that never
    /// reported are marked failed.
    pub fn finish(&mut self, ok: bool) {
        self.phase = TeamPhase::Done;
        self.sealed = true;
        if !ok {
            for a in &mut self.agents {
                if a.status == TeamStatus::Working {
                    a.status = TeamStatus::Failed;
                }
            }
        }
    }

    fn feed_line(&mut self, line: &str) {
        // Plan items look like `  2. Builder — create the modules…`.
        if !line.starts_with("[team]") {
            if self.phase == TeamPhase::Working && !self.sealed {
                if let Some(agent) = parse_plan_item(line) {
                    self.agents.push(agent);
                }
            }
            return;
        }
        let body = line.trim_start_matches("[team]").trim();
        if let Some(goal) = body.strip_prefix("decomposing goal:") {
            *self = TeamState { goal: goal.trim().to_string(), ..Default::default() };
        } else if body.starts_with("plan:") {
            self.phase = TeamPhase::Working;
        } else if let Some(v) = body.strip_prefix("reviewer:") {
            // B-34: the synthesis repeats this line after `results for:`;
            // it must not move a finished team back to "reviewing".
            if self.phase != TeamPhase::Done {
                self.phase = TeamPhase::Reviewing;
            }
            self.verdict = Some(v.trim().to_string());
            self.sealed = true;
        } else if body.starts_with("results for:") {
            self.phase = TeamPhase::Done;
            self.sealed = true;
        } else if let Some(rest) = body.strip_prefix("worker ") {
            self.sealed = true;
            if let Some((idx, status)) = parse_worker_line(rest) {
                if let Some(a) = self.agents.get_mut(idx) {
                    a.status = status;
                }
            }
        }
    }
}

/// M7: one spawned subagent in the agents panel (live status card).
#[derive(Clone, Debug)]
pub struct AgentCard {
    pub agent_id: String,
    pub name: String,
    pub task: String,
    pub state: TeamStatus,
    pub detail: String,
    /// When the panel first saw this agent — the card shows elapsed time.
    pub started: std::time::Instant,
    /// B-41: when the card first became Done or Failed; its clock stops here.
    pub finished: Option<std::time::Instant>,
}

/// `2. Builder — create the modules…` → agent (title, task). Strict enough
/// that model prose won't accidentally match: leading `N. ` + em-dash.
pub(super) fn parse_plan_item(line: &str) -> Option<TeamAgent> {
    let (num, rest) = line.split_once(". ")?;
    if num.trim().parse::<usize>().is_err() {
        return None;
    }
    let (title, task) = rest.split_once('—')?;
    let title = title.trim();
    let task = task.trim();
    if title.is_empty() || task.is_empty() {
        return None;
    }
    Some(TeamAgent { title: title.to_string(), task: task.to_string(), status: TeamStatus::Working })
}

/// `0 (sub) done` / `1 (sub) failed: …` → (index, status).
pub(super) fn parse_worker_line(rest: &str) -> Option<(usize, TeamStatus)> {
    let (idx, tail) = rest.split_once(" (")?;
    let idx = idx.trim().parse::<usize>().ok()?;
    // B-34: the error text after `failed:` may contain ") " itself.
    let status = if tail.contains(") failed: ") || tail.ends_with(") failed") {
        TeamStatus::Failed
    } else if tail.ends_with(") done") {
        TeamStatus::Done
    } else {
        return None;
    };
    Some((idx, status))
}

/// Greedy word-wrap to at most `max_lines` lines of `width` chars.
pub fn wrap_n(s: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut words = s.split_whitespace().peekable();
    while words.peek().is_some() {
        if lines.len() == max_lines {
            if let Some(last) = lines.last_mut() {
                if !last.ends_with('…') {
                    last.push('…');
                }
            }
            break;
        }
        let w = words.next().unwrap_or("");
        // B-35: terminal columns (wide characters count double).
        let need = unicode_width::UnicodeWidthStr::width(cur.as_str())
            + usize::from(!cur.is_empty())
            + unicode_width::UnicodeWidthStr::width(w);
        if need <= width {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(w);
        } else if cur.is_empty() {
            // One over-long word: hard-cut it to width - 1 columns (B-35).
            let mut cut = String::new();
            for ch in w.chars() {
                let next = unicode_width::UnicodeWidthStr::width(cut.as_str())
                    + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if next > width.saturating_sub(1) {
                    break;
                }
                cut.push(ch);
            }
            cur.push_str(&cut);
            cur.push('…');
            lines.push(std::mem::take(&mut cur));
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(w);
        }
    }
    if !cur.is_empty() {
        if lines.len() < max_lines {
            lines.push(cur);
        } else if let Some(last) = lines.last_mut() {
            // B-41: a word was cut off — say so, like the loop above does.
            if !last.ends_with('…') {
                last.push('…');
            }
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// B-35: the part of `input` that fits in `width` terminal columns with
/// the cursor visible, and the cursor's column inside that part. Columns
/// are display widths (日 = 2). A line break shows as ⏎, a tab as →.
pub fn input_view(input: &str, cursor: usize, width: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let shown: Vec<(char, usize)> = input
        .chars()
        .map(|c| {
            let c = match c {
                '\n' => '⏎',
                '\t' => '→',
                c if c.is_control() => '·',
                c => c,
            };
            (c, c.width().unwrap_or(0))
        })
        .collect();
    let width = width.max(1);
    let before: usize = shown.iter().take(cursor).map(|(_, w)| w).sum();
    // Skip whole characters from the left until the cursor fits.
    let need = before.saturating_sub(width - 1);
    let (mut start, mut skipped) = (0, 0);
    while start < shown.len() && skipped < need {
        skipped += shown[start].1;
        start += 1;
    }
    let mut out = String::new();
    let mut used = 0;
    for (c, w) in &shown[start..] {
        if used + w > width {
            break;
        }
        out.push(*c);
        used += w;
    }
    (out, before - skipped)
}

pub struct App {
    pub entries: Vec<Entry>,
    pub input: String,
    pub cursor: usize,
    pub approval: Option<PendingApproval>,
    pub busy: bool,
    /// B-15: approvals that arrived while another one was on screen.
    pub approval_queue: std::collections::VecDeque<PendingApproval>,
    /// B-15: rows the approval detail is scrolled down.
    pub approval_scroll: u16,
    /// T-099: the furthest useful `approval_scroll`; `draw` sets it each frame.
    pub approval_scroll_max: std::cell::Cell<u16>,
    /// B-15: when the current approval appeared (decision keys wait a moment).
    pub approval_since: Option<std::time::Instant>,
    pub should_quit: bool,
    pub session_id: Option<String>,
    pub scroll: u16,
    /// B-14: how far the transcript can scroll up, in screen rows. `draw`
    /// sets it every frame (it knows the wrapped height); `on_key` caps
    /// `scroll` with it.
    pub scroll_max: std::cell::Cell<u16>,
    /// M2: current plan/build mode (display; core holds authority).
    pub mode: AgentMode,
    /// M3: inline /edit preview awaiting accept, and Tab jump state.
    pub pending_edit: Option<PendingEdit>,
    pub jumps: JumpTracker,
    /// M5: latest endpoint probe — (reachable, latency_ms).
    pub api: Option<(bool, u64)>,
    /// Laziness Meter: (added, deleted, reuse hints) for the status bar.
    pub stats: Option<(u64, u64, u64)>,
    /// Mission picker (open when `/mission <goal>` has no kind yet).
    pub mission_picker: Option<MissionPicker>,
    /// Turbo mode indicator (core holds authority).
    pub turbo: bool,
    /// Team-mode panel: live worker cards while `/team` runs (and after,
    /// as a summary). `None` until the first `[team]` line arrives.
    pub team: Option<TeamState>,
    /// M6: command palette for the line currently being typed.
    pub palette: Option<Palette>,
    /// M6: `/help` reference overlay is open.
    pub help: bool,
    /// B-41: how far the help overlay is scrolled, in rows.
    pub help_scroll: u16,
    /// B-41: how far the edit preview is scrolled, in rows.
    pub edit_scroll: u16,
    /// B-41: the furthest useful scroll of help / the edit preview. `draw`
    /// sets them each frame (like `scroll_max`), so ↓ stops at the end.
    pub help_scroll_max: std::cell::Cell<u16>,
    pub edit_scroll_max: std::cell::Cell<u16>,
    /// M6: `/sessions` picker overlay.
    pub sessions: Option<SessionsOverlay>,
    /// M6: the model the core will use (shown in the header).
    pub model: Option<String>,
    /// M7: spawned-subagent cards for the agents panel (live while they run).
    pub agents: Vec<AgentCard>,
    /// Answers offered by the last `ask_user` question (typing its number picks one).
    pub last_question_options: Vec<String>,
    /// Redraw counter — drives the busy spinner frame.
    pub tick: u64,
}

impl App {
    pub fn new() -> Self {
        Self {
            entries: vec![Entry {
                kind: EntryKind::System,
                text: "opencode-rust — type a prompt and press Enter. Type `/` for the \
                       command palette, F1 or /help for the full reference."
                    .into(),
            }],
            input: String::new(),
            cursor: 0,
            approval: None,
            busy: false,
            approval_queue: std::collections::VecDeque::new(),
            approval_scroll: 0,
            approval_scroll_max: std::cell::Cell::new(u16::MAX),
            approval_since: None,
            should_quit: false,
            session_id: None,
            scroll: 0,
            scroll_max: std::cell::Cell::new(0),
            mode: AgentMode::Build,
            pending_edit: None,
            jumps: JumpTracker::default(),
            api: None,
            stats: None,
            mission_picker: None,
            turbo: false,
            team: None,
            palette: None,
            help: false,
            help_scroll: 0,
            edit_scroll: 0,
            // Unknown until the first frame is drawn: no cap yet.
            help_scroll_max: std::cell::Cell::new(u16::MAX),
            edit_scroll_max: std::cell::Cell::new(u16::MAX),
            sessions: None,
            model: None,
            agents: Vec::new(),
            last_question_options: Vec::new(),
            tick: 0,
        }
    }

    pub fn push(&mut self, kind: EntryKind, text: impl Into<String>) {
        self.entries.push(Entry { kind, text: text.into() });
    }

    /// Apply a server event to the transcript.
    pub fn on_event(&mut self, ev: ServerEvent) {
        match ev {
            // Counts are read on demand with /usage; the TUI shows nothing live.
            ServerEvent::Usage { .. } => {}
            ServerEvent::AssistantDelta { text, .. } => {
                // Team panel listens on the same stream the transcript shows.
                // B-34: only a real team start opens the panel, not any text
                // that happens to mention "[team]".
                if text.contains("[team] decomposing goal:") || self.team.is_some() {
                    let team = self.team.get_or_insert_with(TeamState::default);
                    team.feed(&text);
                }
                // Append to last assistant entry or create one.
                match self.entries.last_mut() {
                    Some(e) if e.kind == EntryKind::Assistant => e.text.push_str(&text),
                    _ => self.push(EntryKind::Assistant, text),
                }
            }
            // A reply broke off and is asked again: drop the part of it that
            // streamed in, or the retry shows after the half reply.
            ServerEvent::ReplyRetracted { chars, .. } => {
                let mut left = chars;
                while left > 0 {
                    // Back over the reply's text (its thinking may sit
                    // between two pieces), never past anything else.
                    let Some(i) = self.entries.iter().rposition(|e| e.kind != EntryKind::Thinking) else { break };
                    if self.entries[i].kind != EntryKind::Assistant {
                        break;
                    }
                    let text = &mut self.entries[i].text;
                    let count = text.chars().count();
                    let keep = count.saturating_sub(left);
                    let cut = text.char_indices().nth(keep).map_or(text.len(), |(b, _)| b);
                    text.truncate(cut);
                    left -= count - keep;
                    if text.is_empty() {
                        self.entries.remove(i);
                    }
                }
            }
            ServerEvent::ReasoningDelta { text, .. } => {
                // Same streaming semantics as assistant text, own entry kind.
                match self.entries.last_mut() {
                    Some(e) if e.kind == EntryKind::Thinking => e.text.push_str(&text),
                    _ => self.push(EntryKind::Thinking, format!("✻ {text}")),
                }
            }
            ServerEvent::ToolStarted { tool, detail, .. } => {
                self.push(EntryKind::Tool, format!("⏺ {tool}({detail})"));
            }
            ServerEvent::ToolFinished { tool, ok, .. } => {
                let mark = if ok { "✓" } else { "✗" };
                self.push(EntryKind::Tool, format!("  {mark} {tool}"));
            }
            ServerEvent::ApprovalNeeded { approval_id, tool, detail, .. } => {
                // B-15: never replace the request on screen — queue the new one.
                let ap = PendingApproval { id: approval_id, tool, detail };
                if self.approval.is_some() {
                    self.approval_queue.push_back(ap);
                } else {
                    self.show_approval(ap);
                }
            }
            ServerEvent::RunFinished { ok, message, .. } => {
                self.busy = false;
                // B-34: a team run that is still on screen ends here.
                if let Some(team) = self.team.as_mut() {
                    if team.phase != TeamPhase::Done {
                        team.finish(ok);
                    }
                }
                if !ok {
                    if let Some(m) = message {
                        self.push(EntryKind::Error, m);
                    }
                } else if let Some(m) = message {
                    if !m.is_empty() {
                        self.push(EntryKind::System, format!("✔ {m}"));
                    }
                }
            }
            ServerEvent::SessionCreated { session_id, .. } => {
                self.session_id = Some(session_id);
            }
            ServerEvent::Heartbeat { ok, latency_ms, detail } => {
                // T-101: say why once when the provider goes down (or is down
                // from the start) and once when it is back — not every beat.
                let was = self.api.map(|(ok, _)| ok);
                if was.map_or(!ok, |was| was != ok) {
                    let mut text = if ok {
                        format!("[api] back online ({latency_ms}ms)")
                    } else {
                        "[api] provider unreachable".to_string()
                    };
                    if !detail.is_empty() {
                        text.push_str(&format!(" — {detail}"));
                    }
                    self.push(EntryKind::System, text);
                }
                self.api = Some((ok, latency_ms));
            }
            ServerEvent::DevServer { url, .. } => {
                // The GUI opens these in the preview panel; in the TUI the URL
                // is the useful part (click-through does not exist there).
                self.push(EntryKind::System, format!("\u{1f310} dev server: {url}"));
            }
            ServerEvent::EditPreview { path, diff, old, new, line, .. } => {
                self.pending_edit = Some(PendingEdit { path, diff, old, new, line });
                self.edit_scroll = 0;
                self.busy = false;
            }
            ServerEvent::EditApplied { path, line, .. } => {
                self.jumps.record(JumpSite { path, line });
                self.busy = false;
            }
            ServerEvent::SessionLoaded { session_id, mode, transcript } => {
                // /fork or /resume switched sessions: replay the transcript.
                self.entries.clear();
                self.team = None; // panel belongs to the old session
                self.agents.clear();

                for line in transcript {
                    let kind = match line.role.as_str() {
                        "user" => EntryKind::User,
                        "assistant" => EntryKind::Assistant,
                        "tool" => EntryKind::Tool,
                        "system" => EntryKind::System,
                        _ => EntryKind::System,
                    };
                    self.push(kind, line.text);
                }
                self.push(EntryKind::System, format!("[switched to session {session_id} · mode {mode}]"));
                self.session_id = Some(session_id);
                if let Some(m) = AgentMode::parse(&mode) {
                    self.mode = m;
                }
                self.busy = false;
                self.scroll = 0;
            }
            ServerEvent::Note { text, .. } => {
                self.push(EntryKind::System, text);
            }
            // New (B-10): the approval ended elsewhere. B-15 closes the
            // overlay here.
            ServerEvent::ApprovalResolved { approval_id, allowed, reason, .. } => {
                // B-15: the request is gone — never keep asking about it.
                let on_screen = self.approval.as_ref().is_some_and(|a| a.id == approval_id);
                self.approval_queue.retain(|a| a.id != approval_id);
                if on_screen {
                    let tool = self.approval.take().map(|a| a.tool).unwrap_or_default();
                    let verdict = if allowed { "allowed" } else { "denied" };
                    self.push(EntryKind::System, format!("[approval] {tool}: {verdict} ({reason})"));
                    self.next_approval();
                }
            }
            ServerEvent::Stats {
                added, deleted, hints, ..
            } => {
                self.stats = Some((added, deleted, hints));
            }
            ServerEvent::SessionList { sessions } => {
                // B-33: an answer to a local command; a run may still be going.
                self.sessions = Some(SessionsOverlay { items: sessions, sel: 0 });
            }
            ServerEvent::ModelInfo { current, available } => {
                self.model = Some(current.clone());
                let mut text = format!("[model] active: {current}\n");
                if available.is_empty() {
                    text.push_str("  (endpoint listed no models — set one with /model <id>)\n");
                } else {
                    text.push_str("  available:\n");
                    for m in available.iter().take(40) {
                        let mark = if *m == current { "●" } else { " " };
                        text.push_str(&format!("   {mark} {m}\n"));
                    }
                }
                self.push(EntryKind::System, text); // B-33: busy unchanged
            }
            ServerEvent::Question { question, options, .. } => {
                // The TUI has no buttons: number the answers; typing one replies.
                let mut text = format!("? {question}");
                for (i, o) in options.iter().enumerate() {
                    text.push_str(&format!("\n   {}. {o}", i + 1));
                }
                if !options.is_empty() {
                    text.push_str("\n   (type an answer, or its number)");
                }
                self.last_question_options = options;
                self.push(EntryKind::System, text);
            }
            ServerEvent::AgentStatus { agent_id, name, task, state, detail, .. } => {
                let state = match state.as_str() {
                    "done" => TeamStatus::Done,
                    "failed" | "stopped" => TeamStatus::Failed,
                    _ => TeamStatus::Working,
                };
                // Upsert by agent id: one card per spawned agent.
                match self.agents.iter().position(|a| a.agent_id == agent_id) {
                    Some(i) => {
                        let card = &mut self.agents[i];
                        // B-41: stop the card's clock the first time it ends.
                        if state != TeamStatus::Working && card.finished.is_none() {
                            card.finished = Some(std::time::Instant::now());
                        }
                        // A message from the parent wakes a finished agent.
                        if state == TeamStatus::Working {
                            card.finished = None;
                        }
                        card.state = state;
                        card.detail = detail;
                    }
                    None => {
                        let now = std::time::Instant::now();
                        let finished = (state != TeamStatus::Working).then_some(now);
                        self.agents.push(AgentCard {
                            agent_id,
                            name,
                            task,
                            state,
                            detail,
                            started: now,
                            finished,
                        })
                    }
                }
            }
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

pub enum InputAction {
    Prompt(String),
    Team(String),
    /// Run a mission: the core expands it into the well-worded brief.
    Mission(crate::mission::Mission),
    /// `/mission status | stop | resume`.
    MissionCtl(String),
    /// Turbo mode: toggle (None) or enable + run this prompt right away.
    Turbo(Option<String>),
    /// Install + vet + bake a skill from a local path or URL.
    SkillInstall(String),
    /// Ponytail review of the uncommitted changes.
    Review,
    /// Laziness Meter report (local, instant).
    Debt,
    /// Deep mode: several checked attempts at one task, best one kept.
    Deep { attempts: Option<usize>, task: String },
    SetMode(AgentMode),
    Compact,
    Undo,
    /// `/undo force`: undo even over work done outside Habi since the run.
    UndoForce,
    Redo,
    Fork,
    Resume(String),
    Diff(Option<String>),
    Skills,
    Approve { id: String, always: bool },
    Deny(String),
    /// M3: run one inline-edit completion and show the diff preview.
    Edit { path: String, instruction: String },
    /// M3: the user accepted the preview — write it.
    ApplyEdit { path: String, old: String, new: String, line: usize },
    /// M6: ask the core for the session list (opens the picker).
    ListSessions,
    /// M6: switch to the session the picker selected.
    SwitchSession(String),
    /// M6: show the active model (None) or switch to it (Some).
    Model(Option<String>),
    /// M6: write the transcript to markdown (default path when None).
    Export(Option<String>),
    /// M6: show this session's todo list.
    Todos,
    /// Token usage and prompt-cache savings.
    Usage,
    /// M6: start a fresh session.
    NewSession,
    /// M6: create a starter AGENTS.md when the repo has none.
    Init,
    /// P-05: show (None) or set (Some) this session's ponytail level.
    Ponytail(Option<crate::agents::PonytailLevel>),
}
