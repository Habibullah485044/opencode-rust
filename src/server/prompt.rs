//! What is sent to the model: the composed system prompt, context budget
//! and compaction, and the chat history built for each request.

use super::*;

impl AppState {
    /// Base + AGENTS.md + skills index + subagent index + mode instructions.
    pub fn composed_system(&self, session_id: &str) -> String {
        let mode = self.mode_for(session_id);
        // The chat's own project: its AGENTS.md and skills.
        let workspace = self.workspace_for(session_id);
        let skills_list = skills::load_skills(&workspace);
        let agents_md = skills::load_agents_md(&workspace);
        let baked = skills::load_baked(&self.skill_base);
        // Read once per chat: a lesson saved by this chat's fix round would
        // otherwise change the system prompt, and every later request would
        // miss the prompt cache. New lessons apply from the next chat.
        // Kept until the chat is deleted (`forget_session`).
        let lessons = {
            let mut frozen = FROZEN_LESSONS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
            frozen.entry(session_id.to_string()).or_insert_with(|| load_lessons(self, &workspace)).clone()
        };
        let request = self.first_request(session_id).unwrap_or_default();
        let map = crate::repomap::for_chat(session_id, &workspace, &self.state_dir, &request, self.cfg.agent.repo_map_tokens);
        let (main, extra) = self.user_prompt.read().map(|p| p.clone()).unwrap_or_default();
        // The user's main prompt (Settings -> System prompt) is the system
        // prompt, with only the tool guide after it: no base instructions,
        // ponytail rules, skills, web guide, map or lessons (the user's call).
        // Approvals and plan mode are enforced in code, not by the prompt.
        if let Some(main) = main.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            return format!("{main}{}", agents::TOOL_GUIDE);
        }
        let mut system = agents::compose_system(PromptInputs {
            workspace: &workspace,
            mode,
            configured: None,
            agents_md: agents_md.as_deref(),
            skills: &skills_list,
            ponytail: self.ponytail_level(session_id),
            baked: &baked,
            lessons: lessons.as_deref(),
            web: agents::is_web_task(&request),
            map: Some(&map),
        });
        // Last, so the user's word on how to work is what the model reads
        // after everything Habi says.
        if let Some(extra) = extra.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            system.push_str("\n\n## The user's own instructions\n\n");
            system.push_str(extra);
        }
        system
    }

    /// Is the user's own main prompt set (then it and the tool guide are the
    /// whole system prompt)?
    pub fn own_prompt_only(&self) -> bool {
        self.user_prompt.read().is_ok_and(|p| p.0.as_deref().is_some_and(|m| !m.trim().is_empty()))
    }

    /// Settings → System prompt: `text` replaces Habi's base instructions
    /// (`replace`) or is added after them. Empty clears it. Saved to the
    /// config file, and used from the next request on.
    pub fn set_user_prompt(&self, text: &str, replace: bool) -> Result<PathBuf> {
        let text = text.trim();
        let (main, extra) = match (text.is_empty(), replace) {
            (true, _) => (None, None),
            (false, true) => (Some(text.to_string()), None),
            (false, false) => (None, Some(text.to_string())),
        };
        let path = crate::settings::save_user_prompt(main.as_deref(), extra.as_deref())?;
        if let Ok(mut p) = self.user_prompt.write() {
            *p = (main, extra);
        }
        Ok(path)
    }

    /// The chat's first user message (what the chat is about). Fixed for
    /// the chat, so prompt additions based on it keep the cached prefix.
    pub fn first_request(&self, session_id: &str) -> Option<String> {
        let msgs = self.db.messages(session_id).ok()?;
        msgs.iter().find(|m| m.role == "user" && !m.parts.iter().any(|p| matches!(p, Part::Text { text } if text.starts_with("[app] ")))).and_then(|m| {
            m.parts.iter().find_map(|p| match p {
                Part::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
    }

}

/// True when `name` may execute in this run (mode + role enforcement).
pub(crate) fn tool_allowed(mode: AgentMode, opts: &LoopOpts, name: &str) -> bool {
    if !mode.allows_tool(name) {
        return false;
    }
    if (name == "spawn" || crate::subagent::is_agent_tool(name)) && !opts.can_spawn {
        return false;
    }
    match &opts.allow {
        Some(allow) => allow.iter().any(|a| a == name),
        None => true,
    }
}

/// F-07: rough size of the next request in tokens: about 3 characters per
/// token for the messages and the tool list. Code and tool results tokenize
/// worse than prose (closer to 3 chars/token), and measuring low would let
/// requests overflow a small window. Good enough to decide when to compact;
/// not a billing number.
pub(super) fn estimate_tokens(msgs: &[ChatMsg], specs: &[crate::provider::ToolSpec]) -> usize {
    let mut chars = serde_json::to_string(specs).map_or(0, |s| s.len());
    for m in msgs {
        chars += m.role.len() + m.content.as_deref().map_or(0, str::len) + 8;
        for c in m.tool_calls.iter().flatten() {
            chars += c.function.name.len() + c.function.arguments.len() + 16;
        }
    }
    chars / 3
}

/// The model's context window: `agent.context_tokens` when set, else what
/// the provider reported (0 = unknown, nothing is limited).
pub fn context_limit(st: &AppState) -> usize {
    match st.cfg.agent.context_tokens {
        0 => st.detected_context.load(Ordering::Relaxed),
        n => n,
    }
}

/// The context window a `/models` answer reports for `model` (Ollama,
/// LM Studio, vLLM and OpenRouter each name the field differently).
pub fn context_from_models(body: &Value, model: &str) -> Option<usize> {
    let list = body.get("data").and_then(Value::as_array)?;
    let entry = list.iter().find(|m| m.get("id").and_then(Value::as_str) == Some(model)).or_else(|| list.first())?;
    ["context_window", "context_length", "max_context_length", "loaded_context_length", "max_model_len"]
        .iter()
        .find_map(|k| entry.get(*k).and_then(Value::as_u64))
        .or_else(|| entry.pointer("/top_provider/context_length").and_then(Value::as_u64))
        .map(|n| n as usize)
        .filter(|n| *n >= 1024)
}

/// Ollama's native `GET /api/ps`: each loaded model's allocated context
/// under `context_length` (`name`/`model` is its tag). `/v1/models` (the
/// OpenAI endpoint) carries no context metadata, so this is where an
/// Ollama context window is found.
pub fn context_from_ollama_ps(body: &Value, model: &str) -> Option<usize> {
    let list = body.get("models").and_then(Value::as_array)?;
    let norm = |s: &str| s.trim().trim_end_matches(":latest").to_string();
    let want = norm(model);
    let entry = list
        .iter()
        .find(|m| {
            ["name", "model"]
                .iter()
                .any(|k| m.get(*k).and_then(Value::as_str).is_some_and(|n| norm(n) == want))
        })
        .or_else(|| list.first())?;
    entry.get("context_length").and_then(Value::as_u64).map(|n| n as usize).filter(|n| *n >= 1024)
}

/// Best-effort `GET {base minus /v1}/api/ps` (an Ollama server, also the
/// one behind the token proxy in `kaggle_setup_cell.py`). `None` for any
/// other server — the caller then simply knows no context size.
pub(super) async fn probe_ollama_context(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) -> Option<usize> {
    let root = base.trim_end_matches('/').trim_end_matches("/v1");
    let req = crate::provider::with_auth(
        http.get(format!("{root}/api/ps")),
        crate::config::ProviderApi::OpenAi,
        key,
    );
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    context_from_ollama_ps(&body, model)
}

/// Shorten old tool results in a request that would not fit the context
/// window (the stored chat keeps them in full): first everything but the
/// five newest results (last-5 observations in full is the measured sweet
/// spot for weak models), then those too. Returns how many were shortened.
pub(super) fn fit_tool_results(msgs: &mut [ChatMsg], limit: usize) -> usize {
    // What the tool list and the reply need on top of the messages.
    const OVERHEAD: usize = 2_500;
    let size = |m: &[ChatMsg]| estimate_tokens(m, &[]) + OVERHEAD;
    if limit == 0 || size(msgs) * 10 <= limit * 9 {
        return 0;
    }
    let tools: Vec<usize> = msgs.iter().enumerate().filter(|(_, m)| m.role == "tool").map(|(i, _)| i).collect();
    let mut cut = 0;
    for (keep_newest, cap) in [(5usize, 1_500usize), (0, 600)] {
        let older = tools.len().saturating_sub(keep_newest);
        for &i in &tools[..older] {
            let Some(c) = msgs[i].content.as_mut() else { continue };
            if c.chars().count() > cap {
                let head: String = c.chars().take(cap).collect();
                *c = format!("{head}\n… [shortened to fit the model's context window; run the tool again if you need the rest]");
                cut += 1;
            }
        }
        if size(msgs) * 10 <= limit * 9 {
            break;
        }
    }
    cut
}

/// F-07: compact the session when the next request would use 80% or more
/// of `agent.context_tokens`. A failed attempt is remembered per session
/// (by message count), so it is not retried on every step. `msgs` is the
/// request about to be sent (built once per step); true when the chat was
/// compacted and the request must be built again.
pub(super) async fn auto_compact_if_needed(
    st: &Shared,
    session_id: &str,
    msgs: &[ChatMsg],
    specs: &[crate::provider::ToolSpec],
    abort: &AtomicBool,
) -> bool {
    let limit = context_limit(st);
    if limit == 0 {
        return false;
    }
    let used = estimate_tokens(msgs, specs);
    if used.saturating_mul(5) < limit.saturating_mul(4) {
        return false;
    }
    let failed = COMPACT_FAILED_AT.get_or_init(|| Mutex::new(HashMap::new()));
    let tried_here = failed
        .lock()
        .map(|m| m.get(session_id) == Some(&msgs.len()))
        .unwrap_or(false);
    if tried_here {
        return false;
    }
    let (text, compacted) = match crate::compact::compact(st, session_id, abort).await {
        Ok(o) => {
            if let Ok(mut m) = failed.lock() {
                m.remove(session_id);
            }
            (
                format!(
                    "context is ~{used} of {limit} tokens: compacted {} older message(s) into a summary",
                    o.removed
                ),
                true,
            )
        }
        Err(e) => {
            if let Ok(mut m) = failed.lock() {
                m.insert(session_id.to_string(), msgs.len());
            }
            (format!("context is ~{used} of {limit} tokens: automatic compaction skipped ({e})"), false)
        }
    };
    let _ = st.events.send(ServerEvent::Note { session_id: session_id.to_string(), text });
    compacted
}

/// Build OpenAI-format messages: system + persisted history including tool calls/results.
/// Put the leading system message where the server will actually read it.
///
/// [`SYSTEM_DELIVERS`] leaves it where it is. Otherwise — the role is
/// dropped, or we have not managed to ask yet — it moves to the END of the
/// request as an `[app]` user note: a message every server does deliver, in
/// a place a server that trims the head of a long request cannot take it
/// away, and out of `keep_task_near_the_tail`'s way (that one skips `[app]`
/// notes when it looks for the user's own request).
pub(crate) fn deliver_system_prompt(msgs: &mut Vec<ChatMsg>, st: &Shared) {
    if st.system_delivery.load(Ordering::Relaxed) != SYSTEM_DROPS {
        return;
    }
    if !msgs.first().is_some_and(|m| m.role == "system") {
        return;
    }
    let text = msgs.remove(0).content.unwrap_or_default();
    if text.trim().is_empty() {
        return;
    }
    msgs.push(ChatMsg {
        role: "user".into(),
        content: Some(format!(
            "[app] Instructions for this chat, sent here because this server has no system role:\n\n{text}"
        )),
        tool_calls: None,
        tool_call_id: None,
    });
}

/// The request for the next step. T-006: the history (the whole chat, read
/// on every step) comes off the async threads.
pub(super) async fn load_chat_messages(st: &Shared, session_id: &str, system: &str) -> Result<Vec<ChatMsg>> {
    let sid = session_id.to_string();
    let history = st.db.run(move |db| db.messages(&sid)).await?;
    chat_messages_from(st, history, system)
}

/// [`load_chat_messages`] read inline, for tests.
#[cfg(test)]
pub(super) fn build_chat_messages(st: &Shared, session_id: &str, system: &str) -> Result<Vec<ChatMsg>> {
    chat_messages_from(st, st.db.messages(session_id)?, system)
}

/// The request built from a chat's saved `history`.
pub(super) fn chat_messages_from(st: &Shared, history: Vec<Message>, system: &str) -> Result<Vec<ChatMsg>> {
    let mut out = vec![ChatMsg {
        role: "system".into(),
        content: Some(system.to_string()),
        tool_calls: None,
        tool_call_id: None,
    }];
    for msg in history {
        match msg.role.as_str() {
            "user" => {
                let mut text = String::new();
                for p in &msg.parts {
                    if let Part::Text { text: t } = p {
                        text.push_str(t);
                    }
                }
                out.push(ChatMsg {
                    role: "user".into(),
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            "assistant" => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for p in &msg.parts {
                    match p {
                        Part::Text { text: t } => text.push_str(t),
                        // Deliberately NOT resent: providers reject/hallucinate
                        // on reasoning in history. Display-only.
                        Part::Reasoning { .. } => {}
                        Part::ToolCall { id, name, input } => {
                            tool_calls.push(crate::provider::ToolCallReq {
                                id: id.clone(),
                                kind: "function".into(),
                                function: crate::provider::FunctionCall {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input)?,
                                },
                            });
                        }
                        Part::ToolResult { .. } => {}
                    }
                }
                out.push(ChatMsg {
                    role: "assistant".into(),
                    content: if text.is_empty() { None } else { Some(text) },
                    tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
                    tool_call_id: None,
                });
            }
            "system" => {
                for p in &msg.parts {
                    if let Part::Text { text: t } = p {
                        out.push(ChatMsg {
                            role: "system".into(),
                            content: Some(t.clone()),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
            }
            "tool" => {
                for p in &msg.parts {
                    if let Part::ToolResult { tool_id, output, is_error } = p {
                        let content =
                            if *is_error { format!("[error]\n{output}") } else { output.clone() };
                        out.push(ChatMsg {
                            role: "tool".into(),
                            content: Some(content),
                            tool_calls: None,
                            tool_call_id: Some(tool_id.clone()),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    // New (B-04): repair what a crash, an abort or a compaction can leave
    // behind before the provider sees it (see `repair_history`).
    let mut out = repair_history(out);
    retire_skill_reads(&mut out);
    fit_tool_results(&mut out, context_limit(st));
    keep_task_near_the_tail(&mut out);
    // Chat templates (Qwen's among them) refuse a request with no user
    // message, and a compacted chat can keep only tool steps in its tail.
    if !out.iter().any(|m| m.role == "user") {
        let at = usize::from(out.first().is_some_and(|m| m.role == "system"));
        out.insert(at, ChatMsg {
            role: "user".into(),
            content: Some("(The earlier conversation is summarized above. Continue with the task.)".into()),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    // Last: moving the prompt to the tail must not hide that the chat has no
    // user message of its own from the check above.
    deliver_system_prompt(&mut out, st);
    Ok(out)
}

/// Model steps a skill file read stays in the chat in full; after that it is
/// a one-line note.
pub(super) const SKILL_READ_KEPT_FOR: usize = 3;

/// A skill file the model read (`read` of a SKILL.md or of a file in a
/// skills folder) rides along with every later request: a run that read
/// brainstorming, frontend-design and build-anything (~7K tokens) resent
/// them on each of its steps, ~200K tokens over 30 steps. Once the model has
/// had it for a few steps, it becomes a note naming the file; reading it
/// again is one call. A retired read stays retired, so the cached start of
/// the request changes once per skill, not on every step.
pub(super) fn retire_skill_reads(msgs: &mut [ChatMsg]) {
    let mut reads: HashMap<String, String> = HashMap::new();
    for m in msgs.iter() {
        for c in m.tool_calls.iter().flatten().filter(|c| c.function.name == "read") {
            let args: Value = serde_json::from_str(&c.function.arguments).unwrap_or_default();
            let path = args["path"].as_str().unwrap_or("").to_string();
            let p = path.replace('\\', "/").to_ascii_lowercase();
            if p.ends_with("/skill.md") || p == "skill.md" || (p.contains("/skills/") && p.ends_with(".md")) {
                reads.insert(c.id.clone(), path);
            }
        }
    }
    if reads.is_empty() {
        return;
    }
    let assistant_after: Vec<usize> = {
        let mut after = vec![0; msgs.len()];
        let mut n = 0;
        for i in (0..msgs.len()).rev() {
            after[i] = n;
            if msgs[i].role == "assistant" {
                n += 1;
            }
        }
        after
    };
    for (i, m) in msgs.iter_mut().enumerate() {
        let Some(path) = m.tool_call_id.as_ref().and_then(|id| reads.get(id)) else { continue };
        let failed = m.content.as_deref().is_some_and(|c| c.starts_with("[error]"));
        if m.role == "tool" && !failed && assistant_after[i] >= SKILL_READ_KEPT_FOR {
            m.content = Some(format!(
                "[app] The skill file {path} was read here and followed in the steps after it. Its text \
                 is left out now to save tokens: read it again if you need its details."
            ));
        }
    }
}

/// Keep the user's request near the END of a long chat as well as at its
/// front.
///
/// A server that drops the head of a long request (this one does: the same
/// request with the task at the front dies, with the task at the end it
/// answers) leaves a chat of nothing but assistant/tool steps, and chat
/// templates then raise "No user query found in messages" — the template
/// runs while rendering, before any token trimming, so only the server
/// dropping messages can cause it. Seen live, twice, in the middle of a
/// review run. The reminder is the app's own note, not a new instruction.
pub(super) fn keep_task_near_the_tail(msgs: &mut Vec<ChatMsg>) {
    /// Steps after the request that are still safe to send as they are.
    /// Twelve messages is a conservative bound, not a measured cliff: the
    /// same request answers with the task at the end and fails with it at
    /// the front, and a chat of 122 tiny messages answered, so the server's
    /// drop has no published threshold to aim at.
    const TAIL_STEPS: usize = 12;
    if let Some(i) = msgs.iter().rposition(|m| m.role == "user") {
        if msgs.len() - 1 - i <= TAIL_STEPS {
            return;
        }
    }
    // The task itself: the newest user message that is not one of the app's
    // own notes (they all start with "[app]").
    let Some(task) = msgs
        .iter()
        .rev()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .find(|t| !t.trim_start().starts_with("[app]"))
    else {
        return;
    };
    let task: String = task.trim().chars().take(240).collect();
    msgs.push(ChatMsg {
        role: "user".into(),
        content: Some(format!(
            "[app] Reminder of the task you are working on: \"{task}\". Continue it; do not start over."
        )),
        tool_calls: None,
        tool_call_id: None,
    });
}

/// Does a provider error say the request was too long for the model? Also
/// the Qwen template's "No user query found", which is what a server that
/// silently drops the oldest messages to fit produces.
pub fn is_context_overflow(err: &str) -> bool {
    let e = err.to_lowercase();
    [
        "no user query found",
        "context length",
        "context window",
        "context_length_exceeded",
        "maximum context",
        "exceeds the context",
        "prompt too long",
        "too many tokens",
        "prompt is too long",
        "input is too long",
        "reduce the length",
    ]
    .iter()
    .any(|k| e.contains(k))
}

/// New (B-04): the result sent for a tool call that never finished.
pub(super) const INTERRUPTED_TOOL_RESULT: &str =
    "[interrupted] this tool call did not finish (the run was stopped); its result is unknown";

/// New (B-04): make a replayed history safe to send. Providers reject (or
/// get confused by) shapes that a crash, an abort or a compaction can leave
/// in the database:
/// - every `tool_calls` id of an assistant message must be answered by a
///   `tool` message right after it: a missing answer gets
///   `INTERRUPTED_TOOL_RESULT`, an answer to an unknown id is dropped;
/// - an assistant message with neither text nor tool calls is dropped;
/// - only the first message may be `system`: system notes at the start (a
///   compaction summary) are joined to it, later ones are sent as user text;
/// - back-to-back user messages become one.
pub(super) fn repair_history(msgs: Vec<ChatMsg>) -> Vec<ChatMsg> {
    let mut out: Vec<ChatMsg> = Vec::with_capacity(msgs.len());
    // Tool call ids of the last assistant message that still need a result.
    let mut open: Vec<String> = Vec::new();
    for m in msgs {
        if m.role != "tool" {
            close_open_calls(&mut out, &mut open);
        }
        match m.role.as_str() {
            "tool" => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                if let Some(i) = open.iter().position(|c| *c == id) {
                    open.remove(i);
                    out.push(m);
                }
                // Otherwise it answers nothing we sent: drop it.
            }
            "assistant" => {
                let has_text = m.content.as_deref().is_some_and(|t| !t.trim().is_empty());
                let ids: Vec<String> = m
                    .tool_calls
                    .as_ref()
                    .map(|calls| calls.iter().map(|c| c.id.clone()).collect())
                    .unwrap_or_default();
                if !has_text && ids.is_empty() {
                    continue;
                }
                open = ids;
                out.push(m);
            }
            "system" => {
                let text = m.content.unwrap_or_default();
                if out.iter().all(|x| x.role == "system") {
                    match out.first_mut() {
                        Some(first) => {
                            let joined =
                                format!("{}\n\n{text}", first.content.as_deref().unwrap_or(""));
                            first.content = Some(joined);
                        }
                        None => out.push(ChatMsg {
                            role: "system".into(),
                            content: Some(text),
                            tool_calls: None,
                            tool_call_id: None,
                        }),
                    }
                } else {
                    push_user_text(&mut out, format!("[system note]\n{text}"));
                }
            }
            "user" => push_user_text(&mut out, m.content.unwrap_or_default()),
            _ => out.push(m),
        }
    }
    close_open_calls(&mut out, &mut open);
    out
}

/// New (B-04): answer every still-open tool call with
/// `INTERRUPTED_TOOL_RESULT`.
pub(super) fn close_open_calls(out: &mut Vec<ChatMsg>, open: &mut Vec<String>) {
    for id in open.drain(..) {
        out.push(ChatMsg {
            role: "tool".into(),
            content: Some(INTERRUPTED_TOOL_RESULT.to_string()),
            tool_calls: None,
            tool_call_id: Some(id),
        });
    }
}

/// New (B-04): add user text; merge it into the previous message when that
/// one is also from the user.
pub(super) fn push_user_text(out: &mut Vec<ChatMsg>, text: String) {
    if let Some(last) = out.last_mut() {
        if last.role == "user" {
            let joined = format!("{}\n\n{text}", last.content.as_deref().unwrap_or(""));
            last.content = Some(joined);
            return;
        }
    }
    out.push(ChatMsg { role: "user".into(), content: Some(text), tool_calls: None, tool_call_id: None });
}

#[cfg(test)]
mod repo_map_tests {
    use super::*;

    /// The chat's system prompt carries the project map, ranked by the
    /// chat's first message, and keeps the same map for the whole chat
    /// (a new file does not change it, so the cached prefix stays valid).
    #[tokio::test]
    async fn the_map_is_in_the_prompt_and_stays_for_the_chat() {
        let dir = std::env::temp_dir().join(format!("ocrs-mapchat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/cart.js"), "export function addItem(cart, item) {}\n").unwrap();
        std::fs::write(dir.join("src/util.js"), "export function pad(s) {}\n").unwrap();
        let st = build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
        let s = st.db.create_session("m", None).unwrap();
        st.db.add_message(&s.id, "user", &[Part::Text { text: "addItem adds the same item twice".into() }]).unwrap();
        let first = st.composed_system(&s.id);
        let map = &first[first.find("## Project map").expect("map in the prompt")..];
        let cart = map.find("src/cart.js: addItem()").expect("cart listed");
        assert!(cart < map.find("src/util.js").unwrap(), "the named file first: {map}");
        std::fs::write(dir.join("src/new.js"), "export function later() {}\n").unwrap();
        assert_eq!(st.composed_system(&s.id), first, "same prompt for the rest of the chat");
        // Off: no map.
        let mut cfg = crate::config::Config::default();
        cfg.agent.repo_map_tokens = 0;
        let off = build_state_ephemeral(cfg, dir.clone()).await;
        let s2 = off.db.create_session("m", None).unwrap();
        assert!(!off.composed_system(&s2.id).contains("## Project map"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod prompt_budget_tests {
    /// Every request starts with the system prompt and the tool list, so
    /// their size is paid on every step of every run. This keeps it from
    /// growing back unnoticed (measured: ~2.8K tokens before the cut, ~2K
    /// after). Raise the limit only for something worth its cost.
    #[tokio::test]
    async fn the_fixed_part_of_a_request_stays_small() {
        let dir = std::env::temp_dir().join(format!("ocrs-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = super::build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
        let s = st.db.create_session("m", None).unwrap();
        let (system, tools, count) = st.request_overhead(&s.id);
        // Skills installed in the user's own folder (this temp workspace has
        // none) are the user's choice, not the fixed part: leave them out.
        let composed = st.composed_system(&s.id);
        let skills = composed.find("\n\n## Skills").map_or(0, |at| {
            let rest = &composed[at + 2..];
            rest.find("\n\n## ").unwrap_or(rest.len()) + 2
        });
        let system = system - skills / 4;
        // 900 → 1000 for the all-tools calling guide (the tool names and
        // arguments a weak model needs spelled out; the user asked for it
        // after the model kept calling tools wrong).
        // 1000 → 1100 for spelling one call out: this model wrote the
        // arguments with no tool name, which nothing can run, and the guide
        // then fixed it live.
        assert!(system <= 1100, "system prompt ~{system} tokens");
        // 1150 → 1200 for `check_page` (~70 tokens; `todo` was trimmed to pay
        // for most of it): checking the rendered page is the cheapest fix.
        // 1200 → 1250 for `ask_user` and multi-edit (read/glob/grep were
        // shortened to pay for half): a question with buttons saves a wrong
        // guess, one edit call with several changes saves round trips.
        // 1250 → 1275 for check_page's `look` (screenshots the model sees)
        // and its running-app URL: without them it judged pages it never saw.
        assert!(tools <= 1275, "tool list ~{tools} tokens ({count} tools)");
        assert!(st.usage_report(&s.id).contains("Each request of this chat starts with about"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
