//! Key and paste handling for [`App`]: `on_key` dispatches to the open
//! overlay's handler, the command palette, or the input line.

use super::*;

impl App {
    /// Handle a key when no overlay is up.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<InputAction> {
        if self.approval.is_some() {
            return self.on_approval_key(key);
        }
        if self.pending_edit.is_some() {
            return self.on_edit_key(key);
        }
        if self.mission_picker.is_some() {
            return self.on_mission_key(key);
        }
        if self.sessions.is_some() {
            return self.on_sessions_key(key);
        }
        if self.help {
            return self.on_help_key(key);
        }
        if self.palette.is_some() && self.on_palette_key(key) {
            return None;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                None
            }
            KeyCode::F(1) => {
                self.help = true;
                self.help_scroll = 0;
                None
            }
            // M3: Tab / Shift+Tab cycle accepted-edit jump sites (local, no model call).
            KeyCode::Tab => {
                self.jump(false);
                None
            }
            KeyCode::BackTab => {
                self.jump(true);
                None
            }
            KeyCode::Enter => self.on_enter(),
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.scroll_transcript(key.code);
                None
            }
            _ => self.on_text_key(key),
        }
    }

    /// Command palette: while a command word is being typed, ↑/↓ move the
    /// selection and Tab completes it. Enter runs what was typed when the
    /// name is already complete, runs a unique zero-arg match outright, and
    /// otherwise completes so arguments can follow.
    /// Returns true when the key was used up here (`on_key` returns `None`);
    /// false lets it fall through to the input line.
    fn on_palette_key(&mut self, key: KeyEvent) -> bool {
        let Some(pal) = self.palette.clone() else { return false };
        match key.code {
            KeyCode::Up => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = p.sel.saturating_sub(1);
                }
                true
            }
            KeyCode::Down => {
                if let Some(p) = self.palette.as_mut() {
                    p.sel = (p.sel + 1).min(p.matches.len().saturating_sub(1));
                }
                true
            }
            KeyCode::Tab => {
                self.complete_palette();
                true
            }
            KeyCode::Esc => {
                self.palette = None;
                true
            }
            KeyCode::Enter => {
                let sel = pal.matches.get(pal.sel).copied();
                let name = sel.map(|i| builtins()[i].name).unwrap_or("");
                let typed = self.input.trim().trim_start_matches('/');
                let takes_args = sel.map(|i| !builtins()[i].args.is_empty()).unwrap_or(true);
                if typed == name {
                    // A full command name was typed — run it as written.
                    self.palette = None;
                    false
                } else if pal.matches.len() == 1 && !takes_args {
                    // One unambiguous zero-arg match: fill it in and run it.
                    self.input = format!("/{name}");
                    self.cursor = self.input.chars().count();
                    self.palette = None;
                    false
                } else {
                    // Partial or ambiguous: complete, then let arguments follow.
                    self.complete_palette();
                    true
                }
            }
            _ => false,
        }
    }

    /// Enter on the input line: send the message or run the command typed.
    fn on_enter(&mut self) -> Option<InputAction> {
        let mut content = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.palette = None;
        if content.trim().is_empty() {
            return None;
        }
        // A number answers the last question with that option.
        let picked = content
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| self.last_question_options.get(i).cloned());
        if let Some(answer) = picked {
            content = answer;
        }
        self.last_question_options.clear();
        if classify(&content) == Some(Command::Quit) {
            self.should_quit = true;
            return None;
        }
        // B-33: one run at a time. A plain prompt (and /mission or
        // /turbo <prompt>) REPLACES the running agent run on the
        // server; these commands would run next to it instead.
        if self.busy
            && matches!(
                classify(&content),
                Some(Command::Team(_))
                    | Some(Command::Compact)
                    | Some(Command::Review)
                    | Some(Command::Deep { .. })
                    | Some(Command::Edit { .. })
                    | Some(Command::SkillInstall(_))
            )
        {
            self.cursor = content.chars().count();
            self.input = content;
            self.push(
                EntryKind::Error,
                "a run is still going — wait for it to finish, then press Enter again",
            );
            return None;
        }
        self.push(EntryKind::User, content.clone());
        let (action, busy) = self.command_action(content);
        // B-33: local commands leave `busy` alone — they must not
        // flip the header to "ready" while a run is still going.
        if busy {
            self.busy = true;
        }
        action
    }

    /// The action for a sent line, and whether it starts an agent run.
    /// Agent-running commands set busy; fast local ones don't.
    fn command_action(&mut self, content: String) -> (Option<InputAction>, bool) {
        match classify(&content) {
            Some(Command::Team(goal)) => {
                // Fresh panel instantly; `[team]` lines fill it in.
                self.team = Some(TeamState::default());
                (Some(InputAction::Team(goal)), true)
            }
            Some(Command::Plan) => {
                self.mode = AgentMode::Plan;
                (Some(InputAction::SetMode(AgentMode::Plan)), false)
            }
            Some(Command::Build) => {
                self.mode = AgentMode::Build;
                (Some(InputAction::SetMode(AgentMode::Build)), false)
            }
            Some(Command::Compact) => (Some(InputAction::Compact), true),
            Some(Command::Undo) => (Some(InputAction::Undo), false),
            Some(Command::UndoForce) => (Some(InputAction::UndoForce), false),
            Some(Command::Redo) => (Some(InputAction::Redo), false),
            Some(Command::Fork) => (Some(InputAction::Fork), false),
            Some(Command::Resume(arg)) => (Some(InputAction::Resume(arg)), false),
            Some(Command::Diff(arg)) => (Some(InputAction::Diff(arg)), false),
            Some(Command::Skills) => (Some(InputAction::Skills), false),
            Some(Command::Review) => (Some(InputAction::Review), true),
            Some(Command::Debt) => (Some(InputAction::Debt), false),
            Some(Command::Deep { attempts, task }) => {
                (Some(InputAction::Deep { attempts, task }), true)
            }
            Some(Command::Edit { path, instruction }) => {
                (Some(InputAction::Edit { path, instruction }), true)
            }
            Some(Command::Mission { kind: Some(kind), goal }) => (
                Some(InputAction::Mission(crate::mission::Mission {
                    kind,
                    effort: crate::mission::Effort::default(),
                    goal,
                })),
                true,
            ),
            Some(Command::MissionCtl(what)) => {
                let busy = what == "resume";
                (Some(InputAction::MissionCtl(what)), busy)
            }
            Some(Command::Mission { kind: None, goal }) => {
                // Open the dialog — Custom checked, High effort, like Freebuff.
                self.mission_picker = Some(MissionPicker {
                    goal,
                    kind_idx: 3,
                    effort_idx: 2,
                });
                (None, false)
            }
            Some(Command::Turbo { prompt }) => {
                // Bare `/turbo` toggles; `/turbo <prompt>` enables + runs.
                match &prompt {
                    None => self.turbo = !self.turbo,
                    Some(_) => self.turbo = true,
                }
                let busy = prompt.is_some();
                (Some(InputAction::Turbo(prompt)), busy)
            }
            Some(Command::SkillInstall(src)) => {
                (Some(InputAction::SkillInstall(src)), true)
            }
            Some(Command::Help) => {
                self.help = true;
                self.help_scroll = 0;
                (None, false)
            }
            Some(Command::Clear) => {
                self.clear_view();
                (None, false)
            }
            Some(Command::Sessions) => (Some(InputAction::ListSessions), false),
            Some(Command::Todos) => (Some(InputAction::Todos), false),
            Some(Command::Usage) => (Some(InputAction::Usage), false),
            Some(Command::New) => (Some(InputAction::NewSession), false),
            Some(Command::Init) => (Some(InputAction::Init), false),
            Some(Command::Model(m)) => (Some(InputAction::Model(m)), false),
            Some(Command::Export(p)) => (Some(InputAction::Export(p)), false),
            Some(Command::Ponytail(level)) => (Some(InputAction::Ponytail(level)), false),
            Some(Command::Quit) => unreachable!("handled above"),
            None => {
                // A builtin verb with missing/invalid args (bare
                // `/resume`, `/ponytail max`, …) is a usage mistake —
                // say so in the transcript instead of silently
                // handing it to the model as chat. Unknown verbs
                // still go through (user command templates, text).
                match crate::commands::usage_error(&content) {
                    Some(msg) => {
                        self.push(EntryKind::Error, msg);
                        (None, false)
                    }
                    None => (Some(InputAction::Prompt(content)), true),
                }
            }
        }
    }

    /// B-14: scroll in screen rows, never past the top of the transcript.
    fn scroll_transcript(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_add(1).min(self.scroll_max.get());
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10).min(self.scroll_max.get());
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
            }
            _ => {}
        }
    }

    /// Line editing: cursor movement, deletion and typed characters.
    fn on_text_key(&mut self, key: KeyEvent) -> Option<InputAction> {
        match key.code {
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let idx = self.cursor - 1;
                    // remove char before cursor
                    if let Some((byte, ch)) = self.input.char_indices().nth(idx) {
                        self.input.remove(byte);
                        self.cursor = idx;
                        let _ = ch;
                    }
                }
                self.refresh_palette();
                None
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let byte = self
                        .input
                        .char_indices()
                        .nth(self.cursor)
                        .map(|(b, _)| b)
                        .unwrap_or(self.input.len());
                    self.input.remove(byte);
                }
                self.refresh_palette();
                None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                if self.cursor < self.input.chars().count() {
                    self.cursor += 1;
                }
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                None
            }
            KeyCode::Char(c) => {
                // B-35: Ctrl+key and Alt+key are shortcuts, not text.
                // Ctrl+Alt together is AltGr on Windows (@ { [ \), which types.
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                if ctrl != alt {
                    return None;
                }
                let byte = self
                    .input
                    .char_indices()
                    .nth(self.cursor)
                    .map(|(b, _)| b)
                    .unwrap_or(self.input.len());
                self.input.insert(byte, c);
                self.cursor += 1;
                self.refresh_palette();
                None
            }
            _ => None,
        }
    }

    /// B-35: insert pasted text at the cursor. Line breaks are kept (the
    /// message may span lines); CRLF and a lone CR become `\n`.
    pub fn on_paste(&mut self, text: &str) {
        let overlay = self.approval.is_some()
            || self.pending_edit.is_some()
            || self.mission_picker.is_some()
            || self.sessions.is_some()
            || self.help;
        if overlay {
            return; // overlays take keys, not text
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let byte = self
            .input
            .char_indices()
            .nth(self.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len());
        self.input.insert_str(byte, &text);
        self.cursor += text.chars().count();
        self.refresh_palette();
    }

    /// M3: inline-edit preview keys: y = apply, n/Esc = reject.
    fn on_edit_key(&mut self, key: KeyEvent) -> Option<InputAction> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                None
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let pe = self.pending_edit.take()?;
                Some(InputAction::ApplyEdit { path: pe.path, old: pe.old, new: pe.new, line: pe.line })
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_edit = None;
                self.push(EntryKind::System, "[edit] rejected".to_string());
                None
            }
            // B-41: long diffs scroll instead of losing their end.
            KeyCode::Up => {
                self.edit_scroll = self.edit_scroll.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.edit_scroll =
                    self.edit_scroll.saturating_add(1).min(self.edit_scroll_max.get());
                None
            }
            KeyCode::PageUp => {
                self.edit_scroll = self.edit_scroll.saturating_sub(10);
                None
            }
            KeyCode::PageDown => {
                self.edit_scroll =
                    self.edit_scroll.saturating_add(10).min(self.edit_scroll_max.get());
                None
            }
            _ => None,
        }
    }

    /// M3: cycle Tab jump sites and show where we landed.
    fn jump(&mut self, backwards: bool) {
        if self.jumps.is_empty() {
            self.push(EntryKind::System, "[jump] no accepted edits yet".to_string());
            return;
        }
        let site = if backwards { self.jumps.prev() } else { self.jumps.next() };
        if let Some(JumpSite { path, line }) = site {
            let pos = self.jumps.position().unwrap_or(0);
            self.push(
                EntryKind::System,
                format!("[jump] {}/{} {path}:{line}", pos, self.jumps.len()),
            );
        }
    }

    /// Mission picker keys: ↑/↓ mission, ←/→ effort, Enter runs, Esc cancels.
    fn on_mission_key(&mut self, key: KeyEvent) -> Option<InputAction> {
        let mp = self.mission_picker.as_mut()?;
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                None
            }
            KeyCode::Up => {
                mp.kind_idx = mp.kind_idx.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                mp.kind_idx = (mp.kind_idx + 1).min(crate::mission::MissionKind::all().len() - 1);
                None
            }
            KeyCode::Left => {
                mp.effort_idx = mp.effort_idx.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                mp.effort_idx = (mp.effort_idx + 1).min(crate::mission::Effort::all().len() - 1);
                None
            }
            KeyCode::Enter => {
                let mp = self.mission_picker.take()?;
                let mission = crate::mission::Mission {
                    kind: crate::mission::MissionKind::all()[mp.kind_idx],
                    effort: crate::mission::Effort::all()[mp.effort_idx],
                    goal: mp.goal,
                };
                self.busy = true; // B-33: the mission runs the agent
                Some(InputAction::Mission(mission))
            }
            KeyCode::Esc => {
                self.mission_picker = None;
                self.push(EntryKind::System, "[mission] cancelled");
                None
            }
            _ => None,
        }
    }

    /// B-15: decision keys are ignored this long after an approval appears.
    const APPROVAL_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

    /// Approval overlay keys: y = once, a = always, n/Esc = deny,
    /// ↑/↓/PgUp/PgDn scroll the detail, Ctrl+C quits (B-15).
    fn on_approval_key(&mut self, key: KeyEvent) -> Option<InputAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return None;
        }
        match key.code {
            KeyCode::Up => self.approval_scroll = self.approval_scroll.saturating_sub(1),
            KeyCode::Down => self.approval_scroll = self.approval_scroll.saturating_add(1),
            KeyCode::PageUp => self.approval_scroll = self.approval_scroll.saturating_sub(10),
            KeyCode::PageDown => self.approval_scroll = self.approval_scroll.saturating_add(10),
            _ => {}
        }
        // Never answer on Ctrl/Alt combinations (Ctrl+A must not mean
        // "always"), nor on keys typed just before the overlay appeared.
        if ctrl || alt || self.approval_since.is_some_and(|t| t.elapsed() < Self::APPROVAL_GRACE) {
            return None;
        }
        let ap = match key.code {
            KeyCode::Char('y' | 'Y' | 'a' | 'A' | 'n' | 'N') | KeyCode::Esc => self.approval.take()?,
            _ => return None,
        };
        let action = match key.code {
            KeyCode::Char('y' | 'Y') => InputAction::Approve { id: ap.id, always: false },
            KeyCode::Char('a' | 'A') => InputAction::Approve { id: ap.id, always: true },
            _ => InputAction::Deny(ap.id),
        };
        self.next_approval();
        Some(action)
    }

    /// B-15: put `ap` on screen with a fresh scroll and grace timer.
    pub(super) fn show_approval(&mut self, ap: PendingApproval) {
        self.approval = Some(ap);
        self.approval_scroll = 0;
        self.approval_since = Some(std::time::Instant::now());
    }

    /// B-15: show the next queued approval, if any.
    pub(super) fn next_approval(&mut self) {
        self.approval_scroll = 0;
        self.approval_since = None;
        if let Some(ap) = self.approval_queue.pop_front() {
            self.show_approval(ap);
        }
    }

    /// M6: `/help` overlay — any key closes it (Ctrl+C still quits).
    fn on_help_key(&mut self, key: KeyEvent) -> Option<InputAction> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            // B-41: arrows scroll the help; any other key closes it.
            KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
            KeyCode::Down => {
                self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_scroll_max.get())
            }
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
            KeyCode::PageDown => {
                self.help_scroll = self.help_scroll.saturating_add(10).min(self.help_scroll_max.get())
            }
            _ => self.help = false,
        }
        None
    }

    /// M6: `/sessions` picker — ↑/↓ select, Enter switches, Esc closes.
    fn on_sessions_key(&mut self, key: KeyEvent) -> Option<InputAction> {
        let len = self.sessions.as_ref().map(|o| o.items.len()).unwrap_or(0);
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                None
            }
            KeyCode::Up => {
                if let Some(o) = self.sessions.as_mut() {
                    o.sel = o.sel.saturating_sub(1);
                }
                None
            }
            KeyCode::Down => {
                if let Some(o) = self.sessions.as_mut() {
                    o.sel = (o.sel + 1).min(len.saturating_sub(1));
                }
                None
            }
            KeyCode::Enter => {
                let ov = self.sessions.take()?;
                let item = ov.items.get(ov.sel)?;
                Some(InputAction::SwitchSession(item.id.clone()))
            }
            _ => {
                self.sessions = None;
                None
            }
        }
    }

    /// M6: complete the highlighted palette entry into the input line.
    fn complete_palette(&mut self) {
        let Some(p) = self.palette.take() else { return };
        let Some(&i) = p.matches.get(p.sel) else { return };
        self.input = format!("/{} ", builtins()[i].name);
        self.cursor = self.input.chars().count();
    }

    /// M6: recompute the palette after the input changed.
    fn refresh_palette(&mut self) {
        if self.help || self.sessions.is_some() || !self.input.starts_with('/') {
            self.palette = None;
            return;
        }
        let m = palette_matches(&self.input);
        self.palette = if m.is_empty() {
            None
        } else {
            Some(Palette { matches: m, sel: 0 })
        };
    }

    /// M6: `/clear` — drop the view, keep the session history.
    fn clear_view(&mut self) {
        self.entries.clear();
        self.scroll = 0;
        self.team = None;
        self.palette = None;
        self.agents.clear();
        self.push(
            EntryKind::System,
            "[view cleared — history kept: /resume to other sessions, /export to save]"
                .to_string(),
        );
    }
}
