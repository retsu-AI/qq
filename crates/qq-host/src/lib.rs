#![forbid(unsafe_code)]
pub mod builtin_auth;
mod catalog;
pub mod mcp;
pub mod plan;
pub mod runtime;

pub use catalog::ModelDiscoveryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostBuild {
    pub source_revision: &'static str,
    pub version: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum McpBuildError {
    #[error(transparent)]
    Configuration(#[from] qq_mcp::McpConfigError),
    #[error("runtime cache is unavailable")]
    CacheUnavailable,
}

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
