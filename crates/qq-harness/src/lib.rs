//! Shared composition infrastructure for QQ's configuration-driven hosts.
//! Configuration/model resolution and outcome driving move here in AC12.2/.3.
#![forbid(unsafe_code)]
pub mod mcp;
pub mod plan;

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
