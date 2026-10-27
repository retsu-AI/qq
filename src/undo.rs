//! `qq undo`: restore the workspace to the snapshot taken before a run.
use std::{
    io::{self, Write},
    path::Path,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use qq_core::undo::{self, Snapshot, SnapshotKind, UndoError};

use crate::cli::UndoArgs;

pub(crate) async fn run(args: UndoArgs) -> ExitCode {
    // Unlocked: `Stdout` locks per write, so no lock is held across the
    // Git steps `execute` awaits.
    let mut stdout = io::stdout();
    match execute(args, &mut stdout).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum CommandError {
    #[error("could not resolve the current directory: {0}")]
    Directory(#[source] io::Error),
    #[error(transparent)]
    Undo(#[from] UndoError),
    #[error("could not write output: {0}")]
    Output(#[from] io::Error),
}

async fn execute(args: UndoArgs, out: &mut impl Write) -> Result<ExitCode, CommandError> {
    let directory = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(CommandError::Directory)?;
    let snapshots = undo::list(&directory).await?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    if args.list {
        if snapshots.is_empty() {
            writeln!(out, "No snapshots for {}.", directory.display())?;
        }
        for snapshot in &snapshots {
            writeln!(out, "{}  {}", snapshot.name, describe(snapshot, now_ms))?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let target = match &args.snapshot {
        Some(name) => {
            let matching: Vec<&Snapshot> = snapshots
                .iter()
                .filter(|snapshot| snapshot.name.starts_with(name.as_str()))
                .collect();
            match matching.as_slice() {
                [one] => *one,
                [] => {
                    eprintln!("error: no snapshot named {name}; qq undo --list shows them");
                    return Ok(ExitCode::from(2));
                }
                _ => {
                    eprintln!(
                        "error: {name} matches more than one snapshot; give more of the name"
                    );
                    return Ok(ExitCode::from(2));
                }
            }
        }
        None => match snapshots
            .iter()
            .find(|snapshot| matches!(snapshot.kind, SnapshotKind::Run(_)))
        {
            Some(newest) => newest,
            None => {
                eprintln!(
                    "error: no run has been snapshotted in {}; snapshots are taken before a run first changes files in a Git work tree",
                    directory.display()
                );
                return Ok(ExitCode::from(2));
            }
        },
    };
    let restored = undo::restore(target).await?;
    writeln!(
        out,
        "Restored {} to snapshot {} ({}).",
        relative(&target.workspace, &directory),
        target.name,
        describe(target, now_ms)
    )?;
    if restored.changes.is_empty() {
        writeln!(out, "Nothing had changed.")?;
    }
    for change in &restored.changes {
        writeln!(out, "  {change}")?;
    }
    if let Some((then, now)) = &restored.head_moved {
        writeln!(
            out,
            "HEAD moved since the snapshot ({} -> {}). Only files were restored; commits and branches are unchanged.",
            short(then.as_deref()),
            short(now.as_deref())
        )?;
    }
    writeln!(out, "To reverse this: qq undo {}", restored.before.name)?;
    Ok(ExitCode::SUCCESS)
}

fn describe(snapshot: &Snapshot, now_ms: u64) -> String {
    let age = ago(now_ms.saturating_sub(snapshot.created_at_ms) / 1000);
    match &snapshot.kind {
        SnapshotKind::Run(run) => format!("before run {run}, {age}"),
        SnapshotKind::BeforeUndo => format!("before an undo, {age}"),
    }
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

fn short(commit: Option<&str>) -> &str {
    commit.map_or("no commit", |commit| &commit[..commit.len().min(10)])
}

fn relative(workspace: &Path, directory: &Path) -> String {
    if workspace == directory {
        "the workspace".to_owned()
    } else {
        workspace.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_are_coarse() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(125), "2 min ago");
        assert_eq!(ago(7_300), "2 h ago");
        assert_eq!(ago(200_000), "2 d ago");
    }
}
