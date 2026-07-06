//! aurveto core library, shared by the CLI, TUI and GUI frontends.

pub mod ai;
pub mod aur;
pub mod config;
pub mod deploy;
pub mod i18n;
pub mod pipeline;
pub mod scan;

#[cfg(feature = "tui")]
pub mod tui;
