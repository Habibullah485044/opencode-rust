//! Running a tool call: routing (MCP, subagent, local), tool details and
//! the permission check with its approval preview.

use super::*;

/// Route a permitted tool call: MCP server, subagent spawn, or local tool.
/// (Mode/role gates already ran in `run_loop`.)
pub(super) async fn execute_routed(
    st: &Shared,
    session_id: &str,
    name: &str,
    input: &Value,
    abort: &AtomicBool,
) -> (String, bool) {
    if crate::mcp::parse_fn_name(name).is_some() {
        // B-20: the registry lock is held only for the lookup (the guard is
        // dropped at the end of this statement), never during the call.
        let route = st.mcp.lock().await.route(name);
        let result = match route {
            Ok(route) => tokio::select! {
                r = route.call(input) => r,
                _ = aborted(abort) => Err(anyhow::anyhow!("aborted")),
            },
            Err(e) => Err(e),
        };
        return match result {
            Ok(out) => (out, true),
            Err(e) => (format!("error: {e:#}"), false),
        };
    }
    if name == "spawn" {
        // Boxed: run_loop → spawn → run_loop is recursive async (E0733).
        let fut = crate::subagent::spawn_subagent(st, session_id, input, abort);
        return match Box::pin(fut).await {
            Ok(out) => (out, true),
            Err(e) => (format!("error: {e:#}"), false),
        };
    }
    if name == "ask_user" {
        let question = input.get("question").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if question.is_empty() {
            return ("error: ask_user needs a `question`".into(), false);
        }
        let options: Vec<String> = input
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).take(6).collect())
            .unwrap_or_default();
        let _ = st.events.send(ServerEvent::Question { session_id: session_id.to_string(), question, options });
        return ("The question is on the user's screen. Stop now; their answer comes as the next message.".into(), true);
    }
    if name == "mcp_search" {
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");
        let specs = st.mcp.lock().await.specs();
        return (crate::mcp::search(&specs, query, 6), true);
    }
    if name == "mcp_call" {
        return ("error: mcp_call needs the `name` of an MCP tool (mcp__<server>__<tool>) from mcp_search".into(), false);
    }
    if crate::subagent::is_agent_tool(name) {
        return crate::subagent::agent_tool(st, session_id, input, abort).await;
    }
    if name == "todo" {
        let op: crate::todo::TodoOp = match serde_json::from_value(input.clone()) {
            Ok(o) => o,
            Err(e) => return (format!("error: invalid todo args: {e}"), false),
        };
        let result = st.todo_store.exec(session_id, op);
        return (serde_json::to_string(&result).unwrap_or_default(), true);
    }
    if name.starts_with("browser_") {
        // Clone the handle out of the lock: the call awaits, and a std::Mutex
        // guard must not cross an await point.
        let handle = st.browser.lock().ok().and_then(|g| g.clone());
        let Some(handle) = handle else {
            return (
                "error: no preview panel is open — run the desktop app (--gui) and open Preview"
                    .to_string(),
                false,
            );
        };
        return match crate::browser::execute(&handle, name, input).await {
            Some(Ok(out)) => (out, true),
            Some(Err(e)) => (format!("error: {e}"), false),
            None => (format!("error: unknown browser tool {name}"), false),
        };
    }
    // grep/glob skip each file a `read` deny rule covers.
    match tools::execute_with(name, input, &st.workspace, abort, &st.engine.read_filter(name)).await {
        Ok(out) => (out, true),
        Err(e) => (format!("error: {e}"), false),
    }
}

/// F-05: built-in tools that take the workspace folder (cwd for `bash`,
/// base for relative paths). Only these are moved into a worker's worktree.
pub(super) const WORKSPACE_TOOLS: [&str; 7] = ["bash", "read", "write", "edit", "glob", "grep", "list"];

/// F-05: like `execute_routed`, but a run with its own workspace (an
/// isolated worker) runs the workspace tools in that folder.
pub(super) async fn execute_for(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    name: &str,
    input: &Value,
    abort: &AtomicBool,
) -> (String, bool) {
    if name == "check_page" {
        let root = opts.workspace.clone().unwrap_or_else(|| st.workspace_for(session_id));
        return check_page_tool(st, &root, input).await;
    }
    if let Some(ws) = &opts.workspace {
        if WORKSPACE_TOOLS.contains(&name) {
            return match tools::execute_with(name, input, ws, abort, &st.engine.read_filter(name)).await {
                Ok(out) => (out, true),
                Err(e) => (format!("error: {e}"), false),
            };
        }
    }
    execute_routed(st, session_id, name, input, abort).await
}

/// The `check_page` tool. In the desktop app the checked page also opens in
/// the preview panel, so the user sees what the agent checked.
pub(super) async fn check_page_tool(st: &Shared, root: &std::path::Path, input: &Value) -> (String, bool) {
    match crate::pagecheck::run_tool(root, input).await {
        Ok((text, report)) => {
            show_in_panel(st, &report.url);
            (text, true)
        }
        Err(e) => (format!("error: {e:#}"), false),
    }
}

/// Open `url` in the desktop app's preview panel, when it is open. Does not
/// wait for the page to load.
pub(super) fn show_in_panel(st: &Shared, url: &str) {
    let handle = st.browser.lock().ok().and_then(|g| g.clone());
    if let Some(handle) = handle {
        let url = url.to_string();
        tokio::spawn(async move {
            let _ = handle.call("navigate", serde_json::json!({ "url": url })).await;
        });
    }
}

/// F-05: like `tool_detail`, but paths are made relative to the run's own
/// workspace when it has one, so permission rules like `src/*` still match.
pub(super) fn tool_detail_for(st: &Shared, opts: &LoopOpts, tool: &str, input: &Value) -> String {
    match (&opts.workspace, tool) {
        (Some(ws), "read" | "write" | "edit" | "glob" | "grep") => {
            let p = input.get("path").and_then(Value::as_str).unwrap_or("");
            normalize_path_detail(ws, p)
        }
        (Some(ws), "list") => {
            normalize_path_detail(ws, input.get("path").and_then(Value::as_str).unwrap_or("."))
        }
        _ => tool_detail(st, tool, input),
    }
}

/// Flatten a tool's input to a human-readable detail string for rules + display.
/// The (old, new) pairs of an `edit` call: its `edits` list, or the single
/// old_str / new_str.
pub(super) fn edit_pairs(input: &Value) -> Vec<(String, String)> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match input.get("edits").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        Some(list) => list.iter().map(|e| (s(e, "old_str"), s(e, "new_str"))).collect(),
        None => vec![(s(input, "old_str"), s(input, "new_str"))],
    }
}

/// MCP tools as the model sees them: one by one when there are few, as
/// `mcp_search` + `mcp_call` when there are many (see `mcp::INLINE_MAX`).
pub(super) fn mcp_specs_for_prompt(specs: Vec<crate::provider::ToolSpec>) -> Vec<crate::provider::ToolSpec> {
    if specs.len() > crate::mcp::INLINE_MAX {
        crate::mcp::lazy_specs(specs.len())
    } else {
        specs
    }
}

pub(super) fn tool_detail(st: &Shared, tool: &str, input: &Value) -> String {
    match tool {
        "bash" => input.get("command").and_then(Value::as_str).unwrap_or("").to_string(),
        "read" | "write" | "edit" | "glob" | "grep" => {
            let p = input.get("path").and_then(Value::as_str).unwrap_or("");
            normalize_path_detail(&st.workspace, p)
        }
        "webfetch" => match input.get("query").and_then(Value::as_str).filter(|q| !q.trim().is_empty()) {
            Some(q) => format!("search: {q}"),
            None => input.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
        },
        "list" => normalize_path_detail(
            &st.workspace,
            input.get("path").and_then(Value::as_str).unwrap_or("."),
        ),
        _ => serde_json::to_string(input).unwrap_or_default(),
    }
}

/// New (B-22): what a `write`/`edit` call will change, for the approval
/// prompt. Empty for other tools (their `detail` already says it all).
pub fn approval_preview(tool: &str, input: &Value) -> String {
    const MAX_LINES: usize = 40;
    const MAX_CHARS: usize = 4_000;
    let field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let mut out = String::new();
    match tool {
        "write" => {
            let content = field("content");
            out.push_str(&format!(
                "write {} — {} line(s), {} byte(s)\n",
                field("path"),
                content.lines().count(),
                content.len()
            ));
            push_prefixed(&mut out, "+ ", content, MAX_LINES);
        }
        "edit" => {
            let all = input.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
            out.push_str(&format!("edit {}{}\n", field("path"), if all { " (every match)" } else { "" }));
            let pairs = edit_pairs(input);
            let share = (MAX_LINES / 2 / pairs.len().max(1)).max(2);
            for (i, (old, new)) in pairs.iter().enumerate() {
                if pairs.len() > 1 {
                    out.push_str(&format!("@@ change {} of {}\n", i + 1, pairs.len()));
                }
                push_prefixed(&mut out, "- ", old, share);
                push_prefixed(&mut out, "+ ", new, share);
            }
        }
        _ => return String::new(),
    }
    if out.len() > MAX_CHARS {
        let mut cut = MAX_CHARS;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n… (preview cut)");
    }
    out
}

/// New (B-22): append up to `max_lines` lines of `text`, each with `prefix`.
pub(super) fn push_prefixed(out: &mut String, prefix: &str, text: &str, max_lines: usize) {
    let total = text.lines().count();
    for line in text.lines().take(max_lines) {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    if total > max_lines {
        out.push_str(&format!("  … {} more line(s)\n", total - max_lines));
    }
}

/// Returns true if the tool call may proceed; blocks on an approval prompt when needed.
/// (No preview. The run loop calls `check_permission_preview`.)
#[allow(dead_code)] // used by tests; other callers may use it too
pub(super) async fn check_permission(
    st: &Shared,
    session_id: &str,
    tool: &str,
    detail: &str,
    abort: &AtomicBool,
) -> bool {
    check_permission_preview(st, session_id, tool, detail, "", abort).await
}

/// New (B-22): `check_permission` plus a `preview` of what the call will
/// change, shown with the approval prompt.
pub(super) async fn check_permission_preview(
    st: &Shared,
    session_id: &str,
    tool: &str,
    detail: &str,
    preview: &str,
    abort: &AtomicBool,
) -> bool {
    match decided_without_prompt(st, session_id, tool, detail) {
        Some(allowed) => allowed,
        None => ask_for_approval(st, session_id, tool, detail, preview, abort).await,
    }
}

/// The answer the rules and the permission mode give without asking the
/// user, or `None` when the user must be asked.
fn decided_without_prompt(st: &Shared, session_id: &str, tool: &str, detail: &str) -> Option<bool> {
    // "Always allow" answers belong to this chat's workspace only (S-07).
    let ws_key = crate::db::workspace_key(&st.workspace_for(session_id));
    let remembered = |t: &str, d: &str| st.db.has_approval(&ws_key, t, d).unwrap_or(false);
    let decision = st.engine.evaluate(tool, detail, remembered);
    // Hard `deny` rules are policy, not prompts: they survive every mode.
    if matches!(decision.action, Action::Deny) {
        return Some(false);
    }
    let allow = decision.action == Action::Allow;
    // Per chat: a running mission approves its own chats, never the others.
    let permitted = match st.perm_mode_for(session_id) {
        // Turbo = the user granted full access: never prompt.
        PermissionMode::Turbo => true,
        PermissionMode::Default => allow,
        // Manual gates the config's implicit allows, but an explicit
        // "always allow" is the user's own grant and still counts.
        PermissionMode::Manual => allow && remembered(tool, detail),
    };
    if permitted {
        return Some(true);
    }
    None
}

/// Ask the user through every client and wait for the answer (or the
/// timeout, or an abort). True when the call was allowed.
async fn ask_for_approval(
    st: &Shared,
    session_id: &str,
    tool: &str,
    detail: &str,
    preview: &str,
    abort: &AtomicBool,
) -> bool {
    // New (B-22): show what will change, right before the prompt.
    if !preview.is_empty() {
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: format!("approval needed — {}", preview.trim_end()),
        });
    }

    let (tx, rx) = oneshot::channel();
    let approval_id = crate::db::new_id("apr");
    // New (B-09): build the request first and keep a copy in the pending
    // map, so a client that missed it can get it again
    // (`AppState::pending_approvals`).
    let request = ServerEvent::ApprovalNeeded {
        session_id: session_id.to_string(),
        approval_id: approval_id.clone(),
        tool: tool.to_string(),
        detail: detail.to_string(),
        preview: preview.to_string(),
    };
    {
        let mut map = match st.approvals.lock() {
            Ok(m) => m,
            Err(_) => return false,
        };
        map.insert(
            approval_id.clone(),
            ApprovalEntry {
                tx,
                tool: tool.to_string(),
                detail: detail.to_string(),
                request: request.clone(),
            },
        );
    }
    let _ = st.events.send(request);
    let timeout = std::time::Duration::from_secs(st.cfg.permissions.approval_timeout_secs);
    let (allowed, reason) = tokio::select! {
        r = rx => match r {
            Ok(res) => (res.allowed, res.reason),
            // The entry was removed without an answer.
            Err(_) => (false, "abort"),
        },
        _ = tokio::time::sleep(timeout) => (false, "timeout"),
        _ = aborted(abort) => (false, "abort"),
    };
    // Always clean up the pending entry.
    if let Ok(mut map) = st.approvals.lock() {
        map.remove(&approval_id);
    }
    // New (B-10): exactly one "no longer pending" event per request, whoever
    // ended it, so every UI can close its modal.
    let _ = st.events.send(ServerEvent::ApprovalResolved {
        session_id: session_id.to_string(),
        approval_id,
        allowed,
        reason: reason.to_string(),
    });
    allowed
}
