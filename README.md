# opencode-rust

A lightweight AI coding agent written in Rust. It talks to any
**OpenAI-compatible** chat endpoint (OpenRouter, OpenAI, Groq, Ollama, Gemini's
OpenAI endpoint, or a local mock) and works inside your project folder. It can
read, search and edit files and run commands. It asks before risky actions
unless you allow them.

Three ways to use it:

| Mode | Command |
|---|---|
| Terminal UI (default) | `opencode-rust` |
| One prompt, no UI | `opencode-rust --headless "fix the failing test"` |
| Desktop app with a browser panel (Windows, WebView2) | `opencode-rust --gui` |

## Features

- Tools for the model: `bash`, `read`, `write`, `edit`, `list`, `glob`, `grep`,
  `webfetch`, `todo`, tools from MCP servers (stdio), and browser tools in the
  desktop app.
- Permissions: allow / ask / deny per tool, pattern rules, approval prompts,
  "always allow".
- Plan mode (read-only) and build mode. Turbo mode (no prompts — use with care).
- Subagents (`explore`, `worker`), `/team` (planner → workers → reviewer → summary),
  `/mission` briefings.
- Sessions stored in SQLite: fork, resume, diff, export to Markdown, `/compact`.
- Git-snapshot `/undo` and `/redo` of the agent's changes.
- Inline `/edit` with a diff preview.
- Skills and slash-command templates from Markdown files; `AGENTS.md` project
  instructions.
- "Ponytail" lazy-senior-dev discipline and an automatic review after changes
  (see Credits).

## Build

You need Rust (stable) and git.

- **Windows:** `cargo build --release` → `target\release\opencode-rust.exe`.
  The desktop app needs the Microsoft Edge WebView2 Runtime (preinstalled on
  Windows 11 and most Windows 10).
- **Linux:** the default build includes the desktop window and needs GTK/WebKit:
  `sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev` (Ubuntu 24.04).
  Without them, build the terminal UI and headless mode only:
  `cargo build --release --no-default-features`.
- **macOS:** not tested.

## Configure

Copy `config.example.toml` to `~/.config/opencode-rust/config.toml`
(Windows: `C:\Users\<you>\.config\opencode-rust\config.toml`), or point the
environment variable `OPENCODE_RUST_CONFIG` at any file. The smallest config:

```toml
[provider]
base_url = "https://openrouter.ai/api/v1"
api_key = "your-key"
model = "openai/gpt-4o"
```

Environment variables override the file: `OPENCODE_RUST_API_KEY`,
`OPENCODE_RUST_BASE_URL`, `OPENCODE_RUST_MODEL`.

Sessions, snapshots and the crash log live in `%APPDATA%\opencode-rust`
(Windows) or `~/.local/share/opencode-rust` (Linux/macOS).

## Use

Run `opencode-rust --help` for all options. Common ones:

| Option | Meaning |
|---|---|
| `--headless "prompt"` | Run one prompt, print the result, exit |
| `--yes` | Headless only: approve tool calls without asking (default: deny) |
| `--gui` | Desktop app instead of the terminal UI |
| `--model NAME` / `--base-url URL` | Override the provider for this run |
| `--port N` | Port of the local API (default: random, loopback only) |
| `preview <url>` | Open a standalone preview window |

The local HTTP API needs a bearer token (`Authorization: Bearer <token>`); the
token comes from `$OPENCODE_RUST_API_TOKEN` or from a fresh random file printed
at start. Only `/api/health` is open, and only to requests addressed to
`127.0.0.1`.

Inside the terminal UI, type `/` for the command palette or press F1:

| Command | What it does |
|---|---|
| `/build`, `/plan` | Normal mode / read-only plan mode |
| `/clear`, `/new` | Clear the view (history kept) / start a fresh session |
| `/compact` | Summarize old history, keep the recent tail |
| `/debt` | Laziness Meter: lines added/deleted, reuse hints |
| `/diff [id\|n]`, `/fork`, `/resume <id\|n>`, `/sessions` | Work with sessions |
| `/edit <file> <what>` | Inline edit with a diff preview |
| `/export [path]` | Write the transcript to Markdown |
| `/help` | Command and key reference |
| `/init` | Create a starter `AGENTS.md` |
| `/mission [kind] <goal>` | Run a briefing (Explore / Commit / Merge PR / Custom) |
| `/model [id]` | Show or switch the model |
| `/deep [n] <task>` | Deep mode: try the task n times (default 3, at most 5), each in its own git worktree; every attempt is checked (tests/build, page check) and the best one is applied (`/undo` takes it back). The Deep button in the desktop app does the same for one message |
| `/parallel <task>` | Same as `/deep 2 <task>` |
| `/review` | Senior-dev review of the uncommitted changes |
| `/skill install <path\|url>`, `/skills` | Install / list skills |
| `/team <goal>` | Plan → parallel workers → review → summary |
| `/todos` | Show this session's todo list |
| `/turbo [prompt]` | Full access, no approvals (careful) |
| `/undo`, `/redo` | Restore / re-apply the last git snapshot |
| `/quit` | Exit |

## Permissions and safety

By default `read`, `list`, `glob` and `grep` are allowed; `bash`, `write`,
`edit` and `webfetch` ask first; unknown tools ask. Change this in
`[permissions.defaults]` and add rules:

```toml
[[permissions.rules]]
tool = "bash"
pattern = "cargo test*"
action = "allow"
```

File rules are matched against the path relative to the workspace (`src/main.rs`);
paths outside the workspace look like `outside:/abs/path`.

In headless mode nobody can answer a prompt — tool calls are denied unless you
pass `--yes` (or set `OPENCODE_RUST_AUTO_APPROVE=1`); `OPENCODE_RUST_AUTO_DENY=1`
always forces deny. Turbo mode skips prompts entirely; only `deny` rules still
apply.

"Always allow" answers belong to the workspace you made them in, not to every
project you ever open.

## Extending

- **Project instructions:** put an `AGENTS.md` in your project root (`/init` makes one).
- **Skills:** `.opencode/skills/<name>.md` or `.opencode/skills/<name>/SKILL.md`,
  with optional frontmatter (`name`, `description`). User-wide:
  `~/.config/opencode-rust/skills/`.
- **Command templates:** `.opencode/commands/<name>.md`; `$ARGUMENTS` is replaced
  by what you type after `/<name>`.
- **MCP servers:**

```toml
[[mcp.servers]]
name = "my-server"
command = "npx"
args = ["-y", "some-mcp-server"]
```

## Development

```
cargo build
cargo test
```

End-to-end runs without a real model: start the mock with
`cargo run --example mock_llm -- 18080`, then run opencode-rust with
`OPENCODE_RUST_BASE_URL=http://127.0.0.1:18080/v1` and `OPENCODE_RUST_MODEL=mock`.

Desktop page tests: `bash tests/gui/run.sh` (needs Microsoft Edge).

Plans and history: `GOALS.md`, `M2-REPORT.md` … `M6-REPORT.md`, `FIX_PLAN.md`.

## Credits and licenses

- opencode-rust is MIT-licensed — see `LICENSE`.
- The ponytail prompts (`src/prompts/ponytail*.md`, built into the system
  prompt) come from https://github.com/DietrichGebert/ponytail, MIT, © DietrichGebert —
  see `NOTICE` and `third_party/ponytail/LICENSE`.
- MiroFish (optional sidecar, AGPL-3.0) is **not** included; run it as a separate program.
