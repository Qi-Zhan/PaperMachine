use papermachine_protocol::AccessPreset;
use papermachine_protocol::ContextReplacementReason;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionRolloutItem;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::TurnOrigin;
use papermachine_store::Store;
use papermachine_store::TurnContextCheckpoint;
use rusqlite::Connection;
use rusqlite::params;
use std::io::Write;
use std::sync::Arc;
use std::sync::Barrier;
use tempfile::tempdir;

mod support;
use support::ActionHarness;

#[test]
fn rollout_reconstructs_completed_context_without_turn_history_copies() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let store = Store::create(&managed).expect("Store should be created");
    let project = store
        .create_project("Rollout", directory.path().join("workspace"))
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session = store
        .get_session(harness.participant.session_id)
        .expect("participant Session should load");
    let turn = harness
        .create_turn(
            &store,
            TurnOrigin::Workflow,
            "question",
            AccessPreset::Research,
        )
        .expect("Turn should be created");
    store.start_turn(turn.id).expect("Turn should start");
    let context = vec![
        message(MessageRole::User, "question"),
        message(MessageRole::Assistant, "answer"),
    ];
    store
        .checkpoint_turn_context(
            turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Append {
                    items: context.clone(),
                },
                usage: TokenUsage::default(),
                completed_model_steps: 1,
                hosted_search_calls_used: 0,
                checkpoint_message: Some("answer".to_string()),
                acknowledged_control_ids: Vec::new(),
            },
        )
        .expect("context should checkpoint");
    store
        .complete_turn(turn.id, "answer".to_string(), TokenUsage::default())
        .expect("Turn should complete");

    let state = store
        .reconstruct_session_rollout(session.id)
        .expect("rollout should reconstruct");
    assert_eq!(state.committed_context, context);
    assert!(state.active_turn.is_none());
    let rollout_status = store
        .session_rollout_status(session.id)
        .expect("rollout status should load");
    assert_eq!(rollout_status.version, 3);
    assert!(rollout_status.last_sequence > 0);
    assert_eq!(
        rollout_status.projected_sequence,
        rollout_status.last_sequence
    );

    let connection =
        Connection::open(managed.join("state/project.db")).expect("Project database should open");
    let document: String = connection
        .query_row(
            "SELECT document_json FROM turns WHERE id = ?1",
            [turn.id.to_string()],
            |row| row.get(0),
        )
        .expect("Turn document should load");
    let document: serde_json::Value =
        serde_json::from_str(&document).expect("Turn JSON should parse");
    assert!(document.get("history").is_none());
}

#[test]
fn opening_store_replays_rollout_ahead_of_sqlite_projection() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let store = Store::create(&managed).expect("Store should be created");
    let project = store
        .create_project("Replay", directory.path().join("workspace"))
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session = store
        .get_session(harness.participant.session_id)
        .expect("participant Session should load");
    let turn = harness
        .create_turn(
            &store,
            TurnOrigin::Workflow,
            "resume me",
            AccessPreset::Research,
        )
        .expect("Turn should be created");
    let before_checkpoint = store.start_turn(turn.id).expect("Turn should start");
    let context = vec![message(MessageRole::User, "resume me")];
    let usage = TokenUsage {
        input_tokens: 17,
        output_tokens: 3,
        ..TokenUsage::default()
    };
    store
        .checkpoint_turn_context(
            turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Append {
                    items: context.clone(),
                },
                usage,
                completed_model_steps: 2,
                hosted_search_calls_used: 1,
                checkpoint_message: None,
                acknowledged_control_ids: Vec::new(),
            },
        )
        .expect("context should checkpoint");
    let records = store
        .list_session_rollout_records(session.id)
        .expect("records should load");
    let checkpoint_sequence = records.last().expect("checkpoint record").sequence;
    drop(store);

    let connection =
        Connection::open(managed.join("state/project.db")).expect("Project database should open");
    connection
        .execute(
            "UPDATE turns SET updated_at = ?1, document_json = ?2 WHERE id = ?3",
            params![
                before_checkpoint.updated_at.to_rfc3339(),
                serde_json::to_string(&before_checkpoint).expect("Turn should serialize"),
                turn.id.to_string()
            ],
        )
        .expect("Turn projection should rewind");
    connection
        .execute(
            "UPDATE session_rollout_projection SET last_sequence = ?1 WHERE session_id = ?2",
            params![checkpoint_sequence - 1, session.id.to_string()],
        )
        .expect("projection cursor should rewind");
    drop(connection);

    let reopened = Store::open(&managed).expect("Store should replay pending rollout");
    let projected = reopened.get_turn(turn.id).expect("Turn should load");
    assert_eq!(projected.completed_model_steps, 2);
    assert_eq!(projected.hosted_search_calls_used, 1);
    assert_eq!(projected.usage, usage);
    let active = reopened
        .reconstruct_session_rollout(session.id)
        .expect("rollout should reconstruct")
        .active_turn
        .expect("Turn should remain active");
    assert_eq!(active.context, context);
    assert!(active.has_checkpoint);
}

#[test]
fn truncated_final_record_is_repaired_without_losing_prior_records() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let store = Store::create(&managed).expect("Store should be created");
    let project = store
        .create_project("Tail repair", directory.path().join("workspace"))
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session = store
        .get_session(harness.participant.session_id)
        .expect("participant Session should load");
    harness
        .create_turn(
            &store,
            TurnOrigin::Workflow,
            "durable",
            AccessPreset::Research,
        )
        .expect("Turn should create a canonical record");
    let rollout_path = store.session_rollout_path(session.id);
    let expected_len = std::fs::metadata(&rollout_path)
        .expect("rollout metadata should load")
        .len();
    drop(store);

    std::fs::OpenOptions::new()
        .append(true)
        .open(&rollout_path)
        .expect("rollout should open")
        .write_all(br#"{"version":1,"session_id""#)
        .expect("partial record should be written");

    let reopened = Store::open(&managed).expect("truncated tail should be repaired");
    assert_eq!(
        reopened
            .list_session_rollout_records(session.id)
            .expect("records should load")
            .len(),
        1
    );
    assert_eq!(
        std::fs::metadata(&rollout_path)
            .expect("rollout metadata should load")
            .len(),
        expected_len
    );
}

#[test]
fn assistant_deltas_are_broadcast_without_entering_durable_history() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
    let project = store
        .create_project("Streaming", directory.path().join("workspace"))
        .expect("Project should be created");
    let session = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let durable_before = store
        .list_session_events(session.id, 0)
        .expect("events should load");
    let mut subscriber = store.subscribe_sessions();

    let delta = store
        .publish_transient_session_event(
            session.id,
            None,
            None,
            SessionEventPayload::AssistantMessageDelta {
                delta: "partial".to_string(),
            },
        )
        .expect("delta should publish");

    assert_eq!(delta.sequence, 0);
    assert_eq!(
        subscriber
            .try_recv()
            .expect("subscriber should receive the delta"),
        delta
    );
    assert_eq!(
        store
            .list_session_events(session.id, 0)
            .expect("events should load"),
        durable_before
    );
    assert!(
        store
            .append_session_event(
                session.id,
                None,
                None,
                SessionEventPayload::AssistantMessageReset,
            )
            .is_err()
    );
}

#[test]
fn compaction_replaces_reconstructed_context_but_keeps_prior_records() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
    let project = store
        .create_project("Compaction", directory.path().join("workspace"))
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session = store
        .get_session(harness.participant.session_id)
        .expect("participant Session should load");
    let turn = harness
        .create_turn(
            &store,
            TurnOrigin::Workflow,
            "large context",
            AccessPreset::Research,
        )
        .expect("Turn should be created");
    store.start_turn(turn.id).expect("Turn should start");
    store
        .checkpoint_turn_context(
            turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Append {
                    items: vec![
                        message(MessageRole::User, "large context"),
                        message(MessageRole::Assistant, "old evidence"),
                    ],
                },
                usage: TokenUsage::default(),
                completed_model_steps: 1,
                hosted_search_calls_used: 0,
                checkpoint_message: None,
                acknowledged_control_ids: Vec::new(),
            },
        )
        .expect("initial context should checkpoint");
    let compacted = vec![message(MessageRole::User, "compacted evidence summary")];
    store
        .checkpoint_turn_context(
            turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Replace {
                    items: compacted.clone(),
                    reason: ContextReplacementReason::Compaction,
                },
                usage: TokenUsage::default(),
                completed_model_steps: 1,
                hosted_search_calls_used: 0,
                checkpoint_message: None,
                acknowledged_control_ids: Vec::new(),
            },
        )
        .expect("compacted context should checkpoint");
    let final_item = message(MessageRole::Assistant, "done");
    store
        .checkpoint_turn_context(
            turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Append {
                    items: vec![final_item.clone()],
                },
                usage: TokenUsage::default(),
                completed_model_steps: 2,
                hosted_search_calls_used: 0,
                checkpoint_message: Some("done".to_string()),
                acknowledged_control_ids: Vec::new(),
            },
        )
        .expect("terminal context should checkpoint");
    store
        .complete_turn(turn.id, "done".to_string(), TokenUsage::default())
        .expect("Turn should complete");

    let state = store
        .reconstruct_session_rollout(session.id)
        .expect("rollout should reconstruct");
    let mut expected = compacted;
    expected.push(final_item);
    assert_eq!(state.committed_context, expected);
    let records = store
        .list_session_rollout_records(session.id)
        .expect("records should load");
    assert!(records.iter().any(|record| matches!(
        record.item,
        SessionRolloutItem::ContextCheckpoint {
            mutation: ModelContextMutation::Replace {
                reason: ContextReplacementReason::Compaction,
                ..
            },
            ..
        }
    )));
}

#[test]
fn concurrent_session_appends_have_one_contiguous_sequence() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("Store should open"),
    );
    let project = store
        .create_project("Writer", directory.path().join("workspace"))
        .expect("Project should be created");
    let session = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let before = store
        .list_session_events(session.id, 0)
        .expect("initial events should load")
        .last()
        .map_or(0, |event| event.sequence);
    let mut writers = Vec::new();
    for index in 0..16 {
        let store = Arc::clone(&store);
        writers.push(std::thread::spawn(move || {
            store
                .append_session_event(
                    session.id,
                    None,
                    None,
                    SessionEventPayload::Warning {
                        message: format!("event-{index}"),
                    },
                )
                .expect("event should append");
        }));
    }
    for writer in writers {
        writer.join().expect("writer should join");
    }
    let records = store
        .list_session_events(session.id, before)
        .expect("events should load");
    assert_eq!(records.len(), 16);
    assert!(
        records
            .iter()
            .enumerate()
            .all(|(index, record)| record.sequence == before + index as u64 + 1)
    );
}

#[test]
fn one_session_writer_admits_only_one_concurrent_active_turn() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("Store should open"),
    );
    let project = store
        .create_project("Turn writer", directory.path().join("workspace"))
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session_id = harness.participant.session_id;
    let barrier = Arc::new(Barrier::new(3));
    let mut writers = Vec::new();
    for input in ["first", "second"] {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let harness = harness.clone();
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            harness.create_turn(&store, TurnOrigin::Workflow, input, AccessPreset::Research)
        }));
    }
    barrier.wait();
    let results = writers
        .into_iter()
        .map(|writer| writer.join().expect("writer should join"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(
        store
            .list_turns(session_id)
            .expect("Turns should load")
            .len(),
        1
    );
}

fn message(role: MessageRole, content: &str) -> ModelInputItem {
    ModelInputItem::Message {
        role,
        content: content.to_string(),
    }
}
