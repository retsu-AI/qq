use crate::{StoreId, WorkspaceId, WorkspaceSummary};
use serde::{Deserialize, Serialize};

pub const MAX_WORKSPACE_PAGE: u16 = 100;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCatalog {
    pub store_id: StoreId,
    pub workspaces: Vec<WorkspaceSummary>,
    pub next: Option<WorkspaceCatalogCursor>,
}
