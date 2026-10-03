//! Project trust (ADR-0042). A project's `.qq/config.ron` can choose models,
//! add providers and MCP servers, and pre-approve commands, so QQ ignores it
//! until the user trusts that exact content. Trust is a decision about files
//! on the host that runs the server; the desktop app supervises a local
//! server, so it scans and records trust with the same `qq-config` calls as
//! `qq trust`. The server reads the trust record on every load, so a grant
//! applies as soon as the project re-reads its catalog.
//!
//! "Trust for this session" is not offered: that grant lives inside the
//! server process, which this app does not share.

use std::path::Path;

use qq_config::{ConfigError, ConfigLoader, LoadRequest, PendingTrust};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("the trust task stopped unexpectedly")]
    TaskFailed,
}

impl Serialize for TrustError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// One project file waiting for trust, described without secrets.
#[derive(Debug, Serialize)]
pub struct PendingFileDto {
    /// The file as the user reads it.
    pub path: String,
    /// The sensitive keys it sets (`model`, `mcp`, `policy.allow_tools`, …).
    pub sections: Vec<&'static str>,
    /// One line per route, provider, MCP server, grant group, or pack list,
    /// worded exactly as `qq trust` and the TUI word them.
    pub declarations: Vec<String>,
}

fn request(workspace: &Path) -> Result<LoadRequest, TrustError> {
    // The same environment-derived request the server builds for this
    // workspace, so both scan the same files (`QQ_CONFIG` included).
    Ok(LoadRequest::from_process_env(workspace, None)?)
}

fn describe(pending: &PendingTrust) -> PendingFileDto {
    let source = pending.source();
    PendingFileDto {
        path: source
            .path()
            .map_or_else(|| source.label().to_owned(), crate::view::display_path),
        sections: pending.sections().to_vec(),
        declarations: pending
            .declarations()
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}

/// Blocking: reads the project's configuration files and the trust record.
pub fn pending(workspace: &Path) -> Result<Vec<PendingFileDto>, TrustError> {
    let loader = ConfigLoader::system()?;
    Ok(loader
        .pending_trust(&request(workspace)?)?
        .iter()
        .map(describe)
        .collect())
}

/// Blocking: records trust for every pending file, as `qq trust` does.
/// Returns what was trusted.
pub fn grant(workspace: &Path) -> Result<Vec<PendingFileDto>, TrustError> {
    let loader = ConfigLoader::system()?;
    Ok(loader
        .grant_pending_trust(&request(workspace)?)?
        .iter()
        .map(describe)
        .collect())
}
