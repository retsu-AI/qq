//! Terminal user interface and client-side state.

#![forbid(unsafe_code)]

mod app;
#[cfg(feature = "bench-support")]
pub mod bench_support;
mod commands;
mod composer;
mod effect;
#[cfg(any(test, feature = "bench-support"))]
pub mod fixtures;
mod input;
mod lazy_port;
mod picker;
mod render;
mod settings;
mod terminal;
mod theme;
mod view;
mod viewport;

pub use app::{ModelOption, TuiError, TuiOptions, run};
pub use lazy_port::LazyPort;
pub use qq_client::{ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState};
pub use settings::{Action, KeyChord, Settings, SettingsBuilder, SettingsError, StatusItem};
pub use theme::{Palette, Theme, ThemeColor, ThemeRoles};
