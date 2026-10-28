//! Composition for QQ's configuration-driven hosts: configuration and
//! credentials in, a `RuntimeLoader`, approval reviewer, workspace grant
//! authority and `ServerHandler` out. The `qq` binary is its first consumer.
#![forbid(unsafe_code)]
pub mod builtin_auth;
mod catalog;
pub mod mcp;
pub mod plan;
pub mod runtime;

pub use catalog::ModelDiscoveryError;

/// The executable hosting the runtime. Process provenance pins the source
/// revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostBuild {
    /// Full source revision, with `-dirty` when the tree was modified.
    pub source_revision: &'static str,
    /// `<crate version>+<short sha>.<commit date>`.
    pub version: &'static str,
}

/// Failure to construct the configured MCP bridge, not a model-discovery error.
#[derive(Debug, thiserror::Error)]
pub enum McpBuildError {
    #[error(transparent)]
    Configuration(#[from] qq_mcp::McpConfigError),
    #[error("runtime cache is unavailable")]
    CacheUnavailable,
}

/// Secret-free endpoint identity; never includes userinfo, query or fragment.
pub fn describe_endpoint(endpoint: &str) -> String {
    match reqwest::Url::parse(endpoint) {
        Ok(url) => {
            let mut described = format!("{}://", url.scheme());
            if let Some(host) = url.host_str() {
                described.push_str(host);
            }
            if let Some(port) = url.port() {
                described.push(':');
                described.push_str(&port.to_string());
            }
            described.push_str(url.path());
            described
        }
        Err(_) => endpoint.split_once("://").map_or_else(
            || "unparseable".to_owned(),
            |(scheme, _)| format!("{scheme}://<unparseable>"),
        ),
    }
}
