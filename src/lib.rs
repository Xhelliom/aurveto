//! aurveto core library, shared by the CLI, TUI and GUI frontends.

/// The running build's version, shown by the frontends so the user can tell
/// which build (AUR package or dev) they are looking at.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod ai;
pub mod aur;
pub mod config;
pub mod deploy;
pub mod i18n;
pub mod pipeline;
pub mod scan;

#[cfg(feature = "tui")]
pub mod tui;
