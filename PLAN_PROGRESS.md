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
