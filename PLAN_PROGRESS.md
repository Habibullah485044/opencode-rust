# Plan progress

Tasks from [FIX_PLAN.md](FIX_PLAN.md), done in the recommended order (Part 2.1).

- [x] B-17 make the `..\evil.rs` test assertion correct on every OS — 2026-10-22 — cargo test: 185 passed
- [x] B-01 `edit` tool: reject an empty `old_str` — 2026-10-22 — cargo test: 186 passed
- [x] B-02 `/edit` must not cut Markdown files or drop the last newline — 2026-10-22 — cargo test: 189 passed
- [x] S-02 Headless mode must not approve tool calls by default — 2026-10-22 — cargo test: 189 passed (3 new); e2e: deny/--yes/--yes+auto-deny/auto-approve checked against the mock provider
- [x] S-05 Keep the provider API key out of child processes — 2026-10-22 — cargo test: 191 passed (2 new); bash/MCP/git children no longer inherit OPENCODE_RUST_API_KEY or OPENCODE_RUST_API_TOKEN
- [x] S-10 GUI `is_local_url`: parse URLs properly — 2026-10-22 — cargo test: 193 passed (2 new); the host check now uses `reqwest::Url`, so userinfo forms cannot smuggle a remote host past the dev-server-only mode
- [x] S-03 Keep file tools inside the workspace — 2026-10-22 — cargo test: 196 passed (3 new); `read`/`write`/`edit`/`list`/`glob`/`grep` now go through `confine_path` (no `..`, no symlink escape, no absolute paths outside the workspace)
- [x] S-04 Canonical path details for permission rules — 2026-10-22 — cargo test: 200 passed (4 new); rule details now resolve `.`/`..`, follow symlinks, mark outside paths with `outside:` and fold case on Windows
- [x] S-06 webfetch: public addresses only, checked redirects, size cap — 2026-10-22 — cargo test: 204 passed (4 new); every hop is resolved and checked, the request is pinned to the checked addresses, redirects are followed by hand (max 5) and the body stops at 2 MB
- [x] S-07 Scope "Always allow" to the workspace, stop duplicate approvals — 2026-10-22 — cargo test: 208 passed (4 new); approvals are keyed by the canonical workspace path, the table has a UNIQUE constraint, and old unscoped grants are moved aside
- [x] S-01 Require a token on the local HTTP API — 2026-10-22 — cargo test: 210 passed (2 new); every route except `/api/health` needs `Authorization: Bearer <token>` and a loopback `Host` header; the token comes from `$OPENCODE_RUST_API_TOKEN` or a fresh random one in `<state_dir>/api-token`
- [x] H-02 Make the desktop GUI an optional cargo feature — 2026-10-22 — cargo test: 210 passed (default) / 185 passed (`--no-default-features`); `tao`/`wry` are optional, `detect_dev_server` moved to `src/browser.rs`
- [x] H-01 Add README.md, LICENSE and NOTICE — 2026-10-22 — no Rust change; ponytail LICENSE downloaded from upstream (MIT, © DietrichGebert), `vendor/` added to `.gitignore`
- [x] H-03 Add GitHub Actions CI (Linux + Windows) — 2026-10-22 — `.github/workflows/ci.yml` with the three jobs; `cargo build --locked --tests` passes locally, YAML parsed
- [x] B-21 Provider stream: error frames, early end, missing delta, ids — 2026-10-22 — cargo test: 216 passed (6 new); error frames fail the stream, an early close is an error, `finish_reason`/`[DONE]` end the reply, parallel tool calls that reuse `index` stay separate, `content` is always sent
- [x] B-03 Cut-off replies (`finish_reason: "length"`) are treated as complete — 2026-10-22 — cargo test: 219 passed (3 new); `one_shot` and the summarizer refuse a half answer, the agent loop marks a cut-off reply and never runs a call with broken arguments
- [x] F-01 Retry the provider request with backoff — 2026-10-22 — cargo test: 224 passed (5 new); `provider.max_retries` (default 3) retries connection errors and 429/500/502/503/504/529 with 1s/2s/4s backoff, honours `Retry-After` up to 30s, never retries 4xx
- [x] B-04 Repair chat history before sending it to the provider — 2026-10-22 — cargo test: 229 passed (5 new); `repair_history` answers dangling tool calls with an "interrupted" result, drops orphan results and empty assistant messages, keeps one system message and merges user messages
- [x] B-05 Compaction: safe split point, exact deletes, one transaction — 2026-10-22 — cargo test: 233 passed (4 new); `safe_split` never keeps a tail that starts with a tool result, `replace_with_summary` deletes exactly the summarized ids in one transaction (messages that arrive meanwhile survive), the summarizer gets `role: text` lines and keeps an earlier summary whole
- [x] B-10 New event `ApprovalResolved` — 2026-10-22 — cargo test: 234 passed (1 new); the waiting function announces exactly one `ApprovalResolved` per request with the reason (`user`/`always`/`timeout`/`abort`/`turbo`), and the TUI learns the new variant
