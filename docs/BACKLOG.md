# opencode-rust: small-task backlog

Built 2026-11-01 from a read-only pass over `.claude/worktrees/integration` (HEAD `d63b83a`,
"Clippy clean under -D warnings"). Every task cites the file and function it touches. Each one is
a single focused change (1-2 files, under about an hour) with a criterion you can check, and none
needs a product decision. Line numbers are approximate (server.rs/gui.rs/tools.rs/main.rs are being
split by other people, so search by function name).

Left out on purpose: the security/correctness fixes that were just merged, and the server.rs /
tools.rs / gui.rs / main.rs / tui draw+on_key splits. FIX_PLAN.md (92/92) and PLAN_PROGRESS.md
have no open items, so nothing here comes from them.

Priorities: **P1** = real bug or robustness problem, **P2** = UX / performance / tests,
**P3** = polish / docs.

Pattern for the db-async tasks: `Db::run` (db.rs) already exists. It clones the handle into
`spawn_blocking`. No server code calls it yet; only main.rs `NewSession` does.

---

## 1. Backend: blocking SQLite calls on async threads (db-async)

- [x] T-001 [db-async] [P1] HTTP read routes off the async threads — replace the direct `st.db.*` calls in server.rs::list_sessions, ::get_messages (get_session + messages) and ::get_mode_route with `st.db.run(..).await` — done when: those three handlers contain no direct `st.db.` call and the F-12 route tests in server.rs `mod tests` pass
- [x] T-002 [db-async] [P1] HTTP write routes off the async threads — same change in server.rs::create_session, ::prompt (session-exists check), ::put_mode_route and ::patch_session_route (get + set_session_title + get, one `Db::run`) — done when: no direct `st.db.` call in those four handlers; route tests pass
- [x] T-003 [db-async] [P1] delete_session_route: run the existence check and `delete_session` through `Db::run`; keep the 10 s run-release wait as it is (server.rs::delete_session_route) — done when: no direct `st.db.` call in the handler; the delete-while-running test still returns 409/204 as before
- [x] T-004 [db-async] [P1] Run start writes the user message and system notes with three sync `add_message` calls (server.rs::agent_inner_with ~2041-2050). Batch them in one `Db::run` — done when: agent_inner_with has no direct `st.db.` call; kick_run tests pass
- [x] T-005 [db-async] [P1] Agent loop message writes (server.rs::run_loop_inner ~3158, 3380, 3386, 3409, 3434, 3452) go through `Db::run` — done when: `st.db.` no longer appears inside run_loop_inner; agent-loop tests pass
- [x] T-006 [db-async] [P1] server.rs::finish_tool_call (~3702, tool-result write) and ::build_chat_messages (~5024, reads the whole history on every step) use `Db::run` — done when: neither calls the DB synchronously; history-repair tests (B-04) pass
- [x] T-007 [db-async] [P2] server.rs::record_usage_as (~2143-2150) runs `add_usage_entry` + `usage_totals` on every model request. Do both in one `Db::run` and send the `Usage` event afterwards — done when: there is no sync DB call in record_usage_as; usage dashboard tests pass
- [x] T-008 [db-async] [P2] History reads in server.rs::turbo_streak (~1208), ::turbo_continuation (~1302), ::first_request (~2231), ::final_check (~4085) and ::last_assistant_text (~4383): async callers fetch messages via `Db::run`; keep the pure helpers (e.g. `turbo_streak_in`) — done when: none of these reads runs synchronously on a tokio worker; turbo tests pass
- [x] T-009 [db-async] [P2] In server.rs::check_permission_preview (~4907), the `remembered` closure calls `db.has_approval` once for every rule it evaluates. Load the workspace's approvals once per tool call via `Db::run` into a `HashSet<(tool, detail)>` and match in memory — done when: one DB query per tool call; permissions/approval tests pass
- [x] T-010 [db-async] [P2] server.rs::AppState::auto_approved (~500-515) walks up to 16 parents with a sync `get_session` per hop on every tool call. Read the parent chain in one `Db::run` (or one recursive CTE added to db.rs) — done when: there is one blocking-pool call per check; mission auto-approval tests pass
- [x] T-011 [db-async] [P2] server.rs::workspace_for (~2113) runs `get_session` + `get_project` synchronously and is called from many async paths. Cache chat → folder in an AppState map filled on the first lookup and cleared in `forget_session` — done when: a test shows the second lookup does not query the DB and that `forget_session` drops the entry
- [x] T-012 [db-async] [P1] server/mission_run.rs::say and ::current call the DB synchronously (say runs once per step note). Make both async over `Db::run` and update their callers — done when: neither has a sync DB call; mission_run tests pass
- [x] T-013 [db-async] [P1] server/mission_run.rs::steps_loop makes 13 sync calls (`create_session`, `mission_step_update`, `mission_step_insert_after`, `add_message`). Route them through `Db::run` — done when: `st.db.` no longer appears in steps_loop; mission tests pass
- [x] T-014 [db-async] [P2] server/mission_run.rs::run_mission start/finish block (~227-296: `mission_start`, `mission`, `mission_set_status`, `create_session`) and ::mission_report (~488) use `Db::run` — done when: there are no sync DB calls in those functions; mission tests pass
- [x] T-015 [db-async] [P2] server/deep.rs::run_deep first `add_message` (~134) goes through `Db::run` — done when: run_deep has no sync DB call; deep tests pass
- [x] T-016 [db-async] [P2] team.rs::run_team (74, 98), ::worker (267), ::run_worker_agent (283) and ::worker_system (346, get_session) use `Db::run` — done when: no sync DB call is left in team.rs outside tests; team tests pass
- [x] T-017 [db-async] [P2] subagent.rs::start (260), ::run_rounds (422), ::report (458, 486) and ::agent_tool_inner (632) use `Db::run` — done when: no sync DB call is left in subagent.rs outside tests; subagent tests pass
- [x] T-018 [db-async] [P2] The TUI action handler (main.rs::tui_run) runs Resume/Diff (`sessions::resolve_session`, `diff_sessions`), Export (`export_transcript`) and Usage (`usage_report`) inline on its async task. Wrap each in `spawn_blocking` / `Db::run` — done when: those four arms make no direct DB or file call on the async task; TUI tests pass

## 2. Known gaps (from the last review)

- [x] T-019 [backend] [P1] `rewind_chat` runs git on the GUI thread. The gui.rs `IpcMsg::Rewind` handler calls server::rewind_chat (up to 50 `gitsnap::undo` runs) inside the event loop. Run it on `rt.spawn_blocking` and post the result back as a new UiMsg that reloads the chat and fills the composer — done when: the event-loop closure no longer calls rewind_chat; rewinding in the GUI still reloads the chat and puts the text back in the box
- [x] T-020 [backend] [P2] One FNV-1a implementation. mcp.rs::short_hash (32-bit) and gitsnap.rs::fnv1a (64-bit) each have their own. Move both into one small module (e.g. `src/hash.rs`, registered in lib.rs) and call it from both places — done when: there is one implementation, and a test pins the known vectors (`""` → 811c9dc5 and cbf29ce484222325) plus the existing MCP tool names and `snaps_` folder names, which must not change
- [x] T-021 [backend] [P1] permissions.rs::runs_code flags `git commit -c <rev>` and `git log -c` as code-running because it checks `-c` anywhere. Treat `-c`, `--config-env` and `--exec-path` as code-running only before the git subcommand — done when: `git commit -c HEAD` and `git log -c` are Allow under `git *`, and every current Ask case in `wildcard_allow_does_not_cover_code_running_arguments` still asks
- [x] T-022 [backend] [P1] worktree.rs `impl Drop for Isolation` calls `wt.report()` and then Worktree::drop, which runs four git commands on the current thread, so an early `?` in async code blocks a tokio worker. When a runtime is current, move the work to `spawn_blocking` (as deep.rs `Trees::drop` does) — done when: `dropped_isolation_saves_a_patch_and_discard_does_not` still passes, and a new current_thread tokio test shows the drop returns without running git inline
- [x] T-023 [backend] [P2] In server/deep.rs::run_deep, if worktree i fails to create, the `wts: Vec<Worktree>` built so far drops on the async thread (each Worktree::drop runs git). Wrap the vector in `Trees` before the loop, or drop it via `spawn_blocking` on the error path — done when: the early-error path runs no git on the worker thread (code check plus a test with a non-git folder)
- [x] T-024 [backend] [P2] gui.rs::request_stop sets the run's flag with a plain `store`, so `server::aborted` only notices it on its 500 ms recheck. Call `server::signal_abort(flag)` instead — done when: a GUI stop wakes `aborted()` at once (test: a waiter resolves well under 500 ms)
- [x] T-025 [backend] [P2] subagent.rs::stop and ::stop_children use `run.abort.store(true)`. Switch both to `server::signal_abort` — done when: there is no plain abort `store` left outside tests (grep `\.store(true` shows only non-abort flags)

## 3. Backend correctness and robustness

- [x] T-026 [backend] [P1] Deleting a chat from the GUI leaks per-chat state. The gui.rs `IpcMsg::DeleteChat` handler calls `st.db.delete_session` but never `st.forget_session` (modes, stats, ponytail levels, auto_approve, FROZEN_LESSONS, COMPACT_FAILED_AT), unlike server.rs::delete_session_route — done when: the handler calls forget_session after a successful delete
- [x] T-027 [backend] [P1] db.rs::Db::delete_session removes messages and the session row but leaves `missions` / `mission_steps` rows (no FK). Delete them in the same transaction — done when: a db test that starts a mission and deletes the session finds `mission(id)` is None and no mission_steps rows remain
- [x] T-028 [backend] [P2] Todos survive deleted chats (todo.rs keeps its own table with no FK). Add `TodoStore::forget(session_id)` and call it from server.rs::delete_session_route and the gui.rs DeleteChat handler — done when: a test adds a todo, deletes the chat through the route, and `list_all` comes back empty
- [x] T-029 [backend] [P1] todo.rs::TodoStore::open opens a second connection on opencode.db with no `busy_timeout`, so a todo write fails with "database is locked" while `Db` is writing. Set `busy_timeout(5s)` as db.rs::Db::open does — done when: a test that writes through Db and TodoStore from two threads on one file sees no lock error
- [x] T-030 [backend] [P2] todo.rs::TodoStore::exec and ::list_all use `self.conn.lock().unwrap()`, so one panic poisons the lock and every later todo call panics. Use `unwrap_or_else(|e| e.into_inner())` as db.rs::Db::conn does — done when: todo.rs has no `.lock().unwrap()`, and a poison test like `a_poisoned_lock_does_not_break_later_calls` passes for TodoStore
- [x] T-031 [backend] [P2] main.rs::run silently ignores `--port abc` (`parse::<u16>().ok()`) and `--model` / `--base-url` given with no value. Exit with code 2 and a message such as "invalid value for --port: abc" — done when: `opencode-rust --port abc` exits 2 with that message; a unit test covers the parsing helper
- [x] T-032 [backend] [P2] config.rs::default_config_path and ::default_state_dir accept an empty `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `APPDATA` and so produce relative paths (config read from the cwd). Ignore empty values, as the XDG spec says — done when: a test with `XDG_CONFIG_HOME=""` gets the `$HOME/.config` path
- [x] T-033 [backend] [P2] `provider.images` is compared case-sensitively in attachments.rs::model_sees_images ("On" falls back to auto), and config.rs::Config::load never checks the value. Compare lowercase, and warn at load for anything other than auto/on/off — done when: a test shows "ON" behaves like "on" and "yes" produces a warning
- [x] T-034 [backend] [P2] `provider.temperature` is never range-checked (config.rs::Config::load). Reject values outside 0.0..=2.0 with an error that names the key — done when: `temperature = 5` fails at startup with a message naming `provider.temperature`; test added
- [x] T-035 [backend] [P2] tools.rs::write_atomic leaves `.<name>.ocrs-<pid>-<n>.tmp` behind when `std::fs::write(&tmp)` fails partway (disk full, quota). Remove the temp file on every error path after it is created — done when: the error branch deletes tmp, and the existing write/edit tests pass
- [x] T-036 [backend] [P2] attachments.rs::safe_name uses only a millisecond stamp, so two pastes with the same name in the same millisecond overwrite each other (`save_bytes` uses `fs::write`). Add a counter, or create with `create_new` and retry — done when: a test saving the same name twice in a tight loop gets two different files
- [x] T-037 [backend] [P2] mcp.rs::Registry::start_all silently skips `[[mcp.servers]]` entries with an empty name or with neither command nor url. Warn with the entry index and what is missing — done when: a config with `name = ""` logs "skipping mcp.servers[0]: needs a name and a command or url"
- [x] T-038 [backend] [P2] When an MCP tool call hits CALL_TIMEOUT (300 s), the server is never told and keeps working. Send `notifications/cancelled` with the request id (mcp.rs stdio call path ~194; mcp_http.rs::call_tool) — done when: a fake-server test receives the cancel notification after a forced short timeout
- [x] T-039 [backend] [P3] mcp_http.rs: send `DELETE` with the `Mcp-Session-Id` header when an HTTP MCP connection is dropped (the MCP spec's session end) — done when: the `fake_mcp` test server counts one DELETE after the registry is dropped
- [x] T-040 [backend] [P2] provider.rs::send_with_retry returns at once when waiting for response headers times out; only send errors and 429/5xx are retried. Treat the header timeout as retryable within `max_retries` — done when: a mock that stalls the first request and answers the second succeeds; the error message after the last try still names `provider.request_timeout_secs`
- [x] T-041 [backend] [P2] provider.rs::send_with_retry reads error bodies with `resp.text().await` and no time limit (only the first 400 chars are kept). Bound the read with `request_timeout` and stop after about 4 KB — done when: a mock that sends an error status and then never finishes the body returns an error within the timeout
- [x] T-042 [backend] [P2] The gui.rs `IpcMsg::NewBranch` check accepts names git rejects (`a..b`, trailing `.lock`, leading `.` or `/`, `@{`, trailing `/`), so the user sees raw git stderr. Extract the check into a function that follows git's ref-format rules (or calls `git check-ref-format --branch`) — done when: a unit test rejects those names with our own message
- [x] T-043 [backend] [P3] The gui.rs `IpcMsg::DeleteChat` and MoveChat handlers do nothing when the chat is running (`if !gs.running.contains(&id)`), with no message. Push a System entry "Stop the run first, then delete/move this chat." — done when: deleting a running chat shows the note
- [x] T-044 [backend] [P3] The gui.rs MoveChat handler ignores `set_session_project` errors (`let _ =`). Show an `EntryKind::Error` like AdoptUnfiled does — done when: a failed move produces an error line
- [x] T-045 [backend] [P2] pagecheck.rs::content_type has no entries for wasm, avif, mp4, webm, xml or map, so they are served as `application/octet-stream`; `WebAssembly.instantiateStreaming` rejects wasm served that way. Add them (`application/wasm` etc.) — done when: a unit test covers every new extension
- [x] T-046 [backend] [P3] gitsnap.rs::git_env: set `GIT_OPTIONAL_LOCKS=0`, so the background `git status`/`diff` calls (server.rs::workspace_fingerprint, gui.rs::git_state) never take `index.lock` while the user runs git — done when: the env var is set on every gitsnap git call and gitsnap tests pass
- [x] T-047 [backend] [P2] tools.rs::list_dir returns every entry, so a folder with 100k files floods the context. Cap it at 1000 lines with "… and N more (use glob to narrow)" — done when: a test with 1100 files gets 1000 lines plus the note
- [x] T-048 [backend] [P3] config.rs `BrowserCfg::profile_dir` is parsed but never used (its doc says "Reserved for Goal C2"). Warn at load when it is set, so users know it has no effect — done when: a config with `profile_dir` prints the warning; test via the warning helper

## 4. Error messages

- [x] T-049 [errors] [P2] provider.rs::send_with_retry formats connection failures with `{e:?}` (a reqwest Debug dump). Move server.rs::heartbeat_error (plain "could not connect: <cause chain>") to provider.rs and use it in both places — done when: a test against a closed port gets a message without `reqwest::Error {`
- [x] T-050 [errors] [P3] A provider 404 shows only "provider returned 404 Not Found: …". In provider.rs::send_with_retry, add a hint for 404: "check provider.model and that base_url ends in /v1" — done when: a mock-404 test sees the hint
- [x] T-051 [errors] [P3] HTTP API errors are bare status codes with an empty body (server.rs route handlers return `StatusCode`). Return `{"error": "..."}` for 400/404/409 (e.g. "session not found", "title is empty", "run did not stop within 10 s"); `internal()` keeps 500 generic — done when: a route test checks the JSON body of a 404 and of a 409
- [x] T-052 [errors] [P2] The GUI "Load models" button hides the provider's error. The gui.rs `IpcMsg::ListModels` handler does `list_models().await.unwrap_or_default()`, and gui.html `setLoading` shows a generic text after 6 s. Pass the error text to the page and show it in `#mdl-status` — done when: a provider answering 401 shows the 401 text under Load models
- [x] T-053 [errors] [P3] mcp.rs::McpConn::spawn: when the command is not found (`ErrorKind::NotFound`), say "`npx` was not found on PATH; on Windows start it through cmd: command = \"cmd\", args = [\"/c\", \"npx\", ...]", matching config.example.toml — done when: a test with a missing command gets the hint

## 5. Logging and observability

- [x] T-054 [logging] [P1] Heartbeat and probe lines go to stderr while the TUI owns the terminal, which garbles the screen. B-48 meant to stop this, but server.rs::start_heartbeat (`eprintln!("{line}")` on online/offline), ::start_context_probe ("[api] model context window") and ::probe_system_delivery still print. Log to `core_log` only (print to stderr only in headless) — done when: no `eprintln!` is left in those three functions, and a TUI run with an unreachable provider shows no stray text
- [x] T-055 [logging] [P2] server.rs::core_log appends to core.log forever, while cache.log is capped (server.rs::log_cache). When core.log passes 2 MB, rename it to core.log.1 and start a new one — done when: a test with a lowered threshold sees the rotation
- [x] T-056 [logging] [P2] MCP start results are printed only with `eprintln!` (mcp.rs::Registry::start_all: connected / failed / skipped). A GUI started from a shortcut has no console. Also write them to `core_log` — done when: core.log contains "[mcp] <name> connected" or "failed to start"
- [x] T-057 [logging] [P2] gui.rs diagnostics use `eprintln!` ("window created", "IPC not understood", "page error", "run start", "blocked a navigation"), which no console shows in a shortcut launch. Add a `gui_log` helper that also calls `server::core_log` — done when: a page error appears in core.log
- [x] T-058 [logging] [P2] server.rs::AppState::decide ignores `add_approval` failures (`let _ =`), so a failed "Always allow" is silently not saved. Log the error with tool and workspace to core_log — done when: a forced DB error writes a core.log line
- [x] T-059 [logging] [P2] agents.rs::read_role_dir drops unreadable or unparsable role files silently ("Unreadable and broken files are left out"). Log the path and reason once to core_log — done when: a test with a broken `.opencode/agents/x.md` writes the line and still loads the other roles
- [x] T-060 [logging] [P3] server/mission_run.rs::say drops `add_message` errors (`let _ =`), so a mission note can be missing from history with no trace. Log failures to core_log — done when: a failed write leaves a core.log line

## 6. Performance

- [x] T-061 [perf] [P2] tui/mod.rs::run calls the blocking `crossterm::event::poll(50ms)` inside an async fn, which holds a tokio worker for the whole session. Switch to `crossterm::event::EventStream` (the `event-stream` feature is already on) with `tokio::select!` over keys and `event_rx` — done when: run has no `event::poll`; TUI tests pass and keys and paste still work
- [x] T-062 [perf] [P3] tui/mod.rs::run redraws every loop (about 20 times a second) even when idle. Redraw only after a key, paste, server event or resize, or on a timer while `app.busy` (spinner) — done when: an idle TUI makes no draw calls between events (test with a draw counter)
- [x] T-063 [perf] [P3] server.rs::run_loop_inner calls `agents::load_subagents` (reads up to three folders) on every step (~3122). Load the roles once per run — done when: one load per run (code check), and spawn-spec tests pass
- [x] T-064 (moved to T-152) [perf] [P3] server.rs::log_cache runs `create_dir_all` + `metadata` + open/append synchronously for every model request on an async thread. Keep the file handle open as core_log does — done when: there is no per-call `create_dir_all`/open; the cache.log format is unchanged
- [x] T-065 [perf] [P3] pagecheck.rs::find_browser probes about 10 fixed paths plus every PATH folder on each check_page/screenshot call. Cache the result in a `OnceLock` (the env-var override still wins) — done when: the second call does no filesystem probe; pagecheck tests pass
- [x] T-066 [perf] [P2] The gui.rs event loop runs `git_state(chat_root)` (two git processes) on the GUI thread whenever `git_stale` is set, e.g. after every run. Compute it on the blocking pool and post the result back — done when: the event loop never calls git_state directly; the branch chip still updates after a run
- [x] T-067 [perf] [P2] The gui.rs `IpcMsg::LoadChanges` handler runs `changes::collect` (git diff of the whole tree) on the GUI thread. Move it to the blocking pool and post `changes_out` back — done when: the handler only starts the work; the Changes dialog still fills
- [x] T-068 [perf] [P3] The gui.rs `IpcMsg::NewBranch` handler runs `git switch -c` on the GUI thread. Move it to the blocking pool — done when: the handler only starts the work; the result note still appears
- [x] T-069 [perf] [P3] On every wake, the gui.rs event loop computes `term_live` with `crate::tools::jobs::list(0)`, which clones every job view. Add `jobs::any_running()` in tools.rs and use it — done when: the event loop no longer calls `jobs::list` to decide the wake interval
- [x] T-070 [perf] [P3] The gui.rs `/usage` path (~1555) calls `st.usage_report`, which builds the full system prompt (repo-map scan, skills) plus three usage_totals queries on the GUI thread. Run it on the blocking pool — done when: the GUI thread no longer calls usage_report
- [x] T-071 [perf] [P3] server.rs::export_transcript reads the whole existing file only to check that it starts with `EXPORT_MARKER`. Read just `EXPORT_MARKER.len()` bytes — done when: the export overwrite and refuse tests still pass
- [x] T-072 [perf] [P3] subagent.rs::wait_for polls the run map every 100 ms. Wait on `server::aborted(abort)` and a per-run Notify (or a broadcast run-finished event) with the timeout — done when: no fixed sleep loop is left; spawn/wait tests pass

## 7. Tests for untested code paths

- [x] T-073 [tests] [P2] tools.rs::closest_region has no test. Cover an exact-match region, an indentation-only difference, no match (returns "") and the 12-line cap — done when: the new test passes
- [x] T-074 [tests] [P2] tools.rs::cap_line has no test. Cover a line of exactly READ_MAX_LINE_CHARS, one char over, and multibyte characters (count chars, not bytes) — done when: the new test passes
- [x] T-075 [tests] [P2] tools.rs `jobs::prune` has no test. It must keep running jobs, user jobs and the newest KEEP_ENDED (20) ended agent jobs — done when: a test with 25 ended agent jobs + 1 running + 1 user job leaves 22
- [x] T-076 [tests] [P2] tools.rs::write_atomic has no direct test. Cover replacing contents, refusing a read-only file, leaving no `.ocrs-*.tmp` behind and keeping permissions (unix) — done when: the new test passes on Windows and Linux
- [x] T-077 [tests] [P2] tools.rs::list_dir: test folders first with `/`, sorted files with sizes, and "(empty directory)" (together with T-047's cap) — done when: the new test passes
- [x] T-078 [tests] [P2] permissions.rs::runs_code is only tested through the rule engine. Add a table test of the function itself: `git.exe -c`, `/usr/bin/git -c`, quoted `'-c'`, `--exec=x` on any command, `find -okdir`, plus non-git `-c` (e.g. `bash -c` is not flagged by this function) — done when: the table test passes
- [x] T-079 [tests] [P2] attachments.rs::safe_name has no test. Cover a path with folders (only the file name is kept), unicode, an all-dots name (→ `file`) and the 80-char cap — done when: the new test passes
- [x] T-080 [tests] [P2] pagecheck.rs::url_path, ::content_type, ::html_unescape and ::between have no tests — done when: one table test covers backslashes, a leading `./`, case-insensitive extensions and the unknown-extension fallback
- [x] T-081 [tests] [P2] mcp.rs::short_hash and ::sanitize_server have no tests. Pin their outputs (tool names are part of the cached prompt and must stay stable) — done when: the test pins two hashes and a sanitized name with spaces and dots
- [x] T-082 [tests] [P2] usage.rs::cost and ::ranked have no direct test. Cover cached tokens priced apart from fresh input, `None` without a price, and ranking order with ties — done when: the new test passes
- [x] T-083 [tests] [P2] keepbest.rs::written_since and `Journal::changed` are untested. Cover filtering by root and time, de-duplication, and the drain at 2000 entries — done when: the new test passes
- [x] T-084 [tests] [P2] changes.rs::comments_message has no test. Cover comments on two files and the line numbers in the message text — done when: the new test passes
- [x] T-085 [tests] [P2] todo.rs: test two TodoStore handles (or TodoStore + Db) on one database file writing at once (goes with T-029) — done when: the test passes ten runs in a row
- [x] T-086 [tests] [P2] tui/draw.rs::truncate_str (wide characters, the exact limit) and ::approval_detail_lines (tab → 4 spaces, control chars → `\u{..}`) are untested — done when: new tests in tui/tests.rs pass
- [x] T-087 [tests] [P2] tui/state.rs::palette_matches is untested. Cover `/` alone (all entries), prefix filtering and no match — done when: the new test in tui/tests.rs passes
- [x] T-088 [tests] [P2] tui/state.rs::parse_plan_item and ::parse_worker_line (the `[team]` line parsers) are untested. Cover good lines, numbering gaps and junk — done when: the new test in tui/tests.rs passes
- [x] T-089 [tests] [P2] tui/state.rs::on_sessions_key is untested. Cover Up at 0, Down clamped to the end, Enter on an empty list (returns None) and that another key closes the picker — done when: the new test passes
- [x] T-090 [tests] [P2] tui/state.rs::on_help_key is untested. Scrolling must clamp at `help_scroll_max`, PageUp must stop at 0, and any other key closes — done when: the new test passes
- [x] T-091 [tests] [P2] server.rs::heartbeat_error is untested. Cover a timeout and a refused connection against a closed local port — done when: the new test passes
- [x] T-092 [tests] [P2] config.rs::key_origin is untested on its own. Cover path and query dropped, case folded, an explicit default port, and the no-host fallback — done when: the new test passes
- [x] T-093 [tests] [P2] provider.rs::is_retryable_send_error is untested. Cover connect error → true, and a body/decode error → false — done when: the new test passes
- [x] T-094 [tests] [P2] server.rs::AppState::forget_session has no test that every per-chat map is cleared (modes, stats, ponytail levels, auto_approve, FROZEN_LESSONS, COMPACT_FAILED_AT) — done when: the test fills each map and checks it is empty afterwards
- [x] T-095 [tests] [P2] db.rs::Db::delete_messages_from is only covered indirectly through rewind_chat. Test two messages with the same `created_at` (the rowid tie-break) and an id from another session (error) — done when: the new test passes
- [x] T-096 [tests] [P2] db.rs::Db::usage_totals: test the `session` and `days` filters (an entry 8 days old is left out of `days = 7`) — done when: the new test passes

## 8. TUI UX polish

- [x] T-097 [tui] [P2] The `/sessions` picker (tui/state.rs::on_sessions_key) only knows Up/Down/Enter. Add Home/End/PageUp/PageDown — done when: a test moves the selection to both ends and by pages
- [x] T-098 [tui] [P3] In tui/state.rs::on_sessions_key, any key other than Up/Down/Enter closes the picker, so a typed letter is lost. Close only on Esc (and Ctrl+C quits as before) and ignore other keys — done when: a test shows a letter key leaves the picker open
- [x] T-099 [tui] [P2] The approval overlay can scroll into blank space: tui/draw.rs::draw_approval clamps `approval_scroll` to the number of lines, not lines minus visible height. Store a max as `help_scroll_max` does and clamp in state.rs — done when: a render test shows the last line stays at the bottom when scrolling past the end
- [x] T-100 [tui] [P3] The header shows the full session id (tui/draw.rs::draw, `session {session}`), which pushes the right-hand facts off narrow terminals. Show a short id (first 12 chars, as server.rs::short_id does) — done when: a TestBackend render at 80 columns shows the api status
- [x] T-101 [tui] [P2] The TUI drops the heartbeat `detail` (tui/state.rs::on_event `Heartbeat { ok, latency_ms, .. }`). Once T-054 removes the stderr line, an offline provider gives no reason. Push one System line with the detail when `ok` changes — done when: a test sends ok→false→true and finds two lines, with no line for unchanged beats
- [x] T-102 [tui] [P2] When the TUI event forwarder lags (main.rs::tui_run, `RecvError::Lagged`), it re-sends approvals but never tells the user that streamed text was skipped. Push a System note "[N events skipped — /resume to reload this chat]" — done when: the Lagged arm sends a note event; a test with a tiny channel shows it

## 9. Desktop GUI UX (src/gui.html unless noted)

- [x] T-103 [gui] [P2] Icon-only buttons are named by their glyph ("×", "→", "□"), because a button's text content takes precedence over its title: titlebar `.tb` min/max/close, `#nav-back`, `#nav-fwd`, `#nav-reload`, `#nav-go`, `#close-browser-btn`, `#hist-close`, `#chg-close`, `#set-close`. Add `aria-label`s — done when: every one has an aria-label; a tests/gui assertion checks them
- [x] T-104 [gui] [P2] Inputs named only by a placeholder or not at all: `#input` (composer), `#url`, `#side-search`, `#hist-search`, `#term-input`, `#git-new-name`. Add `aria-label`s — done when: every input/textarea outside Settings has a label or aria-label (tests/gui assertion)
- [x] T-105 [gui] [P2] The transcript `#log` has no live-region role, so screen readers never hear new replies. Add `role="log"` and `aria-relevant="additions"`, plus `aria-busy="true"` while a reply streams (set where `busyNow` is updated) — done when: the attributes are present and toggle with busy (tests/gui assertion)
- [x] T-106 [gui] [P2] The connection status `#api` / `#apitxt` changes to offline silently. Add `role="status"` — done when: the attribute is present
- [x] T-107 [gui] [P2] Closing History, Changes, Settings or Usage leaves focus on `<body>` (closeHistory / closeChanges / closeSettings / closeUsage only hide the box). Remember `document.activeElement` on open and restore it on close — done when: a tests/gui check opens Settings from `#settings-btn`, presses Esc, and finds focus back on the button
- [x] T-108 [gui] [P2] The `role="dialog" aria-modal="true"` cards (`.hist-card`, `#ap-card`) do not keep focus inside, so Tab walks into the page behind. Add a small focus trap for the open dialog (Tab and Shift+Tab wrap) — done when: tabbing from the last control of an open dialog lands on its first control
- [x] T-109 [gui] [P2] The Git menu: `#git-chip` has `aria-haspopup="menu"` but `aria-expanded` is never set, and `.git-item` buttons have no `role="menuitem"`. Set both when the menu opens and closes (git-chip click handler ~6015) — done when: aria-expanded follows `gitMenu.hidden`
- [x] T-110 [gui] [P2] The Git menu only closes on Escape from `#git-new-name`, and arrow keys do nothing. Handle Escape on `#git-menu` (close and focus `#git-chip`) and ArrowUp/ArrowDown between items; focus the first item on open — done when: the menu can be driven by keyboard alone
- [x] T-111 [gui] [P2] Sidebar chat rows (gui.html `chatRow`, `role="button"`) open on Enter but not Space, and the active chat is shown only by the `.active` class. Handle Space, and set `aria-current="page"` on the active row — done when: Space opens a chat and the active row has aria-current
- [x] T-112 [gui] [P3] The chat-row status dot (`.st.needs` / `.st.run` in `chatRow`) only has a `title`, so screen readers say nothing. Add visually-hidden text or an `aria-label` such as "waiting for approval" / "working" — done when: the row's accessible name includes the state
- [x] T-113 [gui] [P3] The Changes dialog shows file status as one letter (`renderChanges`: N/A/D/R/M). Add an `aria-label`/title such as "new", "added", "deleted", "renamed", "modified" — done when: each status span has the full word
- [x] T-114 [gui] [P2] The Changes button and `#chg-refresh` give no feedback until Rust answers (git diff can take seconds). Show "Loading changes…" in `#chg-sub` and disable Refresh until `renderChanges` runs — done when: the loading text shows between click and render (tests/gui stub)
- [x] T-115 [gui] [P3] Usage opens only when its data arrives (`window.showUsage` un-hides `#usage`; `openUsage` only posts `load_usage`), so a click seems to do nothing. Open the dialog at once with "Loading…" — done when: `#usage` is visible right after the click
- [x] T-116 [gui] [P3] The approval dialog `#ap-card` uses `aria-labelledby="ap-title"` only, so the command being approved is not read. Add `aria-describedby="ap-detail"` — done when: the attribute is present
- [x] T-117 [gui] [P2] Light-theme contrast: `--text-dim: #67858a` on `--bg: #f3faf9` is about 3.7:1 (below WCAG AA 4.5:1) and is used in 42 places for text. Darken it (e.g. `#536f75`, about 5:1) — done when: the light-theme `--text-dim` is at least 4.5:1 on `--bg` and `--surface`
- [x] T-118 [gui] [P2] Dark-theme contrast: `--text-dim: #5b7b81` on `--bg: #071114` is about 4.2:1. Lighten it (e.g. `#66878d`, about 4.9:1) — done when: the dark-theme `--text-dim` is at least 4.5:1 on `--bg`
- [x] T-119 [gui] [P3] `.side-search input { outline: 0 }`, and the only focus cue is `.side-search:focus-within` changing the background to 7% white. Add an accent border or box-shadow as `#comp:focus-within` has — done when: the focused search box shows a visible ring in both themes
- [x] T-120 [gui] [P3] There are two `@media (max-width: 900px)` rules for `#empty .chips` (~2201 sets one column; ~2360 overrides it with two). Delete the dead one — done when: only one rule is left and the 900 px layout looks unchanged
- [x] T-121 [gui] [P2] The Settings tabs (`.set-nav` `role="tab"`) have no ArrowLeft/ArrowRight/Home/End handling, no roving tabindex and no `aria-controls`, while the effort radio group in the same dialog already does arrows. Add them to `showSettingsPane` and its wiring — done when: arrow keys move between tabs and show the matching pane
- [x] T-122 [gui] [P3] Terminal tabs (`addTab` in the term render, `role="tab"`) never set `aria-selected`; only the `.on` class marks the current one. Set `aria-selected` — done when: exactly one term tab has `aria-selected="true"`
- [x] T-123 [gui] [P3] The `/` and `@` autocomplete (`#ac-menu role="listbox"`, items `role="option"`) is not linked to the composer. Give `#input` `aria-controls="ac-menu"`, `aria-expanded`, and `aria-activedescendant` pointing at the picked item (items need ids) — done when: the attributes follow `ac.pick` while the menu is open

## 10. Docs

- [x] T-124 [docs] [P3] README "Use" table lacks `--timeout N` (headless limit, exit 124) and `OPENCODE_RUST_HEADLESS_TIMEOUT` (main.rs::print_help, ::headless_timeout), and the `preview` sub-options `--title` / `--exit-after-ms` — done when: README lists them with the same defaults as print_help
- [x] T-125 [docs] [P3] README has no environment-variable list. Add a table: `OPENCODE_RUST_CONFIG`, `_API_KEY`, `_BASE_URL`, `_MODEL`, `_API_TOKEN`, `_AUTO_APPROVE`, `_AUTO_DENY`, `_HEADLESS_TIMEOUT`, `_SHELL` (tools.rs::shell_kind), `_BROWSER` (pagecheck.rs::find_browser), `XDG_CONFIG_HOME` — done when: every `OPENCODE_RUST_*` name grep finds in src (except the test-only `_PREVIEW_SMOKE`) is in README
- [x] T-126 [docs] [P3] README "Permissions and safety" says webfetch asks first, but config.rs `PermissionsCfg::default` allows `webfetch`, `check_page`, `ask_user`, `todo`, `spawn`, `agent` and the read-only browser tools. Fix the text and the Features tool list (add `check_page`, `ask_user`, `spawn`/`agent`, `browser_*`) — done when: README matches `PermissionsCfg::default`
- [x] T-127 [docs] [P3] The README command table lacks `/ponytail [lite|full|ultra|off]` and `/usage`, which commands.rs::builtins has — done when: every builtin in commands.rs::builtins appears in the README table
- [x] T-128 [docs] [P3] README ("MCP servers (stdio)") and docs/mcp-servers.md ("talks to it over stdin/stdout") do not mention Streamable HTTP servers (`url`, `headers` in config.rs::McpServerCfg; mcp_http.rs). Add a short HTTP example — done when: both documents show a `url = "https://…"` entry with a `headers` table
- [x] T-129 [docs] [P3] config.example.toml lacks `[server] port` (config.rs::ServerCfg) and the whole `[browser]` section (`enabled`, `home_url`, `allow_remote`; `profile_dir` marked as not used yet) — done when: every field of ServerCfg and BrowserCfg appears, commented, with its default
- [x] T-130 [docs] [P3] config.example.toml lacks `provider.images`, `permissions.approval_timeout_secs`, `agent.auto_verify` and `agent.check_pages`, and its `[permissions.defaults]` block omits the other built-in defaults (`check_page`, `ask_user`, `todo`, `spawn`, `agent`, `browser_*`) — done when: every field of ProviderCfg, PermissionsCfg and AgentCfg except the internal `api_key_origin`/`key_withheld_for` appears
- [x] T-131 [docs] [P3] README mentions only "sessions, snapshots and the crash log". List the state-dir files (opencode.db, core.log, cache.log, crash.log, the API token file, worker-patches/, snaps_*) and that the config path honours `XDG_CONFIG_HOME` (config.rs::default_config_path) — done when: README has the list
- [x] T-132 [docs] [P3] Custom subagent roles are undocumented: agents.rs::load_subagents reads `~/.config/opencode-rust/agents/*.md`, `.claude/agents/` and `.opencode/agents/`; project roles without `tools:` are read-only and get a `project-` prefix on a name clash. Add an "Extending" bullet — done when: README describes the three folders and the read-only default
- [x] T-133 [docs] [P3] Stale comments: main.rs::run says "--gui: the desktop app (egui shell)" (egui was dropped for wry/WebView2), and config.rs::Config::load has `let _ = workspace; // reserved: project-level .opencode-rust.toml in M2` — done when: both comments match the code
- [x] T-134 [docs] [P3] GOALS.md "Appendix A — IPC schema (target state)" lists 9 page→Rust verbs, while gui.rs::parse_ipc handles many more (rewind, new_branch, load_changes, set_price, term_run…). Replace the table with a pointer to parse_ipc and its tests, or update it — done when: the appendix no longer contradicts parse_ipc

## 11. Dependency and tooling hygiene

- [x] T-135 [tooling] [P3] Cargo.toml depends on `tokio-stream`, but nothing in src/, examples/ or tests/ uses it. Remove it — done when: `cargo build --locked` and `cargo test` pass without it (Cargo.lock updated)
- [x] T-136 [tooling] [P3] Cargo.toml enables tokio's `signal` feature, but there is no `tokio::signal` or `ctrl_c` in the code. Drop the feature — done when: the build and tests pass without it
- [x] T-137 [tooling] [P3] Cargo.toml has no `rust-version`, though the code needs a recent toolchain (`File::try_lock` in gui.rs and pagecheck.rs: Rust 1.89; `Option::is_none_or`: 1.82). Declare it — done when: `rust-version = "1.89"` (or whatever `cargo +<ver> check` proves) is set
- [x] T-138 [tooling] [P3] CI clippy runs only `--no-default-features` on Linux, so gui.rs, preview.rs and browser.rs (gui feature) are never linted. Add a Windows clippy step with default features to .github/workflows/ci.yml — done when: the workflow has the step and it passes on a run
- [x] T-139 [tooling] [P3] INIT_TIMEOUT and CALL_TIMEOUT are defined twice (mcp.rs:22-23 and mcp_http.rs:19-21, plus LIST_TIMEOUT in both). Keep one set in mcp.rs and import it in mcp_http.rs — done when: each constant exists once
- [x] T-140 [tooling] [P3] There is no formatting check. Run `cargo fmt --check`; if it is clean, add it as a CI step in ci.yml, otherwise format in one dedicated commit (after the in-flight splits land) and then add the step — done when: CI runs `cargo fmt --check`

---

## Tasks that touch the same files (do not run these in parallel)

The in-flight splits of server.rs, tools.rs, gui.rs, main.rs, tui/draw.rs and tui/state.rs
(on_key) touch the same code as every task below that lists those files, so schedule those
tasks after the matching split lands.

| File | Tasks |
|---|---|
| src/server.rs | T-001 T-002 T-003 T-004 T-005 T-006 T-007 T-008 T-009 T-010 T-011 T-028 T-049 T-051 T-054 T-055 T-058 T-063 T-064 T-071 T-091 T-094 |
| src/server/mission_run.rs | T-012 T-013 T-014 T-060 |
| src/server/deep.rs | T-015 T-023 |
| src/gui.rs | T-019 T-024 T-026 T-028 T-042 T-043 T-044 T-052 T-057 T-066 T-067 T-068 T-069 T-070 |
| src/gui.html (+ tests/gui) | T-052 T-103 T-104 T-105 T-106 T-107 T-108 T-109 T-110 T-111 T-112 T-113 T-114 T-115 T-116 T-117 T-118 T-119 T-120 T-121 T-122 T-123 |
| src/main.rs | T-018 T-031 T-102 T-133 |
| src/tui/mod.rs | T-061 T-062 |
| src/tui/state.rs | T-097 T-098 T-099 T-101 |
| src/tui/draw.rs | T-099 T-100 |
| src/tui/tests.rs | T-086 T-087 T-088 T-089 T-090 (and the tests of T-097..T-101) |
| src/tools.rs | T-035 T-047 T-069 T-073 T-074 T-075 T-076 T-077 |
| src/todo.rs | T-028 T-029 T-030 T-085 |
| src/db.rs | T-010 (if done as a CTE) T-027 T-095 T-096 |
| src/config.rs | T-032 T-033 T-034 T-048 T-092 T-133 |
| src/attachments.rs | T-033 T-036 T-079 |
| src/mcp.rs | T-020 T-037 T-038 T-039 T-053 T-056 T-081 T-139 |
| src/mcp_http.rs | T-038 T-039 T-139 |
| src/provider.rs | T-040 T-041 T-049 T-050 T-093 |
| src/gitsnap.rs | T-020 T-046 |
| src/permissions.rs | T-021 T-078 |
| src/subagent.rs | T-017 T-025 T-072 |
| src/team.rs | T-016 |
| src/worktree.rs | T-022 |
| src/pagecheck.rs | T-045 T-065 T-080 |
| src/agents.rs | T-059 |
| src/usage.rs / keepbest.rs / changes.rs | T-082 / T-083 / T-084 (independent) |
| README.md | T-124 T-125 T-126 T-127 T-128 T-131 T-132 |
| docs/mcp-servers.md | T-128 |
| config.example.toml | T-129 T-130 |
| GOALS.md | T-134 |
| Cargo.toml (+ Cargo.lock) | T-135 T-136 T-137 |
| .github/workflows/ci.yml | T-138 T-140 |

Ordering notes (dependencies rather than file clashes):
- Do T-054 (stop stderr in the TUI) together with or before T-101 (show the heartbeat detail in the TUI); otherwise the TUI loses its only offline reason.
- T-029 before T-085. T-047 together with T-077. T-020 before T-081.
- T-028 needs T-026's forget_session call site in the same gui.rs handler; do them one after the other.
- T-061 and T-062 both rewrite tui/mod.rs::run; do T-061 first.
- T-024, T-025 and T-072 all assume `server::signal_abort` / `server::aborted`; keep their names stable during the server.rs split.

## 12. Found while working the backlog
- [x] T-141 [gui] [P1] tests/gui/run.sh: the browser runtime suite fails "waitFor posts a success frame" and "waitFor reports a timeout" (pre-existing, from browser.rs RUNTIME_JS) — done when: both pass and run.sh exits 0 (or 2 with no browser)
- [x] T-142 [gui] [P3] Opening Changes from the git menu returns focus to <body> on close, because the menu item is hidden by then; return it to #git-chip instead — done when: a panel.js assertion covers it
- [x] T-143 [backend] [P2] gui.rs gui_command duplicates mission start, undo/redo and turbo/team handling that src/bin_cli/shared.rs now shares; move the shared helpers into the lib (e.g. server or commands) and use them from all three frontends — done when: one copy remains and TUI/headless/GUI tests pass
- [x] T-144 [tests] [P2] Timing flakes under machine load: mcp::b19_startup_tests::servers_start_at_the_same_time and server::tests::approval_resolved_reports_every_outcome fail when the machine is busy, pass alone. Make them independent of wall-clock speed (wider margins or event-driven waits) — done when: both pass 10/10 while another cargo build runs
- [x] T-145 [tests] [P3] T-056/T-059 tests read the real default_state_dir()/core.log; point core_log at a temp dir in tests so they don't touch the user's log — done when: the tests pass with a fresh temp state dir and leave the real core.log untouched
- [x] T-146 [db-async] [P2] Finish T-009: add `Db::approvals(workspace) -> HashSet<(String,String)>` in db.rs and make tool_exec.rs `decided_without_prompt` do one query per tool call — done when: one approvals query per tool call (test with a counter or code check)
- [x] T-147 [db-async] [P2] Finish T-014: make server/mission_run.rs::mission_report async over Db::run and update its callers in bin_cli/headless.rs, bin_cli/tui_run.rs and gui/slash.rs — done when: no sync DB call in mission_report; mission tests pass
- [x] T-148 [db-async] [P2] Remaining sync DB calls on async paths: run.rs::finish_run, verify.rs::run_review add_message, agent_loop.rs::rewind_chat (called off-thread from GUI now, check TUI/HTTP callers), deep.rs::run_attempt worker.db calls, and inline `workspace_for` lookups outside Db::run — done when: each moves through Db::run or is documented as already on a blocking thread
- [x] T-149 [backend] [P1] T-011 added an AppState.workspaces cache (chat → folder). The GUI must call `st.forget_workspaces()` after `set_session_project` (MoveChat in gui/dispatch.rs or gui/chats.rs, and the call near gui.rs:239), after `adopt_unfiled_sessions` and after `remove_project` — otherwise a moved chat keeps running in its old folder. Check HTTP/TUI paths that change a chat's project too — done when: a test moves a chat and workspace_for returns the new folder
- [x] T-150 [db-async] [P2] Finish T-007: switch agent_loop.rs (~777), deep.rs (~317) and compact.rs (~145) to the awaited `record_usage_as_async`, then delete the sync `record_usage_as` — done when: no sync usage write remains
- [x] T-151 [db-async] [P3] Finish T-010: make `perm_mode_for` async over Db::run (callers in tool_exec, deep, run, agent_loop) — done when: auto_approved reads the parent chain in one Db::run when uncached
- [x] T-152 [perf] [P3] T-064 (skipped): log_cache now lives in server/agent_loop.rs; keep its file handle open like core_log — done when: one open per process
