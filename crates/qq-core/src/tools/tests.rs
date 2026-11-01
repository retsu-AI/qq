use std::fs;

mod argument_repairs;
mod dispatch;
mod edit;
mod read;
mod search;
mod shell;
mod tree;

use super::*;

fn run_tool(workspace: &Workspace, state: &FileState, name: &str, arguments: &str) -> ToolOutput {
    execute_blocking(
        workspace,
        state,
        name,
        arguments,
        &ToolCancellation::new(RunCancellation::new()),
    )
}
