//! What the page gets: the JSON for `window.render(...)`, with only the
//! transcript entries that changed since the last paint (B-37).

use super::*;

impl BrowserView {
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
                "models_error": { "seq": self.settings.models_error.0, "text": self.settings.models_error.1 },
                "config_path": self.settings.config_path,
                "system_prompt": self.settings.system_prompt,
                "prompt_replaces": self.settings.prompt_replaces,
                // Rounded: f32 0.2 would reach the page as 0.20000000298.
                "temperature": self.settings.temperature.map(|t| (t as f64 * 100.0).round() / 100.0),
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

pub(super) fn kind_class(k: &EntryKind) -> char {
    match k {
        EntryKind::User => 'u',
        EntryKind::Assistant => 'a',
        EntryKind::Thinking => 't',
        EntryKind::Tool => 'w',
        EntryKind::System => 's',
        EntryKind::Error => 'x',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
