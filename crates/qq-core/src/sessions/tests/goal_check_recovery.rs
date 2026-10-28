use super::*;

async fn queued_check(store: &Store, workspace: &Path) -> ClaimedRun {
    let (workspace_id, session, _) = create_claimed_parent(store, workspace).await;
    let store_id = store.store_id();
    store
        .call_write(Priority::Control, move |connection| {
            crate::sessions::settlement::recover_interrupted_runs(connection, store_id)?;
            let transaction = store::begin_unit(connection)?;
            transaction.execute(
                "UPDATE sessions SET approval_mode='full' WHERE id=?1",
                [session.to_string()],
            )?;
            let mut goal = crate::sessions::goals::set(
                &transaction,
                session,
                crate::sessions::goals::Specification {
                    objective: "verify the same task after restart".into(),
                    check: Some(qq_protocol::GoalCheck {
                        command: "printf once >> counter; printf verified".into(),
                        timeout_ms: 60_000,
                    }),
                    budget: serde_json::from_value(
                        serde_json::json!({"max_goal_runs":2,"deadline_ms":now_ms()+300_000}),
                    )?,
                },
                None,
                false,
                now_ms(),
                GoalPolicy::default(),
            )?;
            goal.status = qq_protocol::GoalStatus::Active;
            goal.counters.runs = 1;
            goal.audit_pending = true;
            goal.audit_revision = Some(goal.revision);
            goal.check_authorization = Some(qq_protocol::GoalCheckAuthorization::Once);
            crate::sessions::goal_check::queue(
                &transaction,
                store_id,
                workspace_id,
                &mut goal,
                now_ms(),
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    store.reserve_next_run(false).await.unwrap().unwrap()
}

#[tokio::test]
async fn check_execution_authority_uses_current_mode_and_requires_auto_containment() {
    use qq_protocol::GoalCheckAuthorization as Authorization;
    let directory = TempDir::new().unwrap();
    let path = directory.path().canonicalize().unwrap();
    let store = Store::open(path.join("sessions.sqlite3")).await.unwrap();
    let (_, session, _) = create_claimed_parent(&store, &path).await;
    let (_, parent, _) = create_claimed_parent(&store, &path).await;
    store
        .call_write(Priority::Control, move |connection| {
            for (mode, authorization, expected) in [
                ("auto", Authorization::TaskScoped, Some(true)),
                ("auto", Authorization::Once, Some(true)),
                ("auto", Authorization::ForGoal, Some(true)),
                ("full", Authorization::TaskScoped, None),
                ("ask", Authorization::TaskScoped, None),
                ("read_only", Authorization::TaskScoped, None),
                ("full", Authorization::ForGoal, Some(false)),
                ("ask", Authorization::Once, Some(false)),
                ("read_only", Authorization::ForGoal, None),
            ] {
                connection.execute(
                    "UPDATE sessions SET approval_mode=?2 WHERE id=?1",
                    params![session.to_string(), mode],
                )?;
                assert_eq!(
                    crate::sessions::goal_check::check_scope_required(
                        connection,
                        session,
                        Some(authorization),
                    )?,
                    expected,
                    "mode={mode}, auth={authorization:?}"
                );
            }
            connection.execute(
                "UPDATE sessions SET parent_id=?2, approval_mode='full' WHERE id=?1",
                params![session.to_string(), parent.to_string()],
            )?;
            connection.execute(
                "UPDATE sessions SET approval_mode='read_only' WHERE id=?1",
                [parent.to_string()],
            )?;
            assert_eq!(
                crate::sessions::goal_check::check_scope_required(
                    connection,
                    session,
                    Some(Authorization::ForGoal),
                )?,
                None
            );
            connection.execute(
                "UPDATE sessions SET approval_mode='auto' WHERE id=?1 OR id=?2",
                params![session.to_string(), parent.to_string()],
            )?;
            assert_eq!(
                crate::sessions::goal_check::check_scope_required(
                    connection,
                    session,
                    Some(Authorization::TaskScoped),
                )?,
                Some(true)
            );
            for mode in ["full", "ask"] {
                connection.execute(
                    "UPDATE sessions SET approval_mode=?2 WHERE id=?1",
                    params![session.to_string(), mode],
                )?;
                assert_eq!(
                    crate::sessions::goal_check::check_scope_required(
                        connection,
                        session,
                        Some(Authorization::ForGoal),
                    )?,
                    None,
                    "{mode} child cannot escape Auto parent containment"
                );
            }
            connection.execute(
                "UPDATE sessions SET approval_mode='auto' WHERE id=?1",
                [session.to_string()],
            )?;
            connection.execute(
                "UPDATE sessions SET approval_mode='ask' WHERE id=?1",
                [parent.to_string()],
            )?;
            assert_eq!(
                crate::sessions::goal_check::check_scope_required(
                    connection,
                    session,
                    Some(Authorization::TaskScoped),
                )?,
                None
            );
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn check_dispatch_revalidates_task_authority_after_restart_and_mode_revocation() {
    use qq_protocol::GoalCheckAuthorization as Authorization;
    let directory = TempDir::new().unwrap();
    let path = directory.path().canonicalize().unwrap();
    let database = path.join("sessions.sqlite3");
    let store = Store::open(database.clone()).await.unwrap();
    let mut claim = queued_check(&store, &path).await;
    let session = claim.identity.session_id;
    Arc::make_mut(claim.goal.as_mut().unwrap()).check_authorization =
        Some(Authorization::TaskScoped);
    store
        .call_write(Priority::Control, move |connection| {
            let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            goal.check_authorization = Some(Authorization::TaskScoped);
            crate::sessions::goals::save(connection, &goal)?;
            connection.execute(
                "UPDATE sessions SET approval_mode='auto' WHERE id=?1",
                [session.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
    let store = Store::open(database).await.unwrap();
    store
        .call_write(Priority::Control, move |connection| {
            assert_eq!(
                crate::sessions::goal_check::execution_scope_required(connection, &claim)?,
                Some(true)
            );
            for mode in ["ask", "read_only", "full"] {
                connection.execute(
                    "UPDATE sessions SET approval_mode=?2 WHERE id=?1",
                    params![session.to_string(), mode],
                )?;
                assert_eq!(
                    crate::sessions::goal_check::execution_scope_required(connection, &claim)?,
                    None,
                    "{mode}"
                );
            }
            connection.execute(
                "UPDATE sessions SET approval_mode='auto' WHERE id=?1",
                [session.to_string()],
            )?;
            assert_eq!(
                crate::sessions::goal_check::execution_scope_required(connection, &claim)?,
                Some(true)
            );
            let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            goal.check_authorization = Some(Authorization::ForGoal);
            crate::sessions::goals::save(connection, &goal)?;
            Arc::make_mut(claim.goal.as_mut().unwrap()).check_authorization =
                Some(Authorization::ForGoal);
            connection.execute(
                "UPDATE sessions SET approval_mode='full' WHERE id=?1",
                [session.to_string()],
            )?;
            assert_eq!(
                crate::sessions::goal_check::execution_scope_required(connection, &claim)?,
                Some(false)
            );
            connection.execute(
                "UPDATE sessions SET approval_mode='read_only' WHERE id=?1",
                [session.to_string()],
            )?;
            assert_eq!(
                crate::sessions::goal_check::execution_scope_required(connection, &claim)?,
                None
            );
            assert_eq!(
                crate::sessions::goals::load_live(connection, session)?
                    .unwrap()
                    .check_authorization,
                Some(Authorization::ForGoal)
            );
            connection.execute(
                "UPDATE sessions SET approval_mode='ask' WHERE id=?1",
                [session.to_string()],
            )?;
            assert_eq!(
                crate::sessions::goal_check::execution_scope_required(connection, &claim)?,
                Some(false)
            );
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn auto_check_under_a_non_auto_parent_settles_as_policy_refusal_without_requeue() {
    use crate::sessions::goal_check::{self, Completion, Start};
    let directory = TempDir::new().unwrap();
    let path = directory.path().canonicalize().unwrap();
    let store = Store::open(path.join("sessions.sqlite3")).await.unwrap();
    let mut claim = queued_check(&store, &path).await;
    Arc::make_mut(claim.goal.as_mut().unwrap()).check_authorization =
        Some(qq_protocol::GoalCheckAuthorization::TaskScoped);
    let session = claim.identity.session_id;
    let (_, parent, _) = create_claimed_parent(&store, &path).await;
    let store_id = store.store_id();
    store
        .call_write(Priority::Control, move |connection| {
            let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            goal.check_authorization = Some(qq_protocol::GoalCheckAuthorization::TaskScoped);
            crate::sessions::goals::save(connection, &goal)?;
            connection.execute(
                "UPDATE sessions SET approval_mode='auto', parent_id=?2 WHERE id=?1",
                params![session.to_string(), parent.to_string()],
            )?;
            connection.execute(
                "UPDATE sessions SET approval_mode='ask' WHERE id=?1",
                [parent.to_string()],
            )?;
            let deadline = claim.check_deadline.unwrap().0;
            let Start::Policy(error) =
                goal_check::try_start(connection, store_id, &claim, deadline)?
            else {
                panic!("an Auto check under an Ask parent must refuse before check start");
            };
            let transaction = store::begin_unit(connection)?;
            goal_check::finish_in_transaction(
                &transaction,
                store_id,
                &claim,
                Completion::Policy(error),
                deadline,
                SettlementCause::Executor,
            )?;
            let goal = crate::sessions::goals::load_live(&transaction, session)?.unwrap();
            assert!(matches!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Configuration { .. }
                    }
                }
            ));
            assert!(goal.pending_run_id.is_none());
            assert!(
                goal.audit_pending,
                "the refused verification remains pending without being scheduled while paused"
            );
            assert_eq!(goal.counters.audits, 0);
            let queued: u32 = transaction.query_row(
                "SELECT COUNT(*) FROM runs WHERE session_id=?1 AND status='queued'",
                [session.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(queued, 0);
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn consumed_once_check_never_requests_human_approval_in_auto() {
    for (mode, automatic) in [("auto", true), ("ask", false), ("full", false)] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().canonicalize().unwrap();
        let store = Store::open(path.join("sessions.sqlite3")).await.unwrap();
        let claim = queued_check(&store, &path).await;
        let session = claim.identity.session_id;
        let store_id = store.store_id();
        store
            .call_write(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                goal.check_authorization = Some(qq_protocol::GoalCheckAuthorization::Consumed);
                crate::sessions::goals::save(connection, &goal)?;
                connection.execute(
                    "UPDATE sessions SET approval_mode=?2 WHERE id=?1",
                    params![session.to_string(), mode],
                )?;
                let transaction = store::begin_unit(connection)?;
                crate::sessions::goal_check::finish_in_transaction(
                    &transaction,
                    store_id,
                    &claim,
                    crate::sessions::goal_check::Completion::Stopped,
                    claim.check_deadline.unwrap().0,
                    SettlementCause::Executor,
                )?;
                let goal = crate::sessions::goals::load_live(&transaction, session)?.unwrap();
                assert_eq!(
                    goal.check_authorization,
                    Some(qq_protocol::GoalCheckAuthorization::Consumed),
                    "settlement must not grant a second check"
                );
                let qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser { cause },
                } = goal.status
                else {
                    panic!("consumed authorization must stop further execution");
                };
                if automatic {
                    assert!(matches!(
                        cause,
                        qq_protocol::GoalNeedsUser::Configuration { .. }
                    ));
                } else {
                    assert_eq!(cause, qq_protocol::GoalNeedsUser::CheckApproval);
                }
                assert!(goal.pending_run_id.is_none());
                transaction.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}
