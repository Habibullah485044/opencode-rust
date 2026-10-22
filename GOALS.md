# GOALS — from a fake browser panel to a real one

Scope: `opencode-rust` desktop app (GUI frontend + Rust backend).

## Status — what is actually built (2026-10-21)

| Slice | State | Evidence |
|---|---|---|
| **A** panel is a real browser | **done** | child WebView2 loads a page: `Url → Loading(true) → Title("smoke-fixture") → Loading(false)`; fixture server logged the GET + favicon |
| A-F1 URL bar / paste | done | `#url` is a real `<input>`; 40→53 headless assertions |
| A-F2 nav controls | done | arrows/reload post `nav_back`/`nav_fwd`/`nav_reload`; disabled state from payload |
| A-F3 geometry | done | `ResizeObserver` + resize → `panel_rect`; collapse hides the native view immediately |
| A-F4 panel states | done | empty card, error card, loading spinner, no white flash |
| A-B1 child view | done | `build_as_child` + `set_bounds`/`set_visible`, hidden on collapse/error/modal |
| A-B2 IPC | done | `browse`, `nav_*`, `panel_rect`, `panel_visible`, `open_dev_server`, `approve`/`deny` |
| A-B3 navigation | done | URL/title/load handlers; `file:`/`javascript:`/`data:` refused with a reason |
| A-B4 context/config | done | `[browser] enabled, home_url, allow_remote, profile_dir`; shared context (no second browser process) |
| A-B5 dev servers | done | `bash` output scanned → `ServerEvent::DevServer` → chip/CTA target |
| **D1** approval UI | **done** | modal with tool/detail/countdown, Allow once / Always / Deny, `y`/`a`/`n`; live run auto-denied a `write` after the 2s timeout, no page errors |
| **B-B1** CDP bridge | **replaced** | no CDP dependency: results come back over the child view's IPC channel (`browser::Handle`). Fewer moving parts, same tool surface |
| **B-B2** tools | **partial** | `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_wait` (proven live: navigate returned the real page title) |
| B-B3 permissions | done | navigate/snapshot/wait `allow`; click/type `ask` → D1 modal |
| B-B4 snapshots | done | page-side pruning to interactive nodes + headings + text; refs (`e12`) invalid after navigation; hard 8k-char cap |
| B-B5 rails | partial | per-request deadlines, ref validation, no ports; identical-call loop guard not built |
| B-F1 activity | partial | tool cards in the transcript + the panel auto-opens when the agent navigates; no dedicated action log/cancel button |
| **C** MCP server / profiles / tabs / downloads / reader / history | **not started** | — |
| **D2** release build, **D3** crash hygiene | **not started** | — |

Verification in this tree: `cargo test` 180/180 · `gui.html` headless suite 53/53 ·
page-runtime suite 16/16 · live E2E navigate round trip.

Not verified: `browser_screenshot` (needs CDP `Page.captureScreenshot` and was not
built), wheel-scrolling inside the native view by a human, and multi-monitor DPI
changes (bounds are recomputed from `getBoundingClientRect`, which should cover it).

---

---

## 0. What the code actually is today

**Frontend** — `src/gui.html` (≈1.6k lines, single file, no build step, embedded
with `include_str!`), driven by `src/gui.rs`:

| Piece | State |
|---|---|
| Custom titlebar (drag, min/max/close) | working, over IPC |
| Sidebar: session list, new chat, API badge | working |
| Transcript: bubbles, streaming, scroll | working |
| Composer | working |
| Browser panel (`#browser`) | **shell only** — chrome drawn, nothing behind it |
| `#url` element | **a `<div>`, not an input** → cannot type or paste |
| Back / Forward / Reload | `<button>`s with **no listeners** |
| "Open Dev Server" CTA | `<button>` with **no listener** |
| `#bcontent` | globe icon + text placeholder; **no page surface at all** |

Only IPC verbs that exist: `send`, `new`, `load`, `ready`, `min`, `max`, `close`.

**Backend**

| Module | Role |
|---|---|
| `server.rs` | axum: `POST/GET /api/sessions`, `GET /api/sessions/{id}/messages`, `POST …/prompt`, `POST …/abort`, `POST /api/approvals/{id}`, `GET /api/events` (SSE) |
| `tools.rs` | agent tools: `bash`, `read`, `write`, + others (spec-driven) |
| `permissions.rs` | allow/ask/deny engine, `approval_timeout_secs` |
| `provider.rs` | OpenAI-compatible SSE client |
| `db.rs` | SQLite: sessions, messages, titles |
| `mcp.rs` | MCP **client** (stdio), tools as `mcp__<server>__<tool>` |
| `subagent.rs` / `team.rs` | child agents, parallel work |
| `skills.rs`, `compact.rs`, `gitsnap.rs`, `mission.rs`, `todo.rs` | supporting systems |
| `preview.rs` | opens a **separate** wry window on a URL (blocks until closed) |
| `tui/` | the terminal UI |

**Engine facts that decide the design** (verified in the vendored crate, wry 0.57):

- `WebViewBuilder::build_as_child(&window)` + `with_bounds(Rect)` → a **second
  real WebView2** can live inside the main window, positioned by us.
- `WebView::set_bounds / set_visible / load_url / reload / go_back / go_forward /
  can_go_back / can_go_forward / evaluate_script / url()` all exist.
- `set_bounds` is documented as effective **only** for child views. `Rect` takes
  `dpi::Position` / `dpi::Size`, so pass `LogicalPosition` / `LogicalSize`:
  **CSS pixels are already logical pixels** — never multiply by `scale_factor`,
  `dpi` does that conversion for us (a classic double-scaling bug otherwise).
- `WebViewExtWindows::webview() -> ICoreWebView2` and `controller()` exist →
  full **in-process CDP** (`CallDevToolsProtocolMethod`) and event receivers.
- `with_clipboard`, `with_ipc_handler`, `with_navigation_handler`,
  `with_document_title_changed_handler`, `with_on_page_load_handler` exist.

---

## 1. The two decisions that shape every task

### D1 — iframe or native child webview?

**Native child WebView2.** An `<iframe>` dies on `X-Frame-Options` / `frame-ancestors`,
so pasting any real link (GitHub, YouTube, docs) would show a blank frame. Only
localhost dev servers would work. The child webview browses everything and shares
the same Chromium runtime already installed for the main window (no new download).

Cost, stated up front: a native child view is a **separate surface**. It cannot be
painted *under* HTML (rounded corners, dropdowns, modals). It must be clipped to the
panel rect, hidden on collapse and whenever the page draws an overlay above it, and
re-positioned on resize. That is a real task (A-B2/A-F3), not a footnote.

### D2 — BrowserOS: fork it, vendor it, or copy its architecture?

Researched the repo and docs. Facts:

- BrowserOS is a **full Chromium fork** (`packages/browseros`, patch sets against
  Chromium), plus a **Bun/TypeScript agent server** and a browser extension.
  It is not a library you can embed.
- **License: AGPL-3.0.** Copying its code into our app (currently permissive/MIT)
  is a copyleft problem; shipping it as a *separate* program is defensible but the
  integration boundary is legally grey.
- Building it needs ~100 GB disk, 16 GB RAM, 3+ hours, a Visual Studio toolchain;
  installers are 200–390 MB. Control surface is MCP over loopback
  (`http://127.0.0.1:9239/mcp`), plus CDP internally.
- Its Chromium version pin and README already disagree — tracking this fork is a
  product in itself.

**Decision: do not fork or vendor BrowserOS.** Adopt the architecture, which is the
valuable part:

1. A dedicated browser-control layer with typed actions (navigate / snapshot /
   click / type / screenshot / wait).
2. **Structured page snapshots** (accessibility/DOM tree, with stable element ids)
   instead of dumping raw HTML into the model's context.
3. An **MCP server** endpoint so *other* agents (Claude Code, Codex, our own
   subagents) can drive our browser — same shape as BrowserOS's `/mcp`.
4. Explicit browser **profiles** (per workspace / per agent) so automation never
   touches the user's signed-in cookies.
5. Tool taxonomy as reference for naming and schemas; re-implemented, not copied.

That lands ~80% of what a user wants from "BrowserOS inside opencode-rust",
at ~5% of the cost, with no AGPL entanglement and no Chromium build farm.

---

## GOAL A — The panel becomes a real browser (user-visible)

**Outcome:** click the Preview button, type or paste `https://news.ycombinator.com`,
press Enter, it loads; Back/Forward/Reload work; the panel behaves like a browser
and collapses cleanly.

### A-B1 — Child webview lifecycle (backend) `M`
- [ ] Add a `Browser` state struct next to the main webview in `gui.rs::run`.
- [ ] Build the child with `WebViewBuilder::new().build_as_child(&window)`,
  `with_visible(false)` initially, `with_clipboard(true)`, background `#070A12`.
- [ ] `set_bounds(Rect { position: LogicalPosition::new(x, y).into(), size:
  LogicalSize::new(w, h).into() })`, fed straight from `getBoundingClientRect()`
  in CSS pixels. No manual DPI math. Unit-test the rect→Rect conversion.
- [ ] `set_visible(false)` when collapsed; `true` + bounds on open.
- [ ] Destroy/recreate on window recreate (maximize/minimize edge cases).

### A-B2 — IPC surface (backend + frontend contract) `S`
- [ ] Extend `IpcMsg`: `Browse(String)`, `NavBack`, `NavFwd`, `NavReload`,
  `PanelRect { x, y, w, h }`, `PanelVisible(bool)`, `OpenDevServer`.
- [ ] Extend `GuiState`/payload with `url`, `title`, `can_back`, `can_forward`,
  `loading` so the chrome reflects reality.
- [ ] Keep `parse_ipc` total: unknown → `None` (existing behavior), add tests for
  every new variant including malformed rects.

### A-B3 — Navigation feedback (backend) `S`
- [ ] `with_navigation_handler` → push URL + `can_back/can_forward` to the page.
- [ ] `with_document_title_changed_handler` → tab title.
- [ ] `with_on_page_load_handler` → loading spinner state.
- [ ] Block `file://` and `javascript:` from the URL bar (allow `http(s)`, `about:blank`,
  `localhost`); log refusals as a page-level error chip.

### A-B4 — Browser context / profile choice (backend) `S`
`WebContext::new(Some(path))` exists, so a dedicated data directory is one line —
**but each distinct user-data folder is a separate WebView2 environment, i.e. a
second browser process group** (~80–150 MB on top). Decide deliberately:

| Option | Cost | Gets you |
|---|---|---|
| **Reuse the main context** (recommended for A) | 0 extra processes | shares the cookie jar with the app page, which loads only local HTML — no network origin, so nothing to leak |
| Dedicated `WebContext` now | +1 browser process | real cookie isolation from day one |

- [ ] Ship A on the shared context; introduce `WebContext` per profile only in
  C2 where profiles are the feature and the cost is justified.
- [ ] Config keys reserved now: `[browser] enabled, home_url, allow_remote, profile_dir`.
- [ ] When profiles land: never let the automation profile touch the user's
  signed-in session; select explicitly.

### A-B5 — Dev-server detection (backend) `M`
- [ ] Parse `bash` tool output / run events for `http://localhost:<port>` and
  `127.0.0.1:<port>`; emit `dev_server{url}` on the event bus.
- [ ] GUI surfaces a "Open localhost:5173" chip; clicking navigates the panel.
- [ ] Honors "Open Dev Server" CTA that today does nothing.

### A-F1 — URL bar becomes real (frontend) `S` ← *this is the paste bug*
- [ ] Replace `<div id="url">` with `<input id="url" spellcheck="false">`.
- [ ] Enter → `postToRust({type:"browse", url})`; Escape → restore current URL.
- [ ] Ctrl+L / Cmd+L focuses and selects the bar; paste works natively once it is
  a real input.
- [ ] `:3000` and `localhost:3000` shorthand → prefix `http://`.
- [ ] Show lock / globe icon by scheme; truncate long URLs in the middle.

### A-F2 — Navigation controls (frontend) `S`
- [ ] Wire `←` `→` `⟳` buttons to `nav_back` / `nav_fwd` / `nav_reload`.
- [ ] Disable arrows via `can_back/can_forward` from the payload.
- [ ] Spinner state on the reload button while `loading`.

### A-F3 — Panel geometry plumbing (frontend) `M`
- [ ] `ResizeObserver` on `#bcontent`; `getBoundingClientRect()` → `window.devicePixelRatio`.
- [ ] Send `panel_rect` on: open, close, sidebar resize, window resize, browser zoom.
- [ ] Send `panel_visible` on collapse/expand, and whenever a modal/menu/overlay
  opens above the panel (native views don't respect z-index).
- [ ] Throttle to one message per animation frame.

### A-F4 — Panel states (frontend) `S`
- [ ] Empty state (current globe card, but with a working "Open Dev Server").
- [ ] Loading skeleton while the child view is hidden.
- [ ] Error card for refused URLs / failed loads (`ERR_NAME_NOT_RESOLVED` etc.).
- [ ] Keep the panel chrome visible during load — never a white flash.

### Acceptance for Goal A
1. Paste a link, press Enter, page renders in the panel.
2. Back/Forward/Reload reflect history correctly.
3. Collapsing the panel hides the native view (no floating rectangle over the chat).
4. Maximize / restore / resize keeps the view exactly inside the panel (no drift).
5. `cargo test` green; new `parse_ipc` + rect-conversion unit tests.

**Risk:** native view clipping is the #1 source of "floating grey rectangle" bugs —
A-F3 must ship with A-B1, not after it.

---

## GOAL B — The agent can drive the browser

**Outcome:** "open the localhost:3000 app, find the login form, fill it, submit,
tell me what happened" works from chat, with the user watching it happen.

### B-B1 — CDP bridge (backend) `M`
- [ ] Add **direct** deps `webview2-com = "0.39"` and `windows = "0.62"` under
  `[target.'cfg(windows)'.dependencies]`. Verified: `Cargo.lock` already resolves
  `webview2-com 0.39.1` / `windows 0.62.2` transitively via wry — matching those
  versions makes wry's `ICoreWebView2` and ours the *same type*. Any other
  version and the types silently fail to unify.
- [ ] `WebViewExtWindows::webview()` → wrap `CallDevToolsProtocolMethod` in an
  async facade (`cdp(method, params_json) -> Value`).
- [ ] Callback→oneshot map keyed by a counter; timeout per call.
- [ ] Subscribe to `Network.requestWillBeSent` / `Page.frameNavigated` for activity.
- [ ] Cross-platform: behind `#[cfg(windows)]`; macOS/GTK stubs return "unsupported".

### B-B2 — Browser tools (backend) `L`
Each is a `ToolSpec` in `tools.rs`'s style, executed against the GUI browser:
- [ ] `browser_navigate{url}`
- [ ] `browser_snapshot{}` — accessibility tree, **not** raw HTML
- [ ] `browser_click{ref}` / `browser_type{ref,text,submit?}`
- [ ] `browser_screenshot{}` — `Page.captureScreenshot` → temp PNG, returned as a path
- [ ] `browser_wait{selector|text|ms}`
- [ ] `browser_tabs{action,id}` (depends on Goal C tabs)
- [ ] Every tool returns a compact JSON envelope `{ok, url, title, note}`.

### B-B3 — Permissions integration (backend) `M`
- [ ] Read-only ops (`navigate`, `snapshot`, `screenshot`) → `allow` by default.
- [ ] Mutating ops (`click` on submit, `type` into password/checkout) → `ask`,
  routed through the existing approval engine.
- [ ] **Depends on the approval UI** (Goal D1) — today an `ask` has no surface and
  silently becomes a denial. Do D1 first or in the same slice.

### B-B4 — Snapshot engineering (backend) `M`
- [ ] Stable refs (`e12`) mapped to DOM nodes for the session, invalidated on nav.
- [ ] Cap tokens: prune to interactive + text nodes, drop decorative divs.
- [ ] Include `role`, `name`, `value`, `href`, `checked`; omit styles/classes.
- [ ] Regression test against a local fixture page with known structure.

### B-B5 — Safety rails (backend) `S`
- [ ] Max 1 navigation in flight; queue the rest.
- [ ] Loop guard: identical tool+args 3× in a row → error back to the model.
- [ ] Never auto-fill credentials from config.
- [ ] CDP stays in-process; open no ports.

### B-F1 — Agent activity view (frontend) `M`
- [ ] Live "browsing…" chip with the current action (`click e12`, `type …`).
- [ ] Optional highlight rectangle drawn by injected CSS for the target element.
- [ ] Action log under the panel (last 20), so the user can audit what was touched.
- [ ] Cancel button → aborts the in-flight tool.

### Acceptance for Goal B
1. Agent completes a 3-step flow (navigate → click → assert text) unattended.
2. Screenshot returns a real image of the panel.
3. A blocked action surfaces an approval request, not a silent denial.
4. Snapshot of a typical page stays under ~2k tokens.

---

## GOAL C — BrowserOS-class features, without BrowserOS

Ordered by value per unit of work; each is independently shippable.

### C1 — Expose an MCP server (backend) `M`
- [ ] Loopback-only axum listener, `POST /mcp` (JSON-RPC 2.0), reusing `mcp.rs`'s
  message shapes in reverse (we act as **server**).
- [ ] Tools = Goal B's browser tools + `bash`/`read`/`write` if the user opts in.
- [ ] Token in the URL path; print it at startup; refuse non-loopback peers.
- [ ] This is what makes Claude Code / Codex / another agent usable against our app.

### C2 — Profiles per workspace/agent (backend) `M`
- [ ] `ProfileStore`: name → `WebContext` dir; select at panel open.
- [ ] `--profile` for subagents; separate cookies/storage.
- [ ] "Automation" profile is default for agent browsing; "Personal" never auto-used.

### C3 — Tabs (frontend + backend) `L`
- [ ] Backend `Vec<BrowserTab>` each with its own child webview (or one webview,
  swapped targets — start with one visible view + a stack of URLs, then split).
- [ ] Frontend tab strip: active/close/new; `+` opens `home_url`.
- [ ] Per-tab title/favicon/history state in the payload.

### C4 — Downloads (backend) `M`
- [ ] `with_download_started_handler` → file lands in `~/Downloads` (or workspace
  `downloads/`), toast in the panel.
- [ ] Never auto-open executables.

### C5 — Reader / extract mode (backend) `S`
- [ ] `browser_snapshot{mode:"article"}` → Readability-style main-content extract,
  stripped to text+links. Cheapest big win for "summarize this page".

### C6 — History + bookmarks (backend + frontend) `M`
- [ ] SQLite tables; sidebar section; Ctrl+H palette.
- [ ] Search by title/URL.

### C7 — Parallel agent browsing (backend) `L`
- [ ] One browser tab per subagent task; results merge into the parent turn.
- [ ] Requires C2 + C3 + B-B1. This is the real "neo" capability.

---

## GOAL D — Close the shell's remaining holes

### D1 — Approval UI (frontend + backend) `M` — **highest user-visible gap**
- [ ] `ServerEvent::ApprovalRequest` already exists in the engine; the GUI drops it.
- [ ] Modal: tool name, exact command/path, diff preview for writes, Allow once /
  Always allow / Deny.
- [ ] Wire to `POST /api/approvals/{id}`; keyboard `y`/`n`.
- [ ] Timeout countdown from `approval_timeout_secs`.

### D2 — Release build `S`
- [ ] `cargo build --release`, smoke-test the exe (not just debug).
- [ ] Decide on an installer (MSIX vs portable folder); icon + version stamping.

### D3 — Crash/edge hygiene `S`
- [ ] WebView2 missing → actionable dialog, not a stack trace.
- [ ] Single-instance guard (two GUIs writing one DB).

---

## Suggested sequencing

| Slice | Contents | Ship criteria |
|---|---|---|
| 1 (½ day) | A-F1 + A-F2 + A-B2 (IPC only, URL is stored, panel remains placeholder) | you can type/paste a URL without the app ignoring you |
| 2 (1–2 days) | A-B1 + A-B3 + A-F3 + A-F4 | real page renders, nav works, collapse is clean |
| 3 (1 day) | A-B4 + A-B5 | dev-server chip, isolated profile |
| 4 (2 days) | D1 approvals → B-B3 | mutations are approvable |
| 5 (2–3 days) | B-B1 + B-B2 + B-B4 + B-F1 | agent drives the browser with a visible log |
| 6 (1 day) | C1 MCP server | external agents can attach |
| 7+ | C2, C3, C5, C4, C6, C7 | each independent |

---

## Non-goals (explicitly deferred)

- Forking Chromium, shipping a separate browser, or bundling BrowserOS.
- Extensions support (WebView2 can load unpacked ones, but it is a rabbit hole).
- Full devtools UI (we can open WebView2's devtools for debugging only).
- Mobile/other-platform parity for Goal B (CDP slice is Windows-first).

---

## Appendix A — IPC schema (target state)

Page → Rust (`window.ipc.postMessage`, JSON):

| `type` | Fields | Meaning |
|---|---|---|
| `send` | `text` | composer submit (exists) |
| `new` / `load` | `id?` | session ops (exist) |
| `ready` | — | page loaded, send first paint (exists) |
| `min` / `max` / `close` | — | window controls (exist) |
| `browse` | `url` | navigate the panel (new) |
| `nav_back` / `nav_fwd` / `nav_reload` | — | history (new) |
| `panel_rect` | `x,y,w,h` (CSS px) | position the child view (new) |
| `panel_visible` | `visible: bool` | show/hide the child view (new) |
| `open_dev_server` | `url?` | CTA / detected localhost (new) |

Rust → page: unchanged — one `window.render(payload)` call; the payload grows
`browser: { url, title, can_back, can_forward, loading, error }`.

## Appendix B — CDP method map (Goal B)

| Tool | CDP methods |
|---|---|
| `browser_navigate` | `Page.navigate`, wait `Page.loadEventFired` (or wry `load_url`) |
| `browser_snapshot` | `Accessibility.getFullAXTree` (+ `DOM.describeNode` for refs) |
| `browser_click` | `DOM.resolveNode` → `Runtime.callFunctionOn` (element.click) or `Input.dispatchMouseEvent` |
| `browser_type` | `DOM.focus` + `Input.insertText` (or `Input.dispatchKeyEvent` per char) |
| `browser_screenshot` | `Page.captureScreenshot{format:"png"}` → temp file path |
| `browser_wait` | poll `Runtime.evaluate` predicate, or `ms` |
| activity feed | events `Network.requestWillBeSent`, `Page.frameNavigated` |

`Runtime.evaluate` and `Page.captureScreenshot` are also reachable through wry's
`evaluate_script` / WebView2 APIs, so each tool has a cheap fallback if a CDP
method is unavailable on an older WebView2 runtime.

## Appendix C — Risk register

| # | Risk | Mitigation |
|---|---|---|
| R1 | Native child view cannot be composited under HTML → grey rectangle over modals/menus | A-F3 sends `panel_visible`; hide on overlay open; verify with a manual checklist |
| R2 | View drifts/clips on maximize, DPI change, or window move | Recompute bounds on resize + `ScaleFactorChanged`; assert with a pixel check |
| R3 | `webview2-com` version mismatch → type errors or silent no-op | Pin to wry's resolved 0.39 / windows 0.62; `cargo tree -i` in CI |
| R4 | CDP is not a stable public API; WebView2 updates move things | Prefer wry/WebView2 APIs where they exist; version-guard CDP calls; degrade to `evaluate_script` |
| R5 | Browsing arbitrary sites inside the app = a real attack surface (phishing, downloads) | Dedicated profile (C2), no auto-open of downloads, block `file://`, show the real URL not a spoofable label |
| R6 | Snapshots blow the context window | B-B4 pruning + hard token cap + `reader` mode |
| R7 | Approval-less tools (B-B3) fail silently | D1 must land before or with B-B3 |

## Appendix D — Test strategy

- **Unit (fast, CI):** `parse_ipc` for every verb incl. malformed input; CSS-px →
  `Rect` conversion; URL normalization (`:3000` → `http://…`, scheme allow-list);
  snapshot pruning on a fixture HTML string.
- **Headless page tests (already proven in this repo):** the transcript scroll
  fix was verified by appending an assertion script to the real `gui.html` and
  running it in `msedge --headless --dump-dom`. Reuse that harness for IPC
  payload rendering, panel states, and tab strip behavior.
- **Integration:** start the app against `examples/mock_llm.rs`, navigate to a
  local fixture server, drive B's tools end-to-end, assert on the fixture's DOM.
- **Manual checklist (native surfaces only):** collapse/expand, maximize,
  DPI scale change, modal over panel, tab switch, download prompt.
- **Regression:** `cargo test` stays green at every slice; each new module gets
  tests in the same commit (repo convention — every module has a `mod tests`).

## Appendix E — Open questions for you

1. **Scope of the panel:** dev-server preview only (localhost, fast, zero safety
   work) or full general-purpose browsing (needs profile isolation, download
   policy, URL trust UI)? The plan assumes the latter but ships the former first.
2. **Default home page** when the panel opens empty: blank, `about:blank`, or a
   small start page with recent dev servers?
3. **Downloads** — silently to `~/Downloads`, or into the workspace `downloads/`
   so agents can read the files they fetched?
4. **Tabs vs single view**: is a tab strip worth the complexity in the first
   release, or is one view + history enough?
5. **Approval UX for browsing** (B-B3): should agent clicks always ask, or only
   when they submit a form / touch a password field?
