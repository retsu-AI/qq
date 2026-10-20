#![forbid(unsafe_code)]

// Desktop composition and all of its host-only modules are absent on mobile.
#[cfg(not(mobile))]
mod desktop;

#[cfg(not(mobile))]
pub use desktop::run;

// The mobile library deliberately registers no host filesystem, engine or
// credential commands until the maintainer-owned enrollment/TLS contracts land.
#[cfg(mobile)]
mod mobile;

#[cfg(mobile)]
pub use mobile::run;
