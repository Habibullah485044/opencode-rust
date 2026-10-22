# Plan progress

Tasks from [FIX_PLAN.md](FIX_PLAN.md), done in the recommended order (Part 2.1).

- [x] B-17 make the `..\evil.rs` test assertion correct on every OS — 2026-10-22 — cargo test: 185 passed
- [x] B-01 `edit` tool: reject an empty `old_str` — 2026-10-22 — cargo test: 186 passed
- [x] B-02 `/edit` must not cut Markdown files or drop the last newline — 2026-10-22 — cargo test: 189 passed
- [x] S-02 Headless mode must not approve tool calls by default — 2026-10-22 — cargo test: 189 passed (3 new); e2e: deny/--yes/--yes+auto-deny/auto-approve checked against the mock provider
- [x] S-05 Keep the provider API key out of child processes — 2026-10-22 — cargo test: 191 passed (2 new); bash/MCP/git children no longer inherit OPENCODE_RUST_API_KEY or OPENCODE_RUST_API_TOKEN
