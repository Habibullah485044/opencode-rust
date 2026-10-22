# M6 — Control & Discoverability: Completion Report

GRADES spec=8 design=8 correctness=8 quality=8; biggest gap: palette/overlays are
logic- and TestBackend-tested, not driven interactively in a live terminal

## Why this milestone

Reviewing the project against its own roadmap (plan §2 feature matrix), everything
M1–M5 shipped was *capability*: tools, team orchestration, missions, turbo, skills,
snapshots, inline edit. What was missing was the other half of a tool people live in
— **control and discovery**. There were 17 builtin commands and the only way to learn
them was one hardcoded footer line; there was no way to see or switch sessions, change
the model, or get anything out of the session.

## Done

| Item | Where | Verified |
|---|---|---|
| **Command registry** — one source of truth for every verb's name, args and purpose | `commands::builtins()`, `Builtin`, `help_text()` | unit tests: alphabetical, unique, documented, and *every registered verb parses* (registry can't drift from `classify`) |
| **Command palette** — typing `/` lists commands filtered by prefix; ↑/↓ select, Tab completes, Enter runs the typed name outright or a unique zero-arg match, else completes for args | `tui.rs` (`Palette`, `palette_matches`, `refresh_palette`, `complete_palette`) | 5 unit tests + TestBackend render test asserting the block renders above the input |
| **`/help` overlay** (also **F1**) — full command + key reference | `commands::help_text()`, help overlay in `draw` | open/close test + render test asserting every command appears |
| **`/sessions` browser** — newest-first list, `↳` marks a fork, Enter switches | `ServerEvent::SessionList`, `SessionInfo`, `AppState::session_list`, `SessionsOverlay` | picker test (selection + carried id + close) + server test on a real DB |
| **`/model [id]`** — shows the active model and what the endpoint advertises, or switches it for every later call | `provider::Client::{set_model,list_models}`, `AppState::{current_model,set_model,available_models}`, `ServerEvent::ModelInfo`, header span | 3 tests: switch round-trip, graceful degradation when the endpoint has no listing, and both listing shapes (OpenAI `data[].id`, Ollama `models[].name`) |
| **`/export [path]`** — standalone markdown transcript | `AppState::transcript_markdown`, wired in `main.rs` | server test on real messages + live binary E2E (default and relative path) |
| **`/todos`** — the session's todo list with open counts | `AppState::todos_report` | server test: empty state, then `1 open / 2 total` with ✓/○ markers |
| **`/init`** — writes a starter `AGENTS.md`, never clobbers a real one | `skills::{STARTER_AGENTS_MD,init_agents_md}` | unit test (write → refuse → empty file still fills) + live binary E2E |
| **`/clear`**, **`/new`** — drop the view (history kept) / start a fresh session | `tui::clear_view`, `InputAction::{NewSession,SwitchSession}` | clear test asserts the session id survives |
| Help text and `--help` updated so the CLI advertises all of it | `main::print_help` | live `--help` |

All new commands work **headless** too (the only exception is the inherently
interactive pair, which say so and exit non-zero): `/help`, `/sessions`, `/todos`,
`/model`, `/export`, `/init`, and honest no-op messages for `/clear` / `/new`.

## Verification

- `cargo test`: **140 passed, 0 failed** (121 → 140; 19 new).
- `cargo build` / `cargo build --tests`: **0 warnings**.
- Live E2E through the debug binary: `/help` prints the full reference; `/init` writes
  then refuses to overwrite; `/todos` reports the honest empty state; `/model` reports
  the active model and degrades to "endpoint listed no models"; `/model stub-v2`
  switches; `/export` and `/export out/run.md` write real files into the workspace;
  `/sessions` lists stored sessions; `/clear` and `/new` explain themselves.
- **Regression checked**: an unknown slash line (`/deploy staging`) still reaches the
  agent as a prompt rather than being swallowed by the palette.

## Bugs found and fixed

- Enter initially *completed* an already-complete command name, so `/diff` or `/turbo`
  alone could no longer be run — two pre-existing tests caught it. Rule is now: typed
  name wins; completion is only for prefixes (unique zero-arg completes **and** runs,
  ambiguous or arg-taking just completes).

## Limitations

- The palette, help and sessions overlays are covered by logic tests plus `TestBackend`
  render assertions — not by driving a real terminal.
- `/model` discovery depends on the endpoint implementing `GET /models`; when it does
  not (or is unreachable) the list is empty and `/model <id>` is still the way to switch.
- `/export` writes into the workspace on purpose (that is usually where you want it) —
  pass a path if you want it elsewhere.
- Todos remain in-memory per session (unchanged from M5): `/todos` reflects this run,
  not a restart.
