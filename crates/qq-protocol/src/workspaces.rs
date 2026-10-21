//! Server-owned workspace discovery, independent of a client's local paths.
use crate::{StoreId, WorkspaceId, WorkspaceSummary};
use serde::{Deserialize, Serialize};

pub const MAX_WORKSPACE_PAGE: u16 = 100;

/// A page position belongs to one store, never to a path or hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCatalogCursor {
    pub store_id: StoreId,
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCatalogRequest {
    pub after: Option<WorkspaceCatalogCursor>,
    pub limit: u16,
}

impl Default for WorkspaceCatalogRequest {
    fn default() -> Self {
        Self {
            after: None,
            limit: 64,
        }
    }
}

/// Registered roots only: listing never opens directories or grants access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCatalog {
    pub store_id: StoreId,
    pub workspaces: Vec<WorkspaceSummary>,
    pub next: Option<WorkspaceCatalogCursor>,
}

impl std::fmt::Display for WorkspaceCatalogCursor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.store_id, self.workspace_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid workspace cursor; expected STORE_ID:WORKSPACE_ID")]
pub struct WorkspaceCatalogCursorError;

impl std::str::FromStr for WorkspaceCatalogCursor {
    type Err = WorkspaceCatalogCursorError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (store, workspace) = value.split_once(':').ok_or(WorkspaceCatalogCursorError)?;
        Ok(Self {
            store_id: store.parse().map_err(|_| WorkspaceCatalogCursorError)?,
            workspace_id: workspace.parse().map_err(|_| WorkspaceCatalogCursorError)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_catalog_cursor_round_trips_and_rejects_partial_or_event_cursors() {
        let cursor = WorkspaceCatalogCursor {
            store_id: StoreId::from_bytes([1; 16]),
            workspace_id: WorkspaceId::from_bytes([2; 16]),
        };
        assert_eq!(
            cursor
                .to_string()
                .parse::<WorkspaceCatalogCursor>()
                .unwrap(),
            cursor
        );
        for invalid in [
            String::new(),
            cursor.store_id.to_string(),
            format!("{}:", cursor.store_id),
            format!("{}:0", cursor.store_id),
            format!("{}:7", cursor),
        ] {
            assert_eq!(
                invalid.parse::<WorkspaceCatalogCursor>(),
                Err(WorkspaceCatalogCursorError)
            );
        }
    }
}
