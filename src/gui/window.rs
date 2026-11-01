//! The app window and its two webviews: the main one showing our own page
//! (locked to it, S-18) and the preview panel's native child view.

use super::*;

pub(super) const APP_HTML: &str = include_str!("../gui.html");

/// The app window. No OS decorations: the page draws its own titlebar (drag
/// region + min/max/close buttons over IPC). Start maximized so the composer
/// is never shoved off-screen on small displays.
pub(super) fn build_window(event_loop: &EventLoop<AppEvent>) -> Result<tao::window::Window> {
    WindowBuilder::new()
        .with_title("Habi \u{2014} agent workspace")
        .with_decorations(false)
        .with_maximized(true)
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_min_inner_size(LogicalSize::new(900.0, 560.0))
        .build(event_loop)
        .map_err(|e| anyhow::anyhow!("creating the app window failed: {e}"))
}

/// The main webview: our own page (`gui.html`), whose messages reach the
/// event loop through `page_tx`.
pub(super) fn build_main_webview(
    window: &tao::window::Window,
    event_loop: &EventLoop<AppEvent>,
    page_tx: std::sync::mpsc::Sender<UiMsg>,
) -> Result<wry::WebView> {
    let ipc_proxy = event_loop.create_proxy();
    wry::WebViewBuilder::new()
        .with_background_color((7, 10, 18, 255))
        // S-18: DevTools only in debug builds (`cargo run`), never in release.
        .with_devtools(cfg!(debug_assertions))
        .with_initialization_script(
            "window.addEventListener('error', (e) => {\n\
             \x20 try { window.ipc.postMessage(JSON.stringify({ type: 'page_error', msg: e.message })) } catch (_) {}\n\
             \x20});",
        )
        .with_html(APP_HTML)
        // S-18: this window only ever shows our own page; refuse every other
        // navigation, so no link or script can swap the app for a page that
        // would then get our IPC channel.
        .with_navigation_handler(main_window_navigation_guard())
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            if let Some(msg) = parse_ipc(req.body()) {
                let _ = page_tx.send(UiMsg::Ipc(msg));
                let _ = ipc_proxy.send_event(AppEvent::Wake);
            } else {
                // Only the type and size: a body (Settings) may hold the API key.
                gui_log(&format!("[gui] IPC not understood: {}", ipc_summary(req.body())));
            }
        })
        .build(window)
        .map_err(|e| {
            anyhow::anyhow!(
                "opening the app window failed: {e}\n\
                 On Windows this needs the Microsoft Edge WebView2 Runtime \
                 (preinstalled on Windows 11; a small slice of Windows 10 \
                 machines need the free 'WebView2 Runtime' download)."
            )
        })
}

/// The preview panel's webview, built hidden. Its callbacks (navigation,
/// title, load state, bridge replies) reach the event loop through `ui_tx`.
pub(super) fn build_preview_webview(
    window: &tao::window::Window,
    event_loop: &EventLoop<AppEvent>,
    ui_tx: &std::sync::mpsc::Sender<UiMsg>,
    bridge_ids: &PendingIds,
    allow_remote: bool,
) -> Result<wry::WebView, wry::Error> {
    let nav_tx = ui_tx.clone();
    let nav_proxy = event_loop.create_proxy();
    let eval_tx = ui_tx.clone();
    let eval_proxy = event_loop.create_proxy();
    let eval_ids = bridge_ids.clone();
    let title_tx = ui_tx.clone();
    let title_proxy = event_loop.create_proxy();
    let load_tx = ui_tx.clone();
    let load_proxy = event_loop.create_proxy();
    wry::WebViewBuilder::new()
        .with_background_color((7, 10, 18, 255))
        .with_visible(false)
        // S-18: no `with_clipboard(true)`: in wry 0.57 on Windows it lets
        // every page in the panel read the clipboard without asking.
        .with_bounds(PanelRect::fallback().to_wry())
        // Goal B: the page-side primitives (snapshot/click/type/wait) and
        // the channel their results come back on.
        .with_initialization_script(crate::browser::RUNTIME_JS)
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            // S-12: only a reply to a request we are waiting for may pass,
            // each id at most once, and never an oversized frame.
            let body = req.body();
            if body.len() > MAX_EVAL_FRAME_BYTES {
                return;
            }
            let Some((id, _)) = crate::browser::parse_reply(body) else {
                return;
            };
            let expected = eval_ids.lock().map(|mut set| set.remove(&id)).unwrap_or(false);
            if !expected {
                return;
            }
            let _ = eval_tx.send(UiMsg::Browser(BrowserEvt::Eval(body.clone())));
            let _ = eval_proxy.send_event(AppEvent::Wake);
        })
        .with_navigation_handler(move |url: String| {
            // S-11: links, redirects and page scripts follow the same rule
            // as typed URLs, including `allow_remote = false`.
            let verdict = check_navigation(&url, allow_remote);
            let allowed = verdict.is_ok();
            match verdict {
                Ok(()) => {
                    let _ = nav_tx.send(UiMsg::Browser(BrowserEvt::Url(url)));
                }
                Err(msg) => {
                    let _ = nav_tx.send(UiMsg::Browser(BrowserEvt::Blocked(msg)));
                }
            }
            let _ = nav_proxy.send_event(AppEvent::Wake);
            allowed
        })
        .with_document_title_changed_handler(move |title: String| {
            let _ = title_tx.send(UiMsg::Browser(BrowserEvt::Title(title)));
            let _ = title_proxy.send_event(AppEvent::Wake);
        })
        .with_on_page_load_handler(move |ev: wry::PageLoadEvent, _url: String| {
            let loading = matches!(ev, wry::PageLoadEvent::Started);
            let _ = load_tx.send(UiMsg::Browser(BrowserEvt::Loading(loading)));
            let _ = load_proxy.send_event(AppEvent::Wake);
        })
        .build_as_child(window)
}

/// Push the current state into the page (no-op until the page is loaded).
/// B-37: only the entries that changed since the last paint are sent.
pub(super) fn paint(webview: &wry::WebView, gs: &GuiState) {
    let _ = webview.evaluate_script(&format!("window.render&&window.render({})", gs.paint_payload()));
    send_term(webview, gs);
    // A Changes view is sent once, apart from the frequent repaints.
    if let Some(json) = gs.changes_out.borrow_mut().take() {
        let _ = webview.evaluate_script(&format!("window.showChanges&&window.showChanges({json})"));
    }
    if let Some(json) = gs.usage_out.borrow_mut().take() {
        let _ = webview.evaluate_script(&format!("window.showUsage&&window.showUsage({json})"));
    }
}

/// New (S-18): may the main window go to `uri`? Only our own page: WebView2
/// reports the page loaded from a string as `about:blank` or as a
/// `data:text/html…` URI (the exact form depends on the runtime version).
/// Chromium itself refuses `data:` navigations started by page scripts.
pub(super) fn main_window_may_navigate(uri: &str) -> bool {
    let u = uri.trim().to_ascii_lowercase();
    u == "about:blank" || u.starts_with("data:text/html")
}

/// New (S-18): navigation handler for the main window. The very first
/// navigation is always our own page (`with_html`), whatever URI the
/// runtime reports for it; after that only `main_window_may_navigate` URIs.
pub(super) fn main_window_navigation_guard() -> impl Fn(String) -> bool + 'static {
    let first = std::sync::atomic::AtomicBool::new(true);
    move |uri: String| {
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) || main_window_may_navigate(&uri) {
            return true;
        }
        gui_log(&format!("[gui] blocked a navigation of the main window to {uri}"));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S-18: the main window only ever shows our own page.
    #[test]
    fn main_window_navigation_is_locked() {
        assert!(main_window_may_navigate("about:blank"));
        assert!(main_window_may_navigate("data:text/html;charset=utf-8;base64,PGgxPg=="));
        assert!(!main_window_may_navigate("https://example.com/"));
        assert!(!main_window_may_navigate("http://localhost:3000/"));
        assert!(!main_window_may_navigate("file:///C:/Windows/win.ini"));
        let guard = main_window_navigation_guard();
        assert!(guard("data:text/html,first".to_string()), "the first load is ours");
        assert!(!guard("https://example.com/".to_string()));
        assert!(guard("about:blank".to_string()));
    }
}
