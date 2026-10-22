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
    // URL sanity: WebView2 needs an absolute http(s)/file URL.
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://")) {
        anyhow::bail!("preview URL must be http(s):// or file:// (got: {url})");
    }
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(format!("{title} — opencode-rust preview"))
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .build(&event_loop)
        .context("creating the preview window")?;
    let _webview = WebViewBuilder::new()
        .with_url(url)
        .with_navigation_handler(|nav| {
            eprintln!("[preview] navigating: {nav}");
            true
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
            eprintln!("[preview] page says: {}", body.body());
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
}
