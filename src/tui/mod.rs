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
    event::{self, Event, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
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
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
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
    let terminal = Terminal::new(backend)?;

    // crossterm events arrive on this thread via std blocking read; poll with timeout
    let mut terminal2 = terminal;
    loop {
        app.tick = app.tick.wrapping_add(1);
        terminal2.draw(|f| draw(f, app))?;

        // Drain pending server events without blocking.
        while let Ok(ev) = event_rx.try_recv() {
            app.on_event(ev);
        }

        // Poll for a key with a short timeout so server events stay responsive.
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(action) = app.on_key(key) {
                        let _ = action_tx.send(action);
                    }
                }
                // B-35: pasted text is inserted as it is; its line breaks
                // stay inside the message instead of pressing Enter.
                Event::Paste(text) => app.on_paste(&text),
                _ => {}
            }
        }

        if app.should_quit {
            break;
        }
    }

    // `_guard` restores the terminal when this function returns.
    Ok(())
}
