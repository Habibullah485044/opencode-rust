//! Server events into the chat they belong to (B-12): streaming text, run
//! state, approvals and agent cards; plus the preview webview's callbacks
//! and the notifications shown while the window is in the background.

use super::*;

impl GuiState {
    /// New (B-12): the chat a transcript event belongs to. `None` for events
    /// that are not transcript text (run state, approvals, heartbeat).
    fn transcript_session(ev: &ServerEvent) -> Option<String> {
        match ev {
            ServerEvent::AssistantDelta { session_id, .. }
            | ServerEvent::ReplyRetracted { session_id, .. }
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
            ServerEvent::AssistantDelta { .. } | ServerEvent::ReasoningDelta { .. } | ServerEvent::ToolStarted { .. }
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
            // A reply broke off and is asked again: drop the part of it that
            // streamed in (back over its text and thinking, never past
            // anything else), or the retry shows after the half reply.
            ServerEvent::ReplyRetracted { chars, .. } => {
                let mut left = chars;
                while left > 0 {
                    let Some(i) = self.entries.iter().rposition(|(k, _)| *k != EntryKind::Thinking) else { break };
                    if self.entries[i].0 != EntryKind::Assistant {
                        break;
                    }
                    let text = &mut self.entries[i].1;
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
                // First reasoning chunk carries the marker; appends merge bare.
                let last_is_thinking = self.entries.last().map(|(k, _)| *k == EntryKind::Thinking).unwrap_or(false);
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
                gui_log(&format!("[gui] run finished ok={ok}"));
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
                let pending =
                    PendingApproval { id: approval_id, session_id, tool, detail, started: std::time::Instant::now() };
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
                        "timeout" => self.push(EntryKind::System, format!("No answer in time — `{tool}` was denied.")),
                        "abort" => self.push(EntryKind::System, format!("Run stopped — `{tool}` was not run.")),
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
}

/// Clear the modal for the request it belongs to and unhide the panel.
/// `answered` is false when the engine already timed out / aborted it.
pub(super) fn settle_approval(gs: &mut GuiState, id: &str, answered: bool) {
    // B-12: also brings up the next queued approval, if any.
    gs.finish_approval(id);
    if !answered {
        gs.push(EntryKind::System, "That approval had already expired.".to_string());
    }
}

/// What to say in a notification for an event that waits on the user (or a
/// finished run), when the window is not in front.
pub(super) fn notice_for(ev: &ServerEvent) -> Option<(&'static str, String)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> ServerEvent {
        ServerEvent::AssistantDelta { session_id: "s".into(), text: text.into() }
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
    fn assistant_deltas_merge_into_one_bubble() {
        let mut g = GuiState::new();
        g.session = Some("s".into());
        g.on_event(delta("Hel"));
        g.on_event(delta("lo!"));
        let assistant: Vec<_> = g.entries.iter().filter(|(k, _)| *k == EntryKind::Assistant).collect();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0].1, "Hello!");
    }

    /// A reply that broke off and is asked again is taken back.
    #[test]
    fn a_retracted_reply_leaves_the_bubble() {
        let mut g = GuiState::new();
        g.session = Some("s".into());
        g.on_event(delta("Hel"));
        g.on_event(delta("lo wor"));
        g.on_event(ServerEvent::ReplyRetracted { session_id: "s".into(), chars: 9 });
        g.on_event(delta("Hello world"));
        let assistant: Vec<_> = g.entries.iter().filter(|(k, _)| *k == EntryKind::Assistant).collect();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0].1, "Hello world");
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
        g.on_event(ServerEvent::RunFinished { session_id: "s".into(), ok: true, message: Some("done".into()) });
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
        g.on_event(ServerEvent::RunFinished { session_id: "b".into(), ok: false, message: Some("b failed".into()) });
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
}
