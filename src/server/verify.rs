//! Checks after a run: the reuse gate, `/review`, the verify spec, final
//! and design checks, lessons, gates and the verify command.

use super::*;

/// Laziness Meter reuse gate: right AFTER a successful `write`, look for what
/// it duplicates. Returns a hint appended to the tool result so the model
/// reads it mid-run (and can remove the duplicate).
pub(super) fn reuse_gate(workspace: &std::path::Path, name: &str, input: &Value) -> Option<String> {
    if name != "write" {
        return None;
    }
    let path = input.get("path").and_then(Value::as_str).unwrap_or("");
    let content = input.get("content").and_then(Value::as_str).unwrap_or("");
    let mut hints: Vec<String> = Vec::new();
    let lines = content.lines().count();
    if lines > 120 {
        hints.push(format!(
            "ponytail: this write is {lines} lines — confirm nothing smaller, or \
             already in the repo, solves it"
        ));
    }
    // New (P-04): one walk for all names, and only real definitions count.
    for (sym, found) in find_symbols(workspace, path, &top_level_symbols(content)) {
        hints.push(format!(
            "ponytail reuse hint: `{sym}` already exists in {found} — use the \
             existing code instead of rewriting it"
        ));
    }
    if hints.is_empty() {
        None
    } else {
        Some(hints.join("\n"))
    }
}

/// Top-level function/class/struct names defined by a code snippet.
pub(super) fn top_level_symbols(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        // New (P-04): indented lines are methods or nested items.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let mut t = line.trim();
        loop {
            let stripped = ["pub ", "export ", "async ", "public ", "static "]
                .iter()
                .find_map(|p| t.strip_prefix(p));
            match stripped {
                Some(s) => t = s.trim_start(),
                None => break,
            }
        }
        const KW: [&str; 7] = [
            "fn ", "def ", "class ", "struct ", "function ", "interface ", "trait ",
        ];
        if let Some(k) = KW.iter().find(|k| t.starts_with(**k)) {
            let rest = &t[k.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            // New (P-04): short and very common names exist everywhere.
            const COMMON: [&str; 12] = [
                "main", "new", "run", "test", "init", "default", "from", "build", "start", "stop",
                "handle", "setup",
            ];
            if name.chars().count() >= 4 && !COMMON.contains(&name.as_str()) && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// New (P-04): ONE walk over the workspace's source files (skipping the file
/// just written) that finds where any of `syms` is DEFINED (`fn name`,
/// `def name`, `class name`, …). Returns (name, file) pairs.
pub(super) fn find_symbols(
    workspace: &std::path::Path,
    skip_path: &str,
    syms: &[String],
) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    if syms.is_empty() {
        return found;
    }
    let skip = workspace.join(skip_path);
    let mut scanned = 0usize;
    for entry in ignore::WalkBuilder::new(workspace)
        .hidden(true)
        .build()
        .flatten()
    {
        if found.len() == syms.len() {
            break; // every name was found
        }
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let p = entry.path();
        if p == skip.as_path() {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ![
            "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "rb", "c", "cpp", "h",
            "cs", "php",
        ]
        .contains(&ext)
        {
            continue;
        }
        scanned += 1;
        if scanned > 3000 {
            break;
        }
        if let Ok(text) = std::fs::read_to_string(p) {
            if text.len() > 512_000 {
                continue;
            }
            for sym in syms {
                if !found.iter().any(|(s, _)| s == sym) && defines(&text, sym) {
                    let rel = p.strip_prefix(workspace).unwrap_or(p).display().to_string();
                    found.push((sym.clone(), rel));
                }
            }
        }
    }
    found
}

/// New (P-04): true when a line of `text` defines `sym` (`fn sym`, `def sym`, …).
pub(super) fn defines(text: &str, sym: &str) -> bool {
    const KW: [&str; 7] = ["fn ", "def ", "class ", "struct ", "function ", "interface ", "trait "];
    text.lines().any(|line| {
        KW.iter().any(|k| {
            line.find(k).is_some_and(|i| {
                let rest = &line[i + k.len()..];
                rest.starts_with(sym)
                    && !rest[sym.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
            })
        })
    })
}

/// Ponytail review pass — read-only senior-dev review of the workspace's
/// uncommitted changes, with the vendored ponytail-review discipline (which
/// explicitly hunts for callers broken OUTSIDE the diff). Streams into the
/// transcript like a normal run.
/// Most model calls one automatic review may make.
pub(super) const REVIEW_MAX_STEPS: usize = 10;

/// Most replies one run asks again after they broke off mid-stream.
pub(super) const MAX_STREAM_DROPS: u32 = 3;

/// The uncommitted changes of `ws` as text for the review (at most about
/// 12K characters). `None` = not a git repository, or nothing changed.
pub(super) fn review_diff(ws: &std::path::Path) -> Option<String> {
    let files = crate::changes::collect(ws).ok()?;
    if files.is_empty() {
        return None;
    }
    let mut out = String::new();
    for f in &files {
        let part = format!("### {} ({})\n```diff\n{}\n```\n", f.path, f.status, f.diff.trim_end());
        if out.len() + part.len() > 12_000 {
            out.push_str(&format!("… and {} more changed file(s); read them if needed.\n", files.len() - out.matches("### ").count()));
            break;
        }
        out.push_str(&part);
    }
    Some(out)
}

/// What the review should look at: the workspace's uncommitted changes
/// (git), else the files the run itself wrote (the tools track every
/// write/edit — a project without git can still be reviewed). `since`
/// bounds the tracked writes (the run's start); `None` = anything this
/// app run recorded. `None` back = nothing known to review.
pub(super) fn review_targets(ws: &std::path::Path, since: Option<std::time::SystemTime>) -> Option<String> {
    if let Some(diff) = review_diff(ws) {
        return Some(diff);
    }
    let files = crate::keepbest::written_since(ws, since.unwrap_or(std::time::UNIX_EPOCH));
    if files.is_empty() {
        return None;
    }
    let mut out = String::new();
    for f in &files {
        let rel = f.strip_prefix(ws).unwrap_or(f);
        let content = std::fs::read_to_string(f).unwrap_or_else(|_| "(unreadable)".to_string());
        let part = format!("### {} (changed by the run)\n```\n{}\n```\n", rel.display(), content.trim_end());
        if out.len() + part.len() > 12_000 {
            out.push_str(&format!("… and {} more changed file(s); read them if needed.\n", files.len() - out.matches("### ").count()));
            break;
        }
        out.push_str(&part);
    }
    Some(out)
}

pub async fn run_review(
    st: &Shared,
    session_id: &str,
    since: Option<std::time::SystemTime>,
    abort: &Arc<AtomicBool>,
) -> Result<String> {
    // What to review goes into the request: the review runs read-only (no
    // git, no shell), and a model told to "find the changes" without them
    // spent its steps saying tools were blocked.
    let ws = st.workspace_for(session_id);
    // git diff and file reads: off the async threads.
    let changes = {
        let ws = ws.clone();
        tokio::task::spawn_blocking(move || review_targets(&ws, since)).await.unwrap_or(None)
    };
    let prompt = match &changes {
        Some(c) => format!(
            "Review the current uncommitted changes in this workspace \
             (ponytail-review). Report findings ordered by severity, and \
             check callers outside the diff. The changes:\n\n{c}"
        ),
        None => "Review the current uncommitted changes in this workspace \
                 (ponytail-review). Report findings ordered by severity, and \
                 check callers outside the diff. No changed files were tracked \
                 here (not a git repository, and what a shell command writes is \
                 invisible to this app) — so use list, read, glob and grep to find \
                 what looks new or changed in the workspace and review that. If \
                 the workspace really is empty, say so in one line."
            .to_string(),
    };
    st.db.add_message(session_id, "user", &[Part::Text { text: prompt }])?;
    let system = format!(
        "{}\n\nWorkspace: {}. The changes are in the user's message. You have NO bash and NO git — \
         calling them gets you 'tool blocked' and wastes your steps. Your tools are exactly: \
         list, read, glob, grep. Call them; never describe a call. Report in full.",
        crate::agents::ponytail_review_rules(),
        ws.display()
    );
    // Read-only work: the small model when one is set, and a short leash
    // (the review resends the chat's history with every step).
    let opts = LoopOpts {
        model: st.cfg.provider.small_model.clone(),
        max_steps: Some(REVIEW_MAX_STEPS.min(st.cfg.agent.max_steps)),
        system,
        emit_activity: true,
        allow: Some(vec!["list".to_string(), "read".to_string(), "glob".to_string(), "grep".to_string()]),
        can_spawn: false,
        workspace: st.run_workspace(session_id),
        agent: None,
    };
    tagged("review", session_id, run_loop(st, session_id, &opts, abort)).await
}

/// F-08: marks the user message that hands a failed check back to the agent.
pub const VERIFY_PREFIX: &str = "[verify]";

/// F-08 verification loop, now with page checks and keep-best rounds. After
/// a top-level run that changed files:
/// - run the project's check command (`agent.verify_command`, or one found
///   in the project when `agent.auto_verify` is on), and
/// - open every web page the run changed in a headless browser at phone and
///   desktop width (`agent.check_pages`).
///
/// Problems go back to the agent as one `[verify]` message, at most
/// `agent.verify_max_rounds` times. A round is kept only when it leaves
/// fewer problems; otherwise the files it wrote are put back (the model
/// cannot judge its own work without a signal, and later rounds often make
/// things worse). Each step shows up as a `Note`.
/// The work spec for a request (see [`crate::spec`]), shown to the user as
/// a note. `None` when it is off, the message is chatter or a question, or
/// the model's reply is not a spec: the run goes on without one.
pub(crate) async fn make_spec(st: &Shared, session_id: &str, request: &str, abort: &AtomicBool) -> Option<crate::spec::Spec> {
    // The user's own main prompt: no planning call of Habi's (and so no final
    // check, which proves the spec). They asked for their prompt alone.
    if !st.cfg.agent.sharpen || st.own_prompt_only() || !crate::spec::needs_spec(request) {
        return None;
    }
    // A turbo continuation is the app talking, not a new request: its spec
    // would be about its own boilerplate (found by the wiring audit).
    if request.starts_with(TURBO_CONTINUE_PREFIX) {
        return None;
    }
    let ws = st.workspace_for(session_id);
    let mut files: Vec<String> = std::fs::read_dir(&ws)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect())
        .unwrap_or_default();
    files.sort();
    files.truncate(30);
    let context = last_assistant_text(st, session_id);
    // The spec step also picks the skills that fit the task: it sees every
    // skill's description, which the prompt no longer lists when there are many.
    let skills: Vec<skills::Doc> = skills::load_skills(&ws)
        .into_iter()
        .filter(|s| !skills::NOT_AUTO_PICKED.contains(&s.name.as_str()))
        .collect();
    let user = crate::spec::prompt(request, context.as_deref(), &files, &skills::skill_menu(&skills));
    let reply = match tagged("spec", session_id, one_shot(st, crate::spec::SYSTEM, &user, abort, false)).await {
        Ok(r) => r,
        Err(e) => {
            core_log(&format!("[spec] {e:#}"));
            return None;
        }
    };
    let mut spec = crate::spec::parse(&reply)?;
    // Only skills that exist (a model can make one up).
    spec.skills.retain(|n| skills.iter().any(|s| s.name.eq_ignore_ascii_case(n)));
    let _ = st.events.send(ServerEvent::Note { session_id: session_id.to_string(), text: crate::spec::for_user(&spec) });
    Some(spec)
}

/// The final check: the model proves each done-when item of the spec with
/// tool calls, in this chat, and fixes what fails (one more round at most).
/// A "PASS" given without a tool call proves nothing and counts as a fail.
/// `Some(true)` when every item passed, `None` when the check could not run
/// (provider down, aborted): that says nothing about the work.
pub(crate) async fn final_check(
    st: &Shared,
    session_id: &str,
    spec: &crate::spec::Spec,
    abort: &AtomicBool,
    budget: Option<&PassBudget>,
) -> Option<bool> {
    let note = |text: String| {
        let _ = st.events.send(ServerEvent::Note { session_id: session_id.to_string(), text });
    };
    let tool_results = || {
        st.db.messages(session_id).map(|m| m.iter().filter(|m| m.role == "tool").count()).unwrap_or(0)
    };
    note(format!("final check — proving {} item(s) with tools…", spec.done_when.len()));
    let mut msg = crate::spec::check_request(spec);
    let mut fixed = false;
    let mut asked_for_lines = false;
    // Tool calls of the round being judged. Carried over when only the
    // PASS/FAIL lines are asked for (the looking was done); reset for a fix.
    let mut looked = false;
    loop {
        if abort.load(Ordering::SeqCst) || !spend(budget, st, session_id, "the final check") {
            return None;
        }
        let before = tool_results();
        if let Err(e) = tagged("final check", session_id, agent_inner(st, session_id, &msg, abort)).await {
            note(format!("final check could not run: {e}"));
            return None;
        }
        looked |= tool_results() > before;
        let (pass, fails) = crate::spec::verdicts(&last_assistant_text(st, session_id).unwrap_or_default());
        let problem = if !looked {
            "you answered without calling a tool, so nothing was proven".to_string()
        } else if fails.is_empty() && pass > 0 {
            note(format!("final check passed — {pass} item(s) proven{}", if fixed { " after a fix round" } else { "" }));
            return Some(true);
        } else if fails.is_empty() && !asked_for_lines {
            // Seen live: a mission step checked everything, then wrote a
            // report without the lines, and the whole step was redone. Asking
            // for the lines costs one short reply.
            asked_for_lines = true;
            note("final check: the reply had no PASS/FAIL lines — asking for them".into());
            msg = crate::spec::lines_request(spec);
            continue;
        } else if fails.is_empty() {
            note("final check: the reply had no PASS/FAIL lines".into());
            return Some(false);
        } else {
            fails.join("; ")
        };
        if fixed {
            break;
        }
        fixed = true;
        looked = false;
        note(format!("final check: not proven yet ({}) — one fix round", problem.chars().take(160).collect::<String>()));
        msg = crate::spec::retry_request(&problem);
    }
    note("final check: some items are still not proven — see the last reply".into());
    Some(false)
}

/// The design review: Habi takes desktop and phone screenshots of the web
/// pages this run changed and asks the model, as the client's art director,
/// to judge them against the brief and the AI look, then fix and look again.
/// Skipped when off, when no page changed, or when the model takes no images
/// (it would be judging pictures it cannot see).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn design_review(
    st: &Shared,
    session_id: &str,
    request: &str,
    spec: Option<&crate::spec::Spec>,
    since: std::time::SystemTime,
    bash_used: bool,
    abort: &AtomicBool,
    budget: Option<&PassBudget>,
) -> Option<()> {
    if !st.cfg.agent.design_review || st.own_prompt_only() {
        return None;
    }
    let ws = st.workspace_for(session_id);
    let pages = changed_web_pages(&ws, since, bash_used, true);
    if pages.is_empty() {
        return None;
    }
    let note = |text: String| {
        let _ = st.events.send(ServerEvent::Note { session_id: session_id.to_string(), text });
    };
    let model = st.current_model().await;
    // Pictures go out in the OpenAI request format; the Anthropic path does
    // not carry them yet, so a review there would judge what it cannot see.
    if st.cfg.provider.api != crate::config::ProviderApi::OpenAi
        || !crate::attachments::model_sees_images(&st.cfg.provider.images, &model)
    {
        note(format!(
            "design review skipped: {model} gets no images here (set provider.images = \"on\" if it takes them)"
        ));
        return None;
    }
    note(format!("design review — screenshots of {} at desktop and phone width…", pages.join(", ")));
    let dir = scratch_dir(&ws).join("shots");
    let mut shots = Vec::new();
    for page in pages.iter().take(2) {
        match crate::pagecheck::page_shots(&ws, page, &dir).await {
            Ok(s) => shots.extend(s),
            Err(e) => note(format!("design review: no screenshots of {page}: {e:#}")),
        }
    }
    if shots.is_empty() || abort.load(Ordering::SeqCst) || !spend(budget, st, session_id, "the design review") {
        return None;
    }
    let goal = spec.map_or(request, |s| s.goal.as_str());
    let msg = crate::spec::design_review_request(goal, spec.and_then(|s| s.design.as_deref()), &shots);
    if let Err(e) = tagged("design review", session_id, agent_inner(st, session_id, &msg, abort)).await {
        note(format!("design review could not run: {e}"));
        return None;
    }
    Some(())
}

pub(super) async fn check_after_run(
    st: &Shared,
    session_id: &str,
    request: &str,
    since: std::time::SystemTime,
    bash_used: bool,
    abort: &Arc<AtomicBool>,
    budget: Option<&PassBudget>,
) -> Option<(bool, String)> {
    let note = |text: String| {
        let _ = st.events.send(ServerEvent::Note { session_id: session_id.to_string(), text });
    };
    let ws = st.workspace_for(session_id);
    let cmd = match verify_command_for(st, &ws, &note) {
        Some(c) if verify_permitted(st, session_id, &c, &note, abort).await => Some(c),
        _ => None,
    };
    let pages = |since| changed_web_pages(&ws, since, bash_used, st.cfg.agent.check_pages);
    let mut gate = Gate { cmd, pages: pages(since), expect: crate::pagecheck::expected_texts(request) };
    if gate.cmd.is_none() && gate.pages.is_empty() {
        return None;
    }
    let timeout = std::time::Duration::from_secs(st.cfg.agent.verify_timeout_secs.max(1));
    let mut best = run_gate(st, &ws, &mut gate, timeout, &note, abort).await?;
    if best.problems == 0 {
        note(format!("{VERIFY_PREFIX} {} passed", gate.label()));
        return Some((true, best.summary));
    }
    let max_rounds = st.cfg.agent.verify_max_rounds;
    let first_report = best.report.clone();
    let mut undone = false;
    for round in 1..=max_rounds {
        if abort.load(Ordering::SeqCst) {
            return None;
        }
        if !spend(budget, st, session_id, "the next fix round") {
            return Some((false, best.summary));
        }
        note(format!("{VERIFY_PREFIX} {} failed — fix round {round} of {max_rounds}", best.summary));
        let mut msg = format!("{VERIFY_PREFIX} {}\n", best.report);
        if undone {
            msg.push_str("Your last fix attempt did not reduce the problems, so its file changes were undone. Try a different fix.\n");
        }
        msg.push_str("Fix these problems, then stop.");
        let journal = crate::keepbest::Journal::begin(&ws);
        if let Err(e) = tagged("fix round", session_id, agent_inner(st, session_id, &msg, abort)).await {
            note(format!("{VERIFY_PREFIX} fix round failed: {e}"));
            return Some((false, best.summary));
        }
        if abort.load(Ordering::SeqCst) {
            return None;
        }
        gate.pages = pages(since);
        let now = run_gate(st, &ws, &mut gate, timeout, &note, abort).await?;
        if now.problems == 0 {
            note(format!("{VERIFY_PREFIX} {} passed after {round} fix round(s)", gate.label()));
            // A failure the checks proved fixed is worth remembering here.
            let fix = last_assistant_text(st, session_id).unwrap_or_default();
            let (path, report) = (lessons_path(st, &ws), first_report.clone());
            let _ = tokio::task::spawn_blocking(move || record_lesson_at(&path, &report, &fix)).await;
            return Some((true, format!("{} (after {round} fix round(s))", now.summary)));
        }
        if now.problems < best.problems {
            note(format!("{VERIFY_PREFIX} round {round} helped: {} → {} problem(s)", best.problems, now.problems));
            best = now;
            undone = false;
        } else {
            let n = journal.restore();
            note(format!(
                "{VERIFY_PREFIX} round {round} did not help ({} → {} problem(s)); put back {n} file(s) it changed",
                best.problems, now.problems
            ));
            undone = true;
        }
    }
    note(format!(
        "{VERIFY_PREFIX} {} still failing after {max_rounds} fix round(s) — over to you",
        best.summary
    ));
    Some((false, best.summary))
}

/// The check command for `ws`: the configured one, else (with
/// `agent.auto_verify`) the project's own. Deny rules still apply. Whether
/// it may run without asking is [`verify_permitted`]'s call.
pub(super) fn verify_command_for(st: &AppState, ws: &std::path::Path, note: &impl Fn(String)) -> Option<String> {
    let configured = st.cfg.agent.verify_command.as_deref().map(str::trim).filter(|c| !c.is_empty());
    let cmd = match configured {
        Some(c) => Some(c.to_string()),
        None if st.cfg.agent.auto_verify => detect_verify_command(ws),
        None => None,
    }?;
    if st.engine.evaluate("bash", &cmd, |_, _| false).action == Action::Deny {
        note(format!("{VERIFY_PREFIX} skipped: a deny rule blocks `{cmd}`"));
        return None;
    }
    Some(cmd)
}

/// May the check command `cmd` run in this chat? The command the user set
/// in their own config (`agent.verify_command`; there is no project-level
/// config) runs without asking: they wrote it. A command found in the
/// project (`cargo check`, `npm test`, `pytest`…) runs the project's code —
/// in an untrusted repository that is code execution — so it goes through
/// the same check as any `bash` call: allow rules, "always allow", turbo and
/// a mission's approvals pass, otherwise the user is asked.
pub(super) async fn verify_permitted(st: &Shared, session_id: &str, cmd: &str, note: &impl Fn(String), abort: &AtomicBool) -> bool {
    let configured = st.cfg.agent.verify_command.as_deref().map(str::trim).is_some_and(|c| c == cmd);
    if configured || check_permission(st, session_id, "bash", cmd, abort).await {
        return true;
    }
    note(format!("{VERIFY_PREFIX} skipped `{cmd}`: running it was not approved"));
    false
}

/// Most lessons kept per project (each is sent with every request).
pub(super) const MAX_LESSONS: usize = 8;

/// Where a project's lessons live: in the app's state folder, not in the
/// project, one file per project folder.
pub(super) fn lessons_path(st: &AppState, ws: &std::path::Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf()).hash(&mut h);
    st.state_dir.join("lessons").join(format!("{:016x}.md", h.finish()))
}

/// The project's lessons for the prompt, or `None`.
pub fn load_lessons(st: &AppState, ws: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(lessons_path(st, ws)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Remember a problem the checks found and a later round fixed (the idea of
/// harness2's evidence-backed lessons, with the checks as the evidence).
/// One line each; newest last; a repeat of the same problem replaces the
/// older line.
#[cfg(test)]
pub(super) fn record_lesson(st: &AppState, ws: &std::path::Path, report: &str, fix: &str) {
    record_lesson_at(&lessons_path(st, ws), report, fix)
}

/// [`record_lesson`] into the lessons file `path` (file work only, so it can
/// run off the async threads).
pub(super) fn record_lesson_at(path: &std::path::Path, report: &str, fix: &str) {
    let problem = report
        .lines()
        .map(str::trim)
        .find(|l| l.chars().next().is_some_and(|c| c.is_ascii_digit()) && l.contains(". "))
        .map(|l| l.split_once(". ").map_or(l, |(_, rest)| rest))
        .or_else(|| report.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("")
        .to_string();
    if problem.is_empty() {
        return;
    }
    let short = |s: &str, n: usize| {
        let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.chars().count() > n {
            one.chars().take(n).collect::<String>() + "…"
        } else {
            one
        }
    };
    let key = short(&problem, 90);
    let fix = short(fix, 200);
    let line = if fix.is_empty() {
        format!("- {} (fixed)", short(&problem, 180))
    } else {
        format!("- {} → fixed by: {fix}", short(&problem, 180))
    };
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("- ") && !l.contains(&key))
        .map(String::from)
        .collect();
    lines.push(line);
    let skip = lines.len().saturating_sub(MAX_LESSONS);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, lines[skip..].join("\n") + "\n");
}

/// The last thing the assistant said in a session (its fix summary).
pub(super) fn last_assistant_text(st: &AppState, session_id: &str) -> Option<String> {
    let msgs = st.db.messages(session_id).ok()?;
    msgs.iter().rev().filter(|m| m.role == "assistant").find_map(|m| {
        m.parts.iter().rev().find_map(|p| match p {
            Part::Text { text } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
    })
}

/// What the post-run check runs.
pub(super) struct Gate {
    pub(super) cmd: Option<String>,
    /// Web pages, relative to the workspace.
    pub(super) pages: Vec<String>,
    /// Texts the pages must show (quoted names from the request).
    pub(super) expect: Vec<String>,
}

impl Gate {
    pub(super) fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.cmd {
            parts.push(format!("`{c}`"));
        }
        if !self.pages.is_empty() {
            parts.push(format!("page check of {}", self.pages.join(", ")));
        }
        parts.join(" and ")
    }
}

/// One pass of the checks.
pub(super) struct GateResult {
    /// Lower is better; 0 = everything passed.
    pub(super) problems: usize,
    /// What the agent reads.
    pub(super) report: String,
    /// One line for the notes.
    pub(super) summary: String,
}

/// Run the gate once. `None` = nothing could run. A check that cannot run
/// here (no browser, a missing test runner) is dropped from `gate` with a
/// note instead of being handed to the agent as its fault.
pub(super) async fn run_gate(
    st: &Shared,
    ws: &std::path::Path,
    gate: &mut Gate,
    timeout: std::time::Duration,
    note: &impl Fn(String),
    abort: &AtomicBool,
) -> Option<GateResult> {
    let mut problems = 0;
    let mut report = String::new();
    let mut summary = Vec::new();
    let mut ran = false;
    if let Some(cmd) = gate.cmd.clone() {
        note(format!("{VERIFY_PREFIX} running `{cmd}`…"));
        match run_verify_command(&cmd, ws, timeout, abort).await {
            Err(e) => {
                note(format!("{VERIFY_PREFIX} could not run `{cmd}`: {e:#}"));
                gate.cmd = None;
            }
            Ok((code, out)) if code != Some(0) && command_unavailable(&out) => {
                note(format!("{VERIFY_PREFIX} skipped `{cmd}`: it is not available here"));
                gate.cmd = None;
            }
            Ok((Some(0), _)) => {
                ran = true;
                summary.push(format!("`{cmd}` passed"));
            }
            Ok((code, out)) => {
                ran = true;
                let status = match code {
                    Some(c) => format!("exit {c}"),
                    None => "timed out or was killed".to_string(),
                };
                problems += failure_count(&out);
                summary.push(format!("`{cmd}` ({status})"));
                report.push_str(&format!(
                    "`{cmd}` failed ({status}). Output (last 4000 chars):\n```\n{}\n```\n",
                    tail_chars(&out, 4000)
                ));
            }
        }
    }
    if abort.load(Ordering::SeqCst) {
        return None;
    }
    let mut shown = false;
    for page in gate.pages.clone() {
        let widths: Vec<String> = crate::pagecheck::DEFAULT_WIDTHS.iter().map(|w| format!("{w}px")).collect();
        note(format!("{VERIFY_PREFIX} opening {page} in a browser at {}…", widths.join(" and ")));
        match crate::pagecheck::check(ws, &page, crate::pagecheck::DEFAULT_WIDTHS, &gate.expect).await {
            Ok(r) => {
                ran = true;
                if !shown {
                    show_in_panel(st, &r.url);
                    shown = true;
                }
                if r.score() > 0 {
                    problems += r.score();
                    summary.push(format!("{page}: {} problem(s)", r.score()));
                    report.push_str(&r.render());
                    report.push('\n');
                } else {
                    summary.push(format!("{page} looks right"));
                }
            }
            Err(e) => {
                note(format!("{VERIFY_PREFIX} page check of {page} skipped: {e:#}"));
                gate.pages.retain(|p| *p != page);
            }
        }
    }
    ran.then(|| GateResult { problems, report: report.trim_end().to_string(), summary: summary.join(", ") })
}

/// Web pages this run changed: files the tools wrote, plus (when shell
/// commands ran) web files modified since the run started.
pub(super) fn changed_web_pages(ws: &std::path::Path, since: std::time::SystemTime, bash_used: bool, enabled: bool) -> Vec<String> {
    if !enabled {
        return Vec::new();
    }
    let mut files = crate::keepbest::written_since(ws, since);
    if bash_used {
        files.extend(crate::pagecheck::recently_modified(ws, since));
    }
    crate::pagecheck::pages_for(ws, &files, 3)
}

/// The project's own check, for `agent.auto_verify`: quick ones that need
/// no setup. `None` when the project has none.
pub fn detect_verify_command(ws: &std::path::Path) -> Option<String> {
    if ws.join("Cargo.toml").is_file() {
        return Some("cargo check --quiet".into());
    }
    if ws.join("go.mod").is_file() {
        return Some("go test ./...".into());
    }
    if let Ok(text) = std::fs::read_to_string(ws.join("package.json")) {
        let test = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["scripts"]["test"].as_str().map(str::to_string));
        if test.is_some_and(|t| !t.contains("no test specified")) {
            return Some("npm test --silent".into());
        }
    }
    let is_test = |name: &str| {
        name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
    };
    let has_py_tests = [ws.to_path_buf(), ws.join("tests")].iter().any(|dir| {
        std::fs::read_dir(dir)
            .map(|it| it.flatten().any(|e| is_test(&e.file_name().to_string_lossy())))
            .unwrap_or(false)
    });
    has_py_tests.then(|| "python -m pytest -q".into())
}

/// The check command itself is missing (not the project failing it).
pub(super) fn command_unavailable(out: &str) -> bool {
    let head: String = out.chars().take(600).collect::<String>().to_ascii_lowercase();
    ["command not found", "is not recognized as an internal or external command", "no module named pytest", "missing script: test"]
        .iter()
        .any(|m| head.contains(m))
}

/// How many failures a failing check's output shows (at least 1).
pub(super) fn failure_count(out: &str) -> usize {
    out.lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("failed") || l.starts_with("fail")
        })
        .count()
        .clamp(1, 99)
}

/// F-08: run the check in the workspace with a timeout. Returns the exit code
/// (`None` = timed out or killed) and stdout + stderr. Stops early on abort.
/// Timeout and abort end the whole process tree, like `bash` does: killing
/// only the shell left `npm test`'s node or `cargo check`'s rustc running.
pub(super) async fn run_verify_command(
    cmd: &str,
    workspace: &std::path::Path,
    timeout: std::time::Duration,
    abort: &AtomicBool,
) -> Result<(Option<i32>, String)> {
    async fn read_all<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut p, &mut buf).await;
        }
        buf
    }
    let mut command = crate::tools::shell_command(cmd);
    command
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // S-05: child processes never get the provider API key or our token.
    crate::tools::scrub_secrets(&mut command);
    // Its own process group (Unix) / Job Object (Windows), so a kill
    // reaches everything it started.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("starting the verify command")?;
    crate::tools::adopt_into_job(&child);
    let pid = child.id();
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let work = async move {
        let (out, err) = tokio::join!(read_all(stdout), read_all(stderr));
        (child.wait().await, out, err)
    };
    tokio::pin!(work);
    let stopped = tokio::select! {
        (status, out, err) = &mut work => {
            let status = status.context("waiting for the verify command")?;
            let mut text = String::from_utf8_lossy(&out).into_owned();
            text.push_str(&String::from_utf8_lossy(&err));
            return Ok((status.code(), text));
        }
        _ = tokio::time::sleep(timeout) => None,
        _ = aborted(abort) => Some(()),
    };
    // The shell still exists (`work` keeps it until we return): end the tree.
    crate::tools::kill_tree(pid).await;
    match stopped {
        None => Ok((None, format!("(stopped: no result after {}s)", timeout.as_secs()))),
        Some(()) => anyhow::bail!("aborted"),
    }
}

/// The last `max` characters of `s` (never splits a character).
pub(super) fn tail_chars(s: &str, max: usize) -> &str {
    let count = s.chars().count();
    if count <= max {
        return s;
    }
    let start = s.char_indices().nth(count - max).map(|(i, _)| i).unwrap_or(0);
    &s[start..]
}
