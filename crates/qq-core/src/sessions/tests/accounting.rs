use super::*;

#[test]
fn occupancy_reuse_requires_exact_shape_and_prefix_and_follows_byte_deltas() {
    let model = test_resolved_model("test/model", "wire-model", 256, None);
    let shape = context_request_shape(&model);
    let prefix = test_static_prefix(2, Some(3));
    let basis = context_occupancy_basis(shape.digest, prefix, 1_000);
    let occupancy = ContextOccupancy {
        context_tokens: 100,
        basis,
    };
    assert_eq!(
        compatible_context_tokens(occupancy, shape, prefix, 1_024),
        // 24 appended bytes are charged at the byte-ratio estimate.
        Some(100 + context::estimate_tokens(24))
    );
    // A shrunken request (assembly-time pruning) keeps the measurement and
    // credits the removed bytes at the estimate ratio.
    assert_eq!(
        compatible_context_tokens(occupancy, shape, prefix, 996),
        Some(99)
    );
    assert_eq!(
        compatible_context_tokens(occupancy, shape, test_static_prefix(4, Some(3)), 1_024),
        None
    );
    assert_eq!(
        compatible_context_tokens(occupancy, shape, test_static_prefix(2, None), 1_024),
        None
    );
}

#[test]
fn unknown_provider_identity_repeats_overflow_but_never_seeds_occupancy() {
    let mut model = test_resolved_model("test/model", "wire-model", 256, None);
    model.request_shape = None;
    let shape = context_request_shape(&model);
    assert!(!shape.provider_identity);
    let prefix = test_static_prefix(2, Some(3));
    let basis = context_occupancy_basis(shape.digest, prefix, 1_000);

    // Overflow suppression: the same route-level shape and prefix repeat
    // the known overflow even when pruning shrank the request.
    assert!(repeats_context_basis(basis, shape, prefix));
    assert!(repeats_context_basis(
        context_occupancy_basis(shape.digest, prefix, 5_000),
        shape,
        prefix
    ));
    assert!(!repeats_context_basis(
        basis,
        shape,
        test_static_prefix(4, Some(3))
    ));
    let mut other_route = model.clone();
    other_route.route = "other/model".to_owned();
    assert!(!repeats_context_basis(
        basis,
        context_request_shape(&other_route),
        prefix
    ));

    // Reuse: a route-level identity cannot prove tokenization shape.
    let occupancy = ContextOccupancy {
        context_tokens: 100,
        basis,
    };
    assert_eq!(
        compatible_context_tokens(occupancy, shape, prefix, 1_024),
        None
    );

    // A known identity for the same route lives in a different digest
    // domain: neither direction can match the other.
    let exact = context_request_shape(&test_resolved_model("test/model", "wire-model", 256, None));
    assert!(exact.provider_identity);
    assert_ne!(exact.digest, shape.digest);
    assert!(!repeats_context_basis(basis, exact, prefix));
}

#[test]
fn occupancy_shape_excludes_pricing_but_invalidates_wire_affecting_changes() {
    let base = test_resolved_model("test/model", "wire-model", 256, None);
    let base_shape = context_request_shape(&base);
    assert!(base_shape.provider_identity);
    let mut priced = base.clone();
    priced.pricing = Some(ModelPricing {
        input_usd_nanos_per_token: 7,
        output_usd_nanos_per_token: 11,
        cache_read_usd_nanos_per_token: None,
        cache_write_usd_nanos_per_token: None,
        context_tier: None,
        provenance: "refreshed catalog".to_owned(),
    });
    assert_eq!(context_request_shape(&priced), base_shape);

    let mut variants = Vec::new();
    let mut changed = base.clone();
    changed.request_shape.as_mut().unwrap().digest = ContentHash::from_bytes([9; 32]);
    variants.push(changed);
    let mut changed = base.clone();
    changed.route = "other/model".to_owned();
    variants.push(changed);
    let mut changed = base.clone();
    changed.provider_model = "other-wire-model".to_owned();
    variants.push(changed);
    let mut changed = base.clone();
    changed.organization = Some("other-org".to_owned());
    variants.push(changed);
    let mut changed = base.clone();
    changed.credential_profile = Some("other-profile".to_owned());
    variants.push(changed);
    let mut changed = base.clone();
    changed.max_output_tokens += 1;
    variants.push(changed);
    let mut changed = base.clone();
    changed.context_window = Some(32_768);
    variants.push(changed);
    let mut changed = base.clone();
    changed.generation.reasoning_effort = CapabilitySupport::Native;
    variants.push(changed);
    let mut changed = base.clone();
    changed.prompt_cache.control = CapabilitySupport::Native;
    variants.push(changed);

    for changed in variants {
        assert_ne!(context_request_shape(&changed).digest, base_shape.digest);
    }
    // Historical and foreign-version identities degrade to the
    // route-level fallback instead of vanishing.
    let mut historical = base.clone();
    historical.version = qq_protocol::ResolvedModelVersion::new(1).unwrap();
    let historical_shape = context_request_shape(&historical);
    assert!(!historical_shape.provider_identity);
    assert_ne!(historical_shape.digest, base_shape.digest);
    let mut future_identity = base;
    future_identity.request_shape.as_mut().unwrap().version =
        qq_protocol::ProviderRequestShapeVersion::new(2).unwrap();
    let future_shape = context_request_shape(&future_identity);
    assert!(!future_shape.provider_identity);
    assert_eq!(future_shape.digest, historical_shape.digest);
}

#[tokio::test]
async fn compatible_occupancy_seeds_the_first_turn_after_restart_and_pricing_refresh() {
    struct OccupancyLoader {
        calls: Arc<AtomicUsize>,
        pricing: Arc<StdMutex<Option<ModelPricing>>>,
    }

    impl RuntimeLoader for OccupancyLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let calls = Arc::clone(&self.calls);
            let pricing = self.pricing.lock().unwrap().clone();
            Box::pin(async move {
                struct OccupancyProvider(Arc<AtomicUsize>);

                impl Provider for OccupancyProvider {
                    fn stream(&self, _request: ModelRequest) -> ProviderStream {
                        let call = self.0.fetch_add(1, Ordering::SeqCst);
                        let text = if call == 0 {
                            "x".repeat(190_000)
                        } else {
                            "done".to_owned()
                        };
                        Box::pin(stream::iter([
                            Ok(qq_provider::ProviderEvent::OutputTextDelta { text }),
                            Ok(qq_provider::ProviderEvent::Completed {
                                usage: Some(qq_provider::ProviderUsage {
                                    input_tokens: if call == 0 { 1_000 } else { 191_000 },
                                    cache_read_input_tokens: 0,
                                    cache_write_input_tokens: 0,
                                    output_tokens: 1,
                                    reasoning_tokens: None,
                                }),
                            }),
                        ]))
                    }
                }

                Runtime::new(OccupancyProvider(calls), "test-model", 1)
                    .map(|runtime| runtime.with_context_window(Some(200_000)))
                    .map(|runtime| loaded_runtime(runtime, &request.workspace, pricing))
                    .map_err(|error| RuntimeLoadError {
                        kind: RunFailureKind::Configuration,
                        message: error.to_string(),
                    })
            })
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let calls = Arc::new(AtomicUsize::new(0));
    let pricing = Arc::new(StdMutex::new(Some(ModelPricing {
        input_usd_nanos_per_token: 1,
        output_usd_nanos_per_token: 2,
        cache_read_usd_nanos_per_token: None,
        cache_write_usd_nanos_per_token: None,
        context_tier: None,
        provenance: "first catalog".to_owned(),
    })));
    let loader = Arc::new(OccupancyLoader {
        calls: Arc::clone(&calls),
        pricing: Arc::clone(&pricing),
    });
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database_path.clone()),
        loader.clone(),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    submit_prompt_to(&runtime, session_id, "first").await;
    let first = collect_through_finished(&mut events).await;
    let after_first = first.last().unwrap().cursor;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    runtime.shutdown().await.unwrap();
    drop(runtime);
    // A live subscription holds the store open (reads stay available after
    // shutdown), which holds the store's ownership; drop it before reopening.
    drop(events);

    *pricing.lock().unwrap() = Some(ModelPricing {
        input_usd_nanos_per_token: 11,
        output_usd_nanos_per_token: 13,
        cache_read_usd_nanos_per_token: None,
        cache_write_usd_nanos_per_token: None,
        context_tier: None,
        provenance: "refreshed catalog".to_owned(),
    });
    let reopened = SessionRuntime::open(SessionRuntimeOptions::new(database_path), loader)
        .await
        .unwrap();
    let mut events = reopened
        .subscribe(SubscribeRequest {
            workspace_id,
            after: after_first,
        })
        .unwrap();
    submit_prompt_to(&reopened, session_id, "second").await;
    let second = collect_through_finished(&mut events).await;

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        second
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::SessionCompacted { .. }))
    );
    assert!(second.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    )));
    reopened.shutdown().await.unwrap();
}

#[test]
fn accounting_aggregate_sums_known_usage_and_cost() {
    let mut aggregate = AccountingAggregate::known_zero();
    aggregate.add(usage(3, 5), Some(11)).unwrap();
    aggregate.add(usage(7, 13), Some(17)).unwrap();

    assert_eq!(
        aggregate.total(),
        AccountingTotal {
            usage: Some(usage(10, 18)),
            estimated_cost_usd_nanos: Some(28),
        }
    );
}

#[test]
fn accounting_aggregate_keeps_unknown_cost_distinct_from_zero() {
    let mut aggregate = AccountingAggregate::known_zero();
    aggregate.add(usage(3, 5), Some(11)).unwrap();
    aggregate.add(usage(7, 13), None).unwrap();
    aggregate.add(usage(1, 2), Some(17)).unwrap();

    let total = aggregate.total();
    assert_eq!(total.usage, Some(usage(11, 20)));
    assert_eq!(total.estimated_cost_usd_nanos, None);
}

#[test]
fn accounting_aggregate_keeps_zero_usage_cost_unknown() {
    let mut aggregate = AccountingAggregate::known_zero();
    aggregate.add(usage(0, 0), None).unwrap();

    assert_eq!(
        aggregate.total(),
        AccountingTotal {
            usage: Some(usage(0, 0)),
            estimated_cost_usd_nanos: None,
        }
    );
}

#[test]
fn accounting_aggregate_rejects_usage_and_cost_overflow() {
    let mut usage_overflow = AccountingAggregate::known_zero();
    usage_overflow.add(usage(u64::MAX, 0), Some(0)).unwrap();
    assert!(matches!(
        usage_overflow.add(usage(1, 0), Some(0)),
        Err(SessionRuntimeError::AccountingUnavailable)
    ));

    let mut cost_overflow = AccountingAggregate::known_zero();
    cost_overflow.add(usage(1, 0), Some(u64::MAX)).unwrap();
    assert!(matches!(
        cost_overflow.add(usage(1, 0), Some(1)),
        Err(SessionRuntimeError::AccountingUnavailable)
    ));
}

#[test]
fn store_accounting_projects_direct_and_subtree_runs_after_restart_and_pruning() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let parent_id = SessionId::generate().unwrap();
    let first_child_id = SessionId::generate().unwrap();
    let second_child_id = SessionId::generate().unwrap();
    let grandchild_id = SessionId::generate().unwrap();
    {
        let (connection, _) = open_database(&database_path).unwrap();
        connection
            .execute(
                "INSERT INTO workspaces(id, path) VALUES (?1, '/accounting-test')",
                [workspace_id.to_string()],
            )
            .unwrap();
        insert_accounting_session(&connection, workspace_id, parent_id, None);
        insert_accounting_session(&connection, workspace_id, first_child_id, Some(parent_id));
        insert_accounting_session(&connection, workspace_id, second_child_id, Some(parent_id));
        insert_accounting_session(
            &connection,
            workspace_id,
            grandchild_id,
            Some(first_child_id),
        );
        insert_accounting_run(
            &connection,
            parent_id,
            "completed",
            Some(usage(2, 3)),
            Some(5),
        );
        insert_accounting_run(
            &connection,
            first_child_id,
            "failed",
            Some(usage(7, 11)),
            Some(13),
        );
        insert_accounting_run(
            &connection,
            second_child_id,
            "cancelled",
            Some(usage(17, 19)),
            None,
        );
        insert_accounting_run(
            &connection,
            grandchild_id,
            "completed",
            Some(usage(23, 29)),
            Some(31),
        );
    }

    let (connection, _) = open_database(&database_path).unwrap();
    let parent = load_session_accounting(&connection, parent_id).unwrap();
    assert_eq!(
        parent.direct,
        AccountingTotal {
            usage: Some(usage(2, 3)),
            estimated_cost_usd_nanos: Some(5),
        }
    );
    // Inclusive is the whole bounded subtree: parent, both children, and
    // the grandchild under the first child.
    assert_eq!(parent.inclusive.usage, Some(usage(49, 62)));
    assert_eq!(parent.inclusive.estimated_cost_usd_nanos, None);

    let first_child = load_session_accounting(&connection, first_child_id).unwrap();
    assert_eq!(first_child.direct.usage, Some(usage(7, 11)));
    assert_eq!(first_child.inclusive.usage, Some(usage(30, 40)));
    assert_eq!(first_child.inclusive.estimated_cost_usd_nanos, Some(44));

    connection
        .execute(
            "DELETE FROM runs WHERE session_id = ?1",
            [second_child_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM sessions WHERE id = ?1",
            [second_child_id.to_string()],
        )
        .unwrap();
    let pruned = load_session_accounting(&connection, parent_id).unwrap();
    // Parent 2/3 + first child 7/11 + grandchild 23/29; cost 5 + 13 + 31.
    assert_eq!(pruned.inclusive.usage, Some(usage(32, 43)));
    assert_eq!(pruned.inclusive.estimated_cost_usd_nanos, Some(49));
}

#[test]
fn store_accounting_includes_measured_active_rows_and_keeps_alias_consistent() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let (connection, _) = open_database(&database_path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/unknown-accounting-test')",
            [workspace_id.to_string()],
        )
        .unwrap();
    insert_accounting_session(&connection, workspace_id, session_id, None);
    insert_accounting_run(
        &connection,
        session_id,
        "completed",
        Some(usage(2, 3)),
        Some(5),
    );
    let active_run = insert_accounting_run(&connection, session_id, "running", None, None);
    assert_eq!(
        load_session_accounting(&connection, session_id)
            .unwrap()
            .direct,
        AccountingTotal {
            usage: Some(usage(2, 3)),
            estimated_cost_usd_nanos: Some(5),
        }
    );

    connection
        .execute(
            "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json)
             VALUES (?1, 1, '[]')",
            [active_run.to_string()],
        )
        .unwrap();
    assert_eq!(
        load_session_accounting(&connection, session_id)
            .unwrap()
            .direct,
        AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        }
    );

    connection
        .execute(
            "UPDATE runs
             SET usage_json = ?2, estimated_cost_usd_nanos = 13
             WHERE session_id = ?1 AND status = 'running'",
            params![
                session_id.to_string(),
                serde_json::to_string(&usage(7, 11)).unwrap(),
            ],
        )
        .unwrap();
    let summary = load_session_summary(&connection, session_id).unwrap();
    assert_eq!(
        summary.accounting.unwrap().direct,
        AccountingTotal {
            usage: Some(usage(9, 14)),
            estimated_cost_usd_nanos: Some(18),
        }
    );
    assert_eq!(summary.estimated_cost_usd_nanos, Some(18));

    insert_accounting_run(&connection, session_id, "failed", None, None);
    assert_eq!(
        load_session_accounting(&connection, session_id)
            .unwrap()
            .direct,
        AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        }
    );
}

#[test]
fn store_accounting_marks_malformed_persisted_usage_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let session_id = SessionId::generate().unwrap();
    let (connection, _) = open_database(&database_path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/malformed-accounting-test')",
            [workspace_id.to_string()],
        )
        .unwrap();
    insert_accounting_session(&connection, workspace_id, session_id, None);
    insert_accounting_run(
        &connection,
        session_id,
        "completed",
        Some(usage(2, 3)),
        Some(5),
    );
    connection
        .execute(
            "UPDATE runs SET usage_json = '{not-json' WHERE session_id = ?1",
            [session_id.to_string()],
        )
        .unwrap();

    let summary = load_session_summary(&connection, session_id).unwrap();
    assert_eq!(
        summary.accounting.unwrap().direct,
        AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        }
    );
}

#[test]
fn deleting_child_persists_deleted_then_refreshed_parent_projection() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let workspace_id = WorkspaceId::generate().unwrap();
    let parent_id = SessionId::generate().unwrap();
    let child_id = SessionId::generate().unwrap();
    let command_id = CommandId::generate().unwrap();
    let (mut connection, store_id) = open_database(&database_path).unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(id, path) VALUES (?1, '/delete-accounting-test')",
            [workspace_id.to_string()],
        )
        .unwrap();
    insert_accounting_session(&connection, workspace_id, parent_id, None);
    insert_accounting_session(&connection, workspace_id, child_id, Some(parent_id));
    insert_accounting_run(
        &connection,
        child_id,
        "completed",
        Some(usage(7, 11)),
        Some(13),
    );
    assert_eq!(
        load_session_accounting(&connection, parent_id)
            .unwrap()
            .inclusive
            .usage,
        Some(usage(7, 11))
    );

    let transaction = connection.transaction().unwrap();
    let committed_through = delete_idle_session(
        &transaction,
        store_id,
        workspace_id,
        child_id,
        command_id,
        17,
    )
    .unwrap();
    transaction.commit().unwrap();

    let events = read_events(&mut connection, workspace_id, 0, 10).unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].event,
        SessionEvent::SessionDeleted { session_id } if session_id == child_id
    ));
    let SessionEvent::SessionUpdated { session } = &events[1].event else {
        panic!("expected refreshed parent projection");
    };
    assert_eq!(events[1].session_id, parent_id);
    assert_eq!(events[1].cursor.sequence, events[0].cursor.sequence + 1);
    assert_eq!(committed_through.cursor, events[1].cursor);
    assert_eq!(
        session.accounting.unwrap().inclusive,
        AccountingTotal {
            usage: Some(usage(0, 0)),
            estimated_cost_usd_nanos: Some(0),
        }
    );
}

#[tokio::test]
async fn routing_spend_survives_preparation_cancellation_and_recovery() {
    for (spent, complete) in [(false, false), (true, false), (true, true)] {
        for recover in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            let (_, session_id, first) = create_claimed_parent(&store, directory.path()).await;
            store
                .finish_run(
                    &first,
                    RunOutcome::Cancelled,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input: vec![InputPart::text("route this task".to_owned())],
                        limits: RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let run = store.reserve_next_run(false).await.unwrap().unwrap();
            assert!(store.record_routing_started(&run).await.unwrap().is_some());
            assert!(
                store.record_routing_started(&run).await.unwrap().is_none(),
                "a routing request cannot dispatch twice"
            );
            let decision = qq_protocol::RoutingDecision {
                model: run.model.clone(),
                reasoning_effort: None,
                outcome: qq_protocol::RoutingOutcome::Fallback,
                reason: "insufficient confidence".to_owned(),
                usage: Some(usage(7, 0)),
                estimated_cost_usd_nanos: Some(294),
            };
            if spent {
                assert!(
                    store
                        .record_routing_spend(
                            &run,
                            qq_protocol::CheckpointSpend {
                                usage: decision.usage,
                                estimated_cost_usd_nanos: decision.estimated_cost_usd_nanos,
                            }
                        )
                        .await
                        .unwrap()
                );
            }
            if complete {
                assert!(
                    store
                        .record_routing_completed(&run, decision.clone())
                        .await
                        .unwrap()
                        .is_some()
                );
                assert!(
                    store
                        .record_routing_completed(&run, decision.clone())
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            if recover {
                store.recover_interrupted_runs().await.unwrap();
                assert!(
                    store.reserve_next_run(false).await.unwrap().is_none(),
                    "restart must not repeat billed routing"
                );
            } else {
                store
                    .finish_reserved_run(&run, RunOutcome::Cancelled)
                    .await
                    .unwrap();
            }
            assert!(store.record_routing_started(&run).await.unwrap().is_none());
            assert!(
                store
                    .record_routing_completed(&run, decision)
                    .await
                    .unwrap()
                    .is_none(),
                "a late response cannot mutate terminal accounting"
            );
            let events = store
                .events_after(run.identity.workspace_id, 0, 100)
                .await
                .unwrap();
            let (summary, finished_usage) = events
                .iter()
                .find_map(|event| match &event.event {
                    SessionEvent::RunFinished {
                        run_id,
                        session,
                        usage,
                        outcome,
                        ..
                    } if *run_id == run.identity.run_id => {
                        assert_eq!(
                            *outcome,
                            if recover {
                                RunOutcome::Interrupted
                            } else {
                                RunOutcome::Cancelled
                            }
                        );
                        Some((session, *usage))
                    }
                    _ => None,
                })
                .unwrap();
            let expected_usage = spent.then_some(usage(7, 0));
            assert_eq!(finished_usage, expected_usage);
            assert_eq!(
                summary.accounting.as_ref().unwrap().direct.usage,
                expected_usage
            );
            assert_eq!(
                summary
                    .accounting
                    .as_ref()
                    .unwrap()
                    .direct
                    .estimated_cost_usd_nanos,
                spent.then_some(294)
            );
            store.close().await.unwrap();
        }
    }
}

struct RoutingTestLoader {
    reject_selected: bool,
    hold: bool,
    routing_calls: Arc<AtomicUsize>,
}

struct FixedTaskRouter(Arc<AtomicUsize>, bool);

impl TaskRouter for FixedTaskRouter {
    fn route(&self, task: String) -> TaskRoutingFuture {
        assert!(task.contains("route this task"));
        self.0.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            return Box::pin(std::future::pending());
        }
        Box::pin(async {
            qq_protocol::RoutingDecision {
                model: ModelSelection {
                    model: Some("test/selected".to_owned()),
                    max_output_tokens: None,
                    organization: None,
                },
                reasoning_effort: Some(qq_provider::ReasoningEffort::Low),
                outcome: qq_protocol::RoutingOutcome::Selected,
                reason: "bounded fixture choice".to_owned(),
                usage: Some(usage(7, 0)),
                estimated_cost_usd_nanos: Some(294),
            }
        })
    }
    fn max_cost_usd_nanos(&self) -> Option<u64> {
        Some(294)
    }
}

impl RuntimeLoader for RoutingTestLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        if self.reject_selected && request.model.model.as_deref() == Some("test/selected") {
            return Box::pin(async {
                Err(RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: "selected route removed".to_owned(),
                })
            });
        }
        let mut runtime = Runtime::with_provider(
            Arc::new(AccountingTextProvider {
                usage: qq_provider::ProviderUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                },
            }),
            "fixture-model",
            256,
        )
        .unwrap();
        if let Some(effort) = request.reasoning_effort {
            runtime = runtime.with_reasoning_effort(effort);
        }
        let loaded = loaded_runtime_for_route(
            runtime,
            &request.workspace,
            request
                .model
                .model
                .unwrap_or_else(|| "test/model".to_owned()),
            Some(ModelPricing {
                input_usd_nanos_per_token: 1,
                output_usd_nanos_per_token: 1,
                cache_read_usd_nanos_per_token: None,
                cache_write_usd_nanos_per_token: None,
                context_tier: None,
                provenance: "fixture".to_owned(),
            }),
        )
        .with_router(Arc::new(FixedTaskRouter(
            Arc::clone(&self.routing_calls),
            self.hold,
        )));
        Box::pin(async { Ok(loaded) })
    }
}

#[tokio::test]
async fn routing_precedes_provider_preparation_and_charges_the_run_once() {
    for (reject_selected, capped) in [(false, false), (true, false), (false, true)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut harness = spawn_harness_with_loader(
            Arc::new(RoutingTestLoader {
                reject_selected,
                hold: false,
                routing_calls: Arc::clone(&calls),
            }),
            1,
        )
        .await;
        let mut limits = RunLimits::default();
        if capped {
            limits.max_total_tokens = Some(6);
        }
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: harness.session_id,
                    input: vec![InputPart::text("route this task".to_owned())],
                    limits,
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let events = collect_through_finished(&mut harness.events).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let decision_index = events
            .iter()
            .position(|event| matches!(event.event, SessionEvent::RoutingCompleted { .. }))
            .unwrap();
        let SessionEvent::RoutingCompleted { decision, .. } = &events[decision_index].event else {
            unreachable!()
        };
        assert_eq!(
            decision.outcome,
            if reject_selected {
                qq_protocol::RoutingOutcome::Fallback
            } else {
                qq_protocol::RoutingOutcome::Selected
            }
        );
        let expected_route = if reject_selected {
            "test/model"
        } else {
            "test/selected"
        };
        assert_eq!(decision.model.model.as_deref(), Some(expected_route));
        let SessionEvent::RunFinished {
            session,
            usage: finished_usage,
            outcome,
            ..
        } = &events.last().unwrap().event
        else {
            panic!("missing settlement")
        };
        if capped {
            assert!(matches!(outcome, RunOutcome::BudgetExhausted { .. }));
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event.event, SessionEvent::ModelTurnCompleted { .. }))
            );
            assert_eq!(*finished_usage, Some(usage(7, 0)));
            assert_eq!(
                session
                    .accounting
                    .as_ref()
                    .unwrap()
                    .direct
                    .estimated_cost_usd_nanos,
                Some(294)
            );
        } else {
            assert_eq!(*outcome, RunOutcome::Completed);
            assert_eq!(*finished_usage, Some(usage(10, 2)));
            assert_eq!(
                session
                    .accounting
                    .as_ref()
                    .unwrap()
                    .direct
                    .estimated_cost_usd_nanos,
                Some(299)
            );
            let start_index = events
                .iter()
                .position(|event| matches!(event.event, SessionEvent::RunStarted { .. }))
                .unwrap();
            assert!(decision_index < start_index);
            assert!(events.iter().any(|event| matches!(&event.event, SessionEvent::ModelTurnCompleted { model, .. } if model.model.as_deref() == Some(expected_route))));
        }
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn routing_cancellation_stops_before_the_main_provider_and_keeps_spend_unknown() {
    let mut harness = spawn_harness_with_loader(
        Arc::new(RoutingTestLoader {
            reject_selected: false,
            hold: true,
            routing_calls: Arc::new(AtomicUsize::new(0)),
        }),
        1,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "route this task").await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), harness.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(event.event, SessionEvent::RoutingStarted { .. }) {
            break;
        }
    }
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    let events = collect_through_finished(&mut harness.events).await;
    assert!(!events.iter().any(|event| matches!(
        event.event,
        SessionEvent::RunStarted { .. } | SessionEvent::ModelTurnCompleted { .. }
    )));
    let SessionEvent::RunFinished {
        session,
        usage,
        outcome,
        ..
    } = &events.last().unwrap().event
    else {
        panic!("missing settlement")
    };
    assert_eq!(*outcome, RunOutcome::Cancelled);
    assert_eq!(*usage, None);
    assert_eq!(
        session
            .accounting
            .as_ref()
            .unwrap()
            .direct
            .estimated_cost_usd_nanos,
        None
    );
    harness.runtime.shutdown().await.unwrap();
}

#[test]
fn routing_or_review_spend_is_not_a_main_model_context_observation() {
    let mut accounting = RunAccountingAccumulator::new(
        None,
        context_occupancy_basis(
            context_request_shape(&test_resolved_model("test/model", "model", 256, None)).digest,
            test_static_prefix(1, None),
            10,
        ),
    );
    accounting = accounting.with_routing_spend(Some(qq_protocol::CheckpointSpend {
        usage: Some(usage(7, 0)),
        estimated_cost_usd_nanos: Some(294),
    }));
    let snapshot = accounting.snapshot();
    assert_eq!(snapshot.usage, Some(usage(7, 0)));
    assert!(
        !snapshot.saw_turn,
        "auxiliary inference must preserve the main model context meter"
    );
    accounting.record_turn(Some(usage(11, 3)));
    let snapshot = accounting.snapshot();
    assert!(snapshot.saw_turn);
    assert_eq!(snapshot.context_tokens, Some(11));
    assert_eq!(snapshot.usage, Some(usage(18, 3)));
}
