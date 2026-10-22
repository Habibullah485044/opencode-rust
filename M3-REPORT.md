# M3 — Borrows (inline edit, next-edit jump, team reviewer): Completion Report

## Done

| Item (plan §"M3 — borrows") | Where | Verified |
|---|---|---|
| Inline `/edit`: prompt → ONE completion (no agent loop) → diff preview → accept | `inline_edit.rs`, `Command::Edit`, TUI overlay (`y` apply / `n` reject), headless auto-apply | unit tests incl. mock-provider E2E + live binary E2E |
| `Tab` next-edit jump between accepted hunks (purely local heuristic, no model call) | `JumpTracker` in `inline_edit.rs` (sorted, deduped, cycle + wrap, Shift+Tab back) | unit tests + TUI tests |
| Team-mode refinement: **reviewer verifies** → synthesis | `team::review` (small-model `one_shot`), verdict embedded in synthesis | unit test + live binary E2E (`[team] reviewer: VERDICT: PASS`) |

Supporting work:
- `server::one_shot` — shared no-tools single completion; team planner refactored onto it.
- `ServerEvent::EditPreview` / `EditApplied` — preview & jump-site events.
- **Snapshot before every applied edit** (`pre-edit`), so `/undo` reverts inline edits too (live-verified).
- Fixed a real latent bug the new assertions exposed: the team test mock's raw-string SSE
  payloads contained literal `\n` and never parsed — the old test passed only via the
  single-subtask fallback. Mock now uses the `serde_json` + `format!` pattern.

## Verification

- `cargo test`: **86 passed, 0 failed** (12 new M3 tests).
- `cargo build --tests` / `--release`: **0 warnings**.
- Live E2E through the release binary against a scripted SSE provider:
  `/edit` printed the diff and applied it (`[edit] applied notes.txt:3`, RC=0) →
  `/undo` restored the file → `/team` ran planner → worker → **reviewer verdict** → synthesis.

## Deferred (per plan wording)

- **Separate approval window** — the plan scopes this to "if GUI chosen"; no GUI yet (M4).
- M4: Tauri shell, embedded preview browser, persistent terminals, IDE-style diff views.

## Limitations

- The TUI overlay (accept/reject keys) and Tab jump are unit-tested on their logic, not
  driven interactively in a live terminal.
- The plan-mode `/edit` gate lives in `main.rs` (in-process mode state), so it is not
  covered by headless E2E — each headless invocation is a fresh process.
