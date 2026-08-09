use chrono::Utc;
use papermachine_protocol::EventId;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionEvent;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionId;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::WorkflowEvent;
use papermachine_protocol::WorkflowEventPayload;
use papermachine_protocol::WorkflowId;

#[test]
fn run_event_uses_a_flat_stable_tag() {
    let event = WorkflowEvent {
        id: EventId::new(),
        sequence: 7,
        project_id: ProjectId::new(),
        workflow_id: WorkflowId::new(),
        occurred_at: Utc::now(),
        payload: WorkflowEventPayload::Warning {
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
        session_id: SessionId::new(),
        turn_id: None,
        step_id: None,
        occurred_at: Utc::now(),
        payload: SessionEventPayload::AssistantMessageReset,
    };

    let value = serde_json::to_value(event).expect("event should serialize");
    assert_eq!(value["type"], "assistant_message_reset");
}

#[test]
fn failed_model_step_exposes_charged_usage() {
    let event = SessionEvent {
        id: EventId::new(),
        sequence: 4,
        session_id: SessionId::new(),
        turn_id: None,
        step_id: None,
        occurred_at: Utc::now(),
        payload: SessionEventPayload::ModelStepFailed {
            step: 1,
            error: "max_output_tokens".to_string(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 100,
                cached_input_tokens: 4,
                cache_write_input_tokens: 0,
            },
        },
    };

    let value = serde_json::to_value(event).expect("event should serialize");
    assert_eq!(value["type"], "model_step_failed");
    assert_eq!(value["usage"]["output_tokens"], 100);
}

#[test]
fn turn_created_event_preserves_message_origin() {
    let event = SessionEvent {
        id: EventId::new(),
        sequence: 5,
        session_id: SessionId::new(),
        turn_id: None,
        step_id: None,
        occurred_at: Utc::now(),
        payload: SessionEventPayload::TurnCreated {
            origin: TurnOrigin::Workflow,
            input: "Investigate route A".to_string(),
            model: "test-model".to_string(),
        },
    };

    let value = serde_json::to_value(event).expect("event should serialize");
    assert_eq!(value["type"], "turn_created");
    assert_eq!(value["origin"], "workflow");
}

#[test]
fn tool_definition_wire_shape_uses_input_schema() {
    let definition = ToolDefinition {
        name: "read_file".to_string(),
        description: "Read one file".to_string(),
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
