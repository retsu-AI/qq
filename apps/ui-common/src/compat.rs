//! The protocol range one build of the UI accepts.
//!
//! `ServerConnection::new` already refuses a server whose protocol differs
//! from the compiled `PROTOCOL_VERSION`; the range here is what the UI
//! *promises* and what it explains to the user before that check runs. Today
//! the range is the single compiled version. Widening it is one edit here,
//! plus whatever `qq-client` needs to actually speak the older version.

use std::{fmt, ops::RangeInclusive};

use qq_protocol::{PROTOCOL_VERSION, ServerInfo};

/// Protocol versions this UI build can drive.
pub const SUPPORTED_PROTOCOL: RangeInclusive<u16> = PROTOCOL_VERSION..=PROTOCOL_VERSION;

/// The server advertises a protocol outside [`SUPPORTED_PROTOCOL`]. The
/// `Display` text is the exact message shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompatibleServer {
    pub display_name: String,
    pub server_version: String,
    pub found: u16,
}

impl IncompatibleServer {
    /// Whether the server is older than this UI (upgrade the server) or
    /// newer (upgrade the UI); the message differs in what to do next.
    #[must_use]
    pub fn server_is_older(&self) -> bool {
        self.found < *SUPPORTED_PROTOCOL.start()
    }
}

impl fmt::Display for IncompatibleServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (start, end) = (SUPPORTED_PROTOCOL.start(), SUPPORTED_PROTOCOL.end());
        let supported = if start == end {
            format!("protocol {start}")
        } else {
            format!("protocols {start}-{end}")
        };
        let next = if self.server_is_older() {
            "Upgrade qq on that server, or use an older build of this UI."
        } else {
            "Update this UI, or run an older qq on that server."
        };
        write!(
            f,
            "{name} (qq {version}) speaks protocol {found}; this UI supports {supported}. {next}",
            name = self.display_name,
            version = self.server_version,
            found = self.found,
        )
    }
}

/// Accepts a server whose advertised protocol falls inside
/// [`SUPPORTED_PROTOCOL`].
pub fn check(info: &ServerInfo) -> Result<(), IncompatibleServer> {
    if SUPPORTED_PROTOCOL.contains(&info.protocol_version) {
        Ok(())
    } else {
        Err(IncompatibleServer {
            display_name: info.display_name.clone(),
            server_version: info.version.clone(),
            found: info.protocol_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use qq_protocol::StoreId;

    use super::*;

    fn info(protocol_version: u16) -> ServerInfo {
        ServerInfo {
            protocol_version,
            version: "0.9.0".to_owned(),
            pid: 42,
            server_id: StoreId::from_bytes([7; 16]),
            display_name: "build-box".to_owned(),
        }
    }

    #[test]
    fn the_compiled_protocol_is_supported() {
        assert_eq!(check(&info(PROTOCOL_VERSION)), Ok(()));
    }

    #[test]
    fn an_older_server_is_refused_and_told_to_upgrade() {
        let error = check(&info(PROTOCOL_VERSION - 1)).unwrap_err();
        assert!(error.server_is_older());
        let message = error.to_string();
        assert_eq!(
            message,
            format!(
                "build-box (qq 0.9.0) speaks protocol {}; this UI supports protocol {}. \
                 Upgrade qq on that server, or use an older build of this UI.",
                PROTOCOL_VERSION - 1,
                PROTOCOL_VERSION
            )
        );
    }

    #[test]
    fn a_newer_server_is_refused_and_the_ui_is_told_to_update() {
        let error = check(&info(PROTOCOL_VERSION + 1)).unwrap_err();
        assert!(!error.server_is_older());
        assert!(
            error
                .to_string()
                .ends_with("Update this UI, or run an older qq on that server.")
        );
    }
}
