# M2 — Agent Layer: Completion Report

GRADES spec=5 design=7 correctness=7 quality=7; biggest gap: no remote/PR, so no CI checks ever ran

## Done (all 8 plan items + the two pass-target extras)

| Item | Where | Tests |
|---|---|---|
| Plan/build agent switch (system prompts + mode state) | `agents.rs`, `server.rs` state, `/plan` `/build` | unit tests + live headless runs |
| Skills loaded from `.md` files | `skills.rs`, `/skills` | unit tests + live run |
| Slash commands loaded from `.md` files | `commands.rs`, server-side expansion | unit tests + live run |
| AGENTS.md project-instruction loader | system-prompt composition in `server.rs` | unit tests |
| `/compact` context compaction | `compact.rs` | unit tests + live "nothing to compact" path |
| Session fork/branch/resume/diff on the fork-ready schema | `sessions.rs`, `/fork` `/resume` `/diff` | unit tests (diff identity, fork, truncation) |
| Git-snapshot undo (shell out to git) | `gitsnap.rs`, `/undo` `/redo` | unit tests + real-binary E2E (run → undo → redo) |
| Subagent spawn | `subagent.rs` (`spawn` tool, restricted toolset, parent/child isolation) | unit tests incl. permission gate |
| Minimal MCP stdio client | `mcp.rs` (parse/initialize/list/call, tool-name routing) | 6 tests incl. a real `node` stdio server |

## Verification

- `cargo test`: **74 passed, 0 failed** (M1: 31 → 43 new tests).
- `cargo build --tests`: **0 warnings** (also `--release`).
- Live E2E through the release binary in a **subdirectory** git workspace: agent run →
  `/undo` → `/redo`, files outside the workspace untouched; headless exit codes
  (`/plan` 0, `/skills` 0, `/undo` 0, `/compact` 1, `/resume` 2) all as designed.
- Bug found and fixed during E2E: git porcelain paths are repo-root-relative, so undo/redo
  now resolves them against the repo root (with 8.3-vs-long-path canonicalization on
  Windows) and skips paths outside the workspace; regression test added.

## Deferred (out of M2 scope per plan)

- M3: team-mode orchestrator refinements, inline `/edit` + next-edit jump, approval window.
- M4: Tauri GUI shell, preview browser, persistent terminals, IDE diff views.
- Plan-wide: Windows Job-object sandbox, size/startup CI measurement.

## Limitations

- The repo has **no git remote**, so no pull request or CI checks could be opened;
  all verification above is local (tests + live binary E2E).
