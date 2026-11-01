//! The preview panel: placing and showing its native child view, the page
//! zoom, and the panel's verbs (URL bar, history, dev-server button).

use super::*;

/// Apply the panel's rect and visibility to the native child view.
///
/// Bounds go first: a view shown at a stale rect flashes a grey/white
/// rectangle outside the panel until the next frame. Child views are not
/// clipped by CSS, so "hidden" really has to mean `set_visible(false)`.
pub(super) fn show_browser(wv: &wry::WebView, view: &BrowserView) {
    if let Some(rect) = view.rect.filter(|r| r.is_usable()) {
        let _ = wv.set_bounds(rect.to_wry());
    }
    let show = view.wants_native_view();
    let _ = wv.set_visible(show);
}

/// Mirror the engine's history state into the payload (drives arrow buttons).
pub(super) fn sync_history(wv: &wry::WebView, view: &mut BrowserView) {
    view.can_back = wv.can_go_back().unwrap_or(false);
    view.can_forward = wv.can_go_forward().unwrap_or(false);
}

/// The page zoom from the page's `devicePixelRatio` and the screen's scale
/// (1 when the page sent none), rounded so float noise does not count.
pub(super) fn page_zoom(dpr: f64, scale_factor: f64) -> f64 {
    if !(dpr.is_finite() && dpr > 0.0 && scale_factor > 0.0) {
        return 1.0;
    }
    ((dpr / scale_factor) * 100.0).round() / 100.0
}

pub(super) fn load_zoom(state_dir: &std::path::Path) -> f64 {
    std::fs::read_to_string(state_dir.join("zoom.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|z| (0.25..=5.0).contains(z))
        .unwrap_or(1.0)
}

pub(super) fn save_zoom(state_dir: &std::path::Path, z: f64) {
    let _ = std::fs::write(state_dir.join("zoom.txt"), format!("{z}"));
}

/// Diagnostic hook: `OPENCODE_RUST_PREVIEW_SMOKE=<url>` opens the panel on
/// that URL at boot and logs every webview callback. It exists because the
/// child view can only be verified by a human clicking the window
/// otherwise — this makes "does a real page actually render?" testable.
pub(super) fn preview_smoke(gs: &mut GuiState, wv: &wry::WebView, allow_remote: bool) {
    let Ok(raw) = std::env::var("OPENCODE_RUST_PREVIEW_SMOKE") else {
        return;
    };
    // S-11: the diagnostic hook obeys the same rule as the URL bar.
    let checked = normalize_url(&raw).and_then(|url| check_navigation(&url, allow_remote).map(|()| url));
    match checked {
        Ok(url) => {
            let rect = PanelRect { x: 800.0, y: 80.0, w: 440.0, h: 700.0 };
            gs.browser.rect = Some(rect);
            gs.browser.open(&url);
            let _ = wv.set_bounds(rect.to_wry());
            let _ = wv.set_visible(true);
            let _ = wv.load_url(&url);
            gui_log(&format!("[preview-smoke] loading {url}"));
        }
        Err(e) => gui_log(&format!("[preview-smoke] refused: {e}")),
    }
}

impl App {
    /// The URL bar. S-11: typed URLs pass the same check as links,
    /// redirects, the dev-server button and the agent.
    pub(super) fn on_browse(&mut self, raw: &str) -> bool {
        let target = normalize_url(raw).and_then(|url| {
            check_navigation(&url, self.st.cfg.browser.allow_remote).map(|()| url)
        });
        match target {
            Ok(url) => match &self.browser_webview {
                Some(wv) => {
                    self.gs.browser.open(&url);
                    show_browser(wv, &self.gs.browser);
                    if let Err(e) = wv.load_url(&url) {
                        self.gs.browser.fail(format!("navigate failed: {e}"));
                        show_browser(wv, &self.gs.browser);
                    }
                }
                None => self.gs.browser.fail(if self.browser_enabled {
                    "Preview panel unavailable on this system."
                } else {
                    "Preview panel is disabled in config ([browser] enabled = false)."
                }),
            },
            Err(e) => {
                self.gs.browser.fail(e);
                if let Some(wv) = &self.browser_webview {
                    show_browser(wv, &self.gs.browser);
                }
            }
        }
        true
    }

    pub(super) fn on_nav_back(&mut self) -> bool {
        if let Some(wv) = &self.browser_webview {
            let _ = wv.go_back();
            sync_history(wv, &mut self.gs.browser);
        }
        true
    }

    pub(super) fn on_nav_fwd(&mut self) -> bool {
        if let Some(wv) = &self.browser_webview {
            let _ = wv.go_forward();
            sync_history(wv, &mut self.gs.browser);
        }
        true
    }

    pub(super) fn on_nav_reload(&self) -> bool {
        if let Some(wv) = &self.browser_webview {
            let _ = wv.reload();
        }
        true
    }

    /// Where the page wants the native view, and the page zoom it implies.
    pub(super) fn on_panel_rect(&mut self, rect: PanelRect, dpr: f64) {
        let z = page_zoom(dpr, self.window.scale_factor());
        if (z - self.page_zoom_now).abs() > 0.001 {
            self.page_zoom_now = z;
            save_zoom(&self.st.state_dir, z);
        }
        self.gs.browser.rect = Some(rect.scaled(z));
        if let Some(wv) = &self.browser_webview {
            show_browser(wv, &self.gs.browser);
        }
    }

    pub(super) fn on_panel_visible(&mut self, visible: bool) {
        self.gs.browser.visible = visible;
        self.gs.browser.user_closed = !visible;
        if let Some(wv) = &self.browser_webview {
            show_browser(wv, &self.gs.browser);
        }
    }

    /// The "Open Dev Server" button: this URL, else the last dev server
    /// seen, else `[browser] home_url`, else `localhost:3000`.
    pub(super) fn on_open_dev_server(&mut self, url: Option<String>) -> bool {
        let target = url
            .or_else(|| self.gs.dev_server.clone())
            .or_else(|| {
                let home = self.gs.browser.home.clone();
                if home.is_empty() { None } else { Some(home) }
            })
            .unwrap_or_else(|| "http://localhost:3000".into());
        // S-11: the button obeys `allow_remote` too.
        let checked = normalize_url(&target).and_then(|url| {
            check_navigation(&url, self.st.cfg.browser.allow_remote).map(|()| url)
        });
        match checked {
            Ok(u) => {
                if let Some(wv) = &self.browser_webview {
                    self.gs.browser.open(&u);
                    show_browser(wv, &self.gs.browser);
                    if let Err(e) = wv.load_url(&u) {
                        self.gs.browser.fail(format!("navigate failed: {e}"));
                        show_browser(wv, &self.gs.browser);
                    }
                } else {
                    self.gs.browser.fail(if self.browser_enabled {
                        "Preview panel unavailable on this system."
                    } else {
                        "Preview panel is disabled in config ([browser] enabled = false)."
                    });
                }
            }
            Err(e) => {
                self.gs.browser.fail(e);
                if let Some(wv) = &self.browser_webview {
                    show_browser(wv, &self.gs.browser);
                }
            }
        }
        true
    }

    /// A callback from the preview webview (navigation, title, load state).
    pub(super) fn on_preview_event(&mut self, ev: BrowserEvt) -> bool {
        if std::env::var_os("OPENCODE_RUST_PREVIEW_SMOKE").is_some() {
            gui_log(&format!("[preview-smoke] {ev:?}"));
        }
        let loaded = matches!(ev, BrowserEvt::Loading(false));
        // B-36: track the start of a load, and refusals.
        if matches!(ev, BrowserEvt::Loading(true)) {
            self.bridge.on_load_started();
        }
        if let BrowserEvt::Blocked(msg) = &ev {
            self.bridge.on_blocked(msg);
        }
        self.gs.on_browser_event(ev);
        // A pending `browser_navigate` resolves on load.
        if loaded {
            self.bridge.on_loaded(&self.gs.browser);
        }
        if let Some(wv) = &self.browser_webview {
            sync_history(wv, &mut self.gs.browser);
            show_browser(wv, &self.gs.browser);
        }
        true
    }
}
