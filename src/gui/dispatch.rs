//! Page messages to handlers: one arm per `IpcMsg` (and preview callback),
//! and a method for each message that does more than a few lines. The panel,
//! terminal and Settings verbs live in `panel`, `terminal` and `settings`.

use super::*;

/// Milliseconds since 1970 (the usage log's clock).
pub(super) fn epoch_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl App {
    /// Apply one page message or preview callback. True = repaint.
    pub(super) fn dispatch(&mut self, msg: UiMsg, control_flow: &mut ControlFlow) -> bool {
        match msg {
            UiMsg::Ipc(IpcMsg::Send(text)) if gui_command(&mut self.gs, &self.st, &self.rt, &text) => true,
            UiMsg::Ipc(IpcMsg::Send(text)) => self.on_send(text),
            // "New chat" opens a chat in the open chat's project.
            UiMsg::Ipc(IpcMsg::NewSession) => {
                let project = self.gs.current_project();
                open_new_chat(&mut self.gs, &self.st, project);
                true
            }
            UiMsg::Ipc(IpcMsg::NewInProject(project)) => {
                open_new_chat(&mut self.gs, &self.st, Some(project));
                true
            }
            UiMsg::Ipc(IpcMsg::AddProject(path)) => self.on_add_project(path),
            UiMsg::Ipc(IpcMsg::RemoveProject(id)) => {
                if let Err(e) = self.st.db.remove_project(&id) {
                    self.gs.push(EntryKind::Error, format!("remove project failed: {e}"));
                }
                refresh_sessions(&mut self.gs, &self.st);
                true
            }
            UiMsg::Ipc(IpcMsg::Pin { id, on }) => {
                let _ = self.st.db.set_session_pinned(&id, on);
                refresh_sessions(&mut self.gs, &self.st);
                true
            }
            UiMsg::Ipc(IpcMsg::Archive { id, on }) => self.on_archive(&id, on),
            UiMsg::Ipc(IpcMsg::Rename { id, title }) => {
                let _ = self.st.db.set_session_title(&id, &title);
                refresh_sessions(&mut self.gs, &self.st);
                true
            }
            UiMsg::Ipc(IpcMsg::MoveChat { id, project }) => {
                if !self.gs.running.contains(&id) {
                    let _ = self.st.db.set_session_project(&id, project.as_deref());
                }
                refresh_sessions(&mut self.gs, &self.st);
                true
            }
            UiMsg::Ipc(IpcMsg::AdoptUnfiled(project)) => {
                if let Err(e) = self.st.db.adopt_unfiled_sessions(&project) {
                    self.gs.push(EntryKind::Error, format!("move chats failed: {e}"));
                }
                refresh_sessions(&mut self.gs, &self.st);
                true
            }
            UiMsg::Ipc(IpcMsg::DeleteChat(id)) => self.on_delete_chat(&id),
            UiMsg::Ipc(IpcMsg::Load(id)) => {
                if self.gs.session.as_deref() != Some(id.as_str()) {
                    self.gs.question = None;
                    load_session(&mut self.gs, &self.st, &id);
                }
                true
            }
            // The page just finished loading: give it the state.
            // B-37: also when the page asks for a full resend.
            UiMsg::Ipc(IpcMsg::Ready) | UiMsg::Ipc(IpcMsg::Resync) => {
                self.gs.resync();
                true
            }
            UiMsg::Ipc(IpcMsg::PageError(msg)) => {
                eprintln!("[gui] page error: {msg}");
                false
            }
            UiMsg::Ipc(IpcMsg::Minimize) => {
                self.window.set_minimized(true);
                false
            }
            UiMsg::Ipc(IpcMsg::MaximizeToggle) => {
                let maxed = self.window.is_maximized();
                self.window.set_maximized(!maxed);
                false
            }
            UiMsg::Ipc(IpcMsg::Close) => {
                self.close(control_flow);
                false
            }
            UiMsg::Ipc(IpcMsg::Stop) => self.on_stop(),
            UiMsg::Ipc(IpcMsg::OpenInEditor(editor)) => {
                let root = chat_root(&self.gs, &self.st);
                let text = match open_in_editor(editor.as_deref(), &root) {
                    Ok(name) => format!("Opened {} in {name}.", root.display()),
                    Err(e) => format!("Open in editor: {e}"),
                };
                self.gs.push(EntryKind::System, text);
                true
            }
            UiMsg::Ipc(IpcMsg::SaveSettings { base_url, model, api_key, effort }) => {
                self.on_save_settings(base_url, model, api_key, effort)
            }
            UiMsg::Ipc(IpcMsg::SavePrompt { text, replace }) => self.on_save_prompt(text, replace),
            UiMsg::Ipc(IpcMsg::SaveTemperature(t)) => self.on_save_temperature(t),
            UiMsg::Ipc(IpcMsg::FindFiles(query)) => {
                let items = find_project_files(&chat_root(&self.gs, &self.st), &query, 12);
                self.gs.file_matches = (query, items);
                true
            }
            UiMsg::Ipc(IpcMsg::TermOpen(open)) => self.on_term_open(open),
            UiMsg::Ipc(IpcMsg::TermRun(command)) => self.on_term_run(command),
            UiMsg::Ipc(IpcMsg::TermStop(id)) => {
                self.on_term_stop(id);
                false
            }
            UiMsg::Ipc(IpcMsg::Rewind { text, skip }) => self.on_rewind(text, skip),
            UiMsg::Ipc(IpcMsg::TermClear) => {
                self.gs.term_entries.clear();
                crate::tools::jobs::clear_user_jobs();
                true
            }
            UiMsg::Ipc(IpcMsg::NewBranch(name)) => self.on_new_branch(name),
            UiMsg::Ipc(IpcMsg::LoadChanges) => {
                let root = chat_root(&self.gs, &self.st);
                let json = match crate::changes::collect(&root) {
                    Ok(files) => serde_json::json!({ "files": files, "root": root.display().to_string() }),
                    Err(e) => serde_json::json!({ "error": format!("{e:#}"), "files": [] }),
                };
                *self.gs.changes_out.borrow_mut() = Some(json.to_string());
                true
            }
            UiMsg::Ipc(IpcMsg::LoadUsage(days)) => {
                self.gs.usage_days = days;
                let json = crate::usage::dashboard(&self.st.db, days, epoch_ms());
                *self.gs.usage_out.borrow_mut() = Some(json.to_string());
                true
            }
            UiMsg::Ipc(IpcMsg::SetPrice { model, price }) => {
                if let Err(e) = self.st.db.set_price(&model, price) {
                    self.gs.push(EntryKind::Error, format!("saving the price failed: {e}"));
                }
                let json = crate::usage::dashboard(&self.st.db, self.gs.usage_days, epoch_ms());
                *self.gs.usage_out.borrow_mut() = Some(json.to_string());
                true
            }
            UiMsg::Ipc(IpcMsg::ReviewComments(comments)) => self.on_review_comments(comments),
            UiMsg::Ipc(IpcMsg::PickFiles) => self.on_pick_files(),
            UiMsg::Ipc(IpcMsg::SaveAttachment { name, data }) => self.on_save_attachment(&name, &data),
            UiMsg::Ipc(IpcMsg::Export) => self.on_export(),
            UiMsg::Ipc(IpcMsg::ListModels) => {
                self.on_list_models();
                false
            }
            UiMsg::Ipc(IpcMsg::AgentStop(id)) => {
                crate::subagent::stop(&id);
                false
            }
            UiMsg::Ipc(IpcMsg::Approve { id, always }) => {
                let answered = self.st.decide(&id, true, always);
                settle_approval(&mut self.gs, &id, answered);
                true
            }
            UiMsg::Ipc(IpcMsg::Deny(id)) => {
                let answered = self.st.decide(&id, false, false);
                settle_approval(&mut self.gs, &id, answered);
                true
            }
            UiMsg::Ipc(IpcMsg::Mode(mode)) => self.on_mode(mode),
            UiMsg::Ipc(IpcMsg::Browse(raw)) => self.on_browse(&raw),
            UiMsg::Ipc(IpcMsg::NavBack) => self.on_nav_back(),
            UiMsg::Ipc(IpcMsg::NavFwd) => self.on_nav_fwd(),
            UiMsg::Ipc(IpcMsg::NavReload) => self.on_nav_reload(),
            // Geometry/no-paint verbs: the page already knows its
            // own layout, so these must not trigger a full repaint.
            UiMsg::Ipc(IpcMsg::PanelRect(rect, dpr)) => {
                self.on_panel_rect(rect, dpr);
                false
            }
            UiMsg::Ipc(IpcMsg::PanelVisible(visible)) => {
                self.on_panel_visible(visible);
                false
            }
            UiMsg::Ipc(IpcMsg::OpenDevServer(url)) => self.on_open_dev_server(url),
            UiMsg::Browser(BrowserEvt::Eval(raw)) => {
                // Page -> Rust result frame for an agent tool call.
                self.bridge.on_eval(&raw);
                false
            }
            UiMsg::Browser(ev) => self.on_preview_event(ev),
        }
    }

    /// A typed message: run it in the open chat.
    fn on_send(&mut self, text: String) -> bool {
        self.gs.question = None; // answered (or passed over)
        self.gs.followups.clear();
        self.gs.input = text;
        let Some(content) = submit_and_name(&mut self.gs, &self.st) else {
            return false;
        };
        eprintln!("[gui] run start: {}", clip(&content, 80));
        let sid = self.gs.session.clone().unwrap_or_default();
        let st = self.st.clone();
        self.rt.spawn(async move {
            st.kick_run(sid, content).await;
        });
        true
    }

    /// Add a project (this folder, or one picked in a dialog) and open a
    /// chat in it. False = the dialog was cancelled.
    fn on_add_project(&mut self, path: Option<String>) -> bool {
        let folder = match path {
            Some(p) => Some(std::path::PathBuf::from(p)),
            None => pick_folder(&self.window),
        };
        let Some(folder) = folder else {
            return false;
        };
        if folder.is_dir() {
            match self.st.db.upsert_project(&folder) {
                Ok(p) => open_new_chat(&mut self.gs, &self.st, Some(p.id)),
                Err(e) => self.gs.push(EntryKind::Error, format!("add project failed: {e}")),
            }
        } else {
            self.gs.push(EntryKind::Error, format!("not a folder: {}", folder.display()));
        }
        true
    }

    fn on_archive(&mut self, id: &str, on: bool) -> bool {
        // A chat with a run going stays until the run ends.
        if !(on && self.gs.running.contains(id)) {
            let _ = self.st.db.set_session_archived(id, on);
            if on && self.gs.is_current(id) {
                let project = self.gs.current_project();
                open_new_chat(&mut self.gs, &self.st, project);
            }
        }
        refresh_sessions(&mut self.gs, &self.st);
        true
    }

    fn on_delete_chat(&mut self, id: &str) -> bool {
        if !self.gs.running.contains(id) {
            if let Err(e) = self.st.db.delete_session(id) {
                self.gs.push(EntryKind::Error, format!("delete failed: {e}"));
            } else if self.gs.is_current(id) {
                self.gs.session = None;
                let project = self.gs.default_project.clone();
                open_new_chat(&mut self.gs, &self.st, project);
            }
        }
        refresh_sessions(&mut self.gs, &self.st);
        true
    }

    /// F-06: the Stop button — the same abort as the HTTP route.
    fn on_stop(&mut self) -> bool {
        let Some(sid) = self.gs.session.clone() else {
            return false;
        };
        self.gs.push(EntryKind::System, "Stopping…".to_string());
        let st = self.st.clone();
        self.rt.spawn(async move {
            if !request_stop(&st, &sid).await {
                eprintln!("[gui] stop: no run found for {sid}");
            }
        });
        true
    }

    /// Rewind the chat to before a user message; the message goes back into
    /// the composer.
    fn on_rewind(&mut self, text: String, skip: usize) -> bool {
        let sid = self.gs.session.clone().unwrap_or_default();
        let note = if self.gs.busy || self.gs.running.contains(&sid) {
            "Stop the run first, then rewind.".to_string()
        } else {
            match server::rewind_chat(&self.st, &sid, &text, skip) {
                Ok(r) => {
                    load_session(&mut self.gs, &self.st, &sid);
                    let shown = text.trim().trim_start_matches('\u{203a}').trim().to_string();
                    self.gs.compose = (self.gs.compose.0 + 1, shown);
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
        self.gs.push(EntryKind::System, note);
        self.gs.git_stale = true;
        true
    }

    /// Make a new git branch in the chat's project and switch to it.
    fn on_new_branch(&mut self, name: String) -> bool {
        let root = chat_root(&self.gs, &self.st);
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
        self.gs.push(EntryKind::System, text);
        self.gs.git_stale = true;
        true
    }

    /// Line comments from the Changes view, sent to the agent.
    fn on_review_comments(&mut self, comments: Vec<(String, u32, String)>) -> bool {
        if comments.is_empty() {
            return false;
        }
        self.gs.input = crate::changes::comments_message(&comments);
        self.gs.question = None;
        if let Some(content) = submit_and_name(&mut self.gs, &self.st) {
            let st2 = self.st.clone();
            let sid = self.gs.session.clone().unwrap_or_default();
            self.rt.spawn(async move {
                st2.kick_run(sid, content).await;
            });
        }
        true
    }

    /// Attach files picked in a dialog.
    fn on_pick_files(&mut self) -> bool {
        let root = chat_root(&self.gs, &self.st);
        let mut markers = Vec::new();
        for file in pick_files(&self.window) {
            match crate::attachments::copy_in(&root, &file) {
                Ok(p) => markers.push(crate::attachments::marker(&p)),
                Err(e) => self.gs.push(EntryKind::System, format!("Not attached: {e:#}")),
            }
        }
        if !markers.is_empty() {
            self.gs.attach = (self.gs.attach.0 + 1, markers);
        }
        true
    }

    /// Attach a pasted or dropped file (`data` is base64).
    fn on_save_attachment(&mut self, name: &str, data: &str) -> bool {
        use base64::Engine;
        let root = chat_root(&self.gs, &self.st);
        let saved = base64::engine::general_purpose::STANDARD
            .decode(data.split_once(',').map_or(data, |(_, b)| b))
            .map_err(anyhow::Error::from)
            .and_then(|bytes| crate::attachments::save_bytes(&root, name, &bytes));
        match saved {
            Ok(p) => self.gs.attach = (self.gs.attach.0 + 1, vec![crate::attachments::marker(&p)]),
            Err(e) => self.gs.push(EntryKind::System, format!("Not attached: {e:#}")),
        }
        true
    }

    /// Write the open chat as Markdown into its project.
    fn on_export(&mut self) -> bool {
        let Some(sid) = self.gs.session.clone() else {
            return false;
        };
        let text = match self.st.export_transcript(&sid, None) {
            Ok(path) => format!("Chat exported to {}", path.display()),
            Err(e) => format!("Export failed: {e:#}"),
        };
        self.gs.push(EntryKind::System, text);
        true
    }
}
