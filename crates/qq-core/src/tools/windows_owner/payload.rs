//! Restore frozen command inputs after Windows creates the contained process.
use super::{arguments, directory, read_packet, unix_ms};
use std::{
    fs::File,
    io,
    os::windows::{io::AsHandle as _, process::CommandExt as _},
    process::{Command, Stdio},
};

pub(super) fn execute() -> io::Result<u8> {
    let token = firehazard::token::open_process_token(
        firehazard::process::get_current_process(),
        firehazard::token::QUERY,
    )
    .map_err(io::Error::from)?;
    if !token.is_app_container().map_err(io::Error::from)? {
        return Err(io::Error::other(
            "native payload entry requires an AppContainer",
        ));
    }
    // Read exactly the bounded packet; leave the original stdin bytes for the
    // actual command, without a buffered reader consuming its prefix.
    let mut input = File::from(std::io::stdin().as_handle().try_clone_to_owned()?);
    let packet = read_packet(&mut input)?;
    let cwd = crate::workspace::Workspace::open(&packet.cwd)?;
    if crate::command_owner::NativeWorkspaceIdentity::held(cwd.root())? != packet.cwd_identity {
        return Err(io::Error::other("native working directory changed"));
    }
    if unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native authorization expired"));
    }
    let line = arguments::command_line(&packet.program, &packet.arguments)?;
    let image = arguments::command_line(&packet.program, &[])?;
    let mut command = Command::new(&packet.program);
    command
        .raw_arg(line[image.len()..].trim_start())
        .env_clear()
        .envs(packet.environment)
        .current_dir(directory::working_directory(&packet.program, &packet.cwd)?)
        .stdin(Stdio::from(input))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // This child inherits the already installed token, native security
    // environment, and owned jobs. No new AppContainer creation can rewrite
    // its frozen profile paths. The outer worker still confirms full teardown.
    let status = command.spawn()?.wait()?;
    std::process::exit(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_payload_entry_outside_containment_before_reading_stdin() {
        let error = super::execute().unwrap_err();
        assert!(error.to_string().contains("requires an AppContainer"));
    }
}
