//! Messages from the page (`window.ipc.postMessage`) and from the preview
//! webview, and the parser that turns an IPC frame into one.

use super::*;

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
    Approve {
        id: String,
        always: bool,
    },
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
    SaveSettings {
        base_url: String,
        model: String,
        api_key: Option<String>,
        effort: String,
    },
    /// Settings → System prompt: the text (empty = none) and whether it
    /// replaces Habi's base instructions.
    SavePrompt {
        text: String,
        replace: bool,
    },
    /// Settings → Temperature: 0..=2, or `None` = the model's default.
    SaveTemperature(Option<f32>),
    /// Settings page: ask the provider which models it has.
    ListModels,
    /// Write the open chat as Markdown into its project.
    Export,
    /// Attach files picked in a dialog.
    PickFiles,
    /// Open the Changes view (the project's diff).
    LoadChanges,
    /// Open the token usage dashboard for the last N days (`None` = all time).
    LoadUsage(Option<i64>),
    /// Set a model's price per 1M tokens (dashboard).
    SetPrice {
        model: String,
        price: crate::db::Price,
    },
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
    Rewind {
        text: String,
        skip: usize,
    },
    /// `@` autocomplete: project files matching the text.
    FindFiles(String),
    /// Send line comments from the Changes view to the agent.
    ReviewComments(Vec<(String, u32, String)>),
    /// Attach a pasted or dropped file (`data` is base64).
    SaveAttachment {
        name: String,
        data: String,
    },
    /// Open the chat's project folder in a code editor (by name, or the first found).
    OpenInEditor(Option<String>),
    /// Projects sidebar: a new chat inside project `id`.
    NewInProject(String),
    /// Add a project: this folder, or (no path) ask with a folder picker.
    AddProject(Option<String>),
    /// Take a project off the list (its chats stay, without a project).
    RemoveProject(String),
    Pin {
        id: String,
        on: bool,
    },
    Archive {
        id: String,
        on: bool,
    },
    Rename {
        id: String,
        title: String,
    },
    /// Move a chat to a project (`None` = no project).
    MoveChat {
        id: String,
        project: Option<String>,
    },
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

/// What an IPC body is, safe to log: its `type` and length, never its
/// fields (a `settings` body carries the API key).
pub(super) fn ipc_summary(raw: &str) -> String {
    let kind = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t.chars().take(40).collect::<String>()));
    format!("type={} ({} bytes)", kind.as_deref().unwrap_or("?"), raw.len())
}

/// Parse one IPC frame; `None` = ignore (unknown or malformed).
pub fn parse_ipc(raw: &str) -> Option<IpcMsg> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    match v.get("type")?.as_str()? {
        "send" => {
            let text = v.get("text")?.as_str()?.trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(IpcMsg::Send(text))
            }
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
            if rect.is_usable() {
                Some(IpcMsg::PanelRect(rect, dpr))
            } else {
                None
            }
        }
        "panel_visible" => Some(IpcMsg::PanelVisible(v.get("visible")?.as_bool()?)),
        "open_dev_server" => Some(IpcMsg::OpenDevServer(v.get("url").and_then(|u| u.as_str()).map(str::to_string))),
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
        "save_prompt" => Some(IpcMsg::SavePrompt {
            text: v.get("text")?.as_str()?.to_string(),
            replace: v.get("replace").and_then(|r| r.as_bool()).unwrap_or(true),
        }),
        // `value` null (or missing) = the model's default; a number must be 0..=2.
        "save_temperature" => match v.get("value") {
            None | Some(serde_json::Value::Null) => Some(IpcMsg::SaveTemperature(None)),
            Some(n) => {
                let t = n.as_f64().filter(|t| (0.0..=2.0).contains(t))?;
                Some(IpcMsg::SaveTemperature(Some(t as f32)))
            }
        },
        "list_models" => Some(IpcMsg::ListModels),
        "export" => Some(IpcMsg::Export),
        "pick_files" => Some(IpcMsg::PickFiles),
        "load_changes" => Some(IpcMsg::LoadChanges),
        "load_usage" => Some(IpcMsg::LoadUsage(v.get("days").and_then(serde_json::Value::as_i64).filter(|d| *d > 0))),
        "set_price" => {
            let model = v.get("model")?.as_str()?.trim().to_string();
            let n = |k: &str| v.get(k).and_then(serde_json::Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0);
            (!model.is_empty()).then_some(IpcMsg::SetPrice {
                model,
                price: crate::db::Price { input: n("input")?, cached: n("cached")?, output: n("output")? },
            })
        }
        "term_open" => Some(IpcMsg::TermOpen(v.get("open")?.as_bool()?)),
        "term_run" => {
            let c = v.get("command")?.as_str()?.trim().to_string();
            if c.is_empty() {
                None
            } else {
                Some(IpcMsg::TermRun(c))
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ipc_body_that_is_not_understood_is_logged_without_its_fields() {
        let raw = r#"{"type":"settingz","api_key":"sk-secret-1"}"#;
        let shown = ipc_summary(raw);
        assert!(!shown.contains("sk-secret"), "{shown}");
        assert!(shown.contains("settingz") && shown.contains(&raw.len().to_string()));
        assert_eq!(ipc_summary("not json sk-x"), "type=? (13 bytes)");
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
        // Usage dashboard: a range in days (none = all time) and prices.
        assert_eq!(parse_ipc(r#"{"type":"load_usage","days":7}"#), Some(IpcMsg::LoadUsage(Some(7))));
        assert_eq!(
            parse_ipc(r#"{"type":"save_prompt","text":"Be brief.","replace":false}"#),
            Some(IpcMsg::SavePrompt { text: "Be brief.".into(), replace: false })
        );
        assert_eq!(parse_ipc(r#"{"type":"load_usage","days":null}"#), Some(IpcMsg::LoadUsage(None)));
        assert_eq!(
            parse_ipc(r#"{"type":"set_price","model":"gpt-6-sol","input":2,"cached":0.5,"output":8}"#),
            Some(IpcMsg::SetPrice {
                model: "gpt-6-sol".into(),
                price: crate::db::Price { input: 2.0, cached: 0.5, output: 8.0 }
            })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"set_price","model":"m","input":-1,"cached":0,"output":0}"#),
            None,
            "no negative prices"
        );
        assert_eq!(
            parse_ipc(r#"{"type":"add_project","path":"C:/code/app"}"#),
            Some(IpcMsg::AddProject(Some("C:/code/app".into())))
        );
        assert_eq!(parse_ipc(r#"{"type":"pin","id":"s","on":true}"#), Some(IpcMsg::Pin { id: "s".into(), on: true }));
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
        assert_eq!(parse_ipc(r#"{"type":"adopt_unfiled","project":"p"}"#), Some(IpcMsg::AdoptUnfiled("p".into())));
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
    fn ipc_parses_save_temperature() {
        assert_eq!(parse_ipc(r#"{"type":"save_temperature","value":0.7}"#), Some(IpcMsg::SaveTemperature(Some(0.7))));
        assert_eq!(parse_ipc(r#"{"type":"save_temperature","value":0}"#), Some(IpcMsg::SaveTemperature(Some(0.0))));
        assert_eq!(parse_ipc(r#"{"type":"save_temperature","value":2}"#), Some(IpcMsg::SaveTemperature(Some(2.0))));
        assert_eq!(
            parse_ipc(r#"{"type":"save_temperature","value":null}"#),
            Some(IpcMsg::SaveTemperature(None)),
            "null = the model's default"
        );
        assert_eq!(parse_ipc(r#"{"type":"save_temperature","value":2.5}"#), None, "above 2 is refused");
        assert_eq!(parse_ipc(r#"{"type":"save_temperature","value":-0.1}"#), None, "below 0 is refused");
        assert_eq!(parse_ipc(r#"{"type":"save_temperature","value":"hot"}"#), None);
    }

    #[test]
    fn ipc_mode_selector_parses_the_three_modes() {
        assert_eq!(parse_ipc(r#"{"type":"mode","mode":"default"}"#), Some(IpcMsg::Mode(PermissionMode::Default)));
        assert_eq!(parse_ipc(r#"{"type":"mode","mode":"manual"}"#), Some(IpcMsg::Mode(PermissionMode::Manual)));
        assert_eq!(parse_ipc(r#"{"type":"mode","mode":"turbo"}"#), Some(IpcMsg::Mode(PermissionMode::Turbo)));
        assert_eq!(parse_ipc(r#"{"type":"mode","mode":"yolo"}"#), None, "unknown mode dropped");
        assert_eq!(parse_ipc(r#"{"type":"mode"}"#), None, "mode without value dropped");
    }

    /// F-06: the Stop button's IPC verb.
    #[test]
    fn ipc_parses_stop() {
        assert_eq!(parse_ipc(r#"{"type":"stop"}"#), Some(IpcMsg::Stop));
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
        assert_eq!(parse_ipc(r#"{"type":"deny","id":"apr_1"}"#), Some(IpcMsg::Deny("apr_1".into())));
    }
}
