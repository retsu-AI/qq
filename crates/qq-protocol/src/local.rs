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
