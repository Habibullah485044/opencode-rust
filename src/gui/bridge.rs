//! Goal B: the agent's browser tools driving the preview webview — request
//! ids (S-12), navigation and wait bookkeeping (B-36), and screenshots (F-04).

use super::*;

/// New (S-12): ids of bridge requests still waiting for a page reply. The
/// preview's IPC handler reads it to drop frames nobody asked for.
pub(super) type PendingIds = Arc<std::sync::Mutex<std::collections::HashSet<u64>>>;

/// New (S-12): biggest reply frame we accept from the preview page. A
/// snapshot is capped far below this by the page runtime.
pub(super) const MAX_EVAL_FRAME_BYTES: usize = 1 << 20;

/// New (S-12): a request id the page cannot guess.
///
/// Ids travel through JavaScript as plain numbers, which are exact only up
/// to 2^53, so the id is kept below that. `RandomState` is seeded randomly
/// for each process; hashing a counter and the clock with it is enough for
/// a local, short-lived token (it is not a cryptographic secret).
pub(super) fn unguessable_id(counter: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(counter);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    h.write_u128(nanos);
    (h.finish() & ((1u64 << 53) - 1)).max(1)
}

/// Goal B: requests from agent tools that are waiting on this webview.
///
/// Every request has a deadline; a page that never answers (a hung script, a
/// destroyed view) fails the tool instead of freezing the run.
pub(super) struct Bridge {
    /// S-12: only feeds `unguessable_id`; ids themselves are random.
    counter: u64,
    pending: HashMap<u64, (tokio::sync::oneshot::Sender<Result<String, String>>, Instant)>,
    /// A navigation still waiting for its load event.
    nav: Option<NavWait>,
}

/// New (B-36): one `browser_navigate` waiting for its page to load.
pub(super) struct NavWait {
    reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    deadline: Instant,
    /// Set once the new page starts loading. A "finished" event that comes
    /// before that belongs to the page we are leaving and must not answer.
    started: bool,
}

/// New (B-36): longest `browser_wait` we run. It must stay below the 25 s
/// request deadline in `Bridge::start`, or the deadline fires first.
pub(super) const MAX_WAIT_MS: u64 = 20_000;

impl Bridge {
    pub(super) fn new() -> Self {
        Self { counter: 0, pending: HashMap::new(), nav: None }
    }

    pub(super) fn busy(&self) -> bool {
        !self.pending.is_empty() || self.nav.is_some()
    }

    /// New (S-12): tell the preview's IPC handler which ids may answer now.
    pub(super) fn publish_ids(&self, ids: &PendingIds) {
        if let Ok(mut set) = ids.lock() {
            set.clear();
            set.extend(self.pending.keys().copied());
        }
    }

    /// Dispatch one request. `None` webview = panel unavailable (config off).
    pub(super) fn start(
        &mut self,
        wv: &Option<wry::WebView>,
        req: crate::browser::Request,
        view: &mut BrowserView,
        allow_remote: bool,
    ) {
        let Some(wv) = wv else {
            let _ = req.reply.send(Err("the preview panel is not available".into()));
            return;
        };
        // S-12: random ids, so a page cannot answer a request before it is
        // sent. A page DOES see the id of a request it runs, so it could
        // still tamper with that reply; `__habi` is frozen and captures
        // postMessage/JSON.stringify at document creation to make that
        // harder, and page output is labelled untrusted for the model.
        self.counter += 1;
        let mut id = unguessable_id(self.counter);
        while self.pending.contains_key(&id) {
            self.counter += 1;
            id = unguessable_id(self.counter);
        }
        let deadline = Instant::now() + Duration::from_secs(25);
        match req.method.as_str() {
            "navigate" => {
                let raw = req.input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                // S-11: the same rule as every other way of opening a page.
                let checked = normalize_url(raw).and_then(|url| check_navigation(&url, allow_remote).map(|()| url));
                match checked {
                    Ok(url) => {
                        view.open(&url);
                        show_browser(wv, view);
                        if let Err(e) = wv.load_url(&url) {
                            let _ = req.reply.send(Err(format!("navigate failed: {e}")));
                            return;
                        }
                        // B-36: a newer navigation replaces an older one, so
                        // tell the older caller instead of dropping it.
                        if let Some(old) = self.nav.take() {
                            let _ = old.reply.send(Err("replaced by a newer navigation".into()));
                        }
                        // Resolved by the first Loading(false) after Loading(true).
                        self.nav = Some(NavWait { reply: req.reply, deadline, started: false });
                    }
                    Err(e) => {
                        let _ = req.reply.send(Err(e));
                    }
                }
            }
            "wait" => {
                let selector = req.input.get("selector").and_then(|v| v.as_str());
                let text = req.input.get("text").and_then(|v| v.as_str());
                if selector.is_none() && text.is_none() {
                    // Plain delay: no page interaction, no blocked UI thread.
                    let ms = req.input.get("ms").and_then(|v| v.as_u64()).unwrap_or(500).clamp(1, MAX_WAIT_MS);
                    let js = format!(
                        "setTimeout(function(){{window.__habi.reply({id},true,{{\"slept_ms\":{ms}}});}}, {ms});"
                    );
                    self.pending.insert(id, (req.reply, deadline));
                    let _ = wv.evaluate_script(&js);
                    return;
                }
                self.pending.insert(id, (req.reply, deadline));
                // B-36: `ms` (when given) is how long to wait for the condition.
                let budget = req.input.get("ms").and_then(|v| v.as_u64()).unwrap_or(8_000).clamp(250, MAX_WAIT_MS);
                let js = crate::browser::wait_script(id, selector, text, budget);
                let _ = wv.evaluate_script(&js);
            }
            "screenshot" => {
                // F-04: WebView2's own capture; other platforms say so.
                #[cfg(windows)]
                capture_screenshot(wv, req.reply);
                #[cfg(not(windows))]
                let _ = req.reply.send(Err("browser_screenshot needs the Windows desktop app (WebView2)".into()));
            }
            method => {
                let Some(js) = crate::browser::eval_wrapper(method, &req.input, id) else {
                    let _ = req.reply.send(Err(format!("unknown browser command `{method}`")));
                    return;
                };
                self.pending.insert(id, (req.reply, deadline));
                let _ = wv.evaluate_script(&js);
            }
        }
    }

    /// Answer a page frame. Returns true when it belonged to a live request.
    pub(super) fn on_eval(&mut self, raw: &str) -> bool {
        let Some((id, res)) = crate::browser::parse_reply(raw) else {
            return false;
        };
        match self.pending.remove(&id) {
            Some((tx, _)) => {
                let _ = tx.send(res);
                true
            }
            None => false,
        }
    }

    /// New (B-36): the panel started loading a page.
    pub(super) fn on_load_started(&mut self) {
        if let Some(nav) = self.nav.as_mut() {
            nav.started = true;
        }
    }

    /// New (B-36): the panel refused a navigation. A waiting
    /// `browser_navigate` gets the reason now instead of a timeout later.
    pub(super) fn on_blocked(&mut self, msg: &str) -> bool {
        let Some(nav) = self.nav.take() else {
            return false;
        };
        let _ = nav.reply.send(Err(msg.to_string()));
        true
    }

    /// A page finished loading: release a waiting `browser_navigate`.
    pub(super) fn on_loaded(&mut self, view: &BrowserView) -> bool {
        // B-36: ignore a "finished" that belongs to the page we are leaving.
        if !self.nav.as_ref().is_some_and(|n| n.started) {
            return false;
        }
        let Some(NavWait { reply: tx, .. }) = self.nav.take() else {
            return false;
        };
        let out = serde_json::json!({
            "ok": true,
            "url": view.url,
            "title": view.title,
        });
        let _ = tx.send(Ok(out.to_string()));
        true
    }

    /// Fail anything past its deadline (called every timed wake).
    pub(super) fn expire(&mut self) {
        let now = Instant::now();
        let stale: Vec<u64> = self.pending.iter().filter(|(_, (_, d))| *d <= now).map(|(id, _)| *id).collect();
        for id in stale {
            if let Some((tx, _)) = self.pending.remove(&id) {
                let _ = tx.send(Err("the page did not answer in time".into()));
            }
        }
        if self.nav.as_ref().is_some_and(|n| n.deadline <= now) {
            if let Some(nav) = self.nav.take() {
                let _ = nav.reply.send(Err("navigation timed out".into()));
            }
        }
    }
}

/// New (F-04): save a PNG of what the preview shows and answer the tool with
/// its path. Windows only: WebView2's own `CapturePreview` (no DevTools
/// protocol needed).
#[cfg(windows)]
pub(super) fn capture_screenshot(wv: &wry::WebView, reply: tokio::sync::oneshot::Sender<Result<String, String>>) {
    use std::cell::RefCell;
    use std::rc::Rc;
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::Win32::UI::Shell::SHCreateMemStream;
    use wry::WebViewExtWindows;

    // SAFETY: SHCreateMemStream has no preconditions; it returns an owned
    // in-memory COM stream (None when out of memory).
    let Some(stream) = (unsafe { SHCreateMemStream(None) }) else {
        let _ = reply.send(Err("could not create a memory stream".into()));
        return;
    };
    // The completion callback and the error path below share the reply:
    // whichever runs first sends it.
    let slot = Rc::new(RefCell::new(Some(reply)));
    let slot_cb = slot.clone();
    let stream_cb = stream.clone();
    let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
        let out = result
            .map_err(|e| format!("capture failed: {e}"))
            .and_then(|()| read_stream(&stream_cb))
            .and_then(|png| save_screenshot(&png));
        if let Some(tx) = slot_cb.borrow_mut().take() {
            let _ = tx.send(out);
        }
        Ok(())
    }));
    // SAFETY: the webview, `stream` and `handler` are valid COM objects;
    // WebView2 calls `handler` once, on this (the UI) thread.
    let started =
        unsafe { wv.webview().CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler) };
    if let Err(e) = started {
        if let Some(tx) = slot.borrow_mut().take() {
            let _ = tx.send(Err(format!("capture failed: {e}")));
        }
    }
}

/// New (F-04): read a whole COM stream into memory.
#[cfg(windows)]
pub(super) fn read_stream(stream: &windows::Win32::System::Com::IStream) -> Result<Vec<u8>, String> {
    use windows::Win32::System::Com::{STREAM_SEEK_END, STREAM_SEEK_SET};
    let mut size = 0u64;
    // SAFETY: plain IStream calls on a valid stream; `buf` outlives `Read`.
    unsafe {
        stream
            .Seek(0, STREAM_SEEK_END, Some(&mut size as *mut u64))
            .map_err(|e| format!("reading the capture: {e}"))?;
        stream.Seek(0, STREAM_SEEK_SET, None).map_err(|e| format!("reading the capture: {e}"))?;
        let mut buf = vec![0u8; size as usize];
        let mut read = 0u32;
        stream
            .Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read as *mut u32))
            .ok()
            .map_err(|e| format!("reading the capture: {e}"))?;
        buf.truncate(read as usize);
        Ok(buf)
    }
}

/// New (F-04): store a PNG in the temp folder and describe it for the model.
/// Platform-neutral, so it is unit-tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn save_screenshot(png: &[u8]) -> Result<String, String> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || &png[..8] != SIGNATURE {
        return Err("the capture is not a PNG image".into());
    }
    // The IHDR chunk always comes first: width and height, big-endian u32.
    let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    let dir = std::env::temp_dir().join("opencode-rust-screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = dir.join(format!("shot-{stamp}.png"));
    std::fs::write(&path, png).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "width": width,
        "height": height,
        "bytes": png.len(),
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S-12: ids are random, large, and still exact as JavaScript numbers.
    #[test]
    fn bridge_ids_are_random_and_js_safe() {
        let a = unguessable_id(1);
        let b = unguessable_id(2);
        assert_ne!(a, b);
        for id in [a, b] {
            assert!(id > 0 && id < (1u64 << 53), "{id} must survive a JS number");
        }
        assert!(a > 1_000_000 || b > 1_000_000, "not a small counter");
    }

    /// S-12: the IPC filter sees exactly the ids that are waiting.
    #[test]
    fn bridge_publishes_only_pending_ids() {
        let mut bridge = Bridge::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        bridge.pending.insert(77, (tx, Instant::now() + Duration::from_secs(5)));
        let ids: PendingIds = Arc::new(std::sync::Mutex::new(std::collections::HashSet::from([5])));
        bridge.publish_ids(&ids);
        let set = ids.lock().unwrap();
        assert!(set.contains(&77));
        assert!(!set.contains(&5), "ids that are no longer waiting are removed");
    }

    /// F-04: a capture is saved as a PNG file and described for the model.
    #[test]
    fn screenshots_are_saved_and_described() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        let out: serde_json::Value = serde_json::from_str(&save_screenshot(&png).unwrap()).unwrap();
        assert_eq!(out["width"], 640);
        assert_eq!(out["height"], 480);
        let path = std::path::PathBuf::from(out["path"].as_str().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), png);
        let _ = std::fs::remove_file(path);
        assert!(save_screenshot(b"not a png at all, sorry").is_err());
    }

    fn waiting_navigation() -> (Bridge, tokio::sync::oneshot::Receiver<Result<String, String>>) {
        let mut bridge = Bridge::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        bridge.nav = Some(NavWait { reply: tx, deadline: Instant::now() + Duration::from_secs(25), started: false });
        (bridge, rx)
    }

    /// B-36: the old page's "finished" must not answer a new navigation.
    #[test]
    fn navigate_ignores_finish_of_the_previous_page() {
        let (mut bridge, mut rx) = waiting_navigation();
        let view = BrowserView { url: "http://localhost:3000/".into(), ..Default::default() };
        assert!(!bridge.on_loaded(&view), "finished before started = the old page");
        assert!(rx.try_recv().is_err(), "still waiting");
        bridge.on_load_started();
        assert!(bridge.on_loaded(&view));
        let out = rx.try_recv().expect("answered").expect("ok");
        assert!(out.contains("localhost:3000"), "{out}");
    }

    /// B-36: a refused navigation answers at once, with the reason.
    #[test]
    fn blocked_navigation_answers_at_once() {
        let (mut bridge, mut rx) = waiting_navigation();
        assert!(bridge.on_blocked("blocked: dev-server-only"));
        let err = rx.try_recv().expect("answered").unwrap_err();
        assert!(err.contains("blocked"), "{err}");
    }

    /// B-36: every wait must end before the 25 s request deadline.
    #[test]
    fn wait_budget_stays_below_the_request_deadline() {
        const { assert!(MAX_WAIT_MS + 2_000 < 25_000) };
    }
}
