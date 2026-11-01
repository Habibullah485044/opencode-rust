//! The running app: what the event loop owns, and one pass of the loop —
//! drain server events, page messages and agent browser requests, then
//! repaint once.

use super::*;

/// Everything the event loop owns. `run` builds it and moves it into the loop.
pub(super) struct App {
    pub(super) gs: GuiState,
    pub(super) st: Arc<server::AppState>,
    pub(super) rt: tokio::runtime::Handle,
    pub(super) window: tao::window::Window,
    /// The main webview (our page).
    pub(super) webview: wry::WebView,
    /// The preview panel's native child view (`None` = disabled or failed).
    pub(super) browser_webview: Option<wry::WebView>,
    pub(super) browser_enabled: bool,
    pub(super) bridge: Bridge,
    /// S-12: shared between the bridge (writes) and the preview IPC handler (reads).
    pub(super) bridge_ids: PendingIds,
    /// The interface zoom now (Ctrl+wheel); saved when it changes.
    pub(super) page_zoom_now: f64,
    /// B-11: the `covered` value the native view got last time.
    pub(super) covered_applied: bool,
    /// Server events, forwarded from the core's bus.
    pub(super) evt_rx: tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    /// Page messages and preview-webview callbacks.
    pub(super) ui_rx: std::sync::mpsc::Receiver<UiMsg>,
    /// Goal B: requests from agent browser tools.
    pub(super) cmd_rx: tokio::sync::mpsc::UnboundedReceiver<crate::browser::Request>,
    /// Results of work done off this thread (`off_thread`), and its waker.
    pub(super) done_tx: std::sync::mpsc::Sender<Done>,
    pub(super) done_rx: std::sync::mpsc::Receiver<Done>,
    pub(super) proxy: tao::event_loop::EventLoopProxy<AppEvent>,
}

/// Work finished on the blocking pool, applied on the event loop. True = repaint.
pub(super) type Done = Box<dyn FnOnce(&mut GuiState, &server::AppState) -> bool + Send>;

/// Run `work` on the blocking pool (git, disk scans), then hand `apply` its
/// result through `tx` and `wake` the loop, which never waits for it.
pub(super) fn off_thread<T: Send + 'static>(
    rt: &tokio::runtime::Handle,
    tx: std::sync::mpsc::Sender<Done>,
    wake: impl FnOnce() + Send + 'static,
    work: impl FnOnce() -> T + Send + 'static,
    apply: impl FnOnce(&mut GuiState, &server::AppState, T) -> bool + Send + 'static,
) {
    rt.spawn_blocking(move || {
        let out = work();
        let done: Done = Box::new(move |gs, st| apply(gs, st, out));
        let _ = tx.send(done);
        wake();
    });
}

impl App {
    /// One event-loop callback.
    pub(super) fn on_loop_event(&mut self, event: Event<'_, AppEvent>, control_flow: &mut ControlFlow) {
        // While a modal countdown is on screen the loop has to wake up on its
        // own; otherwise a heartbeat-less idle window freezes the timer.
        // Never overwrite `Exit`: every callback starts here, so resetting it
        // would strand a windowless process after the user closes the window.
        if !matches!(*control_flow, ControlFlow::Exit) {
            let term_live = self.gs.term_open && crate::tools::jobs::any_running();
            *control_flow = if self.gs.approval.is_some() || self.bridge.busy() || term_live {
                ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(500))
            } else {
                ControlFlow::Wait
            };
        }
        match event {
            Event::UserEvent(AppEvent::Wake) | Event::MainEventsCleared => self.tick(control_flow),
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                gui_log("[gui] close requested");
                self.close(control_flow);
            }
            // tao/winit signal a dead message loop this way; without it a
            // closed window can leave the process running invisibly.
            Event::LoopDestroyed => {
                gui_log("[gui] window closed — exiting");
                notify_cleanup(&self.window);
                // The event loop never returns, so main's stop_all is never
                // reached: dev servers the agent started would outlive the window.
                crate::tools::jobs::stop_all_now();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    }

    /// [`off_thread`] with this loop's channel and waker.
    pub(super) fn off_thread<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        apply: impl FnOnce(&mut GuiState, &server::AppState, T) -> bool + Send + 'static,
    ) {
        let proxy = self.proxy.clone();
        let wake = move || {
            let _ = proxy.send_event(AppEvent::Wake);
        };
        off_thread(&self.rt, self.done_tx.clone(), wake, work, apply);
    }

    /// Exit, after asking first while a run is going (closing stops it).
    pub(super) fn close(&self, control_flow: &mut ControlFlow) {
        if confirm_close(&self.window, &self.gs) {
            notify_cleanup(&self.window);
            *control_flow = ControlFlow::Exit;
        }
    }

    /// A wake-up: apply everything that arrived, then paint once if needed.
    fn tick(&mut self, control_flow: &mut ControlFlow) {
        let mut changed = self.drain_server_events();
        // B-12: another client added or switched a chat.
        if self.gs.sessions_stale {
            self.gs.sessions_stale = false;
            refresh_sessions(&mut self.gs, &self.st);
        }
        // A running command's new output reaches an open terminal.
        send_term(&self.webview, &self.gs);
        if self.gs.git_stale {
            self.gs.git_stale = false;
            let root = chat_root(&self.gs, &self.st);
            *self.gs.term_root.borrow_mut() = root.clone();
            // T-066: two git processes — on the blocking pool, not here.
            self.off_thread(
                move || git_state(&root),
                |gs, _, git| {
                    gs.git = git;
                    true
                },
            );
        }
        while let Ok(msg) = self.ui_rx.try_recv() {
            changed |= self.dispatch(msg, control_flow);
        }
        while let Ok(done) = self.done_rx.try_recv() {
            changed |= done(&mut self.gs, &self.st);
        }
        // Goal B: agent tools ask this loop to act on the webview.
        while let Ok(req) = self.cmd_rx.try_recv() {
            self.bridge.start(&self.browser_webview, req, &mut self.gs.browser, self.st.cfg.browser.allow_remote);
            changed = true;
        }
        // Requests that never get an answer must not hang the run.
        self.bridge.expire();
        // S-12: after start/expire/answers, publish which ids may reply.
        self.bridge.publish_ids(&self.bridge_ids);
        // B-13: a modal whose timeout passed long ago cannot block the
        // window forever, even if its "resolved" event was lost.
        if self.gs.expire_stale_approval(Instant::now()) {
            changed = true;
        }
        // A live countdown forces a repaint on every timed wake.
        if self.gs.approval.is_some() {
            changed = true;
        }
        // B-11: the approval modal is HTML, and the native preview
        // view paints above all HTML. Hide the view while a modal is
        // open and bring it back after — before the page repaints.
        let covered = self.gs.approval.is_some();
        if covered != self.covered_applied {
            self.covered_applied = covered;
            self.gs.browser.covered = covered;
            if let Some(wv) = &self.browser_webview {
                show_browser(wv, &self.gs.browser);
            }
        }
        if changed {
            paint(&self.webview, &self.gs);
        }
    }

    /// Server events into the chats they belong to. True = any arrived.
    fn drain_server_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.evt_rx.try_recv() {
            // Something waits on the user (or a run ended) while they
            // look elsewhere: flash the taskbar button (it does not
            // steal focus) and show a notification.
            if !self.window.is_focused() {
                if let Some((title, body)) = notice_for(&ev) {
                    if !matches!(ev, ServerEvent::RunFinished { .. }) {
                        self.window.request_user_attention(Some(tao::window::UserAttentionType::Critical));
                    }
                    notify(&self.window, title, &body);
                }
            }
            self.gs.on_event(ev);
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-019: the work runs on the blocking pool, not the caller's thread,
    /// and its result is applied by whoever drains the channel (the loop).
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn off_thread_runs_the_work_elsewhere_and_posts_the_result() {
        let st = server::build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
        let (tx, rx) = std::sync::mpsc::channel::<Done>();
        let (woke_tx, woke_rx) = std::sync::mpsc::channel();
        let here = std::thread::current().id();
        off_thread(
            &tokio::runtime::Handle::current(),
            tx,
            move || woke_tx.send(()).unwrap(),
            || std::thread::current().id(),
            move |gs, _, ran_on| {
                gs.push(EntryKind::System, format!("elsewhere={}", ran_on != here));
                true
            },
        );
        woke_rx.recv_timeout(Duration::from_secs(10)).expect("the loop was woken");
        let done = rx.try_recv().expect("a result was posted");
        let mut gs = GuiState::new();
        assert!(done(&mut gs, &st));
        assert_eq!(gs.entries.last().unwrap().1, "elsewhere=true");
    }
}
