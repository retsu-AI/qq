use super::*;

#[tokio::test]
async fn unknown_workspace_subscription_churn_retains_no_feeds() {
    let harness = scripted_runs_harness(ApprovalMode::ReadOnly, Vec::new()).await;
    let before = harness.runtime.inner.store.retained_feeds();
    for _ in 0..256 {
        let workspace_id = WorkspaceId::generate().unwrap();
        let mut events = harness
            .runtime
            .subscribe_published(SubscribeRequest {
                workspace_id,
                after: EventCursor {
                    store_id: harness.runtime.inner.store.store_id(),
                    workspace_id,
                    sequence: 0,
                },
            })
            .unwrap();
        assert_eq!(harness.runtime.inner.store.retained_feeds(), before);
        assert_eq!(
            events.next().await.unwrap().unwrap_err(),
            SessionRuntimeError::WorkspaceNotFound
        );
        drop(events);
        assert_eq!(harness.runtime.inner.store.retained_feeds(), before);
    }
    harness.runtime.close().await.unwrap();
}

#[tokio::test]
async fn disconnected_subscribers_release_the_feed_and_replay_missed_commits() {
    let harness = scripted_runs_harness(ApprovalMode::ReadOnly, Vec::new()).await;
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
    let request = SubscribeRequest {
        workspace_id: harness.workspace_id,
        after: EventCursor {
            store_id: harness.runtime.inner.store.store_id(),
            workspace_id: harness.workspace_id,
            sequence: 0,
        },
    };
    let mut first = harness.runtime.subscribe_published(request).unwrap();
    let mut second = harness.runtime.subscribe_published(request).unwrap();
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
    let initial = first.next().await.unwrap().unwrap();
    assert_eq!(second.next().await.unwrap().unwrap().json, initial.json);
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 1);
    drop(first);
    let created = create_session(&harness.runtime, harness.workspace_id, None).await;
    let live = second.next().await.unwrap().unwrap();
    assert_eq!(live.envelope.cursor, created.committed_through);
    drop(second);
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);

    let missed = create_session(&harness.runtime, harness.workspace_id, None).await;
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
    let mut restarted = harness.runtime.subscribe_published(request).unwrap();
    assert_eq!(restarted.next().await.unwrap().unwrap().json, initial.json);
    let newest = create_session(&harness.runtime, harness.workspace_id, None).await;
    for cursor in [
        created.committed_through,
        missed.committed_through,
        newest.committed_through,
    ] {
        assert_eq!(
            restarted.next().await.unwrap().unwrap().envelope.cursor,
            cursor
        );
    }
    drop(restarted);
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
    harness.runtime.close().await.unwrap();
}

#[tokio::test]
async fn canceled_feed_attachment_releases_queued_and_unreceived_replies() {
    for cancel_before_reply in [true, false] {
        let harness = scripted_runs_harness(ApprovalMode::ReadOnly, Vec::new()).await;
        let (entered, waiting) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let store = harness.runtime.inner.store.clone();
        let hold = tokio::spawn(async move {
            store
                .call(Priority::Control, move |_| {
                    let _ = entered.send(());
                    blocked
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        waiting.await.unwrap();
        let mut events = Some(
            harness
                .runtime
                .subscribe_published(SubscribeRequest {
                    workspace_id: harness.workspace_id,
                    after: EventCursor {
                        store_id: harness.runtime.inner.store.store_id(),
                        workspace_id: harness.workspace_id,
                        sequence: 0,
                    },
                })
                .unwrap(),
        );
        assert!(events.as_mut().unwrap().next().now_or_never().is_none());
        assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
        if cancel_before_reply {
            drop(events.take());
        }
        release.send(()).unwrap();
        hold.await.unwrap().unwrap();
        // The FIFO control barrier also waits for the attachment reply.
        harness
            .runtime
            .inner
            .store
            .call(Priority::Control, |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            harness.runtime.inner.store.retained_feeds(),
            usize::from(!cancel_before_reply)
        );
        drop(events);
        assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
        harness.runtime.close().await.unwrap();
    }
}

#[tokio::test]
async fn failed_initial_replay_releases_its_feed() {
    let harness = scripted_runs_harness(ApprovalMode::ReadOnly, Vec::new()).await;
    harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            connection.execute("UPDATE events SET envelope_json = 'invalid'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let mut events = harness
        .runtime
        .subscribe_published(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after: EventCursor {
                store_id: harness.runtime.inner.store.store_id(),
                workspace_id: harness.workspace_id,
                sequence: 0,
            },
        })
        .unwrap();
    assert_eq!(
        events.next().await.unwrap().unwrap_err(),
        SessionRuntimeError::CODEC
    );
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
    harness.runtime.close().await.unwrap();
}

/// A store written while Jev existed holds its routing and checkpoint events
/// and `delegate: "jev"` resolutions. Protocol 36 removed those features, but
/// the journal is gapless and permanent: replay must still decode every row
/// instead of failing the subscriber with a codec error.
#[tokio::test]
async fn replay_decodes_events_from_removed_jev_features() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![Vec::new()]).await;
    submit_prompt(&harness, "hello").await;
    collect_through_finished(&mut harness.events).await;
    let legacy = [
        r#"{"type":"routing_started","run_id":"r"}"#,
        r#"{"type":"routing_completed","run_id":"r","decision":{"outcome":"selected"}}"#,
        r#"{"type":"checkpoint_started","run_id":"r","phase":"tool_result"}"#,
        r#"{"type":"checkpoint_reviewed","run_id":"r","outcome":"supported","feedback":"ok"}"#,
    ];
    let sequences = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            let mut statement =
                connection.prepare("SELECT sequence FROM events ORDER BY sequence LIMIT 4")?;
            let sequences = statement
                .query_map([], |row| row.get::<_, u64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for (sequence, event) in sequences.iter().zip(legacy) {
                connection.execute(
                    "UPDATE events SET envelope_json=json_set(envelope_json,'$.event',json(?2))
                     WHERE sequence=?1",
                    params![sequence, event],
                )?;
            }
            Ok(sequences)
        })
        .await
        .unwrap();
    assert_eq!(sequences.len(), 4, "the fixture needs four journal rows");
    let mut events = harness
        .runtime
        .subscribe_published(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after: EventCursor {
                store_id: harness.runtime.inner.store.store_id(),
                workspace_id: harness.workspace_id,
                sequence: 0,
            },
        })
        .unwrap();
    for sequence in sequences {
        let published = events.next().await.unwrap().unwrap();
        assert_eq!(published.envelope.cursor.sequence, sequence);
        assert_eq!(published.envelope.event, SessionEvent::Retired);
        // The bytes a server forwards over SSE are the normalized form,
        // which a protocol-36 client decodes; never the legacy row.
        let forwarded: SessionEventEnvelope = serde_json::from_str(&published.json).unwrap();
        assert_eq!(forwarded, published.envelope);
        assert!(
            published.json.contains(r#""type":"retired""#),
            "{}",
            published.json
        );
    }
    harness.runtime.close().await.unwrap();
}

/// D1: a subscriber that keeps up performs one catch-up read when it
/// attaches and no store read per event afterwards; the live and replayed
/// deliveries are byte-identical and the sequence is contiguous.
#[tokio::test]
async fn live_subscribers_read_the_store_once_and_receive_committed_bytes() {
    let harness = scripted_runs_harness(ApprovalMode::Ask, vec![Vec::new()]).await;
    // Attach eight subscribers, each catching up from the session cursor.
    // Their catch-up reads happen when first polled, so poll each once
    // before the run so the baseline includes them.
    let mut subscribers: Vec<PublishedEventStream> = (0..8)
        .map(|_| {
            harness
                .runtime
                .subscribe_published(SubscribeRequest {
                    workspace_id: harness.workspace_id,
                    after: EventCursor {
                        store_id: harness.runtime.inner.store.store_id(),
                        workspace_id: harness.workspace_id,
                        sequence: 0,
                    },
                })
                .unwrap()
        })
        .collect();
    let mut caught_up = Vec::new();
    for subscriber in &mut subscribers {
        // The SessionCreated event is already committed; catch-up
        // delivers it. Two control reads: the page and the empty page.
        caught_up.push(subscriber.next().await.unwrap().unwrap());
    }
    let reads_before = harness.runtime.inner.store.catch_up_reads();

    let run_id = submit_prompt(&harness, "hello").await;
    let mut streams = Vec::with_capacity(8);
    for subscriber in &mut subscribers {
        let mut observed = vec![];
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), subscriber.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let finished = matches!(event.envelope.event, SessionEvent::RunFinished { .. });
            observed.push(event);
            if finished {
                break;
            }
        }
        streams.push(observed);
    }
    let events_per_subscriber = streams[0].len();
    assert!(events_per_subscriber >= 4, "{events_per_subscriber} events");
    // Every event reached every subscriber from the live feed: zero
    // catch-up reads during the run.
    assert_eq!(
        harness.runtime.inner.store.catch_up_reads(),
        reads_before,
        "8 subscribers observing {events_per_subscriber} events must not read the store"
    );
    // Attaching cost each of the eight subscribers exactly one read; the
    // harness's own stream is never polled here.
    assert_eq!(reads_before, 8);

    // Every subscriber saw the same contiguous sequence with the same
    // bytes, and the bytes equal a fresh replay from the store.
    let replay = harness
        .runtime
        .inner
        .store
        .published_events_after(
            harness.workspace_id,
            caught_up[0].envelope.cursor.sequence,
            128,
        )
        .await
        .unwrap();
    for observed in &streams {
        assert_eq!(observed.len(), replay.len());
        let mut expected = caught_up[0].envelope.cursor.sequence;
        for (live, replayed) in observed.iter().zip(&replay) {
            expected += 1;
            assert_eq!(live.envelope.cursor.sequence, expected, "contiguous");
            assert_eq!(live.json, replayed.json, "live bytes equal stored bytes");
            assert_eq!(live.envelope, replayed.envelope);
        }
    }
    let _ = run_id;
    harness.runtime.shutdown().await.unwrap();
}

/// A reconnecting subscriber whose cursor is still inside the workspace
/// ring attaches and catches up from memory: no store job, byte-identical
/// events, contiguous sequence. A cursor the ring does not cover, or a
/// workspace with no ring, still reads SQLite.
#[tokio::test]
async fn a_warm_reconnect_replays_from_the_ring_without_a_store_read() {
    let harness = scripted_runs_harness(ApprovalMode::Ask, vec![Vec::new()]).await;
    let request = SubscribeRequest {
        workspace_id: harness.workspace_id,
        after: EventCursor {
            store_id: harness.runtime.inner.store.store_id(),
            workspace_id: harness.workspace_id,
            sequence: 0,
        },
    };
    // Cold attach: one store read validates the workspace and pages.
    let mut anchor = harness.runtime.subscribe_published(request).unwrap();
    let created = anchor.next().await.unwrap().unwrap();
    assert_eq!(harness.runtime.inner.store.catch_up_reads(), 1);

    // Sequence 0 predates the ring, so a second cold subscriber at the
    // same cursor still goes to SQLite even though a ring now exists.
    let mut cold = harness.runtime.subscribe_published(request).unwrap();
    assert_eq!(cold.next().await.unwrap().unwrap().json, created.json);
    assert_eq!(harness.runtime.inner.store.catch_up_reads(), 2);
    drop(cold);

    // The anchor's short page seeded the tail: a subscriber exactly at
    // the tail attaches warm and observes the run entirely from memory.
    let mut warm_at_tail = harness
        .runtime
        .subscribe_published(SubscribeRequest {
            after: created.envelope.cursor,
            ..request
        })
        .unwrap();
    assert!(warm_at_tail.next().now_or_never().is_none());
    assert_eq!(harness.runtime.inner.store.catch_up_reads(), 2);

    submit_prompt(&harness, "hello").await;
    let mut live = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), anchor.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let finished = matches!(event.envelope.event, SessionEvent::RunFinished { .. });
        live.push(event);
        if finished {
            break;
        }
    }
    assert!(live.len() >= 4, "{} events", live.len());
    for expected in &live {
        let observed = warm_at_tail.next().await.unwrap().unwrap();
        assert_eq!(observed.json, expected.json);
    }
    assert_eq!(harness.runtime.inner.store.catch_up_reads(), 2);

    // Reconnect from the middle of the run: the page comes from the ring.
    let midpoint = live[1].envelope.cursor;
    let mut reconnect = harness
        .runtime
        .subscribe_published(SubscribeRequest {
            after: midpoint,
            ..request
        })
        .unwrap();
    for expected in &live[2..] {
        let observed = reconnect.next().await.unwrap().unwrap();
        assert_eq!(observed.envelope.cursor, expected.envelope.cursor);
        assert_eq!(
            observed.json, expected.json,
            "ring bytes equal stored bytes"
        );
    }
    assert!(reconnect.next().now_or_never().is_none());
    assert_eq!(
        harness.runtime.inner.store.catch_up_reads(),
        2,
        "warm reconnects must not read the store"
    );

    // The ring vouches only for cursors it covers.
    let mut cold_again = harness.runtime.subscribe_published(request).unwrap();
    assert_eq!(cold_again.next().await.unwrap().unwrap().json, created.json);
    assert_eq!(harness.runtime.inner.store.catch_up_reads(), 3);

    drop((anchor, warm_at_tail, reconnect, cold_again));
    assert_eq!(harness.runtime.inner.store.retained_feeds(), 0);
    // With no ring, the same cursor is cold once more.
    let mut after_release = harness
        .runtime
        .subscribe_published(SubscribeRequest {
            after: midpoint,
            ..request
        })
        .unwrap();
    assert_eq!(
        after_release.next().await.unwrap().unwrap().envelope.cursor,
        live[2].envelope.cursor
    );
    assert_eq!(harness.runtime.inner.store.catch_up_reads(), 4);
    harness.runtime.shutdown().await.unwrap();
}

/// D1: a subscriber that lags past the feed capacity is redirected to
/// SQLite catch-up and still observes every event exactly once, in order.
#[tokio::test]
async fn a_lagging_subscriber_catches_up_from_the_store_without_gaps_or_duplicates() {
    // Each scripted read-only tool run commits a dozen or so events;
    // enough runs overrun the feed while the slow subscriber sits unpolled.
    let runs = feed::FEED_CAPACITY / 14;
    let script = vec![("read_file", r#"{"path":"AGENTS.md"}"#.to_owned())];
    let mut harness = scripted_runs_harness(ApprovalMode::Ask, vec![script; runs]).await;
    let mut slow = harness
        .runtime
        .subscribe_published(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after: EventCursor {
                store_id: harness.runtime.inner.store.store_id(),
                workspace_id: harness.workspace_id,
                sequence: 0,
            },
        })
        .unwrap();
    // Poll once so it is attached live, then leave it unpolled.
    let first = slow.next().await.unwrap().unwrap();
    let mut last = None;
    for index in 0..runs {
        submit_prompt(&harness, &format!("run {index}")).await;
        let observed = collect_through_finished(&mut harness.events).await;
        last = observed.last().map(|event| event.cursor.sequence);
    }
    let last = last.unwrap();
    assert!(
        last as usize > feed::FEED_CAPACITY,
        "{last} events must exceed the feed capacity to lag"
    );

    let mut observed = vec![first];
    while observed.last().unwrap().envelope.cursor.sequence < last {
        let event = tokio::time::timeout(Duration::from_secs(10), slow.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        observed.push(event);
    }
    for (index, event) in observed.iter().enumerate() {
        assert_eq!(
            event.envelope.cursor.sequence,
            index as u64 + 1,
            "no gap, no duplicate"
        );
    }
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_approval_keeps_its_immutable_inputs_without_delivery_events() {
    for (tool, arguments, expected) in [
        ("__test_mutate", "{}", approval::ToolClass::Mutating),
        (
            "__test_shell",
            r#"{"command":"git push origin main","cwd":"crates"}"#,
            approval::ToolClass::Shell {
                command: "git push origin main".to_owned(),
                cwd: Some("crates".to_owned()),
                read_only: false,
            },
        ),
        (
            "ask_user",
            r#"{"questions":[{"prompt":"Which crate?","options":["core","provider"],"free_text":true}]}"#,
            approval::ToolClass::Interactive {
                question: Some(QuestionPreview {
                    questions: vec![qq_protocol::Question {
                        prompt: "Which crate?".to_owned(),
                        options: vec!["core".to_owned(), "provider".to_owned()],
                        free_text: true,
                    }],
                }),
            },
        ),
        #[cfg(feature = "tool-fetch")]
        (
            "fetch",
            r#"{"url":"https://docs.invalid/a"}"#,
            approval::ToolClass::Network {
                host: Some("docs.invalid".to_owned()),
                refusal: None,
            },
        ),
    ] {
        let mut harness = approval_harness(
            ApprovalMode::Ask,
            tool,
            arguments,
            1,
            DEFAULT_APPROVAL_TIMEOUT,
        )
        .await;
        let (observed, call) = collect_until_approval_requested(&mut harness.events).await;
        let expected_previews = observed
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::ToolApprovalRequested {
                    shell,
                    edit,
                    question,
                    fetch,
                    delegated,
                    ..
                } => Some(tool_calls::ApprovalPreviews {
                    shell: shell.as_deref().cloned(),
                    edit: edit.clone(),
                    question: question.as_deref().cloned(),
                    fetch: fetch.as_deref().cloned(),
                    delegated: *delegated,
                }),
                _ => None,
            })
            .unwrap();
        let stored_previews: String = harness
            .runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT approval_inputs_json FROM tool_calls WHERE id=?1",
                    [call.id.to_string()],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<tool_calls::ApprovalPreviews>(&stored_previews).unwrap(),
            expected_previews,
            "recovery must retain the full server-computed preview, including presentation inputs"
        );
        let workspace_id = harness.workspace_id;
        let session_id = harness.session_id;
        let run_id = call.run_id;
        let call_id = call.id;
        // Keep a genuine pre-recovery store for both migration cases.
        for missing in [false, true] {
            let path = harness._directory.path().join(if missing {
                "legacy-missing.sqlite3"
            } else {
                "legacy-complete.sqlite3"
            });
            let backup = path.clone();
            harness
                .runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    connection.execute("VACUUM INTO ?1", [backup.to_str().unwrap()])?;
                    Ok(())
                })
                .await
                .unwrap();
            let legacy = Connection::open(&path).unwrap();
            legacy
                .execute_batch(
                    "DROP TRIGGER tool_calls_clear_approval_inputs;
                ALTER TABLE tool_calls DROP COLUMN approval_inputs_json;
                UPDATE metadata SET value='60' WHERE key='schema_version';",
                )
                .unwrap();
            if missing {
                legacy.execute("DELETE FROM events", []).unwrap();
            }
            drop(legacy);
            let (migrated, _) = store::open_database(&path).unwrap();
            migrated.execute("DELETE FROM events", []).unwrap();
            let recovered = tool_calls::load_pending_approval_class(
                &migrated,
                workspace_id,
                session_id,
                run_id,
                call_id,
            );
            if missing {
                assert_eq!(recovered.unwrap_err(), SessionRuntimeError::CODEC);
                // No guess or global startup failure for an incomplete legacy hold.
                assert_eq!(
                    migrated
                        .query_row(
                            "SELECT state FROM tool_calls WHERE id=?1",
                            [call_id.to_string()],
                            |row| row.get::<_, String>(0)
                        )
                        .unwrap(),
                    "awaiting_approval"
                );
            } else {
                assert_eq!(recovered.unwrap(), (tool.to_owned(), expected.clone()));
            }
            drop(migrated);
            let (reopened, _) = store::open_database(&path).unwrap();
            assert_eq!(
                reopened
                    .query_row(
                        "SELECT value FROM metadata WHERE key='schema_version'",
                        [],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                "61"
            );
        }
        let recovered = harness
            .runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                // Durable inputs must not cross any of these authority boundaries.
                for (workspace, session, run) in [
                    (WorkspaceId::generate().unwrap(), session_id, run_id),
                    (workspace_id, SessionId::generate().unwrap(), run_id),
                    (workspace_id, session_id, RunId::generate().unwrap()),
                ] {
                    assert!(
                        tool_calls::load_pending_approval_class(
                            connection, workspace, session, run, call_id
                        )
                        .is_err()
                    );
                }
                connection.execute(
                    "DELETE FROM events WHERE workspace_id=?1",
                    [workspace_id.to_string()],
                )?;
                tool_calls::load_pending_approval_class(
                    connection,
                    workspace_id,
                    session_id,
                    run_id,
                    call_id,
                )
            })
            .await
            .unwrap();
        assert_eq!(recovered, (tool.to_owned(), expected));
        respond_approval(&harness.runtime, run_id, call_id, ApprovalDecision::Deny)
            .await
            .unwrap();
        collect_through_finished(&mut harness.events).await;
        let retained: Option<String> = harness
            .runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT approval_inputs_json FROM tool_calls WHERE id=?1",
                    [call_id.to_string()],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            retained, None,
            "resolved input snapshots must not accumulate"
        );
        harness.runtime.shutdown().await.unwrap();
    }
}
