//! Entry point for the command process; no provider or agent runtime opens here.

#[cfg(any(target_os = "linux", windows))]
pub(crate) mod client;

use std::{error::Error, io, path::PathBuf};

use qq_core::command_owner::CommandOwner;
use qq_server::{ReserveOutcome, ServerOptions, ServerPaths};

pub async fn run(directory: PathBuf) -> Result<(), Box<dyn Error>> {
    if !directory.is_absolute() {
        return Err(io::Error::other("command owner state directory must be absolute").into());
    }
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(windows)]
    let mut interrupt = tokio::signal::windows::ctrl_c()?;
    let stop = async {
        #[cfg(unix)]
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
        #[cfg(windows)]
        interrupt.recv().await;
    };
    tokio::pin!(stop);
    let options = ServerOptions::for_command_owner(ServerPaths::new(&directory))
        .with_version(crate::cli::BUILD_VERSION);
    let reservation = match qq_server::reserve(options).await? {
        ReserveOutcome::Existing(connection) => {
            println!(
                "qq command owner already running at {}",
                connection.address()
            );
            return Ok(());
        }
        ReserveOutcome::Reserved(reservation) => reservation,
    };
    let owner = CommandOwner::open(
        directory.join("commands"),
        env!("QQ_SOURCE_REVISION"),
        qq_core::command_owner::CommandLauncher::new(std::env::current_exe()?),
    )
    .await?;
    let mut server = match reservation.start_command_owner(owner.clone()) {
        Ok(server) => server,
        Err(error) => {
            let _ = owner.shutdown().await;
            return Err(error.into());
        }
    };
    println!(
        "qq command owner listening at {}",
        server.connection().address()
    );
    tokio::select! {
        stopped = server.finished() => {
            let drained = owner.shutdown().await;
            stopped?;
            drained?;
            return Ok(());
        }
        _ = &mut stop => {},
        _ = owner.wait_for_failure() => {},
    }
    server.begin_shutdown();
    let drained = owner.shutdown().await;
    let stopped = server.shutdown().await;
    stopped?;
    drained?;
    Ok(())
}
