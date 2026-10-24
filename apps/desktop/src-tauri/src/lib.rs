#![forbid(unsafe_code)]

// The mobile library deliberately registers no host filesystem, engine or
// credential commands until the maintainer-owned enrollment/TLS contracts land.
#[cfg(mobile)]
mod mobile;

#[cfg(mobile)]
pub use mobile::run;
