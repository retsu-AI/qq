use super::*;

pub(super) async fn cleanup(
    inner: &super::runtime::SessionRuntimeInner,
    recover: bool,
) -> Result<(), SessionRuntimeError> {
    let _cleanup = inner
        .fork_cleanup
        .acquire()
        .await
        .map_err(|_| SessionRuntimeError::Unavailable)?;
    let resources = inner.store.call_write(store::Priority::AwaitControl, move |connection| {
        if recover {
            connection.execute("UPDATE fork_resources SET state='deleting' WHERE state='creating' AND NOT EXISTS(SELECT 1 FROM sessions WHERE id=fork_resources.session_id)", [])?;
        }
        let mut statement = connection.prepare(&format!("SELECT session_id,workspace,root FROM fork_resources f WHERE state='deleting' AND (root IS NULL OR NOT EXISTS(SELECT 1 FROM sessions s WHERE s.execution_root IS NOT NULL AND {}))",commands::paths_overlap_sql("s.execution_root", "f.root")))?;
        Ok(statement.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?)))?.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    for (session, workspace, root) in resources {
        let id = parse_id(&session)?;
        crate::undo::cleanup_session(Path::new(&workspace), root.as_deref().map(Path::new), id)
            .await
            .map_err(|error| SessionRuntimeError::UndoFailed(error.into()))?;
        inner
            .store
            .call_write(store::Priority::AwaitControl, move |connection| {
                connection.execute(
                    "DELETE FROM fork_resources WHERE session_id=?1 AND state='deleting'",
                    [session],
                )?;
                Ok(())
            })
            .await?;
        probe::PROBE.passed(probe::Checkpoint::SessionResourcesCleaned(id));
    }
    Ok(())
}
pub(super) fn copy_run_evidence(
    connection: &Connection,
    source: &str,
    run: RunId,
    session: SessionId,
) -> Result<(), SessionRuntimeError> {
    for table in ["model_turns", "tool_calls"] {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let projection = columns
            .iter()
            .map(|column| match column.as_str() {
                "id" => "lower(hex(randomblob(16)))".to_owned(),
                "run_id" => "?2".to_owned(),
                _ => format!("\"{column}\""),
            })
            .collect::<Vec<_>>()
            .join(",");
        let columns = columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<_>>()
            .join(",");
        connection.execute(
            &format!(
                "INSERT INTO {table}({columns}) SELECT {projection} FROM {table} WHERE run_id=?1"
            ),
            params![source, run.to_string()],
        )?;
    }
    connection.execute("INSERT INTO tool_spills SELECT fresh.id,?3,?2,s.tool,s.digest,s.content,s.content_bytes,s.omitted_from_line,s.created_at_ms,s.evicted_at_ms FROM tool_spills s JOIN tool_calls old ON old.id=s.tool_call_id JOIN tool_calls fresh ON fresh.run_id=?2 AND fresh.turn_ordinal=old.turn_ordinal AND fresh.call_ordinal=old.call_ordinal WHERE old.run_id=?1",params![source,run.to_string(),session.to_string()])?;
    let mut statement = connection.prepare("SELECT old.id,fresh.id,s.tool,s.digest FROM tool_calls old JOIN tool_calls fresh ON fresh.run_id=?2 AND fresh.turn_ordinal=old.turn_ordinal AND fresh.call_ordinal=old.call_ordinal JOIN tool_spills s ON s.tool_call_id=old.id WHERE old.run_id=?1")?;
    let handles = statement
        .query_map(params![source, run.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (old, fresh, tool, digest) in handles {
        let old = crate::tools::output::result_handle(&tool, parse_id(&old)?, &digest);
        let fresh = crate::tools::output::result_handle(&tool, parse_id(&fresh)?, &digest);
        connection.execute("UPDATE model_turns SET tool_results_projection_json=replace(tool_results_projection_json,?2,?3) WHERE run_id=?1",params![run.to_string(),old,fresh])?;
        connection.execute(
            "UPDATE tool_calls SET result=replace(result,?2,?3) WHERE run_id=?1",
            params![run.to_string(), old, fresh],
        )?;
    }
    let mut statement = connection.prepare("SELECT child_run_id FROM child_deliveries WHERE parent_run_id=?1 UNION SELECT child_run_id FROM child_reports WHERE parent_run_id=?1")?;
    let children = statement
        .query_map([source], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for child in children {
        let fresh = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        connection.execute("INSERT INTO child_deliveries SELECT ?4,?3,?2,created_at_ms,delivered_at_ms,delivery_ordinal,turn_ordinal,text FROM child_deliveries WHERE parent_run_id=?1 AND child_run_id=?5 AND delivered_at_ms IS NOT NULL",params![source,run.to_string(),session.to_string(),fresh.to_string(),child])?;
        connection.execute("INSERT INTO child_reports SELECT ?4,child_turn_ordinal,?3,?2,delivered_at_ms,delivery_ordinal,turn_ordinal,text FROM child_reports WHERE parent_run_id=?1 AND child_run_id=?5",params![source,run.to_string(),session.to_string(),fresh.to_string(),child])?;
    }
    connection.execute("UPDATE runs SET status=(SELECT status FROM runs WHERE id=?1),outcome_json=(SELECT outcome_json FROM runs WHERE id=?1),plan_descriptor_json=(SELECT plan_descriptor_json FROM runs WHERE id=?1),plan_identity_json=(SELECT plan_identity_json FROM runs WHERE id=?1),prompt_identity_json=(SELECT prompt_identity_json FROM runs WHERE id=?1),resolved_model_json=(SELECT resolved_model_json FROM runs WHERE id=?1),usage_json=(SELECT usage_json FROM runs WHERE id=?1),estimated_cost_usd_nanos=(SELECT estimated_cost_usd_nanos FROM runs WHERE id=?1),started_at_ms=(SELECT started_at_ms FROM runs WHERE id=?1),finished_at_ms=(SELECT finished_at_ms FROM runs WHERE id=?1) WHERE id=?2",params![source,run.to_string()])?;
    connection.execute(
        "INSERT INTO rewound_turns SELECT ?2,turn_ordinal FROM rewound_turns WHERE run_id=?1",
        params![source, run.to_string()],
    )?;
    connection.execute("INSERT INTO turn_checkpoints(session_id,run_id,turn_ordinal,prompt_ordinal,baseline,checkpoint,rewound) SELECT ?3,?2,turn_ordinal,prompt_ordinal,baseline,checkpoint,rewound FROM turn_checkpoints WHERE run_id=?1",params![source,run.to_string(),session.to_string()])?;
    connection.execute("INSERT INTO prompt_baselines SELECT ?3,?2,prompt_ordinal,baseline FROM prompt_baselines WHERE run_id=?1",params![source,run.to_string(),session.to_string()])?;
    Ok(())
}
