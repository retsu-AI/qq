#![forbid(unsafe_code)]

mod app;
#[cfg(feature = "bench-support")]
pub mod bench_support;
mod commands;
mod composer;
#[cfg(feature = "bench-support")]
pub mod design;
mod effect;
#[cfg(any(test, feature = "bench-support"))]
pub mod fixtures;
mod herdr;
mod input;
mod lazy_port;
mod picker;
mod render;
mod settings;
mod startup;
mod terminal;
mod theme;
mod view;
mod viewport;

pub use app::{GoalDefaults, ModelOption, ProviderRemedy, TuiError, TuiOptions, run};
pub use commands::{CommandRow, command_rows, slash_names};
pub use lazy_port::LazyPort;
pub use qq_client::{
    ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState, StartupUpdate,
};
pub use settings::{Action, KeyChord, Settings, SettingsBuilder, SettingsError, StatusItem};
pub use startup::StartupPort;
pub use theme::{Palette, SyntaxOverrides, Theme, ThemeColor, ThemeRoles};
