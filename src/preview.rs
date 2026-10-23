//! M4: embedded preview browser.
//!
//! Deliberately NOT Tauri and NOT a pure-Rust engine: a ~100-line Rust shell
//! (tao window + wry) around the OS **WebView2** (out-of-process Chromium).
//! Research (recorded in M4-REPORT.md) showed this is the lowest-RAM option
//! that can still execute real page JavaScript: pure-Rust Blitz runs no JS
//! at all (and its own memory issues report 200–400 MB), Servo embeds an
//! experimental API with higher RAM, while WebView2 keeps its heavy browser
//! processes out-of-process and shareable with the rest of the OS.

use anyhow::{Context, Result};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

/// Open a native window rendering `url`. Blocks until the window is closed.
/// `exit_after` self-closes the window after a duration (E2E automation aid,
/// e.g. `preview <url> --exit-after-ms 6000`).
pub fn open_preview(url: &str, title: &str, exit_after: Option<std::time::Duration>) -> Result<()> {
    let t0 = std::time::Instant::now();
    // URL sanity: WebView2 needs an absolute http(s)/file URL (any letter case).
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file://")) {
        anyhow::bail!("preview URL must be http(s):// or file:// (got: {})", safe_log(url));
    }
    // Pages may move to http(s) and about:blank; to file:// only when the
    // window itself was opened on a local file.
    let allow_file = lower.starts_with("file://");
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(format!("{title} — opencode-rust preview"))
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .build(&event_loop)
        .context("creating the preview window")?;
    let _webview = WebViewBuilder::new()
        .with_url(url)
        .with_navigation_handler(move |nav| {
            let ok = preview_nav_allowed(&nav, allow_file);
            eprintln!("[preview] {} {}", if ok { "navigating:" } else { "blocked:" }, safe_log(&nav));
            ok
        })
        .with_initialization_script(
            "window.addEventListener('load', () => {\n\
             \x20 try { window.ipc.postMessage('PAGE_LOAD ' + location.href) } catch (e) {}\n\
             \x20});\n\
             window.addEventListener('error', (e) => {\n\
             \x20 try { window.ipc.postMessage('PAGE_ERROR ' + e.message) } catch (_) {}\n\
             \x20});",
        )
        .with_ipc_handler(move |body: wry::http::Request<String>| {
            eprintln!("[preview] page says: {}", safe_log(body.body()));
        })
        .build(&window)
        .map_err(|e| {
            anyhow::anyhow!(
                "creating the embedded browser failed: {e}\n\
                 On Windows this needs the Microsoft Edge WebView2 Runtime \
                 (preinstalled on Windows 11; a small slice of Windows 10 machines \
                 need the free 'WebView2 Runtime' download from Microsoft)."
            )
        })?;
    eprintln!("[preview] window up in {}ms — {url}", t0.elapsed().as_millis());

    let started = std::time::Instant::now();
    let mut auto_logged = false;
    // `ControlFlow::WaitUntil` alone did not wake the loop on Windows (the
    // window stayed open forever, seen with a page that failed to load), so
    // a timer thread sends the loop an event when the time is up.
    if let Some(d) = exit_after {
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(d);
            let _ = proxy.send_event(());
        });
    }
    event_loop.run(move |event, _, control_flow| {
        let mut exit = false;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                eprintln!("[preview] close requested — shutting down");
                exit = true;
            }
            Event::LoopDestroyed => {
                eprintln!("[preview] event loop destroyed (clean exit)");
            }
            _ => {}
        }
        if let Some(d) = exit_after {
            if started.elapsed() >= d {
                if !auto_logged {
                    eprintln!("[preview] exit-after {d:?} reached — shutting down");
                    auto_logged = true;
                }
                exit = true;
            }
        }
        *control_flow = if exit {
            ControlFlow::Exit
        } else {
            match exit_after {
                Some(d) => ControlFlow::WaitUntil(started + d),
                None => ControlFlow::Wait,
            }
        };
    });
}

/// Navigation rule for the standalone window: web pages and about:blank
/// always; local files only when the window started on one.
fn preview_nav_allowed(url: &str, allow_file: bool) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower == "about:blank"
        || (allow_file && lower.starts_with("file://"))
}

/// Page-provided text for the terminal: control characters (escape codes,
/// line breaks) are shown escaped, and long text is cut to 300 characters.
fn safe_log(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().take(300) {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_absolute_urls() {
        // The URL guard runs before any window is created, so this is safe
        // headlessly.
        assert!(open_preview("localhost:5173", "t", None).is_err());
        assert!(open_preview("example.com", "t", None).is_err());
        // http(s) and file pass the guard but then fail later in a headless
        // environment for other reasons — we only assert the guard here, so
        // use invalid-scheme only to keep CI headless-safe.
    }

    #[test]
    fn guard_message_names_the_problem() {
        let err = open_preview("ftp://x", "t", None).unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[test]
    fn navigation_rule_allows_only_web_pages() {
        assert!(preview_nav_allowed("https://example.com/", false));
        assert!(preview_nav_allowed("HTTP://localhost:5173/", false));
        assert!(preview_nav_allowed("about:blank", false));
        assert!(!preview_nav_allowed("file:///etc/hosts", false));
        assert!(preview_nav_allowed("file:///C:/site/index.html", true));
        assert!(!preview_nav_allowed("javascript:void(0)", true));
    }

    #[test]
    fn page_text_is_logged_safely() {
        let s = safe_log("ok\u{1b}[31mred\nnext");
        assert!(!s.contains('\u{1b}') && !s.contains('\n'), "{s}");
        assert!(s.contains("ok") && s.contains("red"));
        assert_eq!(safe_log(&"x".repeat(1000)).len(), 300);
    }
}
