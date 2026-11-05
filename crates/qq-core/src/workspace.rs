mod access;
pub(crate) mod atomic_write;
mod bounded_read;
mod file_state;
mod guidance;
pub mod index;
mod instructions;
mod prepare;
pub mod skills;

pub use access::WorkspacePathError;
#[cfg(not(target_os = "linux"))]
pub(crate) use access::blocking_capacity;
pub(crate) use access::{Workspace, WriteTarget, blocking_permits};
#[cfg(test)]
pub(crate) use atomic_write::hold_tool_apply;
pub(crate) use atomic_write::{ExpectedFile, apply_atomically};
pub(crate) use bounded_read::{BoundedReadError, read_bounded};
pub(crate) use file_state::{FileState, FileStateUpdate, content_hash};
#[cfg(test)]
pub(crate) use guidance::load_entry;
pub(crate) use guidance::{
    GuidanceError, GuidanceRequest, ParsedInvocation, SelectedGuidance, parse_invocation,
    valid_name as valid_slash_name,
};
pub use instructions::WorkspaceInstructionError;
#[doc(hidden)]
pub use instructions::bench_support as scoped_instructions_bench_support;
pub(crate) use instructions::{
    ScopedInstructions, WorkspaceInstructions, load_with_sources as load_instructions_with_sources,
};
#[cfg(test)]
pub(crate) use prepare::test_pause_after_workspace_open;
pub(crate) use prepare::{
    WorkspacePreparationError, load_disclosed_skill, prepare_guidance, prepare_workspace,
};
#[cfg(test)]
pub(crate) use prepare::{hold_blocking_preparation, pause_blocking_preparation};
pub use skills::{SkillEntry, SkillIndex, SkillKind};
