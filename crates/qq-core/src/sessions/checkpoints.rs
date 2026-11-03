use super::*;

use super::runtime::SessionRuntimeInner;
pub(super) fn list(
    connection: &Connection,
    session: SessionId,
) -> Result<Vec<qq_protocol::TurnCheckpoint>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached("SELECT run_id,turn_ordinal,prompt_ordinal,baseline,checkpoint,rewound FROM (SELECT * FROM turn_checkpoints WHERE session_id=?1 AND ((run_id,turn_ordinal) IN (SELECT run_id,turn_ordinal FROM turn_checkpoints WHERE session_id=?1 ORDER BY prompt_ordinal DESC,turn_ordinal DESC LIMIT 200) OR (run_id,turn_ordinal) IN (SELECT source_run_id,turn_ordinal FROM checkpoint_pins))) ORDER BY prompt_ordinal,turn_ordinal")?;
    let rows = statement
        .query_map([session.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, bool>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(run, turn, prompt, baseline, checkpoint, rewound)| {
            Ok(qq_protocol::TurnCheckpoint {
                run_id: parse_id(&run)?,
                turn_ordinal: turn,
                prompt_ordinal: prompt,
                baseline,
                checkpoint,
                rewound,
            })
        })
        .collect()
}

pub(super) fn prompts(
    connection: &Connection,
    session: SessionId,
) -> Result<Vec<qq_protocol::ForkPrompt>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached("SELECT id,output,turn FROM (SELECT id,run_id,substr(output,1,2048) AS output,row_number() OVER (ORDER BY ordinal) AS turn FROM messages WHERE session_id=?1 AND role='user' AND steering=0) selected WHERE NOT EXISTS(SELECT 1 FROM rewound_turns z WHERE z.run_id=selected.run_id AND z.turn_ordinal=0) ORDER BY turn DESC LIMIT 200")?;
    let rows = statement
        .query_map([session.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .rev()
        .map(|(id, text, turn)| {
            Ok(qq_protocol::ForkPrompt {
                turn,
                message_id: parse_id(&id)?,
                text: truncate_utf8(text, 1024),
            })
        })
        .collect()
}

pub(super) struct PendingCheckpoint {
    prompt: bool,
    done: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}

pub(super) async fn barrier(inner: &SessionRuntimeInner, run: RunId) {
    let done = inner
        .checkpoints
        .lock()
        .expect("checkpoint registry")
        .get(&run)
        .map(|pending| pending.done.clone());
    if let Some(mut done) = done {
        let _ = done.wait_for(|done| *done).await;
        let pending = inner
            .checkpoints
            .lock()
            .expect("checkpoint registry")
            .remove(&run);
        if let Some(pending) = pending
            && pending.task.await.is_err()
        {
            inner.failed.send_replace(true);
        }
    }
}

pub(super) fn capture_prompt(inner: &Arc<SessionRuntimeInner>, claimed: &ClaimedRun) {
    let owner = Arc::clone(inner);
    let claimed = claimed.clone();
    let run = claimed.identity.run_id;
    let session = claimed.identity.session_id;
    let (completed, done) = watch::channel(false);
    let task = tokio::spawn(async move {
        let root = PathBuf::from(&claimed.workspace);
        let repository = tokio::task::spawn_blocking(move || {
            root.ancestors().any(|path| path.join(".git").exists())
        })
        .await;
        if !matches!(repository, Ok(true)) {
            completed.send_replace(true);
            return;
        }
        let result = async {
            let previous = owner.store.call(store::Priority::AwaitControl, move |connection| {
                Ok(connection.query_row("SELECT baseline FROM prompt_baselines WHERE session_id=?1 ORDER BY prompt_ordinal DESC LIMIT 1", [session.to_string()], |row| row.get::<_, String>(0)).optional()?)
            }).await?;
            let previous = previous.map(|commit| crate::undo::Snapshot {
                name: commit.clone(), commit, kind: crate::undo::SnapshotKind::Run(run), workspace: PathBuf::new(), head: None, created_at_ms: 0, skipped: Vec::new(),
            });
            let baseline = crate::undo::turn_checkpoint(Path::new(&claimed.workspace), session, run, 0, true, previous.as_ref()).await.map_err(|error| SessionRuntimeError::UndoFailed(error.into()))?;
            if let Some(baseline) = baseline {
                owner.store.call_write(store::Priority::AwaitControl, move |connection| {
                    connection.execute("INSERT OR REPLACE INTO prompt_baselines(session_id,run_id,prompt_ordinal,baseline) SELECT ?1,?2,m.ordinal,?3 FROM messages m JOIN runs r ON r.user_message_id=m.id WHERE r.id=?2", params![session.to_string(),run.to_string(),baseline.commit])?;
                    Ok(())
                }).await?;
            }
            Ok::<_, SessionRuntimeError>(())
        }.await;
        if let Err(error) = result {
            tracing::warn!(%error, "could not capture prompt baseline");
        }
        completed.send_replace(true);
    });
    inner
        .checkpoints
        .lock()
        .expect("checkpoint registry")
        .insert(
            run,
            PendingCheckpoint {
                prompt: true,
                done,
                task,
            },
        );
}

pub(super) async fn settle(inner: &SessionRuntimeInner, claimed: &ClaimedRun, cancelled: bool) {
    if cancelled {
        let pending = {
            let mut checkpoints = inner.checkpoints.lock().expect("checkpoint registry");
            if checkpoints
                .get(&claimed.identity.run_id)
                .is_some_and(|pending| pending.prompt)
            {
                checkpoints.remove(&claimed.identity.run_id)
            } else {
                None
            }
        };
        if let Some(pending) = pending {
            pending.task.abort();
            let _ = pending.task.await;
        }
    }
    barrier(inner, claimed.identity.run_id).await;
    let pending = inner
        .turn_baselines
        .lock()
        .expect("turn baselines")
        .remove(&claimed.identity.run_id);
    if let Some((turn, baseline)) = pending {
        capture_now(inner, claimed, turn, baseline).await;
    }
    let run = claimed.identity.run_id;
    let session = claimed.identity.session_id;
    let recorded=inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        let prompt:u64=connection.query_row("SELECT ordinal FROM messages WHERE id=(SELECT user_message_id FROM runs WHERE id=?1)",[run.to_string()],|row|row.get(0))?;
        let mut statement=connection.prepare("SELECT turn_ordinal FROM model_turns WHERE run_id=?1 AND NOT EXISTS(SELECT 1 FROM tool_calls c WHERE c.run_id=?1 AND c.turn_ordinal=model_turns.turn_ordinal AND c.effect IN ('mutating','shell','external') AND c.started_at_ms IS NOT NULL) ORDER BY turn_ordinal")?;
        let turns=statement.query_map([run.to_string()],|row|row.get::<_,u32>(0))?.collect::<Result<Vec<_>,_>>()?;
        drop(statement);
        for turn in turns {
            connection.execute("INSERT OR IGNORE INTO turn_checkpoints(session_id,run_id,turn_ordinal,prompt_ordinal,baseline,checkpoint) SELECT ?1,?2,?3,?4,COALESCE((SELECT checkpoint FROM turn_checkpoints WHERE run_id=?2 AND turn_ordinal<?3 ORDER BY turn_ordinal DESC LIMIT 1),baseline),COALESCE((SELECT checkpoint FROM turn_checkpoints WHERE run_id=?2 AND turn_ordinal<?3 ORDER BY turn_ordinal DESC LIMIT 1),baseline) FROM prompt_baselines WHERE run_id=?2",params![session.to_string(),run.to_string(),turn,prompt])?;
        }
        Ok(())
    }).await;
    if recorded.is_err() {
        inner.failed.send_replace(true);
    }
}

pub(super) fn capture(
    inner: &Arc<SessionRuntimeInner>,
    claimed: &ClaimedRun,
    turn: u32,
    baseline: crate::undo::Snapshot,
) {
    let owner = Arc::clone(inner);
    let claimed = claimed.clone();
    let run = claimed.identity.run_id;
    let (completed, done) = watch::channel(false);
    let task = tokio::spawn(async move {
        capture_now(&owner, &claimed, turn, baseline).await;
        completed.send_replace(true);
    });
    inner
        .checkpoints
        .lock()
        .expect("checkpoint registry")
        .insert(
            run,
            PendingCheckpoint {
                prompt: false,
                done,
                task,
            },
        );
}

async fn capture_now(
    owner: &SessionRuntimeInner,
    claimed: &ClaimedRun,
    turn: u32,
    baseline: crate::undo::Snapshot,
) {
    let run = claimed.identity.run_id;
    let workspace = Path::new(&claimed.workspace);
    probe::PROBE
        .reached(probe::Checkpoint::TurnCapture(run, turn))
        .await;
    match crate::undo::turn_checkpoint(
        workspace,
        claimed.identity.session_id,
        run,
        turn,
        false,
        Some(&baseline),
    )
    .await
    {
        Ok(Some(head)) => {
            if let Err(error) = crate::undo::retain_baseline(
                workspace,
                claimed.identity.session_id,
                run,
                turn,
                &baseline.commit,
            )
            .await
            {
                tracing::warn!(%error,"could not retain turn baseline");
            }
            let session = claimed.identity.session_id;
            let base = baseline.commit;
            let commit = head.commit;
            let persisted = owner.store.call_write(store::Priority::AwaitControl, move |connection| {
                    let prompt: u64 = connection.query_row("SELECT ordinal FROM messages WHERE id=(SELECT user_message_id FROM runs WHERE id=?1)", [run.to_string()], |row| row.get(0))?;
                    connection.execute("INSERT OR REPLACE INTO turn_checkpoints(session_id,run_id,turn_ordinal,prompt_ordinal,baseline,checkpoint) VALUES (?1,?2,?3,?4,?5,?6)",params![session.to_string(),run.to_string(),turn,prompt,base,commit])?;
                    let mut statement=connection.prepare("SELECT baseline,checkpoint FROM turn_checkpoints WHERE session_id=?1 AND ((run_id,turn_ordinal) IN (SELECT run_id,turn_ordinal FROM turn_checkpoints WHERE session_id=?1 ORDER BY prompt_ordinal DESC,turn_ordinal DESC LIMIT 200) OR (run_id,turn_ordinal) IN (SELECT source_run_id,turn_ordinal FROM checkpoint_pins)) UNION ALL SELECT baseline,baseline FROM prompt_baselines WHERE session_id=?1")?;
                    let rows=statement.query_map([session.to_string()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
                    Ok(rows.into_iter().flat_map(|(base,head)|[base,head]).collect::<Vec<_>>())
                }).await;
            match persisted {
                Ok(retained) => {
                    if let Err(error) =
                        crate::undo::prune_turns(workspace, session, &retained).await
                    {
                        tracing::warn!(%error,"could not prune turn checkpoints");
                    }
                }
                Err(error) => {
                    tracing::error!(%error,"could not persist turn checkpoint");
                    owner.failed.send_replace(true);
                }
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%error,"could not capture turn checkpoint"),
    }
}

impl SessionRuntime {
    pub(super) async fn checkpoint_command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        let replay_command = command.clone();
        if let Some(receipt) = self
            .inner
            .store
            .call(store::Priority::Control, move |connection| {
                commands::recorded_receipt(connection, command_id, &replay_command)
            })
            .await?
        {
            return Ok(receipt);
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            let lifecycle = runtime.inner.lifecycle.read().await;
            if *runtime.inner.shutdown.borrow() || runtime.inner.draining.load(Ordering::Acquire) {
                return Err(SessionRuntimeError::Unavailable);
            }
            let slot = super::runtime::RestoreSlot::new(&runtime.inner);
            let mut path = None;
            if matches!(
                command,
                SessionCommand::RewindSession { .. } | SessionCommand::ForkSession { .. }
            ) {
                let session=match &command {SessionCommand::RewindSession {session_id,..}|SessionCommand::ForkSession {session_id,..}=>*session_id,_=>return Err(SessionRuntimeError::CONSTRAINT)};
                runtime.inner.store.call(store::Priority::Control,move |connection| {
                    let busy:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM side_questions WHERE session_id=?1 AND state='running')",[session.to_string()],|row|row.get(0))?;
                    if busy {return Err(SessionRuntimeError::SessionActive)}
                    Ok(())
                }).await?;
                let cmd = command.clone();
                match runtime
                    .inner
                    .store
                    .call_write(store::Priority::Control, move |connection| {
                        commands::begin_workspace_restore(connection, command_id, &cmd)
                    })
                    .await?
                {
                    commands::WorkspaceRestore::Replayed(receipt) => return Ok(receipt),
                    commands::WorkspaceRestore::Started(reserved) => path = Some(reserved),
                }
            }
            drop(lifecycle);
            if let SessionCommand::RewindSession { session_id, .. } | SessionCommand::ForkSession { session_id, .. } = &command {
                probe::PROBE.reached(probe::Checkpoint::WorkspaceReserved(*session_id)).await;
            }
            let result = AssertUnwindSafe(runtime.perform_checkpoint_command(command_id, command))
                .catch_unwind()
                .await;
            if let Some(path) = path {
                runtime.inner.store.clear_workspace_restore(path).await?;
            }
            drop(slot);
            runtime.request_schedule();
            result.map_err(|_| SessionRuntimeError::Unavailable)?
        })
        .await
        .map_err(|_| SessionRuntimeError::Unavailable)?
    }

    async fn perform_checkpoint_command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        let session = match &command {
            SessionCommand::ListCheckpoints { session_id }
            | SessionCommand::TurnDiff { session_id, .. }
            | SessionCommand::RewindSession { session_id, .. }
            | SessionCommand::ForkSession { session_id, .. } => *session_id,
            _ => return Err(SessionRuntimeError::CONSTRAINT),
        };
        let cmd = command.clone();

        let data=self.inner.store.call(store::Priority::Control,move |connection| {
            if let Some(receipt)=commands::recorded_receipt(connection,command_id,&cmd)? {return Ok(Err(receipt))}
            commands::check_command_limit(connection, &cmd)?;
            let reserved:Option<String>=connection.query_row("SELECT request_json FROM temp.restoring_paths WHERE command_id=?1",[command_id.to_string()],|row|row.get(0)).optional()?;
            if let Some(reserved)=reserved && reserved!=serde_json::to_string(&cmd)? {return Err(SessionRuntimeError::IdempotencyConflict)}
            let workspace=commands::session_workspace(connection,session)?;
            let (status,path):(String,String)=connection.query_row("SELECT s.status,COALESCE(s.execution_root,w.path) FROM sessions s JOIN workspaces w ON w.id=s.workspace_id WHERE s.id=?1",[session.to_string()],|row|Ok((row.get(0)?,row.get(1)?)))?;
            if matches!(cmd,SessionCommand::RewindSession {..}|SessionCommand::ForkSession {..}) && status!="idle" {return Err(SessionRuntimeError::SessionActive)}
            let count:u32=connection.query_row("SELECT COUNT(*) FROM sessions WHERE workspace_id=?1",[workspace.to_string()],|row|row.get(0))?;
            if matches!(cmd,SessionCommand::ForkSession {..}) && count>=MAX_SESSIONS_PER_WORKSPACE {return Err(SessionRuntimeError::SessionLimitReached)}
            Ok(Ok((workspace,path,list(connection,session)?,prompts(connection,session)?)))
        }).await?;
        let (workspace, path, checkpoints, prompts) = match data {
            Ok(data) => data,
            Err(receipt) => return Ok(receipt),
        };
        let outcome = match &command {
            SessionCommand::ListCheckpoints { .. } => CommandOutcome::CheckpointsListed {
                session_id: session,
                checkpoints,
                prompts,
            },
            SessionCommand::TurnDiff {
                run_id,
                turn_ordinal,
                ..
            } => {
                let point = checkpoints
                    .iter()
                    .find(|point| point.run_id == *run_id && point.turn_ordinal == *turn_ordinal)
                    .ok_or(SessionRuntimeError::RunNotFound)?;
                let mut diff =
                    crate::undo::turn_diff(Path::new(&path), &point.baseline, &point.checkpoint)
                        .await
                        .map_err(|error| SessionRuntimeError::UndoFailed(error.into()))?;
                let truncated = diff.len() > 256 * 1024;
                if truncated {
                    diff = truncate_utf8(diff, 256 * 1024)
                }
                CommandOutcome::TurnDiff { diff, truncated }
            }
            SessionCommand::RewindSession {
                run_id,
                turn_ordinal,
                ..
            } => {
                let point = checkpoints
                    .iter()
                    .find(|point| {
                        !point.rewound
                            && point.run_id == *run_id
                            && point.turn_ordinal == *turn_ordinal
                    })
                    .ok_or(SessionRuntimeError::RunNotFound)?
                    .clone();
                let restored = crate::undo::restore_commit(Path::new(&path), &point.checkpoint)
                    .await
                    .map_err(|error| SessionRuntimeError::UndoFailed(error.into()))?;
                CommandOutcome::SessionRewound {
                    session_id: session,
                    run_id: *run_id,
                    turn_ordinal: *turn_ordinal,
                    before: restored.before.name,
                }
            }
            SessionCommand::ForkSession { turn, .. } => {
                let prompt = match turn {
                    Some(turn) => prompts.iter().find(|prompt| prompt.turn == *turn),
                    None => prompts.last(),
                }
                .ok_or(SessionRuntimeError::RunNotFound)?
                .clone();
                let message = prompt.message_id;
                let ordinal = self
                    .inner
                    .store
                    .call(store::Priority::Control, move |connection| {
                        let ordinal: u64 = connection.query_row(
                            "SELECT ordinal FROM messages WHERE id=?1",
                            [message.to_string()],
                            |row| row.get(0),
                        )?;
                        Ok(ordinal)
                    })
                    .await?;
                let point = checkpoints
                    .iter()
                    .rfind(|point| point.prompt_ordinal < ordinal && !point.rewound)
                    .or_else(|| {
                        checkpoints
                            .iter()
                            .find(|point| point.prompt_ordinal == ordinal)
                    });
                let baseline = self.inner.store.call(store::Priority::Control, move |connection| {
                    Ok(connection.query_row("SELECT run_id,baseline FROM prompt_baselines WHERE session_id=?1 AND prompt_ordinal=?2",params![session.to_string(),ordinal],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional()?)
                }).await?;
                let (pin_run, pin_turn, commit) = match baseline {
                    Some((run, commit)) => (parse_id::<RunId>(&run)?, 0, commit),
                    None => {
                        let point = point.ok_or(SessionRuntimeError::RunNotFound)?;
                        (
                            point.run_id,
                            point.turn_ordinal,
                            if point.prompt_ordinal < ordinal {
                                point.checkpoint.clone()
                            } else {
                                point.baseline.clone()
                            },
                        )
                    }
                };
                let prompt_text = self
                    .inner
                    .store
                    .call(store::Priority::Control, move |connection| {
                        let message = load_message(connection, message)?;
                        Ok(message.output)
                    })
                    .await?;
                let fork = SessionId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
                let root = self.inner.worktree_directory.join(fork.to_string());
                let resource_root = root.to_string_lossy().into_owned();
                self.inner.store.call_write(store::Priority::AwaitControl, move |connection| {
                    connection.execute("INSERT INTO fork_resources(session_id,workspace,root,state) SELECT ?1,path,?2,'creating' FROM workspaces WHERE id=?3",params![fork.to_string(),resource_root,workspace.to_string()])?;
                    Ok(())
                }).await?;
                let result = async {
                let root = crate::undo::fork_worktree(Path::new(&path), &root, fork, &commit)
                    .await
                    .map_err(|error| SessionRuntimeError::UndoFailed(error.into()))?;
                let checkpoints=checkpoints.into_iter().filter(|point|point.prompt_ordinal<ordinal).collect::<Vec<_>>();
                let mut commits = checkpoints.iter().flat_map(|point| [point.baseline.clone(), point.checkpoint.clone()]).collect::<Vec<_>>();
                commits.extend(self.inner.store.call(store::Priority::Control, move |connection| {
                    let mut statement = connection.prepare("SELECT baseline FROM prompt_baselines WHERE session_id=?1 AND prompt_ordinal<=?2")?;
                    Ok(statement.query_map(params![session.to_string(),ordinal],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)
                }).await?);
                crate::undo::pin_fork_history(Path::new(&path), fork, &commits)
                    .await
                    .map_err(|error| SessionRuntimeError::UndoFailed(error.into()))?;
                let execution_root = root.to_string_lossy().into_owned();
                let root = execution_root.clone();
                let selected_prompt = prompt_text.clone();
                let store_id = self.inner.store.store_id();
                let fork_command = command.clone();
                if let Some(error) = probe::FAULTS.take(probe::StoreFault::ForkCreated(session)) { return Err(error); }
                self.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
                    let transaction=store::begin_unit(connection)?;
                    let count:u32=transaction.query_row("SELECT COUNT(*) FROM sessions WHERE workspace_id=?1",[workspace.to_string()],|row|row.get(0))?;
                    if count>=MAX_SESSIONS_PER_WORKSPACE {return Err(SessionRuntimeError::SessionLimitReached)}
                    transaction.execute("UPDATE fork_resources SET state='ready' WHERE session_id=?1",[fork.to_string()])?;
                    transaction.execute("INSERT INTO sessions(id,workspace_id,title,status,model,max_output_tokens,organization,approval_mode,created_at_ms,updated_at_ms,profile,model_is_fallback,reasoning_effort,execution_root) SELECT ?1,workspace_id,title,'idle',model,max_output_tokens,organization,approval_mode,?2,?2,profile,model_is_fallback,reasoning_effort,?3 FROM sessions WHERE id=?4",params![fork.to_string(),now_ms(),root,session.to_string()])?;
                    let connection=&transaction;
                    let mut statement=connection.prepare("SELECT id,run_id,ordinal FROM messages WHERE session_id=?1 AND ordinal<?2 ORDER BY ordinal")?;
                    let messages=statement.query_map(params![session.to_string(),ordinal],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u64>(2)?)))?.collect::<Result<Vec<_>,_>>()?;
                    drop(statement);
                    connection.execute("INSERT OR IGNORE INTO attachment_blobs SELECT ?2,blob_key,content,content_bytes,created_at_ms,evicted_at_ms FROM attachment_blobs WHERE session_id=?1",params![session.to_string(),fork.to_string()])?;
                    let mut runs=HashMap::new();
                    for (source_id,source_run,message_ordinal) in messages {
                        let message=load_message(connection,parse_id(&source_id)?)?;
                        let run=if let Some(run)=runs.get(&source_run) {*run} else {
                            let run=RunId::generate().map_err(|_|SessionRuntimeError::Unavailable)?;
                            let command=CommandId::generate().map_err(|_|SessionRuntimeError::Unavailable)?;
                            connection.execute("INSERT INTO runs(id,session_id,command_id,user_message_id,assistant_message_id,status,outcome_json,created_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?4,'completed',?5,?6,?6)",params![run.to_string(),fork.to_string(),command.to_string(),MessageId::generate().map_err(|_|SessionRuntimeError::Unavailable)?.to_string(),serde_json::to_string(&RunOutcome::Completed)?,now_ms()])?;
                            fork::copy_run_evidence(connection,&source_run,run,fork)?;
                            runs.insert(source_run,run);
                            run
                        };
                        let id=MessageId::generate().map_err(|_|SessionRuntimeError::Unavailable)?;
                        let role=match message.role {MessageRole::User=>"user",MessageRole::Assistant=>"assistant"};
                        connection.execute("INSERT INTO messages(id,session_id,run_id,ordinal,turn_ordinal,role,state,output,refusal,created_at_ms,steering,truncated) VALUES (?1,?2,?3,?4,?5,?6,'complete',?7,?8,?9,?10,?11)",params![id.to_string(),fork.to_string(),run.to_string(),message_ordinal,message.turn_ordinal,role,message.output,message.refusal,message.created_at_ms,message.steering,message.truncated])?;
                        connection.execute("UPDATE messages SET state=(SELECT state FROM messages WHERE id=?1) WHERE id=?2",params![source_id,id.to_string()])?;
                        connection.execute("INSERT INTO message_attachments SELECT ?2,ordinal,?3,path,digest,blob_key,window_start,window_end,window_total FROM message_attachments WHERE message_id=?1",params![source_id,id.to_string(),fork.to_string()])?;
                        connection.execute("UPDATE messages SET input_json=(SELECT input_json FROM messages WHERE id=?1) WHERE id=?2",params![source_id,id.to_string()])?;
                        if message.role==MessageRole::User&&!message.steering {
                            connection.execute("UPDATE runs SET user_message_id=?2 WHERE id=?1",params![run.to_string(),id.to_string()])?;
                        } else if message.role==MessageRole::Assistant {
                            connection.execute("UPDATE runs SET assistant_message_id=?2 WHERE id=?1",params![run.to_string(),id.to_string()])?;
                        }
                    }
                    connection.execute("INSERT OR REPLACE INTO checkpoint_pins(fork_session_id,source_run_id,turn_ordinal) VALUES (?1,?2,?3)",params![fork.to_string(),pin_run.to_string(),pin_turn])?;
                    connection.execute("INSERT INTO fork_history(session_id,prompt) VALUES (?1,?2)",params![fork.to_string(),selected_prompt])?;
                    transcript::load_model_context(connection,fork,u64::MAX).map_err(|_|SessionRuntimeError::SnapshotTooLarge)?;
                    connection.execute("DELETE FROM turn_checkpoints WHERE session_id=?1 AND NOT EXISTS(SELECT 1 FROM json_each(?2) p WHERE json_extract(p.value,'$.turn_ordinal')=turn_checkpoints.turn_ordinal AND json_extract(p.value,'$.checkpoint')=turn_checkpoints.checkpoint AND json_extract(p.value,'$.baseline')=turn_checkpoints.baseline)",params![fork.to_string(),serde_json::to_string(&checkpoints)?])?;
                    let outcome=CommandOutcome::SessionForked {session_id:fork,source_session_id:session,turn:prompt.turn,prompt:prompt_text,execution_root};
                    let receipt=commands::record_checkpoint_receipt(connection,store_id,command_id,&fork_command,workspace,outcome)?;
                    transaction.commit()?;
                    Ok(receipt)
                }).await
                }.await;
                if result.is_err() {
                    self.inner
                        .store
                        .call_write(store::Priority::AwaitControl, move |connection| {
                            connection.execute(
                                "UPDATE fork_resources SET state='deleting' WHERE session_id=?1",
                                [fork.to_string()],
                            )?;
                            Ok(())
                        })
                        .await?;
                    fork::cleanup(&self.inner, false).await?;
                }
                if let Ok(receipt) = &result {
                    self.inner.notify(receipt.committed_through);
                }
                return result;
            }
            _ => return Err(SessionRuntimeError::CONSTRAINT),
        };
        let store_id = self.inner.store.store_id();
        let receipt = self
            .inner
            .store
            .call_write(store::Priority::AwaitControl, move |connection| {
                commands::record_checkpoint_command(
                    connection, store_id, command_id, &command, workspace, outcome,
                )
            })
            .await?;
        self.inner.notify(receipt.committed_through);
        Ok(receipt)
    }
}
