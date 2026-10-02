//! Framework-neutral glue shared by the QQ web shell and every remote.
//!
//! Three things live here and nothing else: the remote contract the shell
//! and its remotes agree on ([`remote`]), the protocol range this build of
//! the UI supports ([`compat`]), and the authenticated probe that turns a
//! server address plus credential into a validated `ServerConnection` or a
//! message a person can act on ([`probe`]). Rendering belongs to the shell
//! and the remotes; transport and reduction belong to `qq-client`.

#![forbid(unsafe_code)]

pub mod compat;
pub mod probe;
pub mod remote;
