//! Starting and finishing runs: turbo continuations, the extra-pass
//! budget, `run_agent`, scratch folders, run slots and aborts.

use super::*;

/// Turbo auto-continuation marker. Consecutive continuation messages count
/// toward the safety cap; any real user message resets the streak.
pub const TURBO_CONTINUE_PREFIX: &str = "TURBO-CONTINUE:";
/// Safety cap: how many back-to-back turbo continuations may run without a
/// real user message. Bounds runaway loops while still being "non-stop".
pub const TURBO_MAX_CONTINUATIONS: u32 = 8;
/// New (B-24): start of the prompt that `run_review` stores as a "user"
/// message. It is not the user talking, so `turbo_streak` skips it.
pub(super) const AUTO_REVIEW_PROMPT_START: &str = "Review the current uncommitted changes";

/// How the app's own "user" messages start. They are told apart from the
/// user's words by these starts (`first_request`, `turbo_streak`…).
pub(super) const APP_MARKERS: [&str; 4] = ["[app]", VERIFY_PREFIX, TURBO_CONTINUE_PREFIX, AUTO_REVIEW_PROMPT_START];

/// Put in front of a user message that starts like one of the app's own
/// (U+2060 WORD JOINER: invisible, and not whitespace, so no `trim` drops it).
pub(super) const USER_TEXT_GUARD: char = '\u{2060}';

/// The user's message as stored: one that starts like an app message
/// (`[app] …`, `[verify] …`, `TURBO-CONTINUE: …`) gets [`USER_TEXT_GUARD`]
/// in front, so typing a marker cannot make it count as the app's (skip the
/// turbo cap, hide the real request). Old rows keep matching as before.
pub fn neutralize_user_text(text: &str) -> String {
    let t = text.trim_start();
    if APP_MARKERS.iter().any(|m| t.starts_with(m)) {
        format!("{USER_TEXT_GUARD}{text}")
    } else {
        text.to_string()
    }
}

/// Trailing run of `[TURBO-CONTINUE]` user messages (non-user messages in
/// between don't break the streak; a real user message does).
#[cfg(test)]
pub(super) fn turbo_streak(st: &Shared, session_id: &str) -> u32 {
    st.db.messages(session_id).map(|m| turbo_streak_in(&m)).unwrap_or(0)
}

/// [`turbo_streak`] over messages already loaded.
pub(super) fn turbo_streak_in(msgs: &[Message]) -> u32 {
    let mut streak = 0;
    for m in msgs.iter().rev() {
        if m.role != "user" {
            continue;
        }
        // F-08: a verify hand-back is not a real user message. Skip it, so
        // it neither counts toward nor resets the turbo safety cap.
        let is_verify = m.parts.iter().any(|p| {
            matches!(p, Part::Text { text } if text.starts_with(VERIFY_PREFIX))
        });
        if is_verify {
            continue;
        }
        // New (B-24): the automatic review adds its own "user" prompt after
        // every run that changed files. It must neither count nor break the
        // streak, or the cap never holds.
        let is_review = m.parts.iter().any(|p| {
            matches!(p, Part::Text { text } if text.starts_with(AUTO_REVIEW_PROMPT_START))
        });
        if is_review {
            continue;
        }
        let is_continue = m.parts.iter().any(|p| {
            matches!(p, Part::Text { text } if text.starts_with(TURBO_CONTINUE_PREFIX))
        });
        if is_continue {
            streak += 1;
        } else {
            break;
        }
    }
    streak
}

/// Was the last turbo continuation answered without a single tool call?
pub(super) fn continuation_answered_without_tools(msgs: &[Message]) -> bool {
    let Some(cont) = msgs.iter().rposition(|m| {
        m.role == "user"
            && m.parts.iter().any(|p| matches!(p, Part::Text { text } if text.starts_with(TURBO_CONTINUE_PREFIX)))
    }) else {
        return false;
    };
    // Only when nothing the user wrote came after it.
    let after = &msgs[cont + 1..];
    !after.is_empty()
        && !after.iter().any(|m| m.role == "user" && !m.parts.iter().any(|p| matches!(p, Part::Text { text } if text.starts_with("[app] ") || text.starts_with(VERIFY_PREFIX))))
        && !after.iter().any(|m| m.parts.iter().any(|p| matches!(p, Part::ToolCall { .. })))
}

/// True when the session's todo list still has open work.
pub(super) fn todos_open(st: &Shared, session_id: &str) -> bool {
    matches!(
        st.todo_store.exec(session_id, crate::todo::TodoOp::List),
        crate::todo::TodoResult::Listed { items } if items.iter().any(|i| !i.completed)
    )
}

/// What a turbo continuation takes over from the run that started it: the
/// prompt's pass budget and its hold on the scratch folder.
pub(super) struct Chain {
    pub(super) budget: PassBudget,
    pub(super) scratch: ScratchGuard,
}

/// Fire-and-forget start of a run whose `busy` slot is already claimed
/// (`flag`). Deliberately NOT async: `run_agent` → spawn → `run_agent` would
/// otherwise make their future types recursive (E0391). A sync call keeps
/// the cycle broken. New (B-24): replaces `spawn_kick_run`.
pub(super) fn spawn_claimed_run(st: Shared, session_id: String, user_content: String, flag: Arc<AtomicBool>, chain: Chain) {
    let slot = RunSlot::new(st.clone(), session_id.clone(), flag.clone());
    tokio::spawn(async move {
        run_agent_in(&st, &session_id, &user_content, flag, Some(chain)).await;
        slot.release().await;
    });
}

/// New (B-24): the prompt for a turbo continuation when this run should go
/// on by itself (turbo on, run ok and not aborted, open todos, streak below
/// the cap), else None. A trailing "?" in the answer does NOT count as more
/// work: asking is the one thing turbo must respect. The model is blocked
/// on the user, and pushing it to "decide for yourself" produces guesses.
pub(super) async fn turbo_continuation(st: &Shared, session_id: &str, result: &Result<LoopEnd>) -> Option<String> {
    if st.perm_mode_for(session_id) != PermissionMode::Turbo || !matches!(result, Ok(LoopEnd::Done(_))) {
        return None;
    }
    if !todos_open(st, session_id) {
        return None;
    }
    // One read of the chat for both checks below (T-008: off the async threads).
    let sid = session_id.to_string();
    let msgs = st.db.run(move |db| db.messages(&sid)).await.unwrap_or_default();
    if turbo_streak_in(&msgs) >= TURBO_MAX_CONTINUATIONS {
        return None;
    }
    // A continuation answered with words only ("the job is complete"): the
    // model is done and the open todo is a leftover. Another round would
    // resend the whole chat for the same answer (seen: 8 rounds, ~360K
    // tokens, for nothing).
    if continuation_answered_without_tools(&msgs) {
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "turbo: stopping — the model says it is done, though a todo is still open".into(),
        });
        return None;
    }
    let open: Vec<String> = match st.todo_store.exec(session_id, crate::todo::TodoOp::List) {
        crate::todo::TodoResult::Listed { items } => items
            .iter()
            .filter(|i| !i.completed)
            .map(|i| format!("{} ({})", i.content, i.id))
            .collect(),
        _ => Vec::new(),
    };
    Some(format!(
        "{TURBO_CONTINUE_PREFIX} you have full access and must not ask the \
         user anything. Make the most reasonable decision yourself, keep \
         working on the remaining todos until none are open, and finish the job. \
         Open todos: {}. One that is already done: mark it complete with its id.",
        open.join("; ")
    ))
}

/// Most extra model passes one user prompt may start on top of its own run:
/// fix rounds, final-check rounds, the design and ponytail reviews, and turbo
/// continuations (each of which runs the whole pipeline again). Without a
/// cap one message could fan out into dozens of full agent loops.
pub const MAX_EXTRA_PASSES: u32 = 12;

/// The extra passes one prompt has left (shared with its continuations).
#[derive(Clone)]
pub(crate) struct PassBudget(Arc<(std::sync::atomic::AtomicU32, AtomicBool)>);

impl PassBudget {
    pub(super) fn new() -> Self {
        Self(Arc::new((std::sync::atomic::AtomicU32::new(MAX_EXTRA_PASSES), AtomicBool::new(false))))
    }

    /// Spend one pass on `what`. False when none are left; the user is told
    /// once per prompt what was skipped.
    pub(super) fn take(&self, st: &AppState, session_id: &str, what: &str) -> bool {
        let (left, told) = &*self.0;
        let mut n = left.load(Ordering::SeqCst);
        while n > 0 {
            match left.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
        if !told.swap(true, Ordering::SeqCst) {
            let _ = st.events.send(ServerEvent::Note {
                session_id: session_id.to_string(),
                text: format!(
                    "skipping {what}: this message already used its {MAX_EXTRA_PASSES} extra passes \
                     (checks, fix rounds, reviews, turbo rounds). Send a message to go on."
                ),
            });
        }
        false
    }
}

/// Spend a pass from `budget`, when there is one (missions and deep mode
/// have their own limits and pass `None`).
pub(super) fn spend(budget: Option<&PassBudget>, st: &AppState, session_id: &str, what: &str) -> bool {
    budget.is_none_or(|b| b.take(st, session_id, what))
}

/// The agent loop entry for a user prompt: records a git snapshot first
/// (so `/undo` can roll the run back), stores the message, expands slash
/// command templates, composes the system prompt, and runs [`run_loop`].
pub async fn run_agent(st: &Shared, session_id: &str, user_content: &str, abort: Arc<AtomicBool>) {
    run_agent_in(st, session_id, user_content, abort, None).await
}

/// [`run_agent`], as a turbo continuation of an earlier run (`chain`) or as
/// a new prompt (`None`).
pub(super) async fn run_agent_in(st: &Shared, session_id: &str, user_content: &str, abort: Arc<AtomicBool>, chain: Option<Chain>) {
    // Safety net before anything mutates the workspace. Failure is not
    // fatal — snapshotting is a convenience, the run must still work.
    record_snapshot_before_run(st, session_id).await;
    let (budget, scratch) = match chain {
        Some(c) => (c.budget, c.scratch),
        None => (PassBudget::new(), ScratchGuard::acquire(st.workspace_for(session_id)).await),
    };
    let before = snapshot_stats(st, session_id);
    // New (P-02): fingerprint the git working tree first, so a run that only
    // ran read-only commands (ls, cargo test, git log) gets no review.
    let tree_before = if st.cfg.agent.auto_review {
        st.workspace_fingerprint(session_id).await
    } else {
        None
    };
    let started = std::time::Instant::now();
    // Coarse file times: a second of slack so this run's first writes count.
    let started_at = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
    let spec = make_spec(st, session_id, user_content, &abort).await;
    let result = agent_inner_with(st, session_id, user_content, spec.as_ref().map(crate::spec::for_model), &abort).await;
    let (touched, bash_used) = workspace_touched(st, session_id, &before, tree_before, started_at).await;
    // The reply already streamed to clients as AssistantDelta — resending it
    // as `message` made the TUI print the whole answer a second time.
    let (ok, message) = match &result {
        Ok(_) => (true, None),
        Err(e) => (false, Some(format!("error: {e}"))),
    };
    // If a newer run replaced us, stay silent: our completion must not flip
    // the TUI's busy state or inject a stale answer into its transcript.
    if st.is_superseded(session_id, &abort).await {
        scratch.release().await;
        return;
    }
    let run = RunFacts { ok, touched, bash_used, started, started_at };
    checks_after_run(st, session_id, user_content, spec.as_ref(), &result, &run, &abort, &budget).await;
    // New (B-24): a turbo continuation is part of the same run. Register
    // it BEFORE this run returns and announce no RunFinished in between:
    // every client (headless, TUI, GUI) sees one run that ends when the
    // last continuation ends.
    if let Some(next) = turbo_continuation(st, session_id, &result).await.filter(|_| budget.take(st, session_id, "another turbo round")) {
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "turbo: continuing on my own — no questions, no waiting".into(),
        });
        let flag = st.claim_run(session_id).await;
        // The continuation holds the scratch folder before this run lets go.
        let chain = Chain { budget: budget.clone(), scratch: scratch.share() };
        spawn_claimed_run(st.clone(), session_id.to_string(), next, flag, chain);
        scratch.release().await;
        return;
    }
    scratch.release().await;
    // What the run left, so a later undo can tell its changes from work
    // done outside Habi afterwards (and refuse to overwrite that).
    let (ws, sd) = (st.workspace_for(session_id), st.state_dir.clone());
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || crate::gitsnap::record_after_run(&ws, &sd)).await {
        core_log(&format!("[snapshot] after run: {e:#}"));
    }
    st.finish_run(session_id, &abort, match message {
        None => Ok(None),
        Some(m) => Err(m),
    })
    .await;
}

/// The git snapshot `/undo` rolls a run back to.
/// New (B-48): git runs off the async threads; errors go to core.log.
async fn record_snapshot_before_run(st: &Shared, session_id: &str) {
    let (ws, sd) = (st.workspace_for(session_id), st.state_dir.clone());
    let snap = tokio::task::spawn_blocking(move || {
        crate::gitsnap::record_before_run(&ws, &sd, "pre-run")
    })
    .await;
    if let Ok(Err(e)) = snap {
        core_log(&format!("[snapshot] {e:#}"));
    }
}

/// Laziness Meter: did this run change the workspace at all, and did it
/// run bash? Successful write/edit calls always count. New (P-02): bash
/// counts only when the git working tree really changed (or when there is
/// no git to ask).
async fn workspace_touched(
    st: &Shared,
    session_id: &str,
    before: &RunStats,
    tree_before: Option<u64>,
    started_at: std::time::SystemTime,
) -> (bool, bool) {
    let after = snapshot_stats(st, session_id);
    let bash_used = after.bash_runs > before.bash_runs;
    // Without git there is no tree to compare: look at file times
    // instead. Assuming a change (as before) started a full review after
    // `node --version` — 13 extra calls on a live test.
    let bash_changed = bash_used
        && match tree_before {
            Some(_) => st.workspace_fingerprint(session_id).await != tree_before,
            None => {
                // A walk of the whole project: off the async threads.
                let ws = st.workspace_for(session_id);
                tokio::task::spawn_blocking(move || files_changed_since(&ws, started_at)).await.unwrap_or(true)
            }
        };
    // Files written by this run's subagents count too.
    let by_agents = !crate::keepbest::written_since(&st.workspace_for(session_id), started_at).is_empty();
    (after.writes > before.writes || after.edits > before.edits || bash_changed || by_agents, bash_used)
}

/// What [`checks_after_run`] needs to know about the run that just ended.
struct RunFacts {
    ok: bool,
    touched: bool,
    bash_used: bool,
    started: std::time::Instant,
    started_at: std::time::SystemTime,
}

/// The checks after a run: verification, the final check against the
/// spec, the design review and the ponytail review. None after an abort.
#[allow(clippy::too_many_arguments)]
async fn checks_after_run(
    st: &Shared,
    session_id: &str,
    user_content: &str,
    spec: Option<&crate::spec::Spec>,
    result: &Result<LoopEnd>,
    run: &RunFacts,
    abort: &Arc<AtomicBool>,
    budget: &PassBudget,
) {
    let RunFacts { ok, touched, bash_used, started, started_at } = *run;
    let aborted = || abort.load(Ordering::SeqCst) || matches!(result, Ok(LoopEnd::Aborted));
    // F-08 verification loop: check the work before the review and
    // before the turn is declared done. Never after an abort.
    if ok && touched && !aborted() {
        check_after_run(st, session_id, user_content, started_at, bash_used, abort, Some(budget)).await;
    }
    // The checks above run tests and pages; this one proves what the
    // user asked for (a PNG that is a PNG, a cart that adds up).
    if let Some(spec) = spec.filter(|_| ok && st.cfg.agent.final_check) {
        if !aborted() {
            final_check(st, session_id, spec, abort, Some(budget)).await;
        }
    }
    // Working is not the same as looking right: the model judges the
    // pages it changed by eye (it built five near-identical "AI" heroes
    // on four models while every check passed).
    if ok && touched && !aborted() {
        design_review(st, session_id, user_content, spec, started_at, bash_used, abort, Some(budget)).await;
    }
    // Auto ponytail review: any run that changed files gets the
    // outside-the-diff senior-dev pass BEFORE the turn is declared done.
    // New (P-03): never after an abort; the user asked us to stop.
    // A `review` agent already checked this run's work: a second review
    // would only repeat it at the cost of another full agent loop.
    let reviewed = crate::subagent::reviewed_since(session_id, started);
    if ok && touched && !aborted() && !reviewed && st.cfg.agent.auto_review && !st.own_prompt_only()
        && budget.take(st, session_id, "the ponytail review")
    {
        let _ = st.events.send(ServerEvent::Note {
            session_id: session_id.to_string(),
            text: "ponytail review — checking the change and what it may have \
                   broken outside the diff…"
                .to_string(),
        });
        if let Err(e) = run_review(st, session_id, Some(started_at), abort).await {
            let _ = st.events.send(ServerEvent::Note {
                session_id: session_id.to_string(),
                text: format!("ponytail review failed: {e}"),
            });
        }
    }
}

/// A run's temporary files live in `<project>/.opencode/scratch/`: git
/// ignores it (its own `.gitignore`), and it is removed when the run ends.
pub fn scratch_dir(workspace: &std::path::Path) -> PathBuf {
    workspace.join(".opencode").join("scratch")
}

/// Create the scratch folder. Returns it, and whether `.opencode` was made
/// for it (then that goes too, so a project is left as it was).
pub(super) fn prepare_scratch(workspace: &std::path::Path) -> (PathBuf, bool) {
    let dir = scratch_dir(workspace);
    let made_parent = !workspace.join(".opencode").exists();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(".gitignore"), "*\n");
    }
    (dir, made_parent)
}

pub(super) fn clear_scratch((dir, made_parent): &(PathBuf, bool)) {
    let _ = std::fs::remove_dir_all(dir);
    if *made_parent {
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent); // only when empty
        }
    }
}

/// Runs holding each project's scratch folder, and whether `.opencode` was
/// made for it. Per project: a run in another project never keeps this one's
/// folder alive (the old check counted every busy chat of every project).
pub(super) fn scratch_users() -> &'static Mutex<HashMap<PathBuf, (usize, bool)>> {
    static USERS: std::sync::OnceLock<Mutex<HashMap<PathBuf, (usize, bool)>>> = std::sync::OnceLock::new();
    USERS.get_or_init(Default::default)
}

/// One more holder of `ws`'s scratch folder; the first one creates it. The
/// folder work happens under the lock, so a create and a remove never cross.
pub(super) fn scratch_hold(ws: &std::path::Path) {
    let mut users = scratch_users().lock().unwrap_or_else(|e| e.into_inner());
    let entry = users.entry(ws.to_path_buf()).or_insert((0, false));
    if entry.0 == 0 {
        entry.1 = prepare_scratch(ws).1;
    }
    entry.0 += 1;
}

/// One holder fewer; the last one removes the folder.
pub(super) fn scratch_let_go(ws: &std::path::Path) {
    let mut users = scratch_users().lock().unwrap_or_else(|e| e.into_inner());
    let Some(entry) = users.get_mut(ws) else { return };
    entry.0 = entry.0.saturating_sub(1);
    if entry.0 == 0 {
        let made_parent = entry.1;
        users.remove(ws);
        clear_scratch(&(scratch_dir(ws), made_parent));
    }
}

/// A run's hold on its project's scratch folder. Let go on every way out of
/// the run: `release` on the normal paths, `Drop` on a panic.
pub(crate) struct ScratchGuard {
    pub(super) ws: Option<PathBuf>,
}

impl ScratchGuard {
    pub(super) async fn acquire(ws: PathBuf) -> Self {
        let key = ws.clone();
        let _ = tokio::task::spawn_blocking(move || scratch_hold(&key)).await;
        Self { ws: Some(ws) }
    }

    /// A second hold on the same folder (for a turbo continuation). The
    /// folder exists already, so this touches no files.
    pub(super) fn share(&self) -> Self {
        if let Some(ws) = &self.ws {
            scratch_hold(ws);
        }
        Self { ws: self.ws.clone() }
    }

    pub(super) async fn release(mut self) {
        if let Some(ws) = self.ws.take() {
            let _ = tokio::task::spawn_blocking(move || scratch_let_go(&ws)).await;
        }
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let Some(ws) = self.ws.take() else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(move || scratch_let_go(&ws));
            }
            Err(_) => scratch_let_go(&ws),
        }
    }
}

impl AppState {
    /// New (P-02): a fingerprint of the git working tree (status plus diff),
    /// or None when the workspace is not a git repository. Git runs off the
    /// async threads.
    pub(crate) async fn workspace_fingerprint(&self, session_id: &str) -> Option<u64> {
        let ws = self.workspace_for(session_id);
        tokio::task::spawn_blocking(move || {
            use std::hash::{Hash, Hasher};
            let status = crate::gitsnap::git(&ws, &["status", "--porcelain=v1", "-uall"]).ok()?;
            let diff = crate::gitsnap::git(&ws, &["diff", "--no-ext-diff", "HEAD"]).unwrap_or_default();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            status.hash(&mut h);
            diff.hash(&mut h);
            Some(h.finish())
        })
        .await
        .ok()
        .flatten()
    }

    /// Laziness Meter report for `/debt`.
    pub fn debt_report(&self, session_id: &str) -> String {
        let s = self
            .stats
            .lock()
            .map(|m| m.get(session_id).cloned().unwrap_or_default())
            .unwrap_or_default();
        format!(
            "debt this session: +{} −{} lines · {} write(s) · {} edit(s) · {} bash · \
             {} reuse hint(s). The point is fewer lines, not golf — ponytail.",
            s.added, s.deleted, s.writes, s.edits, s.bash_runs, s.hints
        )
    }

    /// `/review` — ponytail review run as the session's current turn.
    pub async fn kick_review(self: &Shared, session_id: String) {
        self.kick_task(session_id, |st, sid, flag| async move {
            let res = run_review(&st, &sid, None, &flag).await;
            st.finish_run(&sid, &flag, res.map(|_| None).map_err(|e| format!("error: {e}"))).await;
        })
        .await;
    }

    /// `/mission <goal>` (or `/mission resume` with `goal: None`): work on
    /// the goal step by step without the user until it is done
    /// (`mission_run`). Stop ends it like any run; resume carries on.
    pub async fn kick_mission(self: &Shared, session_id: String, goal: Option<String>) {
        // `/mission --deep <goal>`: every step is a Deep run.
        let (deep, goal) = match goal {
            Some(g) => match g.trim_start().strip_prefix("--deep") {
                Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => (true, Some(rest.trim().to_string())),
                _ => (false, Some(g)),
            },
            None => (false, None),
        };
        self.kick_task(session_id, move |st, sid, flag| async move {
            let res = mission_run::run_mission(&st, &sid, goal.as_deref(), deep, &flag).await;
            st.finish_run(&sid, &flag, res.map(Some).map_err(|e| format!("mission: {e:#}"))).await;
        })
        .await;
    }

    /// `/deep [n] <task>` (and `/parallel <task>`, which is `/deep 2`): try
    /// the task in several git worktrees, check each attempt and apply the
    /// best one (`deep::run_deep`). `None` = `agent.deep_attempts`.
    pub async fn kick_deep(self: &Shared, session_id: String, task: String, attempts: Option<usize>) {
        let n = attempts.unwrap_or(self.cfg.agent.deep_attempts);
        self.kick_task(session_id, move |st, sid, flag| async move {
            let res = deep::run_deep(&st, &sid, &task, n, &flag).await;
            let outcome = match res {
                Ok(text) => Ok((!text.is_empty()).then_some(text)),
                Err(e) if flag.load(Ordering::SeqCst) => Err(format!("deep mode stopped: {e}")),
                Err(e) => Err(format!("deep mode failed: {e:#}")),
            };
            st.finish_run(&sid, &flag, outcome).await;
        })
        .await;
    }

    /// Start an agent run for `user_content`, first aborting any run already
    /// active on this session. Every interactive prompt path (HTTP /prompt,
    /// the TUI action bridge, the CLI pre-prompt) goes through here so a
    /// stuck run can always be replaced by the next message.
    pub async fn kick_run(self: &Shared, session_id: String, user_content: String) {
        // The user's own words: they never pass for one of the app's notes.
        let user_content = neutralize_user_text(&user_content);
        self.kick_task(session_id, move |st, sid, flag| async move {
            run_agent(&st, &sid, &user_content, flag).await;
        })
        .await;
    }
}

/// New (B-06): the "busy slot" API. Every kind of run registers its abort
/// flag in `busy`, so `/abort`, a new prompt, the TUI and the GUI can stop it.
impl AppState {
    /// Register a new run for `session_id` and return its abort flag. A run
    /// already active on the session is told to stop (it is replaced).
    pub async fn claim_run(&self, session_id: &str) -> Arc<AtomicBool> {
        let mut busy = self.busy.write().await;
        if let Some(old) = busy.remove(session_id) {
            signal_abort(&old);
        }
        let flag = Arc::new(AtomicBool::new(false));
        busy.insert(session_id.to_string(), flag.clone());
        flag
    }

    /// Free the session's slot, but only while `flag` still owns it.
    pub async fn release_run(&self, session_id: &str, flag: &Arc<AtomicBool>) {
        let mut busy = self.busy.write().await;
        if busy.get(session_id).map(|f| Arc::ptr_eq(f, flag)).unwrap_or(false) {
            busy.remove(session_id);
            run_released().notify_waiters();
        }
    }

    /// True when a newer run took the session's slot from `flag`.
    pub async fn is_superseded(&self, session_id: &str, flag: &Arc<AtomicBool>) -> bool {
        let busy = self.busy.read().await;
        busy.get(session_id).map(|f| !Arc::ptr_eq(f, flag)).unwrap_or(false)
    }

    /// The end of a run, for the clients: on `Ok(Some(text))` the text is
    /// saved and streamed as the last reply, then one `RunFinished` (an
    /// `Err` carries its message). Nothing at all when a newer run took the
    /// slot: that run reports for itself. False then.
    pub async fn finish_run(&self, session_id: &str, flag: &Arc<AtomicBool>, outcome: std::result::Result<Option<String>, String>) -> bool {
        if self.is_superseded(session_id, flag).await {
            return false;
        }
        let (ok, message) = match outcome {
            Ok(text) => {
                if let Some(text) = text {
                    let _ = self.db.add_message(session_id, "assistant", &[Part::Text { text: text.clone() }]);
                    let _ = self.events.send(ServerEvent::AssistantDelta { session_id: session_id.to_string(), text });
                }
                (true, None)
            }
            Err(m) => (false, Some(m)),
        };
        let _ = self.events.send(ServerEvent::RunFinished { session_id: session_id.to_string(), ok, message });
        true
    }

    /// Ask the session's active run to stop. False when nothing runs.
    pub async fn abort_run(&self, session_id: &str) -> bool {
        // Its spawned agents stop with it.
        crate::subagent::stop_children(session_id);
        let busy = self.busy.read().await;
        match busy.get(session_id) {
            Some(flag) => {
                signal_abort(flag);
                true
            }
            None => false,
        }
    }

    /// True while a run is registered for the session.
    pub async fn run_active(&self, session_id: &str) -> bool {
        self.busy.read().await.contains_key(session_id)
    }

    /// Run `work` as the session's current run: registered in `busy`,
    /// replacing any active run, released when it ends — also when it
    /// panics (see [`RunSlot`]). `work` gets the state, the session id and
    /// the run's abort flag.
    pub async fn kick_task<F, Fut>(self: &Shared, session_id: String, work: F)
    where
        F: FnOnce(Shared, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let flag = self.claim_run(&session_id).await;
        let slot = RunSlot::new(self.clone(), session_id.clone(), flag.clone());
        let st = self.clone();
        tokio::spawn(async move {
            work(st, session_id, flag).await;
            slot.release().await;
        });
    }
}

/// A claimed busy slot, freed when the run ends. Normally by `release`; on
/// a panic the task's future is dropped and `Drop` frees it, so a crashed
/// run cannot leave its chat "busy" for good (DELETE then answered 409).
pub(crate) struct RunSlot {
    pub(super) st: Shared,
    pub(super) session_id: String,
    pub(super) flag: Option<Arc<AtomicBool>>,
}

impl RunSlot {
    pub(super) fn new(st: Shared, session_id: String, flag: Arc<AtomicBool>) -> Self {
        Self { st, session_id, flag: Some(flag) }
    }

    pub(super) async fn release(mut self) {
        if let Some(flag) = self.flag.take() {
            self.st.release_run(&self.session_id, &flag).await;
        }
    }
}

impl Drop for RunSlot {
    fn drop(&mut self) {
        let Some(flag) = self.flag.take() else { return };
        if let Ok(mut busy) = self.st.busy.try_write() {
            if busy.get(&self.session_id).is_some_and(|f| Arc::ptr_eq(f, &flag)) {
                busy.remove(&self.session_id);
                run_released().notify_waiters();
            }
            return;
        }
        // The lock is taken right now: free the slot from a task instead.
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            let (st, session_id) = (self.st.clone(), std::mem::take(&mut self.session_id));
            h.spawn(async move { st.release_run(&session_id, &flag).await });
        }
    }
}

/// Woken whenever a run frees its busy slot (DELETE waits on it).
pub(super) fn run_released() -> &'static tokio::sync::Notify {
    static N: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

/// Wakers for everything waiting on an abort flag, by the flag's address
/// (the flags are plain `AtomicBool`s shared with the other modules).
pub(super) fn abort_wakers() -> &'static Mutex<HashMap<usize, Arc<tokio::sync::Notify>>> {
    static W: std::sync::OnceLock<Mutex<HashMap<usize, Arc<tokio::sync::Notify>>>> = std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

/// Stop a run: set its abort flag and wake whatever waits on it at once.
pub fn signal_abort(flag: &AtomicBool) {
    flag.store(true, Ordering::SeqCst);
    let key = flag as *const AtomicBool as usize;
    let waker = abort_wakers().lock().ok().and_then(|m| m.get(&key).cloned());
    if let Some(n) = waker {
        n.notify_waiters();
    }
}

/// Resolves once `flag` is set. [`signal_abort`] wakes it at once (this
/// replaced 100 ms sleep loops); a flag set some other way (a plain `store`
/// in another module) is still seen within `RECHECK`.
pub async fn aborted(flag: &AtomicBool) {
    const RECHECK: std::time::Duration = std::time::Duration::from_millis(500);
    let key = flag as *const AtomicBool as usize;
    let waiter = AbortWaiter::new(key);
    loop {
        let notified = waiter.notify.notified();
        tokio::pin!(notified);
        // Registered before the check, so a signal in between is not lost.
        notified.as_mut().enable();
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(RECHECK) => {}
        }
    }
}

/// One waiter's share of a flag's `Notify`; the last one out removes it.
pub(super) struct AbortWaiter {
    pub(super) key: usize,
    pub(super) notify: Arc<tokio::sync::Notify>,
}

impl AbortWaiter {
    pub(super) fn new(key: usize) -> Self {
        let mut map = abort_wakers().lock().unwrap_or_else(|e| e.into_inner());
        let notify = map.entry(key).or_default().clone();
        Self { key, notify }
    }
}

impl Drop for AbortWaiter {
    fn drop(&mut self) {
        let mut map = abort_wakers().lock().unwrap_or_else(|e| e.into_inner());
        // The map's copy and ours: nobody else waits on this flag.
        if map.get(&self.key).is_some_and(|n| Arc::ptr_eq(n, &self.notify)) && Arc::strong_count(&self.notify) <= 2 {
            map.remove(&self.key);
        }
    }
}

pub(super) async fn agent_inner(
    st: &Shared,
    session_id: &str,
    user_content: &str,
    abort: &AtomicBool,
) -> Result<LoopEnd> {
    agent_inner_with(st, session_id, user_content, None, abort).await
}

/// [`agent_inner`] with a note the model gets right after the request (the
/// work spec). A message, not part of the system prompt: the prompt stays
/// the same for the chat and keeps hitting the provider's cache.
pub(super) async fn agent_inner_with(
    st: &Shared,
    session_id: &str,
    user_content: &str,
    note: Option<String>,
    abort: &AtomicBool,
) -> Result<LoopEnd> {
    // T-004: the request, its notes and the system prompt in one trip off
    // the async threads (DB writes, command templates, AGENTS.md, skills,
    // lessons and the project map are all disk work). In this order: the
    // prompt reads the first request back.
    let (system, workspace) = {
        let (st2, sid, content) = (st.clone(), session_id.to_string(), user_content.to_string());
        st.db
            .run(move |db| {
                db.add_message(&sid, "user", &[Part::Text { text: content.clone() }])?;
                if let Some(note) = note {
                    db.add_message(&sid, "system", &[Part::Text { text: note }])?;
                }
                // M2: expand slash-command templates (`.opencode/commands/*.md`);
                // the raw input stays visible to the user, the expansion is a system note.
                if let Some((rendered, doc)) = skills::resolve_command(&st2.workspace_for(&sid), &content) {
                    db.add_message(
                        &sid,
                        "system",
                        &[Part::Text {
                            text: format!("[slash command `/{}` expanded]\n{}", doc.name, rendered),
                        }],
                    )?;
                }
                Ok((st2.composed_system(&sid), st2.run_workspace(&sid)))
            })
            .await
            .context("saving the request and composing the system prompt")?
    };
    let opts = LoopOpts {
        model: None,
        max_steps: None,
        system,
        emit_activity: true,
        allow: None,
        can_spawn: true,
        workspace,
        agent: None,
    };
    run_loop_inner(st, session_id, &opts, abort).await
}
