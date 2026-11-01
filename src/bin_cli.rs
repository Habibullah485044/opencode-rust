//! Binary-only modules of `main.rs` (not part of the library): the
//! headless and TUI runners. The command steps they share with the
//! desktop app are in the library (`opencode_rust::frontend`).

pub(crate) mod headless;
pub(crate) mod tui_run;
