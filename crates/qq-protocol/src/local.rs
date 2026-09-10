use std::{fmt, net::SocketAddr};

use crate::{PROTOCOL_VERSION, ServerInfo};
use thiserror::Error;

const TOKEN_HEX_BYTES: usize = 64;

/// Authenticated coordinates for a running local QQ server.
///
/// This is a process-local capability, not an externally versioned HTTP wire
/// type. Its formatted representations always redact the bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct LocalServerConnection {
    address: SocketAddr,
    bearer_token: String,
    server_info: ServerInfo,
}

impl LocalServerConnection {
    pub fn new(
        address: SocketAddr,
        bearer_token: String,
        server_info: ServerInfo,
    ) -> Result<Self, LocalConnectionError> {
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(LocalConnectionError::InvalidAddress);
        }
        if bearer_token.len() != TOKEN_HEX_BYTES
            || !bearer_token
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(LocalConnectionError::InvalidToken);
        }
        if server_info.protocol_version != PROTOCOL_VERSION {
            return Err(LocalConnectionError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found: server_info.protocol_version,
            });
        }
        if !server_info.is_well_formed() {
            return Err(LocalConnectionError::InvalidServerInfo);
        }
        Ok(Self {
            address,
            bearer_token,
            server_info,
        })
    }

    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    #[must_use]
    pub const fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    #[must_use]
    pub fn endpoint(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    /// Exposes the bearer token to transport adapters that must authorize a
    /// request. Do not log or persist the returned value outside the private
    /// local-server metadata file.
    #[must_use]
    pub fn expose_bearer_token(&self) -> &str {
        &self.bearer_token
    }

    #[must_use]
    pub fn matches_bearer_token(&self, candidate: &[u8]) -> bool {
        constant_time_eq(candidate, self.bearer_token.as_bytes())
    }

    /// The transport-neutral connection a client attaches with. Loopback is
    /// the one place plain HTTP is permitted, so this cannot fail.
    #[must_use]
    pub fn to_server_connection(&self) -> ServerConnection {
        ServerConnection {
            base_url: format!("http://{}", self.address),
            credential: self.bearer_token.clone(),
            server_info: self.server_info.clone(),
        }
    }
}

impl From<LocalServerConnection> for ServerConnection {
    fn from(local: LocalServerConnection) -> Self {
        local.to_server_connection()
    }
}

/// Longest `ServerConnection` base URL accepted, in bytes.
pub const MAX_BASE_URL_BYTES: usize = 2048;
/// Longest client credential accepted, in bytes.
pub const MAX_CREDENTIAL_BYTES: usize = 512;

/// Authenticated coordinates for any QQ server a client may attach to: the
/// local loopback instance or a remote one reached over TLS.
///
/// `base_url` is `scheme://host[:port]` with no path, userinfo, query, or
/// fragment. Plain `http` is accepted only for loopback hosts; every other
/// host must use `https`, so a plaintext credential can never leave the
/// machine by construction. Formatted representations redact the credential.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConnection {
    base_url: String,
    credential: String,
    server_info: ServerInfo,
}

impl ServerConnection {
    pub fn new(
        base_url: &str,
        credential: String,
        server_info: ServerInfo,
    ) -> Result<Self, ServerConnectionError> {
        let base_url = normalize_base_url(base_url)?;
        if credential.is_empty()
            || credential.len() > MAX_CREDENTIAL_BYTES
            || !credential
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\')
        {
            return Err(ServerConnectionError::InvalidCredential);
        }
        if server_info.protocol_version != PROTOCOL_VERSION {
            return Err(ServerConnectionError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found: server_info.protocol_version,
            });
        }
        if !server_info.is_well_formed() {
            return Err(ServerConnectionError::InvalidServerInfo);
        }
        Ok(Self {
            base_url,
            credential,
            server_info,
        })
    }

    /// `scheme://host[:port]` without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub const fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.base_url.starts_with("http://")
    }

    /// `path` must begin with `/`.
    #[must_use]
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Exposes the credential to transport adapters that must authorize a
    /// request. Do not log or persist the returned value outside a client's
    /// private credential store.
    #[must_use]
    pub fn expose_credential(&self) -> &str {
        &self.credential
    }
}

impl fmt::Debug for ServerConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConnection")
            .field("base_url", &self.base_url)
            .field("credential", &"[REDACTED]")
            .field("server_info", &self.server_info)
            .finish()
    }
}

impl fmt::Display for ServerConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({}, protocol {}, credential [REDACTED])",
            self.base_url, self.server_info.display_name, self.server_info.protocol_version
        )
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ServerConnectionError {
    #[error("server URL must be http://<loopback>[:port] or https://<host>[:port] with no path")]
    InvalidBaseUrl,
    #[error("plain http is only permitted for loopback servers; use https")]
    PlaintextOffLoopback,
    #[error("client credential is invalid")]
    InvalidCredential,
    #[error("server metadata is invalid")]
    InvalidServerInfo,
    #[error("server protocol version {found} does not match client version {expected}")]
    ProtocolMismatch { expected: u16, found: u16 },
}

/// Validates `scheme://host[:port][/]` without pulling in a URL crate: the
/// grammar accepted here is deliberately narrow.
fn normalize_base_url(candidate: &str) -> Result<String, ServerConnectionError> {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate.len() > MAX_BASE_URL_BYTES {
        return Err(ServerConnectionError::InvalidBaseUrl);
    }
    let (scheme, rest) = if let Some(rest) = candidate.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = candidate.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(ServerConnectionError::InvalidBaseUrl);
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@', ' '])
        || authority.chars().any(char::is_control)
    {
        return Err(ServerConnectionError::InvalidBaseUrl);
    }
    let (host, port) = split_host_port(authority)?;
    if host.is_empty() {
        return Err(ServerConnectionError::InvalidBaseUrl);
    }
    if let Some(port) = port
        && (port.is_empty() || port.parse::<u16>().is_err() || port == "0")
    {
        return Err(ServerConnectionError::InvalidBaseUrl);
    }
    if scheme == "http" && !is_loopback_host(host) {
        return Err(ServerConnectionError::PlaintextOffLoopback);
    }
    Ok(format!("{scheme}://{authority}"))
}

fn split_host_port(authority: &str) -> Result<(&str, Option<&str>), ServerConnectionError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(ServerConnectionError::InvalidBaseUrl);
        };
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(ServerConnectionError::InvalidBaseUrl);
        }
        return match after {
            "" => Ok((host, None)),
            after => after
                .strip_prefix(':')
                .map(|port| (host, Some(port)))
                .ok_or(ServerConnectionError::InvalidBaseUrl),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host, Some(port))),
        Some(_) => Err(ServerConnectionError::InvalidBaseUrl),
        None => Ok((authority, None)),
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

impl fmt::Debug for LocalServerConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalServerConnection")
            .field("address", &self.address)
            .field("bearer_token", &"[REDACTED]")
            .field("server_info", &self.server_info)
            .finish()
    }
}

impl fmt::Display for LocalServerConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (pid {}, protocol {}, token [REDACTED])",
            self.address, self.server_info.pid, self.server_info.protocol_version
        )
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum LocalConnectionError {
    #[error("local server address must be a nonzero loopback socket")]
    InvalidAddress,
    #[error("local server bearer token is invalid")]
    InvalidToken,
    #[error("local server process metadata is invalid")]
    InvalidServerInfo,
    #[error("server protocol version {found} does not match client version {expected}")]
    ProtocolMismatch { expected: u16, found: u16 },
}

fn constant_time_eq(candidate: &[u8], expected: &[u8]) -> bool {
    let mut difference = candidate.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        let candidate_byte = candidate.get(index).copied().unwrap_or_default();
        difference |= usize::from(candidate_byte ^ expected_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreId;

    #[test]
    fn local_connection_redacts_its_bearer_token() {
        let token = "a".repeat(TOKEN_HEX_BYTES);
        let connection = LocalServerConnection::new(
            "127.0.0.1:1234".parse().unwrap(),
            token.clone(),
            ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "0.1.0".to_owned(),
                pid: 1,
                server_id: StoreId::from_bytes([9; 16]),
                display_name: "devbox".to_owned(),
            },
        )
        .unwrap();

        assert!(connection.matches_bearer_token(token.as_bytes()));
        assert!(!format!("{connection:?}").contains(&token));
        assert!(!connection.to_string().contains(&token));
    }

    #[test]
    fn server_connection_accepts_loopback_http_and_remote_https_only() {
        let info = ServerInfo {
            protocol_version: PROTOCOL_VERSION,
            version: "0.1.0".to_owned(),
            pid: 1,
            server_id: StoreId::from_bytes([9; 16]),
            display_name: "devbox".to_owned(),
        };
        let ok = |url: &str| ServerConnection::new(url, "tok-en_1".to_owned(), info.clone());
        let err = |url: &str| ok(url).unwrap_err();

        assert_eq!(
            ok("http://127.0.0.1:4000").unwrap().base_url(),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            ok("http://localhost:4000/").unwrap().base_url(),
            "http://localhost:4000"
        );
        assert_eq!(
            ok("http://[::1]:4000").unwrap().base_url(),
            "http://[::1]:4000"
        );
        assert!(ok("http://127.0.0.1:4000").unwrap().is_loopback());
        let remote = ok("https://build-box.tail1234.ts.net").unwrap();
        assert_eq!(remote.base_url(), "https://build-box.tail1234.ts.net");
        assert!(!remote.is_loopback());
        assert_eq!(
            remote.endpoint("/v1/health"),
            "https://build-box.tail1234.ts.net/v1/health"
        );
        assert!(ok("https://100.64.0.7:8443").is_ok());
        assert!(ok("https://[fd7a::1]:8443").is_ok());

        assert_eq!(
            err("http://100.64.0.7:4000"),
            ServerConnectionError::PlaintextOffLoopback
        );
        assert_eq!(
            err("http://build-box:4000"),
            ServerConnectionError::PlaintextOffLoopback
        );
        assert_eq!(err("ftp://x"), ServerConnectionError::InvalidBaseUrl);
        assert_eq!(err("https://"), ServerConnectionError::InvalidBaseUrl);
        assert_eq!(
            err("https://host/v1"),
            ServerConnectionError::InvalidBaseUrl
        );
        assert_eq!(
            err("https://host?x=1"),
            ServerConnectionError::InvalidBaseUrl
        );
        assert_eq!(
            err("https://user@host"),
            ServerConnectionError::InvalidBaseUrl
        );
        assert_eq!(err("https://host:0"), ServerConnectionError::InvalidBaseUrl);
        assert_eq!(
            err("https://host:99999"),
            ServerConnectionError::InvalidBaseUrl
        );
        assert_eq!(err("https://[::1"), ServerConnectionError::InvalidBaseUrl);
        assert_eq!(
            err("https://::1:443"),
            ServerConnectionError::InvalidBaseUrl
        );
        assert_eq!(
            err(&format!("https://{}", "h".repeat(MAX_BASE_URL_BYTES))),
            ServerConnectionError::InvalidBaseUrl
        );

        assert_eq!(
            ServerConnection::new("https://h", String::new(), info.clone()).unwrap_err(),
            ServerConnectionError::InvalidCredential
        );
        assert_eq!(
            ServerConnection::new("https://h", "has space".to_owned(), info.clone()).unwrap_err(),
            ServerConnectionError::InvalidCredential
        );
        assert_eq!(
            ServerConnection::new(
                "https://h",
                "t".to_owned(),
                ServerInfo {
                    protocol_version: PROTOCOL_VERSION + 1,
                    ..info.clone()
                }
            )
            .unwrap_err(),
            ServerConnectionError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found: PROTOCOL_VERSION + 1
            }
        );
        assert_eq!(
            ServerConnection::new("https://h", "t".to_owned(), ServerInfo { pid: 0, ..info })
                .unwrap_err(),
            ServerConnectionError::InvalidServerInfo
        );
    }

    #[test]
    fn server_connection_redacts_its_credential_and_local_conversion_is_lossless() {
        let token = "a".repeat(TOKEN_HEX_BYTES);
        let local = LocalServerConnection::new(
            "127.0.0.1:1234".parse().unwrap(),
            token.clone(),
            ServerInfo {
                protocol_version: PROTOCOL_VERSION,
                version: "0.1.0".to_owned(),
                pid: 1,
                server_id: StoreId::from_bytes([9; 16]),
                display_name: "devbox".to_owned(),
            },
        )
        .unwrap();
        let connection = local.to_server_connection();
        assert_eq!(connection.base_url(), "http://127.0.0.1:1234");
        assert_eq!(
            connection.endpoint("/v1/health"),
            local.endpoint("/v1/health")
        );
        assert_eq!(connection.expose_credential(), token);
        assert_eq!(connection.server_info(), local.server_info());
        assert!(!format!("{connection:?}").contains(&token));
        assert!(!connection.to_string().contains(&token));
    }

    #[test]
    fn local_connection_rejects_every_invalid_capability_field() {
        let valid_info = || ServerInfo {
            protocol_version: PROTOCOL_VERSION,
            version: "0.1.0".to_owned(),
            pid: 1,
            server_id: StoreId::from_bytes([9; 16]),
            display_name: "devbox".to_owned(),
        };
        let token = || "a".repeat(TOKEN_HEX_BYTES);

        assert_eq!(
            LocalServerConnection::new("192.0.2.1:1234".parse().unwrap(), token(), valid_info(),)
                .unwrap_err(),
            LocalConnectionError::InvalidAddress
        );
        assert_eq!(
            LocalServerConnection::new("127.0.0.1:0".parse().unwrap(), token(), valid_info(),)
                .unwrap_err(),
            LocalConnectionError::InvalidAddress
        );
        assert_eq!(
            LocalServerConnection::new(
                "127.0.0.1:1234".parse().unwrap(),
                "a".repeat(TOKEN_HEX_BYTES - 1),
                valid_info(),
            )
            .unwrap_err(),
            LocalConnectionError::InvalidToken
        );
        assert_eq!(
            LocalServerConnection::new(
                "127.0.0.1:1234".parse().unwrap(),
                token(),
                ServerInfo {
                    pid: 0,
                    ..valid_info()
                },
            )
            .unwrap_err(),
            LocalConnectionError::InvalidServerInfo
        );
        assert_eq!(
            LocalServerConnection::new(
                "127.0.0.1:1234".parse().unwrap(),
                token(),
                ServerInfo {
                    display_name: "bad\u{7}name".to_owned(),
                    ..valid_info()
                },
            )
            .unwrap_err(),
            LocalConnectionError::InvalidServerInfo
        );
        assert_eq!(
            LocalServerConnection::new(
                "127.0.0.1:1234".parse().unwrap(),
                token(),
                ServerInfo {
                    protocol_version: PROTOCOL_VERSION + 1,
                    ..valid_info()
                },
            )
            .unwrap_err(),
            LocalConnectionError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                found: PROTOCOL_VERSION + 1,
            }
        );
    }
}
