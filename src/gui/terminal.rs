//! The terminal panel: its view (the user's tab and the agent's jobs), the
//! app's own `cd`, and the page's terminal verbs.

use super::*;

/// One line of work in the terminal panel's own tab.
#[derive(Debug, Clone, PartialEq)]
pub enum TermEntry {
    /// A command run as a background job (its output lives in `jobs`).
    Job(String),
    /// Text the app printed itself (`cd`, a command that could not start).
    Note(String),
}

/// Most output shown per command in the terminal tab, and per agent job.
pub(super) const TERM_TAIL: usize = 8_000;
pub(super) const JOB_TAIL: usize = 20_000;

/// The terminal panel's view: the folder, the shell, the user's tab (what
/// was typed and printed) and one tab per background job of the agent.
pub(super) fn term_view(gs: &GuiState, cwd: &std::path::Path) -> serde_json::Value {
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
pub(super) fn send_term(webview: &wry::WebView, gs: &GuiState) {
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
pub(super) fn cd_target(
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
pub(super) fn strip_verbatim(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => std::path::PathBuf::from(rest),
        _ => p,
    }
}

impl App {
    /// Terminal panel: shown or hidden.
    pub(super) fn on_term_open(&mut self, open: bool) -> bool {
        self.gs.term_open = open;
        *self.gs.term_root.borrow_mut() = chat_root(&self.gs, &self.st);
        self.gs.term_last.borrow_mut().clear(); // send the view again when shown
        true
    }

    /// Terminal panel: `cd` is handled here; anything else runs as a job.
    pub(super) fn on_term_run(&mut self, command: String) -> bool {
        let cwd = self.gs.term_cwd.clone().unwrap_or_else(|| chat_root(&self.gs, &self.st));
        if let Some(target) = cd_target(&command, &cwd, &chat_root(&self.gs, &self.st)) {
            let note = match target {
                Ok(dir) => {
                    let text = format!("$ {command}\n{}\n", dir.display());
                    self.gs.term_cwd = Some(dir);
                    text
                }
                Err(e) => format!("$ {command}\n{e}\n"),
            };
            self.gs.term_entries.push(TermEntry::Note(note));
        } else {
            // Jobs read their pipes on tokio tasks.
            let _guard = self.rt.enter();
            match crate::tools::jobs::spawn(&command, &cwd, true) {
                Ok(id) => self.gs.term_entries.push(TermEntry::Job(id)),
                Err(e) => self.gs.term_entries.push(TermEntry::Note(format!("$ {command}\ncould not start: {e:#}\n"))),
            }
        }
        true
    }

    /// Terminal panel: stop a command (`None` = the running one typed there).
    pub(super) fn on_term_stop(&self, id: Option<String>) {
        let running = crate::tools::jobs::list(0);
        let target = id.or_else(|| {
            running.iter().rev().find(|j| j.by_user && j.exit.is_none()).map(|j| j.id.clone())
        });
        if let Some(id) = target {
            self.rt.spawn(async move {
                let _ = crate::tools::jobs::stop(&id).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
