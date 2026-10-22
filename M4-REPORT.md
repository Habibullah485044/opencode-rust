# M4 (phase 1) — Embedded Preview Browser: Completion Report

GRADES spec=8 design=8 correctness=8 quality=7; biggest gap: RAM/cold-start measured on one machine only

## What shipped

`opencode-rust preview <url> [--title T] [--exit-after-ms N]` — a native window
rendering any URL through an embedded browser. Implementation: [`preview.rs`](src/preview.rs),
~120 lines of Rust (`tao` 0.37 window + `wry` 0.57 WebView2), no GUI framework.

## Engine decision (researched, user-directed: lowest RAM wins)

| Option | Verdict |
|---|---|
| **wry + OS WebView2** (chosen) | Real JS, out-of-process Chromium shared with the OS → lowest practical RAM; shell is pure Rust |
| Blitz (pure Rust) | **Runs no JavaScript at all** (explicitly out of scope) — React/Vite previews break; its own memory issue reports 200–400 MB for TodoMVC |
| Servo | Real JS but SpiderMonkey (not pure Rust), experimental embed API, higher RAM, compat gaps |

Sources: github.com/DioxusLabs/blitz (+issue #286), servo.org/blog, learn.microsoft.com
WebView2 process model, crates.io wry/tao.

## Measured (this machine, Windows, scripted SSE-free static page with JS canary)

| Metric | Value |
|---|---|
| Rust shell process RSS | **17.7–18.0 MB** |
| WebView2 processes for our window | **~227–266 MB across 6 procs** (baseline foreign app's 147–154 MB / 6 procs subtracted) |
| Binary size (release) | 4,986,880 bytes (~4.8 MB) — +200 KB vs pre-M4 |
| Window-up, warm | **595–1,153 ms** |
| Window-up, cold WebView2 runtime spin-up | ~5–6 s (first launch only) |
| Headless core ready (plan budget item) | 6–11 ms (measured in earlier E2E) |

## Verified live through the real binary (multiple runs, debug AND release)

- Window opens, navigates, **page JavaScript executes and interacts** — a canary
  `fetch('/js-ran')` in the page reached the local test server every run (logged hits).
- IPC init script confirmed `PAGE_LOAD` (navigation + JS both alive).
- Graceful close (WM_CLOSE and `--exit-after-ms`): `close requested → event loop
  destroyed (clean exit)`, exit code 0.
- **No process leak**: all 6 WebView2 child processes are reaped on exit (count returns
  exactly to the pre-existing foreign baseline; attributed via parent PID — the 6
  lingering processes on this machine belong to another app, not ours).

## Limitations

- RAM/startup measured on one machine, one simple page; heavy pages cost more (documented
  by Microsoft as workload-dependent).
- WebView2 Runtime is preinstalled on Win11 and most Win10; the error message tells the
  user what to install if missing (small Win10 slice).
- Cold first launch (~5–6 s) includes WebView2 runtime process spin-up; warm launches are
  sub-second. Profile caching may improve this later.
- Remaining M4 (persistent terminals, IDE-style diff views, TUI `!open` integration) not
  started — phased per user's choice.
