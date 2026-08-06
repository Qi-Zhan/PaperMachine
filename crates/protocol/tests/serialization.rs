use chrono::Utc;
use papermachine_protocol::EventId;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionEvent;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionId;
use papermachine_protocol::TokenUsage;
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
