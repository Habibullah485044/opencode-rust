//! The TUI: state + input handling ([`state`]), drawing ([`draw`]), and the
//! terminal loop below. The `pub use` re-exports keep `tui::{App, ...}` stable.
pub mod draw;
pub mod state;
#[cfg(test)]
mod tests;

pub use draw::draw;
pub use state::*;

use crate::server::ServerEvent;
use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures::{Stream, StreamExt};
use ratatui::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// B-16: true while the TUI owns the terminal (raw mode + alternate screen).
static TERMINAL_TAKEN: AtomicBool = AtomicBool::new(false);

/// B-16: claim the "restore" job. Returns true exactly once per take.
fn release_terminal() -> bool {
    TERMINAL_TAKEN.swap(false, Ordering::SeqCst)
}

/// B-16: give the terminal back — end raw mode, leave the alternate screen,
/// show the cursor. Does nothing if the TUI does not own the terminal.
/// Every step is tried even when an earlier one fails.
fn restore_terminal() {
    if release_terminal() {
        let _ = disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = out.execute(crossterm::event::DisableBracketedPaste);
        let _ = out.execute(LeaveAlternateScreen);
        let _ = out.execute(crossterm::cursor::Show);
    }
}

/// B-16: restores the terminal when dropped — on the normal exit and on
/// every early `?` return out of [`run`].
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// B-16: restore the terminal BEFORE the panic message is printed. With
/// `panic = "abort"` no destructor runs on a panic, but the hook does. It
/// chains to the previous hook (main.rs `install_crash_log`), so the
/// message and crash.log are still written.
fn install_panic_restore() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}
/// Run the TUI event loop. `on_action` is invoked for user intents;
/// events come in over a channel from the SSE reader task.
pub async fn run(
    app: &mut App,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    action_tx: tokio::sync::mpsc::UnboundedSender<InputAction>,
) -> Result<()> {
    install_panic_restore();
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    TERMINAL_TAKEN.store(true, Ordering::SeqCst);
    // From here on, any early return (`?`) restores the terminal.
    let _guard = TerminalGuard;
    stdout.execute(EnterAlternateScreen)?;
    // B-35: a paste arrives as one `Event::Paste`, not as typed keys.
    stdout.execute(crossterm::event::EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // T-061: keys come from crossterm's async EventStream, so no tokio
    // worker is held in a blocking poll for the whole session.
    run_loop(app, &mut terminal, EventStream::new(), event_rx, action_tx).await?;
    // `_guard` restores the terminal when this function returns.
    Ok(())
}

/// T-062: how often the screen is redrawn while something on it moves
/// (the busy spinner, a running agent card's clock).
const ANIMATE_EVERY: Duration = Duration::from_millis(100);

/// The event loop behind [`run`], over any backend and input stream (tests
/// drive it with a `TestBackend`). Waits on keys, server events and — only
/// while something animates — a timer, and draws only after one of them
/// (T-062), so an idle TUI does no work. Ends on quit or when `input` ends;
/// returns how many frames it drew.
pub async fn run_loop<B, S>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    mut input: S,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    action_tx: tokio::sync::mpsc::UnboundedSender<InputAction>,
) -> Result<u64>
where
    B: Backend,
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut animate = tokio::time::interval(ANIMATE_EVERY);
    animate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut events_open = true;
    let mut dirty = true;
    let mut draws = 0u64;
    loop {
        if dirty {
            terminal.draw(|f| draw(f, app))?;
            draws += 1;
            dirty = false;
        }
        if app.should_quit {
            break;
        }
        let animating = app.busy || app.agents.iter().any(|a| a.state == TeamStatus::Working);
        tokio::select! {
            ev = event_rx.recv(), if events_open => match ev {
                Some(ev) => {
                    app.on_event(ev);
                    // Fold a burst of events into one frame.
                    while let Ok(ev) = event_rx.try_recv() {
                        app.on_event(ev);
                    }
                    dirty = true;
                }
                // The core is gone; keys still work (Ctrl+C quits).
                None => events_open = false,
            },
            key = input.next() => match key {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if let Some(action) = app.on_key(key) {
                        let _ = action_tx.send(action);
                    }
                    dirty = true;
                }
                // B-35: pasted text is inserted as it is; its line breaks
                // stay inside the message instead of pressing Enter.
                Some(Ok(Event::Paste(text))) => {
                    app.on_paste(&text);
                    dirty = true;
                }
                Some(Ok(Event::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            _ = animate.tick(), if animating => {
                // The spinner frame moves with time, not with redraws.
                app.tick = app.tick.wrapping_add(1);
                dirty = true;
            }
        }
    }
    Ok(draws)
}

/// The note shown when the TUI fell behind the core and missed events.
pub fn lagged_note(skipped: u64) -> ServerEvent {
    ServerEvent::Note {
        // The TUI shows a note whatever its session id.
        session_id: String::new(),
        text: format!("[{skipped} events skipped — /resume to reload this chat]"),
    }
}

/// Core events → the TUI's channel. `pending` lists the approvals still
/// waiting, re-sent after a lag so no prompt is silently lost (B-09); the
/// lag itself is told to the user (T-102). Ends when either side closes.
pub async fn forward_events(
    mut rx: tokio::sync::broadcast::Receiver<ServerEvent>,
    evt_tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
    pending: impl Fn() -> Vec<ServerEvent>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if evt_tx.send(ev).is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => {
                // T-102: streamed text was lost — say so, and how to recover.
                if evt_tx.send(lagged_note(n)).is_err() {
                    break;
                }
                for ev in pending() {
                    let _ = evt_tx.send(ev);
                }
            }
            Err(RecvError::Closed) => break,
        }
    }
}
