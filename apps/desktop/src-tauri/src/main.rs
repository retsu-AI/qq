// No console window behind the app in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

// Keep desktop composition in the binary, including its Windows resources.
#[cfg(not(mobile))]
mod desktop;

#[cfg(not(mobile))]
fn main() {
    desktop::run();
}

#[cfg(mobile)]
fn main() {
    qq_desktop_lib::run();
}
