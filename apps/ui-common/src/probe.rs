//! The authenticated handshake with one `qq serve`: `GET /v1/health` with the
//! bearer credential, the protocol check, then a validated `ServerConnection`.
//! Every failure carries a message written for the person at the keyboard;
//! nothing here logs or echoes the credential.

use qq_protocol::{ServerConnection, ServerConnectionError, ServerInfo};
use thiserror::Error;

use crate::compat::{self, IncompatibleServer};

/// Longest `/v1/health` body accepted, in bytes; the real one is well under 1 KiB.
pub const MAX_HEALTH_BYTES: usize = 4096;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ProbeError {
    #[error("{0}")]
    InvalidAddress(ServerConnectionError),
    #[error("could not reach {base_url}: {reason}")]
    Unreachable { base_url: String, reason: String },
    #[error("{base_url} rejected the credential")]
    Unauthorized { base_url: String },
    #[error("{base_url} answered {status} to the health probe; is this a qq server?")]
    UnexpectedStatus { base_url: String, status: u16 },
    #[error("{base_url} returned an unreadable health response; is this a qq server?")]
    MalformedHealth { base_url: String },
    #[error("{0}")]
    Incompatible(IncompatibleServer),
    #[error("{base_url} advertised invalid server metadata")]
    InvalidServerInfo { base_url: String },
}

/// Validates the address and credential locally, probes the server, and
/// returns the connection every `qq-client` call is built from.
pub async fn probe(base_url: &str, credential: &str) -> Result<ServerConnection, ProbeError> {
    // Validate before any network request so a malformed address or an
    // unusable credential is reported without leaking the attempt off-box.
    let placeholder = ServerInfo {
        protocol_version: qq_protocol::PROTOCOL_VERSION,
        version: "probe".to_owned(),
        pid: 1,
        server_id: qq_protocol::StoreId::from_bytes([0; 16]),
        display_name: "probe".to_owned(),
    };
    let shape = ServerConnection::new(base_url, credential.to_owned(), placeholder)
        .map_err(ProbeError::InvalidAddress)?;
    let base_url = shape.base_url().to_owned();
    let http = reqwest::Client::new();
    let response = match http
        .get(shape.endpoint("/v1/health"))
        .bearer_auth(credential)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return Err(ProbeError::Unreachable {
                reason: reason_without_url(&error),
                base_url,
            });
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ProbeError::Unauthorized { base_url });
    }
    if status != reqwest::StatusCode::OK {
        return Err(ProbeError::UnexpectedStatus {
            base_url,
            status: status.as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HEALTH_BYTES as u64)
    {
        return Err(ProbeError::MalformedHealth { base_url });
    }
    let bytes = match response.bytes().await {
        Ok(bytes) if bytes.len() <= MAX_HEALTH_BYTES => bytes,
        Ok(_) | Err(_) => return Err(ProbeError::MalformedHealth { base_url }),
    };
    let Ok(info) = serde_json::from_slice::<ServerInfo>(&bytes) else {
        return Err(ProbeError::MalformedHealth { base_url });
    };
    if let Err(incompatible) = compat::check(&info) {
        return Err(ProbeError::Incompatible(incompatible));
    }
    match ServerConnection::new(&base_url, credential.to_owned(), info) {
        Ok(connection) => Ok(connection),
        Err(ServerConnectionError::InvalidServerInfo) => {
            Err(ProbeError::InvalidServerInfo { base_url })
        }
        Err(other) => Err(ProbeError::InvalidAddress(other)),
    }
}

/// reqwest's browser errors embed the full request URL; the message already
/// names the server and must never carry a query string or credential.
fn reason_without_url(error: &reqwest::Error) -> String {
    let text = error.to_string();
    match text.split_once(" for url (") {
        Some((head, _)) => head.to_owned(),
        None => text,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn a_bad_address_fails_before_any_request() {
        let error = futures_executor::block_on(probe("ftp://nope", "token-1234"));
        assert_eq!(
            error,
            Err(ProbeError::InvalidAddress(
                ServerConnectionError::InvalidBaseUrl
            ))
        );
        let error = futures_executor::block_on(probe("https://build-box.example", ""));
        assert_eq!(
            error,
            Err(ProbeError::InvalidAddress(
                ServerConnectionError::InvalidCredential
            ))
        );
        let error = futures_executor::block_on(probe("http://build-box.example", "token-1234"));
        assert_eq!(
            error,
            Err(ProbeError::InvalidAddress(
                ServerConnectionError::PlaintextOffLoopback
            ))
        );
    }

    #[test]
    fn messages_name_the_server_and_never_the_credential() {
        let unauthorized = ProbeError::Unauthorized {
            base_url: "https://build-box.example".to_owned(),
        };
        assert_eq!(
            unauthorized.to_string(),
            "https://build-box.example rejected the credential"
        );
        let unexpected = ProbeError::UnexpectedStatus {
            base_url: "https://build-box.example".to_owned(),
            status: 404,
        };
        assert_eq!(
            unexpected.to_string(),
            "https://build-box.example answered 404 to the health probe; is this a qq server?"
        );
    }
}
