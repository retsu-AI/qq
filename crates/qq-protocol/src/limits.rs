pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

pub const MAX_WORKSPACE_BYTES: usize = 4096;
pub const MAX_MODEL_BYTES: usize = 512;
pub const MAX_ORGANIZATION_BYTES: usize = 512;

/// Response-body bounds a client enforces per route; the server bounds what it
/// emits to the same numbers so a well-formed reply is never refused.
pub const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_MODEL_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CAPABILITIES_BYTES: usize = 256 * 1024;
pub const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
/// One SSE wire event: the JSON envelope plus framing and field-name overhead.
pub const MAX_SSE_WIRE_EVENT_BYTES: usize = MAX_EVENT_BYTES + 16 * 1024;
