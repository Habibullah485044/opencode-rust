//! Per-chat views of the state: workspace, token usage, transcripts and
//! the `/usage` and `/todos` reports.

use super::*;

impl AppState {
    /// The folder chat `session_id` works in: its project's folder, or the
    /// folder the app started in (no project, or the folder is gone).
    pub fn workspace_for(&self, session_id: &str) -> PathBuf {
        self.db
            .get_session(session_id)
            .ok()
            .flatten()
            .and_then(|s| s.project_id)
            .and_then(|p| self.db.get_project(&p).ok().flatten())
            .map(|p| PathBuf::from(p.root))
            .filter(|root| root.is_dir())
            .unwrap_or_else(|| self.workspace.clone())
    }

    /// The folder a run of chat `session_id` overrides the start folder
    /// with (`LoopOpts::workspace`), or `None` when it is the start folder.
    pub fn run_workspace(&self, session_id: &str) -> Option<PathBuf> {
        let ws = self.workspace_for(session_id);
        (ws != self.workspace).then_some(ws)
    }

    /// Save one reply's token counts (`/usage`, the usage dashboard) and tell
    /// the UIs. What it was for comes from the [`USAGE_TAG`] scope it runs
    /// in; a call outside a chat (planning, edits) takes the scope's chat.
    pub fn record_usage(&self, session_id: &str, model: &str, u: &crate::provider::Usage) {
        let tag = usage_tag();
        let session_id = if session_id.is_empty() { tag.1.as_deref().unwrap_or("") } else { session_id };
        self.record_usage_as(tag.0, session_id, model, u);
    }

    /// [`AppState::record_usage`] with the purpose given.
    pub fn record_usage_as(&self, purpose: &str, session_id: &str, model: &str, u: &crate::provider::Usage) {
        if let Err(e) = self.db.add_usage_entry(session_id, model, purpose, u) {
            core_log(&format!("[core] saving token usage failed: {e}"));
            return;
        }
        if session_id.is_empty() {
            return;
        }
        let session = self.db.usage_totals(Some(session_id), None).unwrap_or_default();
        let _ = self.events.send(ServerEvent::Usage {
            session_id: session_id.to_string(),
            last: *u,
            session,
        });
    }

}

impl AppState {
    /// M2 transcript replay for `/resume` and `/fork`.
    pub fn transcript(&self, session_id: &str) -> Result<Vec<TranscriptLine>> {
        Ok(self
            .db
            .messages(session_id)?
            .iter()
            .map(|m| TranscriptLine {
                role: m.role.clone(),
                text: crate::sessions::flatten_msg(m),
            })
            .collect())
    }

    /// M6: `/sessions` — newest first, with a marker for child sessions.
    pub fn session_list(&self) -> Vec<SessionInfo> {
        self.db
            .list_sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|s| SessionInfo {
                id: s.id,
                title: s.title,
                parent_id: s.parent_id,
                updated_at: s.updated_at,
            })
            .collect()
    }

    /// M6: the model the shared client will use next.
    pub async fn current_model(&self) -> String {
        self.client.lock().await.model.clone()
    }

    /// M6: `/model <id>` — switch the model every subsequent call uses.
    pub async fn set_model(&self, model: &str) {
        self.client.lock().await.set_model(model);
    }

    /// M6: `/model` with no argument — ask the endpoint what it offers.
    pub async fn available_models(&self) -> Vec<String> {
        // B-29: list with a copy, so the lock is not held during the request.
        let client = self.client.lock().await.clone();
        client.list_models().await.unwrap_or_default()
    }

    /// M6: `/export` — the session transcript as a standalone markdown file.
    pub fn transcript_markdown(&self, session_id: &str) -> String {
        let title = self
            .db
            .get_session(session_id)
            .ok()
            .flatten()
            .map(|s| s.title)
            .unwrap_or_else(|| "session".to_string());
        let mut out = format!("# {title}\n\n`{session_id}`\n\n");
        for line in self.transcript(session_id).unwrap_or_default() {
            let who = match line.role.as_str() {
                "user" => "**You**",
                "assistant" => "**Agent**",
                "tool" => "_tool_",
                "system" => "_system_",
                other => other,
            };
            out.push_str(&format!("### {who}\n\n{}\n\n", line.text.trim()));
        }
        out
    }

    /// New (S-16): first line of every exported transcript. `/export` only
    /// replaces an existing file that starts with it (or with `--force`).
    pub const EXPORT_MARKER: &'static str = "<!-- opencode-rust transcript export -->";

    /// New (S-16): `/export [--force] [path]`: write the transcript as
    /// markdown INSIDE the workspace and return the file written. Refuses
    /// paths that leave the workspace, and refuses to replace a file that is
    /// not an earlier export unless `--force` is given.
    pub fn export_transcript(&self, session_id: &str, arg: Option<&str>) -> Result<PathBuf> {
        let mut raw = arg.unwrap_or("").trim();
        let mut force = false;
        if let Some(rest) = raw.strip_prefix("--force") {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                force = true;
                raw = rest.trim();
            }
        }
        let name = if raw.is_empty() {
            format!("opencode-rust-transcript-{session_id}.md")
        } else {
            raw.to_string()
        };
        let workspace = self.workspace_for(session_id);
        let path = Self::export_path(&workspace, &name)?;
        // The text check above cannot see links: a symlinked folder (or
        // file) inside the workspace would carry the write outside it.
        Self::export_stays_inside(&workspace, &path)?;
        if path.exists() && !force {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if !existing.starts_with(Self::EXPORT_MARKER) {
                anyhow::bail!(
                    "{} already exists and is not an earlier export; pick another name or use /export --force {name}",
                    path.display()
                );
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Again with the folders made: one could have been swapped for a link.
        Self::export_stays_inside(&workspace, &path)?;
        let md = format!("{}\n{}", Self::EXPORT_MARKER, self.transcript_markdown(session_id));
        std::fs::write(&path, md).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// New (S-16): `raw` as a path inside `workspace`, checked on the text
    /// only: no `..`, and an absolute path must start with the workspace.
    pub(super) fn export_path(workspace: &std::path::Path, raw: &str) -> Result<PathBuf> {
        use std::path::Component;
        let p = std::path::Path::new(raw);
        let rel = if p.is_absolute() {
            p.strip_prefix(workspace)
                .map_err(|_| {
                    anyhow::anyhow!(
                        "export path must be inside the workspace ({}); use a relative path",
                        workspace.display()
                    )
                })?
                .to_path_buf()
        } else {
            p.to_path_buf()
        };
        let mut clean = PathBuf::new();
        for part in rel.components() {
            match part {
                Component::Normal(x) => clean.push(x),
                Component::CurDir => {}
                _ => anyhow::bail!("export path must stay inside the workspace (no `..`): {raw}"),
            }
        }
        if clean.as_os_str().is_empty() {
            anyhow::bail!("export path is empty");
        }
        Ok(workspace.join(clean))
    }

    /// `path` (from [`Self::export_path`]) on disk: its deepest existing
    /// folder, links resolved, must still be inside the workspace, and the
    /// file itself must not be a link.
    pub(super) fn export_stays_inside(workspace: &std::path::Path, path: &std::path::Path) -> Result<()> {
        let root = std::fs::canonicalize(workspace).with_context(|| format!("reading {}", workspace.display()))?;
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d.exists() {
                let real = std::fs::canonicalize(d).with_context(|| format!("reading {}", d.display()))?;
                if !real.starts_with(&root) {
                    anyhow::bail!("export path leaves the workspace through a link: {}", path.display());
                }
                break;
            }
            dir = d.parent();
        }
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            anyhow::bail!("export path is a link; pick another name: {}", path.display());
        }
        Ok(())
    }

    /// M6: `/todos` — the session's todo list as a transcript block.
    /// `/usage`: tokens for this chat, the last 7 days and all time, and how
    /// much of the input was read from the provider's prompt cache (billed
    /// at a fraction of the normal input price).
    /// Rough size in tokens of what every main-agent request of this chat
    /// starts with: the system prompt and the tool list (about 4 characters
    /// per token). The part the conversation does not change.
    pub fn request_overhead(&self, session_id: &str) -> (usize, usize, usize) {
        let mut specs = tools::tool_specs();
        if self.browser.lock().map(|g| g.is_some()).unwrap_or(false) {
            specs.extend(crate::browser::specs());
        }
        if let Ok(mcp) = self.mcp.try_lock() {
            specs.extend(mcp_specs_for_prompt(mcp.specs()));
        }
        let roles = agents::load_subagents(&self.workspace_for(session_id));
        specs.push(crate::subagent::spawn_spec(&roles));
        specs.extend(crate::subagent::agent_tool_specs());
        let specs = self.mode_for(session_id).filter_specs(specs);
        let tools_chars = serde_json::to_string(&specs).map_or(0, |s| s.len());
        (self.composed_system(session_id).len() / 4, tools_chars / 4, specs.len())
    }

    pub fn usage_report(&self, session_id: &str) -> String {
        let rows = [
            ("This chat", self.db.usage_totals(Some(session_id), None)),
            ("Last 7 days", self.db.usage_totals(None, Some(7))),
            ("All time", self.db.usage_totals(None, None)),
        ];
        let mut out = String::from("Token usage (cached = read from the provider's prompt cache)\n");
        out.push_str(&format!(
            "  {:<12} {:>8} {:>9} {:>17} {:>9}\n",
            "", "requests", "input", "cached", "output"
        ));
        let mut week = crate::db::UsageTotals::default();
        for (label, t) in rows {
            let t = t.unwrap_or_default();
            if label == "Last 7 days" {
                week = t;
            }
            out.push_str(&format!(
                "  {:<12} {:>8} {:>9} {:>17} {:>9}\n",
                label,
                t.requests,
                short_count(t.input),
                format!("{} ({}%)", short_count(t.cached), percent(t.cached, t.input)),
                short_count(t.output),
            ));
        }
        if week.requests == 0 {
            out.push_str("No model calls recorded yet.");
        } else if week.cached == 0 && week.requests >= 3 {
            out.push_str(
                "No cache reads reported. Either the provider does not cache prompts, does not \
                 report it, or (Anthropic models) needs `prompt_cache = \"on\"` under [provider].",
            );
        } else {
            out.push_str(&format!(
                "Cached input is billed at a fraction of the normal price (Anthropic about 10%, \
                 OpenAI and DeepSeek 10-50%). Output ({} tokens this week) is never cached.",
                short_count(week.output)
            ));
        }
        let (system, tools, count) = self.request_overhead(session_id);
        out.push_str(&format!(
            "\nEach request of this chat starts with about {} tokens before the conversation: \
             instructions {}, tool list {} ({count} tools).",
            short_count((system + tools) as u64),
            short_count(system as u64),
            short_count(tools as u64),
        ));
        out.push_str(&format!(
            "\nEvery request is logged in {} (cached share, and whether it started like the one before).",
            self.state_dir.join("cache.log").display()
        ));
        out
    }

    pub fn todos_report(&self, session_id: &str) -> String {
        let items = self.todo_store.list_all(session_id);
        if items.is_empty() {
            return "[todos] none yet — the agent adds them as it plans".to_string();
        }
        let open = items.iter().filter(|i| !i.completed).count();
        let mut out = format!("[todos] {} open / {} total\n", open, items.len());
        for i in &items {
            let mark = if i.completed { "✓" } else { "○" };
            out.push_str(&format!("  {mark} {}  ({})\n", i.content, i.id));
        }
        out
    }
}

/// 1234 → "1.2K", 3_400_000 → "3.4M" (for `/usage`).
fn short_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// `part` as a whole percent of `whole` (0 when `whole` is 0).
fn percent(part: u64, whole: u64) -> u64 {
    (part * 100).checked_div(whole).unwrap_or(0)
}
