use chrono::Utc;
use papermachine_protocol::AgentId;
use papermachine_protocol::EventId;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionEvent;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionId;
use papermachine_protocol::ToolDefinition;

#[test]
fn session_event_uses_a_flat_stable_tag() {
    let event = SessionEvent {
        id: EventId::new(),
        sequence: 7,
        project_id: ProjectId::new(),
        session_id: SessionId::new(),
        agent_id: Some(AgentId::new()),
        turn_id: None,
        step_id: None,
        occurred_at: Utc::now(),
        payload: SessionEventPayload::Warning {
            message: "check evidence".to_string(),
        },
    };

    let value = serde_json::to_value(event).expect("event should serialize");
    assert_eq!(value["type"], "warning");
    assert_eq!(value["sequence"], 7);
    assert_eq!(value["message"], "check evidence");
}

#[test]
fn session_message_reset_has_a_stable_sse_tag() {
    let event = SessionEvent {
        id: EventId::new(),
        sequence: 3,
        project_id: ProjectId::new(),
        session_id: SessionId::new(),
        agent_id: None,
        turn_id: None,
        step_id: None,
        occurred_at: Utc::now(),
        payload: SessionEventPayload::AssistantMessageReset,
    };

    let value = serde_json::to_value(event).expect("event should serialize");
    assert_eq!(value["type"], "assistant_message_reset");
}

#[test]
fn model_step_event_is_only_a_durable_reference() {
    let event = SessionEvent {
        id: EventId::new(),
        sequence: 4,
        project_id: ProjectId::new(),
        session_id: SessionId::new(),
        agent_id: Some(AgentId::new()),
        turn_id: None,
        step_id: None,
        occurred_at: Utc::now(),
        payload: SessionEventPayload::ModelStepFailed,
    };

    let value = serde_json::to_value(event).expect("event should serialize");
    assert_eq!(value["type"], "model_step_failed");
    assert!(value.get("usage").is_none());
    assert!(value.get("error").is_none());
}

#[test]
fn turn_created_event_does_not_duplicate_turn_fields() {
    let event = SessionEvent {
        id: EventId::new(),
        sequence: 5,
        project_id: ProjectId::new(),
        session_id: SessionId::new(),
        agent_id: Some(AgentId::new()),
        turn_id: None,
        step_id: None,
        occurred_at: Utc::now(),
        payload: SessionEventPayload::TurnCreated,
    };

    let value = serde_json::to_value(event).expect("event should serialize");
    assert_eq!(value["type"], "turn_created");
    assert!(value.get("input").is_none());
    assert!(value.get("model").is_none());
}

#[test]
fn tool_definition_wire_shape_uses_input_schema() {
    let definition = ToolDefinition {
        name: "exec_command".to_string(),
        description: "Run one command".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        supports_parallel: true,
    };
    let value = serde_json::to_value(&definition).expect("ToolDefinition should serialize");

    assert_eq!(value["input_schema"]["type"], "object");
    assert!(value.get("parameters").is_none());
    assert_eq!(
        serde_json::from_value::<ToolDefinition>(value).expect("ToolDefinition should deserialize"),
        definition
    );
}
