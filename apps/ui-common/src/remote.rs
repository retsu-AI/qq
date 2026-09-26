//! The remote contract (ADR-0017): what a shell and an independently
//! deployed remote agree on, and nothing about how either is built.
//!
//! A remote is an ES module emitted by wasm-bindgen that exports:
//!
//! ```text
//! default(init)                                    // the wasm-bindgen initializer
//! mount(root: HTMLElement, config: string) -> void // throws on a bad config
//! unmount() -> void
//! ```
//!
//! `config` is the JSON form of [`RemoteConfig`]. The shell owns server
//! profiles and credentials; a remote receives only the servers it should
//! bind and opens its own `qq-client` connections against them. Separate wasm
//! instances share state by protocol, never by memory.
//!
//! The shell discovers remotes from a [`RemoteManifest`] (`remotes.json` next
//! to the shell's `index.html`), whose entries may point at any origin that
//! serves the module and its wasm with CORS.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Bumped when `RemoteConfig` changes incompatibly. A remote refuses a
/// config from another contract version rather than guessing.
pub const REMOTE_CONTRACT_VERSION: u16 = 1;

/// Most servers a shell hands to one remote; matches the W3 `ServerSet` bound.
pub const MAX_REMOTE_SERVERS: usize = 16;
/// Most remotes a manifest may list.
pub const MAX_REMOTES: usize = 16;
/// Longest `config` string a remote accepts, in bytes.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// What the shell passes to `mount`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub contract: u16,
    /// Servers the remote should bind, in display order. Empty is legal: the
    /// remote renders its "no servers" state.
    pub servers: Vec<RemoteServer>,
}

/// One server the remote may talk to. `server_id` is the durable identity
/// from `ServerInfo`; `base_url` is `scheme://host[:port]`; `credential` is
/// the bearer token the remote presents. It travels in memory between two
/// wasm instances on the same page and must never be written to a URL,
/// storage the shell does not own, or a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteServer {
    pub server_id: String,
    pub display_name: String,
    pub base_url: String,
    pub credential: String,
}

impl RemoteConfig {
    #[must_use]
    pub fn new(servers: Vec<RemoteServer>) -> Self {
        Self {
            contract: REMOTE_CONTRACT_VERSION,
            servers,
        }
    }

    /// Parses and validates the `config` argument of `mount`.
    pub fn parse(json: &str) -> Result<Self, RemoteConfigError> {
        if json.len() > MAX_CONFIG_BYTES {
            return Err(RemoteConfigError::TooLarge { bytes: json.len() });
        }
        let config: Self = match serde_json::from_str(json) {
            Ok(config) => config,
            Err(error) => return Err(RemoteConfigError::Malformed(error.to_string())),
        };
        if config.contract != REMOTE_CONTRACT_VERSION {
            return Err(RemoteConfigError::Contract {
                expected: REMOTE_CONTRACT_VERSION,
                found: config.contract,
            });
        }
        if config.servers.len() > MAX_REMOTE_SERVERS {
            return Err(RemoteConfigError::TooManyServers {
                count: config.servers.len(),
            });
        }
        if let Some(server) = config.servers.iter().find(|server| {
            server.server_id.is_empty()
                || server.base_url.is_empty()
                || server.credential.is_empty()
        }) {
            return Err(RemoteConfigError::IncompleteServer {
                server_id: server.server_id.clone(),
            });
        }
        Ok(config)
    }

    /// The JSON the shell passes to `mount`.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| String::from("{}"))
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum RemoteConfigError {
    #[error("remote config is {bytes} bytes; the limit is {MAX_CONFIG_BYTES}")]
    TooLarge { bytes: usize },
    #[error("remote config is not valid JSON: {0}")]
    Malformed(String),
    #[error("remote contract {found} is not supported; this remote speaks contract {expected}")]
    Contract { expected: u16, found: u16 },
    #[error("remote config lists {count} servers; the limit is {MAX_REMOTE_SERVERS}")]
    TooManyServers { count: usize },
    #[error("remote config server {server_id:?} is missing an id, address, or credential")]
    IncompleteServer { server_id: String },
}

/// `remotes.json`: the remotes a shell offers and where each is deployed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteManifest {
    pub remotes: Vec<RemoteEntry>,
}

/// `module` and `wasm` are URLs, absolute or relative to the shell's
/// `index.html`. `name` is the stable key used in the shell's route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub name: String,
    pub title: String,
    pub module: String,
    pub wasm: String,
}

impl RemoteManifest {
    pub fn parse(json: &str) -> Result<Self, RemoteManifestError> {
        if json.len() > MAX_CONFIG_BYTES {
            return Err(RemoteManifestError::TooLarge { bytes: json.len() });
        }
        let manifest: Self = match serde_json::from_str(json) {
            Ok(manifest) => manifest,
            Err(error) => return Err(RemoteManifestError::Malformed(error.to_string())),
        };
        if manifest.remotes.len() > MAX_REMOTES {
            return Err(RemoteManifestError::TooManyRemotes {
                count: manifest.remotes.len(),
            });
        }
        for (index, entry) in manifest.remotes.iter().enumerate() {
            let name_ok = !entry.name.is_empty()
                && entry
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
            if !name_ok
                || entry.title.is_empty()
                || entry.module.is_empty()
                || entry.wasm.is_empty()
            {
                return Err(RemoteManifestError::InvalidEntry {
                    name: entry.name.clone(),
                });
            }
            if manifest.remotes[..index]
                .iter()
                .any(|earlier| earlier.name == entry.name)
            {
                return Err(RemoteManifestError::DuplicateName {
                    name: entry.name.clone(),
                });
            }
        }
        Ok(manifest)
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum RemoteManifestError {
    #[error("remotes.json is {bytes} bytes; the limit is {MAX_CONFIG_BYTES}")]
    TooLarge { bytes: usize },
    #[error("remotes.json is not valid JSON: {0}")]
    Malformed(String),
    #[error("remotes.json lists {count} remotes; the limit is {MAX_REMOTES}")]
    TooManyRemotes { count: usize },
    #[error(
        "remotes.json entry {name:?} needs a kebab-case name, a title, a module URL, and a wasm URL"
    )]
    InvalidEntry { name: String },
    #[error("remotes.json lists {name:?} twice")]
    DuplicateName { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(id: &str) -> RemoteServer {
        RemoteServer {
            server_id: id.to_owned(),
            display_name: id.to_owned(),
            base_url: "https://build-box.example".to_owned(),
            credential: "token-1234".to_owned(),
        }
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = RemoteConfig::new(vec![server("a"), server("b")]);
        assert_eq!(RemoteConfig::parse(&config.to_json()), Ok(config));
        assert_eq!(
            RemoteConfig::parse(&RemoteConfig::new(vec![]).to_json())
                .unwrap()
                .servers,
            vec![]
        );
    }

    #[test]
    fn config_refuses_other_contracts_and_bad_servers() {
        let json = r#"{"contract":2,"servers":[]}"#;
        assert_eq!(
            RemoteConfig::parse(json),
            Err(RemoteConfigError::Contract {
                expected: REMOTE_CONTRACT_VERSION,
                found: 2
            })
        );
        assert!(matches!(
            RemoteConfig::parse("{"),
            Err(RemoteConfigError::Malformed(_))
        ));
        let mut incomplete = server("a");
        incomplete.credential.clear();
        let json = RemoteConfig::new(vec![incomplete]).to_json();
        assert_eq!(
            RemoteConfig::parse(&json),
            Err(RemoteConfigError::IncompleteServer {
                server_id: "a".to_owned()
            })
        );
        let json = RemoteConfig::new(
            (0..=MAX_REMOTE_SERVERS)
                .map(|i| server(&i.to_string()))
                .collect(),
        )
        .to_json();
        assert_eq!(
            RemoteConfig::parse(&json),
            Err(RemoteConfigError::TooManyServers {
                count: MAX_REMOTE_SERVERS + 1
            })
        );
        let oversized = format!(
            "{{\"contract\":1,\"servers\":[],\"pad\":\"{}\"}}",
            "x".repeat(MAX_CONFIG_BYTES)
        );
        assert!(matches!(
            RemoteConfig::parse(&oversized),
            Err(RemoteConfigError::TooLarge { .. })
        ));
    }

    #[test]
    fn manifest_validates_names_and_rejects_duplicates() {
        let json = r#"{"remotes":[
            {"name":"sessions","title":"Sessions","module":"remotes/sessions/qq-sessions.js","wasm":"remotes/sessions/qq-sessions_bg.wasm"}
        ]}"#;
        let manifest = RemoteManifest::parse(json).unwrap();
        assert_eq!(manifest.remotes[0].name, "sessions");

        let bad_name = json.replace("\"sessions\"", "\"Sessions Remote\"");
        assert_eq!(
            RemoteManifest::parse(&bad_name),
            Err(RemoteManifestError::InvalidEntry {
                name: "Sessions Remote".to_owned()
            })
        );
        let entry = r#"{"name":"sessions","title":"Sessions","module":"a.js","wasm":"a.wasm"}"#;
        let duplicate = format!("{{\"remotes\":[{entry},{entry}]}}");
        assert_eq!(
            RemoteManifest::parse(&duplicate),
            Err(RemoteManifestError::DuplicateName {
                name: "sessions".to_owned()
            })
        );
    }
}
