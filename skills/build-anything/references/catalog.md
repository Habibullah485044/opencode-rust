# Bundled skills: when to open which

These come from two open-source projects and are kept as published (see each
folder's LICENSE, NOTICE and SOURCE.txt). Open a file only when its row matches
what you are doing. They were written for other agents (MiniMax Code, Claude
Code, Codex); where they name another agent's tool, use ours:

| Their tool | Ours |
|---|---|
| Bash / run_terminal_command | `bash` (`background: true` for servers and watchers) |
| Read / read_files | `read` (`outline: true` for big files) |
| Edit / str_replace / Write | `edit`, `write` |
| Glob / Grep / code_search | `glob`, `grep` |
| Task / spawn_agents / subagent | `spawn`, `agent` |
| TodoWrite / write_todos | `todo` |
| WebFetch / web_search | `webfetch` (`url` or `query`) |

## MCode Stack (`mcode-stack/`, MIT; engineering-* adapted from Google Engineering Practices, CC-BY 3.0)

| When | Open |
|---|---|
| Reviewing a diff or a finished change | `mcode-stack/engineering-code-review/SKILL.md` |
| A second opinion from a clean context after you implement | `mcode-stack/independent-review/SKILL.md` |
| Writing review comments for someone else's change | `mcode-stack/engineering-review-comments/SKILL.md` |
| Answering review comments on your change | `mcode-stack/engineering-review-feedback/SKILL.md` |
| A change is too big; splitting into small PRs or steps | `mcode-stack/engineering-small-prs/SKILL.md` |
| Writing a commit message or PR description | `mcode-stack/engineering-change-descriptions/SKILL.md` |
| A hotfix or emergency change under time pressure | `mcode-stack/engineering-emergency-changes/SKILL.md` |
| Deciding whether to `spawn` a subagent or do it yourself | `mcode-stack/subagent-split-check/SKILL.md` (+ `checklists/subagent-split.md`) |
| A command that takes minutes (builds, test suites, servers) | `mcode-stack/long-command-check/SKILL.md` (+ `checklists/long-command.md`) |
| A long session: what to keep, compact, or hand off | `mcode-stack/long-session-check/SKILL.md` (+ `checklists/long-session.md`) |
| Handing the task to a new session or person | `mcode-stack/handoff-doc/SKILL.md` |
| Looking back at how a run went, to make the next one cheaper | `mcode-stack/trajectory-review/SKILL.md` |
| Too many tools or too much context per request | `mcode-stack/tool-budget-audit/SKILL.md`, `mcode-stack/context-tax-audit/SKILL.md` |
| Changing an agent harness and measuring the effect | `mcode-stack/harness-experiment/SKILL.md` |
| The user keeps doing a manual step for the agent | `mcode-stack/lifting-loops/SKILL.md` |
| Writing or rewriting posts/content (Chinese-focused) | `mcode-stack/making-content/SKILL.md` |

Do not run the scripts inside these folders unless the skill's own steps call for
them and the user agrees.

## REA, Reverse Engineer Anything (`rea/`, MIT)

Open `rea/SKILL.md` when a question depends on **how shipped software works
inside**: a binary or desktop app, an Electron app, a browser extension, an
Android app, a packaged or minified web bundle, firmware, or what a live site
does at runtime. Not for code whose full source is already in the project: read
the source instead.

- REA works through its MCP server. The skill alone adds no tools. If no tools
  starting with `mcp__rea__` are in your list, tell the user it needs installing
  (`npx rea-agents setup`, from the official morluto/rea project) and continue
  with ordinary tools where you can.
- Only for software the user owns or is allowed to analyse, and never to get
  around licensing, DRM, paywalls or access controls.
- Fits the method: "understand how X works" becomes subtasks
  (`inspect_artifact` → `find_feature` → `trace_behaviour` → `explain`), each
  with REA evidence as its check. The reference files in `rea/references/` cover
  each kind of target.
