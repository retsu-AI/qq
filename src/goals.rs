//! Goal scripting over an existing authenticated server, without store ownership.
use std::{
    io::{self, Write},
    path::PathBuf,
};

use qq_protocol::{
    CommandId, GoalBudget, GoalCheck, GoalControl, GoalIdentity, GoalSnapshot, SessionCommand,
    SnapshotRequest,
};
use thiserror::Error;

use crate::cli::{GoalCommand, GoalSetArgs};

#[derive(Debug, Error)]
pub enum GoalError {
    #[error("could not resolve the workspace directory: {0}")]
    Workspace(#[source] io::Error),
    #[error("goal preparation stopped unexpectedly")]
    PreparationStopped,
    #[error("could not discover qq serve: {0}")]
    Discovery(#[source] qq_server::ServerError),
    #[error("no QQ server is running; start qq serve, then retry this command")]
    NoServer,
    #[error("server request failed: {0}")]
    Client(#[source] qq_client::ClientError),
    #[error("goal configuration could not be loaded: {0}")]
    Configuration(#[source] qq_config::ConfigError),
    #[error("session is not present in the selected workspace")]
    SessionMissing,
    #[error("this session has no goal; use qq goal set first")]
    GoalMissing,
    #[error("an achieved goal requires --replace")]
    Achieved,
    #[error("this goal has no completion check")]
    CheckMissing,
    #[error("--command differs from the current check; review qq goal show before authorizing it")]
    CheckChanged,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("secure command identity is unavailable")]
    IdentityUnavailable,
    #[error("could not encode the goal result: {0}")]
    Encoding(#[source] serde_json::Error),
    #[error("could not write the goal result: {0}")]
    Output(#[source] io::Error),
}

impl GoalError {
    pub(crate) fn exit_code(&self) -> std::process::ExitCode {
        match self {
            Self::Invalid(_)
            | Self::SessionMissing
            | Self::GoalMissing
            | Self::Achieved
            | Self::CheckMissing
            | Self::CheckChanged => std::process::ExitCode::from(2),
            Self::Workspace(_)
            | Self::PreparationStopped
            | Self::Discovery(_)
            | Self::NoServer
            | Self::Client(_)
            | Self::Configuration(_)
            | Self::IdentityUnavailable
            | Self::Encoding(_)
            | Self::Output(_) => std::process::ExitCode::FAILURE,
        }
    }
}

pub async fn run(command: GoalCommand, overrides: &crate::CliOverrides) -> Result<(), GoalError> {
    let requested = command.target().workspace.clone();
    let workspace = tokio::task::spawn_blocking(move || {
        let path = match requested {
            Some(path) => path,
            None => std::env::current_dir()?,
        };
        std::fs::canonicalize(path)
    })
    .await
    .map_err(|_| GoalError::PreparationStopped)?
    .map_err(GoalError::Workspace)?;
    let connection = match qq_server::discover().await.map_err(GoalError::Discovery)? {
        Some(connection) => connection,
        None => return Err(GoalError::NoServer),
    };
    let client = qq_client::SessionClient::new(connection).map_err(GoalError::Client)?;
    let (workspace_id, _) = client
        .resolve_workspace(&workspace)
        .await
        .map_err(GoalError::Client)?;
    let snapshot = client
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(command.target().session),
            1,
            1,
        ))
        .await
        .map_err(GoalError::Client)?;
    let session = match snapshot.focused {
        Some(session)
            if snapshot.workspace.id == workspace_id
                && session.summary.id == command.target().session =>
        {
            session
        }
        _ => return Err(GoalError::SessionMissing),
    };
    let json = command.target().json;
    if matches!(command, GoalCommand::Show(_)) {
        return write_result(&mut io::stdout().lock(), session.goal.as_ref(), json);
    }
    let defaults = if matches!(command, GoalCommand::Set(_)) {
        Some(load_defaults(workspace, overrides.clone()).await?)
    } else {
        None
    };
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| GoalError::Invalid("system clock predates the Unix epoch"))?;
    let now = u64::try_from(elapsed.as_millis())
        .map_err(|_| GoalError::Invalid("system clock exceeds the supported range"))?;
    let command = prepare_command(&command, session.goal.as_deref(), defaults.as_ref(), now)?;
    let id = CommandId::generate().map_err(|_| GoalError::IdentityUnavailable)?;
    let receipt = client
        .command(id, command)
        .await
        .map_err(GoalError::Client)?;
    write_result(&mut io::stdout().lock(), &receipt, json)
}

async fn load_defaults(
    workspace: PathBuf,
    overrides: crate::CliOverrides,
) -> Result<qq_config::GoalsConfig, GoalError> {
    tokio::task::spawn_blocking(move || {
        let request = overrides
            .load_request_in(&workspace)
            .map_err(GoalError::Configuration)?
            .with_untrusted_project_config_ignored();
        let loader = qq_config::ConfigLoader::system().map_err(GoalError::Configuration)?;
        loader
            .load_for_client(&request)
            .map(|snapshot| snapshot.goals().clone())
            .map_err(GoalError::Configuration)
    })
    .await
    .map_err(|_| GoalError::PreparationStopped)?
}

fn write_result(
    output: &mut impl Write,
    result: impl serde::Serialize,
    compact: bool,
) -> Result<(), GoalError> {
    if compact {
        serde_json::to_writer(&mut *output, &result)
    } else {
        serde_json::to_writer_pretty(&mut *output, &result)
    }
    .map_err(GoalError::Encoding)?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(GoalError::Output)
}

fn prepare_command(
    command: &GoalCommand,
    current: Option<&GoalSnapshot>,
    defaults: Option<&qq_config::GoalsConfig>,
    now: u64,
) -> Result<SessionCommand, GoalError> {
    let expected = current.map(|goal| GoalIdentity {
        goal_id: goal.goal_id,
        revision: goal.revision,
    });
    let session_id = command.target().session;
    let action = match command {
        GoalCommand::Set(args) => {
            return prepare_set(
                args,
                current,
                defaults.ok_or(GoalError::PreparationStopped)?,
                now,
            );
        }
        GoalCommand::Show(_) => return Err(GoalError::Invalid("show does not submit a command")),
        GoalCommand::Pause(_) => GoalControl::Pause,
        GoalCommand::Resume(_) => GoalControl::Resume,
        GoalCommand::Clear(_) => GoalControl::Clear,
        GoalCommand::ApproveCheck(args) => {
            let check = current
                .ok_or(GoalError::GoalMissing)?
                .check
                .as_ref()
                .ok_or(GoalError::CheckMissing)?;
            if check.command != args.command {
                return Err(GoalError::CheckChanged);
            }
            GoalControl::ApproveCheck {
                scope: args.scope.into(),
                command: check.command.clone(),
            }
        }
    };
    Ok(SessionCommand::GoalControl {
        session_id,
        expected: expected.ok_or(GoalError::GoalMissing)?,
        action,
    })
}

pub(crate) fn prepare_set(
    args: &GoalSetArgs,
    current: Option<&GoalSnapshot>,
    defaults: &qq_config::GoalsConfig,
    now: u64,
) -> Result<SessionCommand, GoalError> {
    if args.replace && current.is_none() {
        return Err(GoalError::GoalMissing);
    }
    if current.is_some_and(|goal| goal.status == qq_protocol::GoalStatus::Achieved) && !args.replace
    {
        return Err(GoalError::Achieved);
    }
    if args.objective.trim().is_empty() || args.objective.len() > 2048 {
        return Err(GoalError::Invalid("objective must contain 1 to 2048 bytes"));
    }
    let editing = current.filter(|_| !args.replace);
    let mut budget = match editing {
        Some(goal) => goal.budget.clone(),
        None => GoalBudget {
            max_goal_runs: defaults.default_max_runs,
            deadline_ms: now
                .checked_add(defaults.default_deadline_seconds * 1000)
                .ok_or(GoalError::Invalid("goal deadline overflows the clock"))?,
            max_total_tokens: None,
            max_cost_usd_nanos: None,
            per_run: crate::goal_run_limits(defaults.per_run),
        },
    };
    if let Some(duration) = args.goal_deadline {
        let duration = u64::try_from(duration.as_millis())
            .map_err(|_| GoalError::Invalid("goal duration is too large"))?;
        budget.deadline_ms = now
            .checked_add(duration)
            .ok_or(GoalError::Invalid("goal deadline overflows the clock"))?;
    }
    if let Some(runs) = args.goal_max_runs {
        budget.max_goal_runs = runs;
    }
    if let Some(cost) = args.max_cost_usd_nanos {
        budget.max_cost_usd_nanos = Some(cost);
    }
    if budget.deadline_ms <= now
        || budget.deadline_ms.saturating_sub(now) > defaults.max_deadline_seconds * 1000
        || budget.max_goal_runs == 0
        || budget.max_goal_runs > defaults.max_runs
    {
        return Err(GoalError::Invalid(
            "goal exceeds current deadline/run bounds or has expired; set --goal-deadline and --goal-max-runs explicitly",
        ));
    }
    let check = if args.clear_check {
        None
    } else if let Some(command) = &args.check {
        if command.trim().is_empty() || command.len() > 1024 || command.contains('\0') {
            return Err(GoalError::Invalid(
                "check must contain 1 to 1024 bytes and no NUL",
            ));
        }
        Some(GoalCheck {
            command: command.clone(),
            timeout_ms: defaults.check_timeout_seconds * 1000,
        })
    } else {
        editing.and_then(|goal| goal.check.clone())
    };
    Ok(SessionCommand::SetGoal {
        session_id: args.target.session,
        objective: args.objective.clone(),
        check,
        budget: Box::new(budget),
        expected: current.map(|goal| GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        }),
        replace: args.replace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use qq_protocol::{
        GoalCheckAuthorization, GoalCounters, GoalId, GoalPauseReason, GoalStatus, RunLimits,
    };
    const SESSION: &str = "019df100000070008000000000000001";
    const NOW: u64 = 1000;

    fn command(operation: &[&str]) -> GoalCommand {
        let mut argv = vec!["qq", "goal"];
        argv.extend_from_slice(operation);
        argv.extend(["--session", SESSION]);
        let parsed = crate::cli::Cli::try_parse_from(argv).unwrap();
        let Some(crate::cli::Command::Goal { command }) = parsed.command else {
            panic!("goal command")
        };
        command
    }

    fn existing() -> GoalSnapshot {
        GoalSnapshot {
            session_id: SESSION.parse().unwrap(),
            goal_id: GoalId::from_bytes([0x61; 16]),
            revision: 7,
            objective: "original".to_owned(),
            check: Some(GoalCheck {
                command: "printf 'exact check'".to_owned(),
                timeout_ms: 1234,
            }),
            check_authorization: Some(GoalCheckAuthorization::Once),
            budget: GoalBudget {
                max_goal_runs: 12,
                deadline_ms: NOW + 60_000,
                max_total_tokens: Some(700),
                max_cost_usd_nanos: Some(800),
                per_run: RunLimits {
                    max_model_turns: Some(4),
                    max_tool_calls: Some(5),
                    ..RunLimits::default()
                },
            },
            checklist: Vec::new(),
            notes: "keep progress".to_owned(),
            status: GoalStatus::Paused {
                reason: GoalPauseReason::User,
            },
            counters: GoalCounters::default(),
            next_run_at_ms: NOW,
            wait: None,
            achievement_claim: None,
            audit_pending: false,
            audit_revision: None,
            pending_run_id: None,
            check_output: None,
        }
    }

    #[tokio::test]
    async fn goal_cli_runtime_preserves_paused_edits_and_rejects_stale_commands() {
        use qq_core::{
            RuntimeLoadFuture, RuntimeLoadRequest, RuntimeLoader, SessionRuntime,
            SessionRuntimeError, SessionRuntimeOptions,
        };
        use qq_protocol::{ApprovalMode, CommandOutcome};
        struct NoModel;
        impl RuntimeLoader for NoModel {
            fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
                panic!("an unapproved check must not dispatch a model")
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(root.join("sessions.sqlite3")),
            std::sync::Arc::new(NoModel),
        )
        .await
        .unwrap();
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: root.display().to_string(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
            panic!("workspace receipt")
        };
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: qq_protocol::ModelSelection {
                        model: Some("test/model".to_owned()),
                        ..Default::default()
                    },
                    approval_mode: ApprovalMode::ReadOnly,
                    profile: Default::default(),
                    reasoning_effort: None,
                    correlation: Default::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            panic!("session receipt")
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut create = command(&["set", "first objective", "--check", "printf checked"]);
        let GoalCommand::Set(args) = &mut create else {
            unreachable!()
        };
        args.target.session = session_id;
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                prepare_command(&create, None, Some(&Default::default()), now).unwrap(),
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal receipt")
        };
        let pause = GoalCommand::Pause(crate::cli::GoalTarget {
            session: session_id,
            workspace: None,
            json: true,
        });
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                prepare_command(&pause, Some(&goal), None, now).unwrap(),
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("pause receipt")
        };
        let mut edit = command(&["set", "edited objective"]);
        let GoalCommand::Set(args) = &mut edit else {
            unreachable!()
        };
        args.target.session = session_id;
        let edit = prepare_command(&edit, Some(&goal), Some(&Default::default()), now).unwrap();
        let stale = prepare_command(&pause, Some(&goal), None, now).unwrap();
        let receipt = runtime
            .command(CommandId::generate().unwrap(), edit)
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal: updated, .. } = receipt.outcome else {
            panic!("edit receipt")
        };
        assert_eq!(updated.objective, "edited objective");
        assert_eq!(updated.budget, goal.budget);
        assert_eq!(updated.check, goal.check);
        assert_eq!(
            updated.status,
            GoalStatus::Paused {
                reason: GoalPauseReason::User
            }
        );
        assert!(matches!(
            runtime.command(CommandId::generate().unwrap(), stale).await,
            Err(SessionRuntimeError::GoalConflict { .. })
        ));
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 1, 1))
            .await
            .unwrap();
        assert_eq!(snapshot.focused.unwrap().goal.unwrap(), updated);
        runtime.close().await.unwrap();
    }

    #[test]
    fn goal_cli_commands_preserve_exact_revision_scope_and_unspecified_edit_limits() {
        let current = existing();
        let expected = GoalIdentity {
            goal_id: current.goal_id,
            revision: current.revision,
        };
        for (args, action) in [
            (vec!["pause"], GoalControl::Pause),
            (vec!["resume"], GoalControl::Resume),
            (vec!["clear"], GoalControl::Clear),
            (
                vec!["approve-check", "once", "--command", "printf 'exact check'"],
                GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::Once,
                    command: "printf 'exact check'".to_owned(),
                },
            ),
            (
                vec![
                    "approve-check",
                    "for-goal",
                    "--command",
                    "printf 'exact check'",
                ],
                GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::ForGoal,
                    command: "printf 'exact check'".to_owned(),
                },
            ),
        ] {
            assert_eq!(
                prepare_command(&command(&args), Some(&current), None, NOW).unwrap(),
                SessionCommand::GoalControl {
                    session_id: current.session_id,
                    expected,
                    action
                }
            );
        }
        let edit = prepare_command(
            &command(&["set", "edited objective"]),
            Some(&current),
            Some(&qq_config::GoalsConfig::default()),
            NOW,
        )
        .unwrap();
        assert_eq!(
            edit,
            SessionCommand::SetGoal {
                session_id: current.session_id,
                objective: "edited objective".to_owned(),
                check: current.check.clone(),
                budget: Box::new(current.budget.clone()),
                expected: Some(expected),
                replace: false
            }
        );
        assert!(matches!(
            prepare_command(
                &command(&["approve-check", "once", "--command", "changed"]),
                Some(&current),
                None,
                NOW
            ),
            Err(GoalError::CheckChanged)
        ));
        assert!(matches!(
            prepare_command(&command(&["pause"]), None, None, NOW),
            Err(GoalError::GoalMissing)
        ));
    }

    #[test]
    fn goal_cli_creation_replacement_and_explicit_edits_use_bounded_workspace_defaults() {
        let defaults = qq_config::GoalsConfig {
            default_deadline_seconds: 22,
            max_deadline_seconds: 45,
            default_max_runs: 7,
            max_runs: 12,
            check_timeout_seconds: 3,
            per_run: qq_config::GoalRunLimits {
                max_model_turns: Some(9),
                ..qq_config::GoalRunLimits::default()
            },
            ..qq_config::GoalsConfig::default()
        };
        let create = prepare_command(
            &command(&[
                "set",
                "new",
                "--check",
                "cargo test",
                "--max-cost-usd",
                "1.000000001",
            ]),
            None,
            Some(&defaults),
            NOW,
        )
        .unwrap();
        let SessionCommand::SetGoal {
            budget,
            check,
            expected,
            replace,
            ..
        } = create
        else {
            panic!("set")
        };
        assert_eq!(budget.deadline_ms, NOW + 22_000);
        assert_eq!(budget.max_goal_runs, 7);
        assert_eq!(budget.per_run.max_model_turns, Some(9));
        assert_eq!(budget.max_cost_usd_nanos, Some(1_000_000_001));
        assert_eq!(
            check,
            Some(GoalCheck {
                command: "cargo test".to_owned(),
                timeout_ms: 3000
            })
        );
        assert!(expected.is_none() && !replace);
        let mut current = existing();
        current.status = GoalStatus::Achieved;
        assert!(matches!(
            prepare_command(
                &command(&["set", "new"]),
                Some(&current),
                Some(&defaults),
                NOW
            ),
            Err(GoalError::Achieved)
        ));
        assert!(matches!(
            prepare_command(
                &command(&["set", "new", "--replace"]),
                None,
                Some(&defaults),
                NOW
            ),
            Err(GoalError::GoalMissing)
        ));
        let replacement = prepare_command(
            &command(&["set", "new", "--replace"]),
            Some(&current),
            Some(&defaults),
            NOW,
        )
        .unwrap();
        assert!(
            matches!(replacement, SessionCommand::SetGoal { expected: Some(expected), replace: true, check: None, budget, .. }
            if expected.goal_id == current.goal_id && budget.deadline_ms == NOW+22_000 && budget.max_total_tokens.is_none())
        );
        current.status = GoalStatus::Paused {
            reason: GoalPauseReason::User,
        };
        let edit = prepare_command(
            &command(&[
                "set",
                "edited",
                "--clear-check",
                "--goal-deadline",
                "40s",
                "--goal-max-runs",
                "11",
            ]),
            Some(&current),
            Some(&defaults),
            NOW,
        )
        .unwrap();
        assert!(
            matches!(edit, SessionCommand::SetGoal { check: None, replace: false, budget, .. }
            if budget.deadline_ms == NOW+40_000 && budget.max_goal_runs == 11 && budget.max_total_tokens == Some(700) && budget.per_run == current.budget.per_run)
        );
        assert!(matches!(
            prepare_command(
                &command(&["set", "new", "--goal-deadline", "1h"]),
                None,
                Some(&defaults),
                NOW
            ),
            Err(GoalError::Invalid(_))
        ));
    }

    #[test]
    fn goal_cli_output_reuses_existing_wire_shapes_and_reports_write_errors() {
        let mut out = Vec::new();
        write_result(&mut out, Some(existing()), true).unwrap();
        let decoded: GoalSnapshot = serde_json::from_slice(&out).unwrap();
        assert_eq!(decoded, existing());
        out.clear();
        write_result(&mut out, None::<GoalSnapshot>, true).unwrap();
        assert_eq!(out, b"null\n");
        assert!(write_result(&mut io::sink(), Some(existing()), false).is_ok());
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(write_result(&mut Broken, Some(existing()), true).is_err());
    }

    #[test]
    fn goal_cli_decimal_cost_and_duration_never_round_or_overflow() {
        for (input, nanos) in [
            ("1.000000001", 1_000_000_001),
            (".5", 500_000_000),
            ("0.000000001", 1),
            ("18446744073.709551615", u64::MAX),
        ] {
            assert_eq!(crate::cli::parse_goal_cost(input).unwrap(), nanos);
        }
        for input in [
            "0",
            "0.0000000001",
            "NaN",
            "inf",
            "-1",
            "1e3",
            "18446744073.709551616",
        ] {
            assert!(crate::cli::parse_goal_cost(input).is_err(), "{input}");
        }
        for input in ["", "h", "0s", "8d", "18446744073709551615d", "é"] {
            assert!(crate::cli::parse_goal_duration(input).is_err(), "{input}");
        }
    }
}

#[cfg(test)]
#[test]
fn goal_cli_distinguishes_argument_errors_from_runtime_failures() {
    use std::process::ExitCode;
    for error in [
        GoalError::Invalid("objective must contain 1 to 2048 bytes"),
        GoalError::SessionMissing,
        GoalError::GoalMissing,
        GoalError::Achieved,
        GoalError::CheckMissing,
        GoalError::CheckChanged,
    ] {
        assert_eq!(error.exit_code(), ExitCode::from(2), "{error}");
    }
    for error in [
        GoalError::NoServer,
        GoalError::PreparationStopped,
        GoalError::IdentityUnavailable,
    ] {
        assert_eq!(error.exit_code(), ExitCode::FAILURE, "{error}");
    }
}
