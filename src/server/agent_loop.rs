//! The agent loop (`run_loop`): model steps, tool calls, the loop guard,
//! nudges, rewinds and per-run statistics.

use super::*;

/// M7: identity of a spawned subagent run — its tool activity is attributed
/// to the agents panel instead of the transcript.
#[derive(Clone)]
pub struct AgentInfo {
    pub id: String,
    /// The chat that spawned it.
    pub parent: String,
    pub name: String,
    pub task: String,
}

/// Per-run loop configuration (who is running and with what privileges).
pub struct LoopOpts {
    /// Fully composed system prompt.
    pub system: String,
    /// Emit ToolStarted/ToolFinished to the event bus (main agent: yes,
    /// team workers/subagents: no — their summary comes back as text).
    pub emit_activity: bool,
    /// Restrict the run to these tool names (subagent role). `None` = the
    /// session's plan/build mode decides the base restriction.
    pub allow: Option<Vec<String>>,
    /// May this run call the `spawn` tool? Only the top-level agent can.
    pub can_spawn: bool,
    /// F-05: run this loop's file and shell tools in another folder (an
    /// isolated worker's git worktree). `None` = the session workspace.
    pub workspace: Option<PathBuf>,
    /// M7: this run is a spawned subagent — status goes to the agents panel.
    pub agent: Option<AgentInfo>,
    /// Model for this run (a subagent role's own model). `None` = the
    /// configured model.
    pub model: Option<String>,
    /// Most model calls in this run. `None` = `agent.max_steps`.
    pub max_steps: Option<usize>,
}

/// F-09 loop guard: the third identical call in a row (same tool, same
/// arguments) is refused instead of run.
pub(crate) const LOOP_GUARD_LIMIT: u32 = 3;

/// F-09: remembers the last tool call of one run and how many times in a
/// row the model asked for exactly that call.
#[derive(Default)]
pub(crate) struct LoopGuard {
    pub(super) last: Option<String>,
    pub(super) streak: u32,
}

impl LoopGuard {
    /// Count one requested call. Returns the error text for the model when
    /// this is the LOOP_GUARD_LIMIT-th identical call in a row (or later).
    /// Any different call resets the count.
    pub(crate) fn check(&mut self, name: &str, input: &Value) -> Option<String> {
        let key = format!("{name} {}", canonical_json(input));
        if self.last.as_deref() == Some(key.as_str()) {
            self.streak += 1;
        } else {
            self.last = Some(key);
            self.streak = 1;
        }
        if self.streak < LOOP_GUARD_LIMIT {
            return None;
        }
        Some(format!(
            "loop guard: you asked for `{name}` with exactly the same arguments {} times \
             in a row, so it was not run again. Repeating it gives the same result. \
             Change your approach: use what the earlier results told you, try different \
             arguments or another tool, or stop and tell the user what blocks you.",
            self.streak
        ))
    }
}

/// JSON text with object keys sorted at every level, so `{"a":1,"b":2}`
/// and `{"b":2,"a":1}` give the same text.
pub(super) fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical_json(&map[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// F-09: run one batch of calls past the loop guard, in call order. A
/// refused call gets its error result right away (started/finished events,
/// stats, saved tool message) and is left out; the rest come back in order.
pub(super) async fn apply_loop_guard(
    st: &Shared,
    opts: &LoopOpts,
    session_id: &str,
    loop_guard: &mut LoopGuard,
    calls: Vec<(String, String, Value)>,
) -> Result<Vec<(String, String, Value)>> {
    let mut keep = Vec::with_capacity(calls.len());
    for (id, name, input) in calls {
        let Some(msg) = loop_guard.check(&name, &input) else {
            keep.push((id, name, input));
            continue;
        };
        let detail = tool_detail(st, &name, &input);
        tool_started(st, opts, session_id, &id, &name, &detail);
        if opts.emit_activity {
            let _ = st.events.send(ServerEvent::Note {
                session_id: session_id.to_string(),
                text: format!("loop guard: stopped a repeated `{name}` call"),
            });
        }
        finish_tool_call(st, opts, session_id, &id, &name, &input, msg, false).await?;
    }
    Ok(keep)
}

/// Did this request start the same way as the chat's previous one? Prompt
/// caches only reuse an unchanged start, so a change here is a miss we
/// caused. Returns how it differs: "same start", "first request",
/// "tool list changed", "system prompt changed" or "message N (role)
/// changed" (N counts from 1 after the system prompt).
pub(super) fn prefix_change(session_id: &str, msgs: &[ChatMsg], specs: &[crate::provider::ToolSpec]) -> String {
    use std::hash::{Hash, Hasher};
    // Per chat: hash of the tool list, hash of each message.
    type Seen = HashMap<String, (u64, Vec<u64>)>;
    static LAST: std::sync::OnceLock<Mutex<Seen>> = std::sync::OnceLock::new();
    let hash = |s: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut h);
        h.finish()
    };
    let tools = hash(&serde_json::to_string(specs).unwrap_or_default());
    let now: Vec<u64> = msgs.iter().map(|m| hash(&serde_json::to_string(m).unwrap_or_default())).collect();
    let mut last = LAST.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let note = match last.get(session_id) {
        None => "first request".to_string(),
        Some((t, _)) if *t != tools => "tool list changed".to_string(),
        Some((_, prev)) => match prev.iter().zip(&now).position(|(a, b)| a != b) {
            None if now.len() >= prev.len() => "same start".to_string(),
            None => "history got shorter (compacted or undone)".to_string(),
            Some(0) => "system prompt changed".to_string(),
            Some(i) => format!("message {i} ({}) changed", msgs[i].role),
        },
    };
    last.insert(session_id.to_string(), (tools, now));
    note
}

/// One line per model request in `<state_dir>/cache.log`: tokens in, how
/// many came from the provider's cache, and whether our side kept the start
/// of the request the same. "same start" with a low share = the provider
/// lost its cache (it expired, or another server answered).
pub(super) fn log_cache(st: &AppState, session_id: &str, usage: Option<&crate::provider::Usage>, prefix: &str) {
    if session_id.is_empty() {
        return;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = match usage {
        Some(u) => format!(
            "{secs} {} in={} cached={} ({}%) out={} {prefix}\n",
            short_id(session_id),
            u.input,
            u.cached,
            (u.cached * 100).checked_div(u.input).unwrap_or(0),
            u.output
        ),
        None => format!("{secs} {} no usage reported {prefix}\n", short_id(session_id)),
    };
    let _ = std::fs::create_dir_all(&st.state_dir);
    let path = st.state_dir.join("cache.log");
    // Keep it small: start over past 2 MB.
    if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub(super) fn short_id(id: &str) -> &str {
    let start = id.len().saturating_sub(8);
    id.get(start..).unwrap_or(id)
}

/// The words a user message is matched by when rewinding: what the chat
/// shows (`/mission x`, `/deep 3 x`) and what is stored (`[mission] x`,
/// `[deep mode, 3 attempts] x`) come out the same.
pub(super) fn rewind_key(text: &str) -> String {
    let t = text.trim().trim_start_matches('\u{203a}').trim().trim_start_matches(USER_TEXT_GUARD);
    let t = t
        .strip_prefix("[mission] ")
        .or_else(|| t.strip_prefix("/mission "))
        .or_else(|| t.strip_prefix("[deep mode, ").and_then(|r| r.split_once("] ").map(|(_, g)| g)))
        .or_else(|| {
            t.strip_prefix("/deep ").map(|r| {
                let (first, rest) = r.split_once(' ').unwrap_or((r, ""));
                if first.parse::<usize>().is_ok() && !rest.is_empty() { rest } else { r }
            })
        })
        .unwrap_or(t);
    t.trim().to_string()
}

/// What a rewind did.
pub struct Rewound {
    pub messages: usize,
    pub undone: usize,
    /// Undo could not put every file back.
    pub problem: Option<String>,
}

/// Rewind chat `session_id` to before the user message that reads `text`
/// (`skip` = how many later messages with the same text to pass over):
/// the project's files go back to the undo points taken since the message
/// before it, and that message and everything after it are deleted.
pub fn rewind_chat(st: &AppState, session_id: &str, text: &str, skip: usize) -> Result<Rewound> {
    let key = rewind_key(text);
    let msgs = st.db.messages(session_id)?;
    let user_text = |m: &Message| -> String {
        m.parts.iter().filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None }).collect()
    };
    let hits: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user" && rewind_key(&user_text(m)) == key)
        .map(|(i, _)| i)
        .collect();
    let Some(&at) = hits.iter().rev().nth(skip) else {
        anyhow::bail!("that message is not in this chat's history any more");
    };
    // Undo points taken after the previous user message belong to the runs
    // being rewound.
    let bound = msgs[..at].iter().rev().find(|m| m.role == "user").map(|m| m.created_at).unwrap_or(0);
    let ws = st.workspace_for(session_id);
    let mut undone = 0;
    let mut problem = None;
    while undone < 50 {
        match crate::gitsnap::newest_undo_time(&ws, &st.state_dir) {
            Some(t) if t > bound => match crate::gitsnap::undo(&ws, &st.state_dir) {
                Ok(Some(_)) => undone += 1,
                Ok(None) => break,
                Err(e) => {
                    undone += 1;
                    problem = Some(format!("{e:#}"));
                    break;
                }
            },
            _ => break,
        }
    }
    let messages = st.db.delete_messages_from(session_id, &msgs[at].id)?;
    Ok(Rewound { messages, undone, problem })
}

/// Did any file under `ws` change at or after `since`? Ignored and build
/// folders are skipped, and the walk stops after 20,000 entries (then it
/// answers yes, to be safe).
pub(super) fn files_changed_since(ws: &std::path::Path, since: std::time::SystemTime) -> bool {
    let walker = ignore::WalkBuilder::new(ws)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "node_modules" | "target" | ".opencode")))
        .build();
    for (n, entry) in walker.flatten().enumerate() {
        if n > 20_000 {
            return true;
        }
        let changed = entry
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .is_some_and(|t| t >= since);
        if changed {
            return true;
        }
    }
    false
}

/// Most "do it now" nudges per run.
pub(super) const MAX_NUDGES: u32 = 2;
/// Sent when a reply only announced work. Starts with a marker so it never
/// counts as the user's words (`first_request`, transcripts).
pub const NUDGE_TEXT: &str = "[app] You said what you will do but made no tool call, so nothing happened. \
    Do it now with your tools. Reply without a tool call only when the whole task is done (then give a \
    short summary of what you changed), or when you need the user to decide something (then ask).";

/// Appended to a reply that hit the model's output token limit.
pub(super) const CUT_OFF_NOTE: &str = "\n\n[reply cut off: the model reached its output token limit]";
/// Appended to the kept part of a reply whose stream kept breaking off.
pub(super) const BROKE_OFF_NOTE: &str =
    "\n\n[reply broke off: the connection dropped mid-reply; the part that arrived is kept]";
/// Asks for the rest of a reply the output token limit cut short.
pub(super) const CONTINUE_TEXT: &str = "[app] Your reply was cut short by the output token limit. Continue exactly \
    where you stopped — do not repeat what was already said. If the whole task is done now, end with \
    one short line instead.";

/// Does a reply end by announcing work it did not do ("I'll build it now.",
/// "Let me fix that:")? Looks at the last two sentences only, so a summary
/// that mentions a plan earlier on does not count, and never fires on a
/// question or on "let me know".
pub fn announces_work(text: &str) -> bool {
    let cleaned = strip_invisible(text);
    let t = cleaned.trim();
    if t.is_empty() || t.ends_with('?') || t.chars().count() > 1500 {
        return false;
    }
    let lower = t.to_lowercase().replace('\u{2019}', "'");
    if lower.contains("let me know") || lower.contains("would you like") || lower.contains("do you want") {
        return false;
    }
    if lower.ends_with(':') {
        return true;
    }
    let sentences: Vec<&str> = lower
        .split(['.', '!', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    const STARTS: [&str; 14] = [
        "i'll ", "i will ", "let me ", "let's ", "i'm going to ", "i am going to ", "now i'll ", "now i will ",
        "next, i", "next i", "first, i", "now let me ", "now, let me ", "i'll now ",
    ];
    sentences.iter().rev().take(2).any(|s| {
        // The sentence itself, or its last clause ("No, not yet. Let me…").
        let clause = s.rsplit([',', ';', '—']).next().unwrap_or(s).trim();
        STARTS.iter().any(|p| s.starts_with(p) || clause.starts_with(p))
    })
}

/// Some proxies report an upstream failure as if it were the model's
/// reply: a whole message like "[Error: HTTP 500: …]". That is an error
/// page, not an answer — saving it teaches the model to echo error text
/// as answers (seen live when a tunnel's proxy wrapped its failures into
/// the reply stream).
pub fn is_wrapped_error(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("[Error: HTTP") && t.ends_with(']')
}

/// Text without zero-width characters. Some models start every reply with
/// a few U+200B, which hid "I'll …" from `announces_work` (seen live).
pub fn strip_invisible(text: &str) -> String {
    text.chars().filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}')).collect()
}

/// Our tool for a name another agent uses (`list_files`, `Bash`,
/// `read_file`, `str_replace`…), with its arguments renamed to ours
/// (`file_path` → `path`, `cmd` → `command`…). `None` = not a known alias.
pub fn tool_alias(name: &str, input: &Value) -> Option<(&'static str, Value)> {
    let n = name.to_ascii_lowercase().replace(['-', ' '], "_");
    let real = match n.as_str() {
        "list_files" | "list_dir" | "list_directory" | "ls" | "listdir" | "dir" => "list",
        "bash" | "shell" | "run_terminal_command" | "run_command" | "execute_command" | "terminal" | "run_shell_command" | "exec" => "bash",
        "read_file" | "read_files" | "view" | "open_file" | "cat" | "view_file" => "read",
        "write_file" | "create_file" | "write_to_file" => "write",
        "edit_file" | "str_replace" | "str_replace_editor" | "replace_in_file" | "apply_edit" | "multiedit" | "multi_edit" => "edit",
        "find_files" | "file_search" | "search_files_by_name" => "glob",
        "search" | "code_search" | "search_files" | "ripgrep" | "rg" | "grep_search" => "grep",
        "web_fetch" | "fetch" | "read_url" | "fetch_url" | "web_search" => "webfetch",
        "todowrite" | "write_todos" | "todo_write" => "todo",
        // Same name in another case: `Read`, `Grep`, `Glob`, `Write`, `Edit`.
        other if ["read", "grep", "glob", "write", "edit", "list"].contains(&other) => {
            ["read", "grep", "glob", "write", "edit", "list"].into_iter().find(|t| *t == other)?
        }
        _ => return None,
    };
    let mut args = input.as_object().cloned().unwrap_or_default();
    let rename = |args: &mut serde_json::Map<String, Value>, from: &str, to: &str| {
        if !args.contains_key(to) {
            if let Some(v) = args.remove(from) {
                args.insert(to.to_string(), v);
            }
        }
    };
    for from in ["file_path", "filepath", "filename", "file", "target_file", "dir", "directory", "dir_path", "relative_workspace_path"] {
        rename(&mut args, from, "path");
    }
    for from in ["cmd", "script", "shell_command"] {
        rename(&mut args, from, "command");
    }
    rename(&mut args, "old_string", "old_str");
    rename(&mut args, "new_string", "new_str");
    rename(&mut args, "old_text", "old_str");
    rename(&mut args, "new_text", "new_str");
    rename(&mut args, "text", "content");
    rename(&mut args, "query", if real == "grep" { "pattern" } else { "query" });
    rename(&mut args, "regex", "pattern");
    rename(&mut args, "url", "url");
    // `list_files` with no path means the project folder.
    if real == "list" && !args.contains_key("path") {
        args.insert("path".into(), Value::String(".".into()));
    }
    // Extra keys other agents send (`description`, `timeout`) are left in;
    // our tools ignore what they do not know.
    Some((real, Value::Object(args)))
}

/// Does a reply claim the model cannot reach the user's files ("I can't see
/// your local folder… paste the contents")? Only short replies count, so a
/// real answer that mentions access in passing does not.
pub fn denies_file_access(text: &str) -> bool {
    let t = strip_invisible(text).to_lowercase().replace('\u{2019}', "'");
    if t.chars().count() > 2500 {
        return false;
    }
    let cannot = ["can't see", "cannot see", "can't read", "cannot read", "can't access", "cannot access",
        "don't have access", "do not have access", "unable to access", "unable to read", "no access to"];
    let files = ["file", "folder", "director", "local", "computer", "system", "project", "repo"];
    let ask = ["paste", "upload", "share", "provide", "send me"];
    cannot.iter().any(|c| t.contains(c)) && files.iter().any(|f| t.contains(f)) && ask.iter().any(|a| t.contains(a))
}

/// How a run loop ended. An abort used to come back as the answer text
/// "aborted", which a model reply of that one word could pass for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoopEnd {
    /// The final answer (or the step-limit note).
    Done(String),
    /// Stopped by the abort flag.
    Aborted,
}

impl LoopEnd {
    /// The old text form, for [`run_loop`]'s callers in other modules.
    pub(crate) fn into_legacy(self) -> String {
        match self {
            LoopEnd::Done(text) => text,
            LoopEnd::Aborted => "aborted".into(),
        }
    }
}

/// The agent loop: load history, call model, execute tools, repeat until done.
/// An aborted run returns the text "aborted" (see [`LoopEnd`]).
pub async fn run_loop(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    abort: &AtomicBool,
) -> Result<String> {
    run_loop_inner(st, session_id, opts, abort).await.map(LoopEnd::into_legacy)
}

/// [`run_loop`] with the ending as a [`LoopEnd`].
pub(crate) async fn run_loop_inner(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    abort: &AtomicBool,
) -> Result<LoopEnd> {
    let mode = st.mode_for(session_id);
    // A mission's chats run like turbo without the user's mode being turbo.
    let turbo = st.perm_mode_for(session_id) == PermissionMode::Turbo;
    // F-09: one loop guard per run (a spawned subagent has its own).
    let mut loop_guard = LoopGuard::default();
    // Replies that only said what the model would do next (see `announces_work`).
    let mut nudges = 0;
    // The history was summarized once this run because it was too long.
    let mut compacted = false;
    let specs = run_specs(st, session_id, opts, mode, turbo).await;

    let mut final_text = String::new();
    // New (B-26): set when the model stopped asking for tools by itself.
    let mut finished = false;
    let max_steps = opts.max_steps.unwrap_or(st.cfg.agent.max_steps);
    // Replies cut off mid-stream (dropped connection, stall, error frame)
    // that were asked again in this run.
    let mut stream_drops = 0u32;
    for _step in 0..max_steps {
        if abort.load(Ordering::SeqCst) {
            return Ok(LoopEnd::Aborted);
        }
        // A3: messages for a subagent from its parent, or finished
        // background reports for a parent, arrive before the next step.
        if let Some(text) = crate::subagent::take_mail(session_id) {
            save_message(st, session_id, "user", vec![Part::Text { text }]).await?;
        }
        // F-07: compact first when the context is nearly full. The request
        // is built once per step, and again only when compaction changed it.
        let mut msgs = load_chat_messages(st, session_id, &opts.system).await?;
        if auto_compact_if_needed(st, session_id, &msgs, &specs, abort).await {
            msgs = load_chat_messages(st, session_id, &opts.system).await?;
        }
        let prefix_note = prefix_change(session_id, &msgs, &specs);
        // B-29: copy the client out of the lock; never hold it while waiting.
        let client = st.client.lock().await.clone();
        let model = opts.model.as_deref().unwrap_or(&client.model);
        // A rate-limit wait can be long: Stop must still end it.
        let mut stream = tokio::select! {
            s = client.stream(model, &msgs, &specs) => match s {
                Ok(s) => s,
                // Too long for the model: summarize the older part once,
                // then ask again, instead of ending on the error.
                Err(e) if !compacted && is_context_overflow(&format!("{e:#}")) => {
                    compacted = true;
                    let _ = st.events.send(ServerEvent::Note {
                        session_id: session_id.to_string(),
                        text: "the chat got too long for the model — summarizing the older part and trying again".into(),
                    });
                    match crate::compact::compact(st, session_id, abort).await {
                        Ok(_) => continue,
                        Err(ce) => return Err(e.context(format!("the chat is too long for the model and could not be shortened: {ce:#}"))),
                    }
                }
                Err(e) => return Err(e),
            },
            _ = aborted(abort) => return Ok(LoopEnd::Aborted),
        };

        let StreamedReply { mut text, reasoning, acc, mut shown, finish_reason, broke_off, request_usage } =
            match read_reply(st, session_id, opts, model, &mut stream, abort, &mut stream_drops).await? {
                StreamOutcome::Reply(reply) => *reply,
                StreamOutcome::Aborted => return Ok(LoopEnd::Aborted),
                StreamOutcome::Dropped => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

        log_cache(st, session_id, request_usage.as_ref(), &prefix_note);
        let mut calls = acc.finish();
        // What the clients got, in characters, before any are taken out.
        let streamed_chars = text.get(..shown).map_or(0, |s| s.chars().count());
        if text.contains(['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}']) {
            (text, shown) = strip_streamed(&text, shown);
        }
        // An upstream failure served as fake reply text (see
        // `is_wrapped_error`). Never save it as an answer: shorten the
        // chat when it says the request was too long, ask again, and fail
        // with the message when it keeps coming back.
        if calls.is_empty() && is_wrapped_error(&text) {
            // It streamed in as reply text: take it off the screen.
            retract_shown(st, session_id, streamed_chars);
            retry_after_wrapped_error(st, session_id, abort, &text, &mut compacted, &mut stream_drops).await?;
            continue;
        }
        // Some models (Qwen through Ollama/llama.cpp without a tool parser)
        // write their tool calls into the reply text: wrapped, as the
        // `CALL {…}` line the guide teaches, or as the bare JSON object.
        // Run them as calls instead of showing the JSON and ending the turn.
        // (The gate used to require the wrapped form, which every other
        // shape failed: the run then ended with the JSON on screen.)
        if calls.is_empty() {
            let (rest, found) = crate::provider::calls_in_text(&text);
            if !found.is_empty() {
                text = rest;
                calls = found;
            }
        }
        // Text held back above that turned out not to be a call: show it now.
        if calls.is_empty() && shown < text.len() && text.is_char_boundary(shown) {
            let _ = st.events.send(ServerEvent::AssistantDelta {
                session_id: session_id.to_string(),
                text: text[shown..].to_string(),
            });
        }
        // B-03: "length" = the model hit its output token limit.
        let truncated = finish_reason.as_deref() == Some("length");
        note_cut_reply(st, session_id, truncated, broke_off, stream_drops, calls.is_empty(), &mut text);
        let cut_off = |v: &Value| v.as_object().map(|o| o.len() == 1 && o.contains_key("raw")).unwrap_or(false);
        if (truncated || broke_off) && calls.iter().any(|(_, _, input)| cut_off(input)) {
            save_cut_off_calls(st, session_id, &reasoning, &text, &calls).await?;
            continue;
        }
        save_assistant_reply(st, session_id, reasoning, &text, &calls).await?;

        if calls.is_empty() {
            if nudge_on_empty_reply(st, session_id, opts, mode, truncated, &specs, &text, &mut nudges).await? {
                continue;
            }
            final_text = text;
            finished = true;
            break;
        }

        let calls = normalize_calls(st, opts, session_id, &specs, &mut loop_guard, calls).await?;
        // A step that only ticks off todos next to a final answer needs no
        // further model call (Freebuff's bookkeeping tools): one request saved.
        let bookkeeping_only = calls.iter().all(|(_, n, i)| {
            n == "todo" && matches!(i.get("op").and_then(Value::as_str), Some("complete" | "remove" | "list"))
        });
        let asked = calls
            .iter()
            .find(|(_, n, _)| n == "ask_user")
            .and_then(|(_, _, i)| i.get("question").and_then(Value::as_str).map(str::to_string));
        run_tool_calls(st, session_id, opts, mode, calls, abort).await?;
        // The user was asked: stop until they answer.
        if let Some(question) = asked.filter(|_| opts.agent.is_none() && !turbo) {
            final_text = if text.trim().is_empty() { question } else { text };
            finished = true;
            break;
        }
        if bookkeeping_only && !text.trim().is_empty() {
            final_text = text;
            finished = true;
            break;
        }
    }
    // New (B-26): the steps ran out while the model still wanted tools. Say
    // so instead of ending with an empty, "successful" answer.
    if !finished {
        return Ok(steps_ran_out(st, session_id, max_steps));
    }
    Ok(LoopEnd::Done(final_text))
}

/// The tool specs one run offers: base tools + MCP tools (+ spawn), then
/// filtered by mode, role, turbo and the run's allow list.
async fn run_specs(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    mode: AgentMode,
    turbo: bool,
) -> Vec<crate::provider::ToolSpec> {
    // Spec list: base tools + MCP tools (+ spawn), then filter by mode/role.
    let mut specs = tools::tool_specs();
    // Browser tools only work while a GUI panel is attached. Advertising them
    // in the TUI/headless just invites calls that can only fail — and it is
    // one more way for a small model to decide "hi" needs tooling.
    if st.browser.lock().map(|g| g.is_some()).unwrap_or(false) {
        specs.extend(crate::browser::specs());
    }
    specs.extend(mcp_specs_for_prompt(st.mcp.lock().await.specs()));
    if opts.can_spawn {
        // Once per run (T-063), and off the async threads: it reads up to
        // three folders (and the chat's project from the DB).
        let roles = {
            let (st, sid) = (st.clone(), session_id.to_string());
            tokio::task::spawn_blocking(move || agents::load_subagents(&st.workspace_for(&sid))).await.unwrap_or_default()
        };
        specs.push(crate::subagent::spawn_spec(&roles));
        specs.extend(crate::subagent::agent_tool_specs());
    }
    let mut specs = mode.filter_specs(specs);
    // A subagent skips `todo`: the plan list belongs to the chat, and the
    // spec is sent with every request.
    if opts.agent.is_some() {
        specs.retain(|s| s.function.name != "todo" && s.function.name != "ask_user");
    }
    // Turbo means "decide yourself": no questions.
    if turbo {
        specs.retain(|s| s.function.name != "ask_user");
    }
    match &opts.allow {
        Some(allow) => specs
            .into_iter()
            .filter(|s| allow.contains(&s.function.name))
            .collect(),
        None => specs,
    }
}

/// One model reply as it streamed in.
struct StreamedReply {
    text: String,
    reasoning: String,
    acc: ToolCallAccumulator,
    /// How much of `text` was already streamed to the clients.
    shown: usize,
    finish_reason: Option<String>,
    /// The stream broke off for good: the part that arrived is kept.
    broke_off: bool,
    request_usage: Option<crate::provider::Usage>,
}

/// How reading one reply's stream ended.
enum StreamOutcome {
    Reply(Box<StreamedReply>),
    /// Cut off before anything was saved: ask the same step again.
    Dropped,
    Aborted,
}

/// Read one reply's stream, showing its text as it comes.
async fn read_reply(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    model: &str,
    stream: &mut futures::stream::BoxStream<'static, Result<StreamEvent>>,
    abort: &AtomicBool,
    stream_drops: &mut u32,
) -> Result<StreamOutcome> {
    let mut request_usage: Option<crate::provider::Usage> = None;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut acc = ToolCallAccumulator::default();
    // How much of `text` was already streamed to the clients.
    let mut shown = 0usize;
    let mut finish_reason: Option<String> = None;
    // Poll the stream in racing with the abort flag: a replaced run must
    // exit promptly even when the transport yields nothing at all.
    let stop = aborted(abort);
    tokio::pin!(stop);
    // The stream broke off for good: the part that arrived is kept.
    let mut broke_off = false;
    loop {
        let ev = tokio::select! {
            ev = stream.next() => ev,
            _ = &mut stop => return Ok(StreamOutcome::Aborted),
        };
        let Some(ev) = ev else { break };
        if abort.load(Ordering::SeqCst) {
            return Ok(StreamOutcome::Aborted);
        }
        // Nothing of a cut-off reply is saved yet: ask the same step again
        // instead of ending the run (a free provider drops streams often).
        let ev = match ev {
            Ok(ev) => ev,
            Err(e) if *stream_drops < MAX_STREAM_DROPS => {
                *stream_drops += 1;
                // The part already on screen is asked again from the
                // start: take it back, or partial + full reply show.
                retract_shown(st, session_id, text.get(..shown).map_or(0, |s| s.chars().count()));
                let _ = st.events.send(ServerEvent::Note {
                    session_id: session_id.to_string(),
                    text: format!(
                        "the model's reply broke off ({e:#}); asking again \
                         ({}/{MAX_STREAM_DROPS})",
                        *stream_drops
                    ),
                });
                return Ok(StreamOutcome::Dropped);
            }
            // The stream keeps breaking off (tunnel, proxy): throwing
            // the reply away and failing the run loses work the user
            // already watched stream in. Keep what arrived instead.
            Err(e) => {
                if text.is_empty() && acc.is_empty() {
                    return Err(e);
                }
                broke_off = true;
                break;
            }
        };
        match &ev {
            StreamEvent::Text(t) => {
                text.push_str(t);
                // Show the reply as it comes, but not a tool call the model
                // writes as text (`<tool_call>…`): hold back from its start,
                // and a possible start at the very end, until we know.
                let limit = text.find("<tool_call").unwrap_or_else(|| {
                    let tail = text.rfind('<').filter(|&i| "<tool_call>".starts_with(&text[i..]));
                    tail.unwrap_or(text.len())
                });
                if limit > shown {
                    let _ = st.events.send(ServerEvent::AssistantDelta {
                        session_id: session_id.to_string(),
                        text: text[shown..limit].to_string(),
                    });
                    shown = limit;
                }
            }
            StreamEvent::Reasoning(t) => {
                reasoning.push_str(t);
                if opts.emit_activity {
                    let _ = st.events.send(ServerEvent::ReasoningDelta {
                        session_id: session_id.to_string(),
                        text: t.clone(),
                    });
                }
            }
            StreamEvent::Usage(u) => {
                st.record_usage_as(if opts.agent.is_some() { "subagent" } else { usage_tag().0 }, session_id, model, u);
                request_usage = Some(*u);
            }
            StreamEvent::Done { finish_reason: fr } => {
                finish_reason = fr.clone();
                break;
            }
            _ => acc.push(&ev),
        }
    }
    Ok(StreamOutcome::Reply(Box::new(StreamedReply { text, reasoning, acc, shown, finish_reason, broke_off, request_usage })))
}

/// The reply was an upstream error served as text. `Ok` means ask the same
/// step again (after shortening the chat when it was too long); `Err` ends
/// the run when the error keeps coming back.
async fn retry_after_wrapped_error(
    st: &Shared,
    session_id: &str,
    abort: &AtomicBool,
    text: &str,
    compacted: &mut bool,
    stream_drops: &mut u32,
) -> Result<()> {
    if !*compacted && is_context_overflow(text) {
        *compacted = true;
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "the chat got too long for the model — summarizing the older part and trying again".into(),
        });
        if crate::compact::compact(st, session_id, abort).await.is_ok() {
            return Ok(());
        }
    }
    if *stream_drops < MAX_STREAM_DROPS {
        *stream_drops += 1;
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: format!(
                "the provider sent an error instead of a reply — asking again \
                 ({}/{MAX_STREAM_DROPS})",
                *stream_drops
            ),
        });
        return Ok(());
    }
    Err(anyhow::anyhow!("the provider kept answering with an error: {}", text.trim()))
}

/// Tell the user a reply was cut off (output limit or a broken stream), and
/// mark a reply without tool calls as cut off for the model.
fn note_cut_reply(
    st: &Shared,
    session_id: &str,
    truncated: bool,
    broke_off: bool,
    stream_drops: u32,
    no_calls: bool,
    text: &mut String,
) {
    if truncated {
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "the model's reply was cut off by its output token limit".to_string(),
        });
        if no_calls {
            text.push_str(CUT_OFF_NOTE);
        }
    }
    if broke_off {
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: format!(
                "the connection broke off {} time(s) — keeping the part of the reply that arrived",
                stream_drops.max(1)
            ),
        });
        if no_calls {
            text.push_str(BROKE_OFF_NOTE);
        }
    }
}

/// Save a cut-off reply whose tool calls lost their arguments, each call
/// answered with an error asking for smaller pieces.
async fn save_cut_off_calls(
    st: &Shared,
    session_id: &str,
    reasoning: &str,
    text: &str,
    calls: &[(String, String, Value)],
) -> Result<()> {
    let mut parts = Vec::new();
    if !reasoning.is_empty() {
        parts.push(Part::Reasoning { text: reasoning.to_string() });
    }
    if !text.is_empty() {
        parts.push(Part::Text { text: text.to_string() });
    }
    for (id, name, input) in calls {
        parts.push(Part::ToolCall { id: id.clone(), name: name.clone(), input: input.clone() });
    }
    let ids: Vec<String> = calls.iter().map(|(id, _, _)| id.clone()).collect();
    let sid = session_id.to_string();
    // One trip off the async threads; the calls go in before their results.
    st.db
        .run(move |db| {
            db.add_message(&sid, "assistant", &parts)?;
            for id in ids {
                let output = "not run: your reply hit the output token limit before this call's \
                              arguments were complete. Send it again in smaller pieces (for \
                              example write a big file in several steps)."
                    .to_string();
                db.add_message(&sid, "tool", &[Part::ToolResult { tool_id: id, output, is_error: true }])?;
            }
            Ok(())
        })
        .await
}

/// Save the assistant's reply: reasoning, text and tool calls.
async fn save_assistant_reply(
    st: &Shared,
    session_id: &str,
    reasoning: String,
    text: &str,
    calls: &[(String, String, Value)],
) -> Result<()> {
    let mut assistant_parts = Vec::new();
    if !reasoning.is_empty() {
        assistant_parts.push(Part::Reasoning { text: reasoning });
    }
    if !text.is_empty() {
        assistant_parts.push(Part::Text { text: text.to_string() });
    }
    for (id, name, input) in calls {
        assistant_parts.push(Part::ToolCall {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        });
    }
    if !assistant_parts.is_empty() {
        save_message(st, session_id, "assistant", assistant_parts).await?;
    }
    Ok(())
}

/// A reply without tool calls that should not end the turn (cut off by the
/// output limit, only a plan, or "I can't see your files"): add a message
/// asking for more and return `true`.
#[allow(clippy::too_many_arguments)]
async fn nudge_on_empty_reply(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    mode: AgentMode,
    truncated: bool,
    specs: &[crate::provider::ToolSpec],
    text: &str,
    nudges: &mut u32,
) -> Result<bool> {
    // A healthy transport cut by the output limit mid-task: ask for
    // the rest instead of ending the turn on half an answer.
    if truncated && *nudges < MAX_NUDGES && !text.trim().is_empty() {
        *nudges += 1;
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "the reply hit its output limit mid-way — asking the model to continue".into(),
        });
        save_message(st, session_id, "user", vec![Part::Text { text: CONTINUE_TEXT.to_string() }]).await?;
        return Ok(true);
    }
    // "I'll build it now." and no tool call: weak models end their
    // turn on a plan. Nothing happened, so ask once or twice for the
    // work itself instead of reporting the plan as the answer.
    if *nudges < MAX_NUDGES && mode == AgentMode::Build && !truncated && announces_work(text) {
        *nudges += 1;
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "the model said what it will do but did not do it — asking it to go ahead".into(),
        });
        save_message(st, session_id, "user", vec![Part::Text { text: NUDGE_TEXT.to_string() }]).await?;
        return Ok(true);
    }
    // "I can't see your files, please paste them": the model forgot
    // it has tools. Remind it once or twice, with the folder.
    if *nudges < MAX_NUDGES && !truncated && !specs.is_empty() && denies_file_access(text) {
        *nudges += 1;
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "the model said it cannot see the files — reminding it that it has tools".into(),
        });
        let (st2, sid, folder) = (st.clone(), session_id.to_string(), opts.workspace.clone());
        st.db
            .run(move |db| {
                let folder = folder.unwrap_or_else(|| st2.workspace_for(&sid));
                let reminder = format!(
                    "[app] You can see the files: you have tools (list, glob, grep, read) that work in the project \
                     folder {}. Do not ask the user to paste or upload anything. Use the tools now to do what \
                     was asked.",
                    folder.display()
                );
                db.add_message(&sid, "user", &[Part::Text { text: reminder }]).map(drop)
            })
            .await?;
        return Ok(true);
    }
    Ok(false)
}

/// The step's tool calls as they will run: other agents' tool names mapped
/// to ours, the loop guard applied, and `mcp_call` unwrapped.
async fn normalize_calls(
    st: &Shared,
    opts: &LoopOpts,
    session_id: &str,
    specs: &[crate::provider::ToolSpec],
    loop_guard: &mut LoopGuard,
    calls: Vec<(String, String, Value)>,
) -> Result<Vec<(String, String, Value)>> {
    // Models trained on other agents call tools by those agents' names
    // (`list_files`, `Bash`, `read_file`…): run them as ours.
    let known: std::collections::HashSet<&str> = specs.iter().map(|s| s.function.name.as_str()).collect();
    let calls: Vec<_> = calls
        .into_iter()
        .map(|(id, name, input)| match tool_alias(&name, &input).filter(|_| !known.contains(name.as_str())) {
            Some((real, args)) if known.contains(real) => (id, real.to_string(), args),
            _ => (id, name, input),
        })
        .collect();
    // F-09 loop guard: an identical call repeated LOOP_GUARD_LIMIT times
    // in a row is answered with an error and never runs (no approval
    // prompt either). Every requested call counts, spawns included.
    let calls = apply_loop_guard(st, opts, session_id, loop_guard, calls).await?;
    // `mcp_call` runs as the MCP tool it names: permissions, role and
    // mode rules and the activity view all see the real tool.
    Ok(calls
        .into_iter()
        .map(|(id, name, input)| match crate::mcp::unwrap_call(&name, &input) {
            Some((inner, args)) => (id, inner, args),
            None => (id, name, input),
        })
        .collect())
}

/// Execute each tool call: role gate → permission gate → route.
/// `spawn` calls run concurrently with each other — the agents panel
/// shows the parallel work — everything else keeps its call order.
async fn run_tool_calls(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    mode: AgentMode,
    calls: Vec<(String, String, Value)>,
    abort: &AtomicBool,
) -> Result<()> {
    // (Concrete boxed type: a `dyn Future + Send` here deadlocks the
    // auto-trait proof for the run_loop ↔ spawn recursion.)
    let mut spawns = Vec::new();
    for (id, name, input) in calls {
        let detail = tool_detail_for(st, opts, &name, &input);
        tool_started(st, opts, session_id, &id, &name, &detail);
        let denied = call_denied(st, session_id, opts, mode, &name, &input, &detail, abort).await;
        if name == "spawn" && denied.is_none() {
            let sid = session_id.to_string();
            let inp = input.clone();
            let fut = Box::pin(async move {
                match crate::subagent::spawn_subagent(st, &sid, &inp, abort).await {
                    Ok(out) => (out, true),
                    Err(e) => (format!("error: {e:#}"), false),
                }
            });
            spawns.push(((id, name, input, detail), fut));
            continue;
        }
        let (output, ok) = match denied {
            Some(m) => (m, false),
            None => execute_for(st, session_id, opts, &name, &input, abort).await,
        };
        finish_tool_call(st, opts, session_id, &id, &name, &input, output, ok).await?;
    }
    // Join the parallel spawns; results are recorded in call order.
    let (meta, futs): (Vec<_>, Vec<_>) = spawns.into_iter().unzip();
    for ((id, name, input, _detail), (output, ok)) in
        meta.into_iter().zip(futures::future::join_all(futs).await)
    {
        finish_tool_call(st, opts, session_id, &id, &name, &input, output, ok).await?;
    }
    Ok(())
}

/// Why a tool call may not run (aborted, blocked by mode/role, or refused
/// by the user), or `None` when it may.
#[allow(clippy::too_many_arguments)]
async fn call_denied(
    st: &Shared,
    session_id: &str,
    opts: &LoopOpts,
    mode: AgentMode,
    name: &str,
    input: &Value,
    detail: &str,
    abort: &AtomicBool,
) -> Option<String> {
    // New (B-06): once the run is aborted, the remaining calls of this
    // turn are not run (each still gets a result, so the history
    // stays valid).
    if abort.load(Ordering::SeqCst) {
        Some("not run: the run was aborted".to_string())
    } else if !tool_allowed(mode, opts, name) {
        // Naming the tools that DO work turns "everything is
        // blocked" flailing into one corrected call (seen live in
        // the review run: it tried Bash twice, then claimed even
        // grep was blocked).
        Some(match &opts.allow {
            Some(allow) => format!(
                "tool blocked: this run can only call these tools: {}",
                allow.join(", ")
            ),
            None => "tool blocked: not allowed in this mode/role".to_string(),
        })
    } else if crate::tools::is_job_control(name, input) && agent_job(input) {
        None // reading or stopping a job it was allowed to start
    } else if !check_permission_preview(
        st,
        session_id,
        name,
        &job_detail(name, input).unwrap_or_else(|| detail.to_string()),
        &approval_preview(name, input),
        abort,
    )
    .await
    {
        Some("permission denied by user".to_string())
    } else {
        None
    }
}

/// The steps ran out while the model still wanted tools: say so.
fn steps_ran_out(st: &Shared, session_id: &str, max_steps: usize) -> LoopEnd {
    let msg = format!(
        "stopped after {max_steps} steps (agent.max_steps) — the task may be unfinished. \
         Send a message to continue, or raise agent.max_steps in the config."
    );
    let _ = st.events.send(ServerEvent::Note {
        session_id: session_id.to_string(),
        text: msg.clone(),
    });
    LoopEnd::Done(msg)
}

/// `text` without invisible characters, and `shown` (bytes of `text` already
/// streamed) moved to the same place in the cleaned text: the shown part
/// shrinks by the characters taken out of it. `min(len)` dropped or doubled
/// the end of the reply.
pub(super) fn strip_streamed(text: &str, shown: usize) -> (String, usize) {
    let cleaned = strip_invisible(text);
    let shown = text.get(..shown).map_or(shown, |head| strip_invisible(head).len());
    let shown = shown.min(cleaned.len());
    (cleaned, shown)
}

/// Take back `chars` characters of reply text already streamed when its
/// step is asked again ([`ServerEvent::ReplyRetracted`]).
pub(super) fn retract_shown(st: &Shared, session_id: &str, chars: usize) {
    if chars > 0 {
        let _ = st.events.send(ServerEvent::ReplyRetracted { session_id: session_id.to_string(), chars });
    }
}

/// Is the `bash {job}` call about a job the agent started (`job<n>`)? The
/// user's own terminal commands (`term<n>`) are not the agent's to read or
/// stop without asking.
pub(super) fn agent_job(input: &Value) -> bool {
    input
        .get("job")
        .and_then(Value::as_str)
        .and_then(|id| id.strip_prefix("job"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// What the approval prompt names for a `bash {job}` call (it has no
/// command, so the usual detail is empty).
pub(super) fn job_detail(name: &str, input: &Value) -> Option<String> {
    if name != "bash" || input.get("command").is_some() {
        return None;
    }
    let id = input.get("job").and_then(Value::as_str)?;
    let stop = input.get("stop").and_then(Value::as_bool).unwrap_or(false);
    Some(if stop { format!("stop job {id}") } else { format!("read job {id}") })
}

/// Tool-started event: transcript for a top-level run, agents-panel status
/// when this run is a spawned subagent.
pub(super) fn tool_started(st: &Shared, opts: &LoopOpts, session_id: &str, id: &str, name: &str, detail: &str) {
    if let Some(a) = &opts.agent {
        agent_status(st, a, "working", format!("{name}({detail})"));
    } else if opts.emit_activity {
        let _ = st.events.send(ServerEvent::ToolStarted {
            session_id: session_id.to_string(),
            call_id: id.to_string(),
            tool: name.to_string(),
            detail: detail.to_string(),
        });
    }
}

/// Record one finished tool call: line accounting, then the reuse gate (its
/// hint is appended to the result so the MODEL reads it mid-run —
/// enforcement, not decoration), stats, the finish event, and the persisted
/// tool message.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_tool_call(
    st: &Shared,
    opts: &LoopOpts,
    session_id: &str,
    id: &str,
    name: &str,
    input: &Value,
    mut output: String,
    ok: bool,
) -> Result<()> {
    // New (P-01): refused, blocked or failed calls changed nothing.
    if ok {
        account_tool(st, session_id, name, input);
    }
    // A dev server announces itself on stdout (Vite, Next, uvicorn, cargo run…).
    // Surfacing it here means the panel can offer "open" without the model
    // having to remember the port.
    if ok && name == "bash" {
        if let Some(url) = crate::browser::detect_dev_server(&output) {
            let _ = st.events.send(ServerEvent::DevServer {
                session_id: session_id.to_string(),
                url,
            });
        }
    }
    if ok && st.cfg.agent.reuse_gate {
        if let Some(hint) = reuse_gate(opts.workspace.as_deref().unwrap_or(&st.workspace), name, input) {
            output.push_str(&format!("\n[{hint}]"));
            bump_hints(st, session_id);
            let _ = st.events.send(ServerEvent::Note {
                session_id: session_id.to_string(),
                text: hint,
            });
        }
    }
    {
        let s = snapshot_stats(st, session_id);
        let _ = st.events.send(ServerEvent::Stats {
            session_id: session_id.to_string(),
            added: s.added,
            deleted: s.deleted,
            hints: s.hints,
        });
    }
    if let Some(a) = &opts.agent {
        agent_status(st, a, "working", format!("{} — {}", name, if ok { "ok" } else { "failed" }));
    } else if opts.emit_activity {
        let _ = st.events.send(ServerEvent::ToolFinished {
            session_id: session_id.to_string(),
            call_id: id.to_string(),
            tool: name.to_string(),
            ok,
        });
    }
    // T-006: off the async threads, awaited before the next call's result.
    save_message(st, session_id, "tool", vec![Part::ToolResult { tool_id: id.to_string(), output, is_error: !ok }]).await
}

/// Save one message off the async threads (`Db::run`) and wait for it: the
/// next step reads the history back, and a tool call's result must follow
/// the call (T-005).
pub(super) async fn save_message(st: &AppState, session_id: &str, role: &str, parts: Vec<Part>) -> Result<()> {
    let (sid, role) = (session_id.to_string(), role.to_string());
    st.db.run(move |db| db.add_message(&sid, &role, &parts).map(drop)).await
}

/// M7: subagent status for the TUI's agents panel.
pub(crate) fn agent_status(st: &Shared, a: &AgentInfo, state: &str, detail: String) {
    let _ = st.events.send(ServerEvent::AgentStatus {
        agent_id: a.id.clone(),
        parent_id: a.parent.clone(),
        name: a.name.clone(),
        task: a.task.clone(),
        state: state.to_string(),
        detail,
    });
}

/// Laziness Meter: current totals for a session.
pub(super) fn snapshot_stats(st: &Shared, session_id: &str) -> RunStats {
    st.stats
        .lock()
        .map(|m| m.get(session_id).cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// Laziness Meter: account a tool call's line impact.
pub(super) fn account_tool(st: &Shared, session_id: &str, name: &str, input: &Value) {
    if let Ok(mut m) = st.stats.lock() {
        let s = m.entry(session_id.to_string()).or_default();
        match name {
            "write" => {
                let content = input.get("content").and_then(Value::as_str).unwrap_or("");
                s.writes += 1;
                s.added += content.lines().count() as u64;
            }
            "edit" => {
                // New (P-01): the edit tool's keys are old_str / new_str
                // (or an `edits` list of them).
                s.edits += 1;
                for (old, new) in edit_pairs(input) {
                    s.deleted += old.lines().count() as u64;
                    s.added += new.lines().count() as u64;
                }
            }
            "bash" => s.bash_runs += 1,
            _ => {}
        }
    }
}

pub(super) fn bump_hints(st: &Shared, session_id: &str) {
    if let Ok(mut m) = st.stats.lock() {
        m.entry(session_id.to_string()).or_default().hints += 1;
    }
}

#[cfg(test)]
mod b03_tests {
    use super::*;

    /// Mock provider. A request whose last message is a tool result gets
    /// "ok"; any other request gets `first` and then `finish_reason: "length"`.
    async fn mock(first: Value) -> String {
        use axum::{routing::post, Router};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::extract::Json<Value>| {
                let first = first.clone();
                async move {
                    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                    let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("").to_string();
                    let payload = if last_role == "tool" {
                        let ok = json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
                        format!("data: {ok}\n\ndata: [DONE]\n\n")
                    } else {
                        let cut = json!({"choices": [{"delta": {}, "finish_reason": "length"}]});
                        format!("data: {first}\n\ndata: {cut}\n\ndata: [DONE]\n\n")
                    };
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/v1")
    }

    async fn state(base: &str, tag: &str) -> (Shared, String, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ocrs-b03-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = base.to_string();
        cfg.provider.model = "mock".into();
        cfg.agent.auto_review = false;
        cfg.permissions.defaults.insert("write".into(), "allow".into());
        let st = build_state_ephemeral(cfg, dir.clone()).await;
        let sid = st.db.create_session(tag, None).unwrap().id;
        (st, sid, dir)
    }

    #[tokio::test]
    async fn cut_off_text_reply_is_marked() {
        let base = mock(json!({"choices": [{"delta": {"content": "half an ans"}, "finish_reason": null}]})).await;
        let (st, sid, dir) = state(&base, "text").await;
        let mut rx = st.events.subscribe();
        run_agent(&st, &sid, "explain", Arc::new(AtomicBool::new(false))).await;
        let msgs = st.db.messages(&sid).unwrap();
        let last = msgs.iter().rev().find(|m| m.role == "assistant").expect("assistant reply");
        let Part::Text { text } = &last.parts[0] else { panic!("wrong part {:?}", last.parts[0]) };
        assert!(text.starts_with("half an ans") && text.contains("reply cut off"), "{text}");
        let mut noted = false;
        while let Ok(ev) = rx.try_recv() {
            if let ServerEvent::Note { text, .. } = ev {
                noted |= text.contains("cut off");
            }
        }
        assert!(noted, "the user is told");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cut_off_tool_call_is_not_run() {
        let call = json!({"index": 0, "id": "call_cut", "function": {
            "name": "write", "arguments": "{\"path\":\"cut.txt\",\"content\":\"hal"}});
        let base = mock(json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": null}]})).await;
        let (st, sid, dir) = state(&base, "tool").await;
        run_agent(&st, &sid, "write it", Arc::new(AtomicBool::new(false))).await;
        assert!(!dir.join("cut.txt").exists(), "a cut-off call must not write anything");
        let msgs = st.db.messages(&sid).unwrap();
        let tool = msgs.iter().find(|m| m.role == "tool").expect("tool result");
        let Part::ToolResult { output, is_error, .. } = &tool.parts[0] else { panic!("wrong part") };
        assert!(*is_error && output.contains("output token limit"), "{output}");
        let last = msgs.iter().rev().find(|m| m.role == "assistant").unwrap();
        assert!(matches!(&last.parts[0], Part::Text { text } if text == "ok"), "the model retried");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn one_shot_refuses_a_cut_off_reply() {
        let base = mock(json!({"choices": [{"delta": {"content": "half"}, "finish_reason": null}]})).await;
        let (st, _sid, dir) = state(&base, "oneshot").await;
        let err = one_shot(&st, "sys", "hi", &AtomicBool::new(false), false).await.unwrap_err();
        assert!(err.to_string().contains("cut off"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
