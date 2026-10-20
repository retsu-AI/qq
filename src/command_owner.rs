//! Entry point for the command process; no provider or agent runtime opens here.

use std::{error::Error, io, path::PathBuf};

use qq_core::command_owner::CommandOwner;
use qq_server::{ReserveOutcome, ServerOptions, ServerPaths};

pub async fn run(directory: PathBuf) -> Result<(), Box<dyn Error>> {
    if !directory.is_absolute() {
        return Err(io::Error::other("command owner state directory must be absolute").into());
    }
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    #[cfg(windows)]
    let mut interrupt = tokio::signal::windows::ctrl_c()?;
    let options =
        ServerOptions::new(ServerPaths::new(&directory)).with_version(crate::cli::BUILD_VERSION);
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
    let owner = CommandOwner::open(directory.join("commands"), env!("QQ_SOURCE_REVISION")).await?;
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
        }
        _ = interrupt.recv() => {
            server.begin_shutdown();
            let drained = owner.shutdown().await;
            let stopped = server.shutdown().await;
            stopped?;
            drained?;
        }
    }
    Ok(())
}
