# MCP servers

opencode-rust starts each server in `[[mcp.servers]]` (see `config.example.toml`)
as a separate program and talks to it over stdin/stdout. A server's tools reach
the model as `mcp__<name>__<tool>`. Calls to MCP tools go through the normal
permission prompt, because they are unknown tools ("ask" by default). To allow
one without asking, add for example `"mcp__context7__query-docs" = "allow"`
under `[permissions.defaults]`.

## Context7 (library documentation)

- Install Node.js. Windows: `command = "cmd"`, `args = ["/c", "npx", "-y", "@upstash/context7-mcp"]`.
  macOS/Linux: `command = "npx"`, `args = ["-y", "@upstash/context7-mcp"]`.
- Optional API key (higher rate limits): set the environment variable
  `CONTEXT7_API_KEY` before starting opencode-rust. MCP servers inherit
  opencode-rust's environment, so the server sees it.
- Tools: `resolve-library-id` (find the library), `query-docs` (ask its docs).

## ast-grep (structural code search)

- Install the ast-grep CLI and uv. Then `command = "uvx"`,
  `args = ["--from", "git+https://github.com/ast-grep/ast-grep-mcp", "ast-grep-server"]`.
- Tools: `find_code`, `find_code_by_rule`, `dump_syntax_tree`, `test_match_code_rule`.

## First start

Run the exact command once in a terminal before adding it to the config. The
first run downloads packages; if that takes longer than 20 seconds, opencode-rust
gives up on the server for that session (the log says `[mcp] failed to start`).

## Serena

Serena is licensed GPL-3.0-or-later. Use it only as a separate MCP server
process configured in `[[mcp.servers]]` (follow Serena's own README for the
command). Never copy Serena code into this MIT-licensed repository.

## Skill

`.opencode/skills/code-research/SKILL.md` tells the model when to use these
tools. To use it in every project, copy the folder to
`%USERPROFILE%\.config\opencode-rust\skills\` (Windows) or
`~/.config/opencode-rust/skills/` (macOS/Linux).
