mod access;
mod bounded_read;
mod file_state;
mod guidance;
pub mod index;
mod instructions;
mod prepare;
pub mod skills;

pub use access::WorkspacePathError;
pub(crate) use access::{Workspace, blocking_permits};
pub(crate) use bounded_read::{BoundedReadError, read_bounded};
pub(crate) use file_state::{FileState, FileStateUpdate, content_hash};
#[cfg(test)]
pub(crate) use guidance::load_entry;
pub(crate) use guidance::{
    GuidanceError, GuidanceRequest, ParsedInvocation, SelectedGuidance, parse_invocation,
    valid_name as valid_slash_name,
};
pub use instructions::WorkspaceInstructionError;
pub(crate) use instructions::{
    WorkspaceInstructions, load_with_sources as load_instructions_with_sources,
};
#[cfg(test)]
pub(crate) use prepare::test_pause_after_workspace_open;
pub(crate) use prepare::{
    WorkspacePreparationError, load_disclosed_skill, prepare_guidance, prepare_workspace,
};
#[cfg(test)]
pub(crate) use prepare::{hold_blocking_preparation, pause_blocking_preparation};
pub use skills::{SkillEntry, SkillIndex, SkillKind};
