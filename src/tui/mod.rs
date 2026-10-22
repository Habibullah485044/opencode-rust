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
use std::time::Duration;
/// Run the TUI event loop. `on_action` is invoked for user intents;
/// events come in over a channel from the SSE reader task.
pub async fn run(
    app: &mut App,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    action_tx: tokio::sync::mpsc::UnboundedSender<InputAction>,
) -> Result<()> {
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
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
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if let Some(action) = app.on_key(key) {
                        let _ = action_tx.send(action);
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    terminal2.backend_mut().execute(LeaveAlternateScreen)?;
    Ok(())
}
