use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use papermachine_agent::AgentCheckpoint;
use papermachine_agent::AgentCheckpointContext;
use papermachine_agent::AgentControlPlane;
use papermachine_agent::AgentEvent;
use papermachine_agent::AgentRuntime;
use papermachine_agent::AgentTurnRequest;
use papermachine_agent::RecordingAgentEventSink;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_model::ModelStream;
use papermachine_model::ScriptedModelClient;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::AgentId;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelToolChoice;
use papermachine_protocol::ProjectId;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::SessionId;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::TurnEnvironmentSnapshot;
use papermachine_protocol::TurnId;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkspaceAttachment;
use papermachine_tools::ReadFileTool;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolRegistry;
use papermachine_tools::WriteFileTool;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn managed_root(fixture_root: &std::path::Path) -> std::path::PathBuf {
    let path = fixture_root.join("managed");
    std::fs::create_dir_all(&path).expect("managed fixture should be created");
    path
}

fn turn_environment(
    fixture_root: &std::path::Path,
    preset: AccessPreset,
) -> TurnEnvironmentSnapshot {
    let workspace = fixture_root
        .canonicalize()
        .expect("workspace fixture should canonicalize");
    let managed = managed_root(fixture_root)
        .canonicalize()
        .expect("managed fixture should canonicalize");
    TurnEnvironmentSnapshot::materialize(
        WorkspaceAttachment::single(workspace.to_string_lossy().into_owned()),
        managed.to_string_lossy().into_owned(),
        preset,
    )
    .expect("fixture environment should materialize")
}

fn read_tools() -> ToolRegistry {
    let catalog = ToolCatalog::builder()
        .register_workspace(ReadFileTool)
        .expect("read tool should register")
        .build();
    let snapshot = catalog
        .materialize_action_tools(Some(&["read_file".to_string()]), AccessPreset::Workspace)
        .expect("read tool set should materialize");
    catalog
        .registry_for_snapshot(&snapshot)
        .expect("read registry should rebuild")
}

fn read_write_tools() -> ToolRegistry {
    let catalog = ToolCatalog::builder()
        .register_workspace(ReadFileTool)
        .expect("read tool should register")
        .register_workspace(WriteFileTool)
        .expect("write tool should register")
        .build();
    let snapshot = catalog
        .materialize_action_tools(
            Some(&["read_file".to_string(), "write_file".to_string()]),
            AccessPreset::Workspace,
        )
        .expect("read-write tool set should materialize");
    catalog
        .registry_for_snapshot(&snapshot)
        .expect("read-write registry should rebuild")
}

#[derive(Clone, Copy)]
struct FinishNowControl;

#[async_trait]
impl AgentControlPlane for FinishNowControl {
    async fn checkpoint(
        &self,
        _context: AgentCheckpointContext,
        _cancellation: CancellationToken,
    ) -> Result<AgentCheckpoint, String> {
        Ok(AgentCheckpoint {
            finish: Some("Use the evidence already gathered.".to_string()),
            ..AgentCheckpoint::default()
        })
    }
}

#[tokio::test]
async fn agent_executes_a_tool_then_follows_up() {
    let model = ScriptedModelClient::new([
        vec![
            ModelEvent::ResponseItemCompleted {
                item: serde_json::json!({
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": "encrypted-reasoning"
                }),
            },
            ModelEvent::ResponseItemCompleted {
                item: serde_json::json!({
                    "type": "function_call",
                    "call_id": "call-write",
                    "name": "write_file",
                    "arguments": r#"{"path":"result.md","content":"evidence"}"#
                }),
            },
            ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            },
        ],
        vec![
            ModelEvent::OutputTextDelta {
                delta: "Research ".to_string(),
            },
            ModelEvent::OutputTextDelta {
                delta: "complete.".to_string(),
            },
            ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 30,
                    output_tokens: 4,
                    cached_input_tokens: 10,
                    cache_write_input_tokens: 0,
                },
            },
        ],
    ]);
    let tools = read_write_tools();
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(Arc::new(model.clone()), tools, Arc::new(events.clone()));
    let directory = tempdir().expect("temporary workspace should be created");
    let request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Use tools and report evidence.",
        "Write the evidence file.",
    );

    let result = runtime
        .run(request, CancellationToken::new())
        .await
        .expect("agent should finish");
    assert_eq!(result.final_message, "Research complete.");
    assert_eq!(result.steps, 2);
    assert_eq!(result.usage.input_tokens, 50);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("result.md"))
            .expect("result file should exist"),
        "evidence"
    );

    let requests = model.requests().expect("requests should be recorded");
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].tools.is_empty());
    assert_eq!(requests[0].tools, requests[1].tools);
    assert_eq!(requests[0].instructions, requests[1].instructions);
    assert_eq!(requests[0].tool_choice, ModelToolChoice::Auto);
    assert_eq!(requests[1].tool_choice, ModelToolChoice::Auto);
    assert!(requests[1]
        .input
        .iter()
        .any(|item| matches!(item, ModelInputItem::FunctionCallOutput { call_id, .. } if call_id == "call-write")));
    assert!(requests[1].input.iter().any(|item| {
        matches!(
            item,
            ModelInputItem::ResponseItem { item }
                if item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning")
        )
    }));
    let events = events.events().await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallCompleted { success: true, .. }))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::MessageDelta { .. }))
            .count(),
        1
    );
    let call_checkpoint = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::HistoryCheckpoint { history, .. }
                    if history.iter().any(|item| matches!(
                        item,
                        ModelInputItem::ResponseItem { item }
                            if item.get("call_id").and_then(serde_json::Value::as_str)
                                == Some("call-write")
                    ))
            )
        })
        .expect("the canonical function call should be checkpointed");
    let call_started = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolCallStarted { call } if call.call_id == "call-write"))
        .expect("the tool call should start");
    assert!(call_checkpoint < call_started);

    let output_checkpoint = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::HistoryCheckpoint { history, .. }
                    if history.iter().any(|item| matches!(
                        item,
                        ModelInputItem::FunctionCallOutput { call_id, .. }
                            if call_id == "call-write"
                    ))
            )
        })
        .expect("the canonical function-call output should be checkpointed");
    let call_completed = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolCallCompleted { call_id, .. } if call_id == "call-write"))
        .expect("the tool call should complete");
    assert!(output_checkpoint < call_completed);
}

#[tokio::test]
async fn duplicate_provider_tool_call_ids_fail_before_any_tool_event_or_execution() {
    let duplicate_call = || ModelEvent::ResponseItemCompleted {
        item: serde_json::json!({
            "type": "function_call",
            "call_id": "duplicate-call",
            "name": "write_file",
            "arguments": r#"{"path":"must-not-exist.txt","content":"unsafe"}"#
        }),
    };
    let model = ScriptedModelClient::new([vec![
        duplicate_call(),
        duplicate_call(),
        ModelEvent::Completed {
            usage: TokenUsage::default(),
        },
    ]]);
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(
        Arc::new(model),
        read_write_tools(),
        Arc::new(events.clone()),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Use tools safely.",
        "Write one file.",
    );

    let error = runtime
        .run(request, CancellationToken::new())
        .await
        .expect_err("duplicate tool-call IDs must fail closed");
    assert!(error.to_string().contains("duplicate call ID"));
    assert!(!directory.path().join("must-not-exist.txt").exists());
    assert!(
        !events
            .events()
            .await
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallStarted { .. }))
    );
}

#[tokio::test]
async fn hosted_search_usage_is_observed_across_a_turn() {
    let model = ScriptedModelClient::new([
        vec![
            ModelEvent::ResponseItemCompleted {
                item: serde_json::json!({
                    "type": "web_search_call",
                    "id": "search-1",
                    "status": "completed",
                    "action": {"type": "search", "query": "research evidence"}
                }),
            },
            ModelEvent::ResponseItemCompleted {
                item: serde_json::json!({
                    "type": "function_call",
                    "call_id": "call-read",
                    "name": "read_file",
                    "arguments": r#"{"path":"evidence.txt"}"#
                }),
            },
            ModelEvent::Completed {
                usage: TokenUsage::default(),
            },
        ],
        vec![
            ModelEvent::OutputTextDelta {
                delta: "Done with search.".to_string(),
            },
            ModelEvent::Completed {
                usage: TokenUsage::default(),
            },
        ],
    ]);
    let tools = read_tools();
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(Arc::new(model.clone()), tools, Arc::new(events.clone()));
    let directory = tempdir().expect("temporary workspace should be created");
    std::fs::write(directory.path().join("evidence.txt"), "evidence")
        .expect("fixture should be written");
    let mut request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Research carefully.",
        "Read the evidence.",
    );
    request.hosted_web_search_supported = true;
    request.web_search_context_size = Some(WebSearchContextSize::Low);

    let result = runtime
        .run(request, CancellationToken::new())
        .await
        .expect("agent should finish");
    assert_eq!(result.final_message, "Done with search.");

    let requests = model.requests().expect("requests should be recorded");
    assert_eq!(requests[0].hosted_tools.len(), 1);
    assert_eq!(requests[1].hosted_tools.len(), 1);
    assert_eq!(requests[1].tool_choice, ModelToolChoice::Auto);
    assert_eq!(
        events
            .events()
            .await
            .iter()
            .filter(|event| matches!(event, AgentEvent::HostedToolCompleted { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn empty_registry_and_no_search_context_omit_all_tools() {
    let model = ScriptedModelClient::new([vec![
        ModelEvent::OutputTextDelta {
            delta: "Enough evidence.".to_string(),
        },
        ModelEvent::Completed {
            usage: TokenUsage::default(),
        },
    ]]);
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(
        Arc::new(model.clone()),
        ToolRegistry::default(),
        Arc::new(events),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let mut request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Research carefully.",
        "Find the answer.",
    );
    request.hosted_web_search_supported = true;

    runtime
        .run(request, CancellationToken::new())
        .await
        .expect("agent should finish");

    let requests = model.requests().expect("requests should be recorded");
    assert!(requests[0].tools.is_empty());
    assert!(requests[0].hosted_tools.is_empty());
}

#[tokio::test]
async fn finish_control_forces_the_next_sample_to_disable_tools() {
    let model = ScriptedModelClient::new([vec![
        ModelEvent::OutputTextDelta {
            delta: "Final answer.".to_string(),
        },
        ModelEvent::Completed {
            usage: TokenUsage {
                input_tokens: 12,
                output_tokens: 4,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
        },
    ]]);
    let runtime = AgentRuntime::new(
        Arc::new(model.clone()),
        read_tools(),
        Arc::new(RecordingAgentEventSink::default()),
    )
    .with_control(Arc::new(FinishNowControl));
    let directory = tempdir().expect("temporary workspace should be created");
    let request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Research carefully.",
        "Continue researching.",
    );

    let result = runtime
        .run(request, CancellationToken::new())
        .await
        .expect("finish control should produce a final response");
    assert_eq!(result.final_message, "Final answer.");

    let requests = model.requests().expect("requests should be recorded");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert!(requests[0].hosted_tools.is_empty());
    assert_eq!(requests[0].instructions, "Research carefully.");
    assert_eq!(requests[0].tool_choice, ModelToolChoice::None);
    assert!(requests[0].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::User, content } if content.contains("finish now"))
    }));
    assert!(requests[0].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::User, content } if content.contains("Do not call tools"))
    }));
}

#[tokio::test]
async fn empty_turn_registry_omits_local_tools_and_model_only_omits_hosted_tools() {
    let model = ScriptedModelClient::new([vec![
        ModelEvent::OutputTextDelta {
            delta: "No tools needed.".to_string(),
        },
        ModelEvent::Completed {
            usage: TokenUsage::default(),
        },
    ]]);
    let runtime = AgentRuntime::new(
        Arc::new(model.clone()),
        ToolRegistry::default(),
        Arc::new(RecordingAgentEventSink::default()),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::ModelOnly),
        directory.path().join("sandbox"),
        "test-model",
        "Answer without tools.",
        "Answer directly.",
    );
    runtime
        .run(request, CancellationToken::new())
        .await
        .expect("model-only turn should finish");
    let requests = model.requests().expect("request should be recorded");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert!(requests[0].hosted_tools.is_empty());
}

#[tokio::test]
async fn long_session_history_is_compacted_before_the_next_sample() {
    let model = ScriptedModelClient::new([
        vec![
            ModelEvent::OutputTextDelta {
                delta: "Verified source A; continue with the unresolved comparison.".to_string(),
            },
            ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 2_600,
                    output_tokens: 20,
                    cached_input_tokens: 2_000,
                    cache_write_input_tokens: 0,
                },
            },
        ],
        vec![
            ModelEvent::OutputTextDelta {
                delta: "Final answer from compacted context.".to_string(),
            },
            ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            },
        ],
    ]);
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(
        Arc::new(model.clone()),
        ToolRegistry::default(),
        Arc::new(events.clone()),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let session_id = SessionId::new();
    let agent_id = AgentId::new();
    let mut request = AgentTurnRequest::new(
        ProjectId::new(),
        session_id,
        agent_id,
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Continue the research.",
        "Resolve the remaining question.",
    );
    request.initial_history = vec![
        ModelInputItem::Message {
            role: MessageRole::User,
            content: "Original research objective".to_string(),
        },
        ModelInputItem::Message {
            role: MessageRole::Assistant,
            content: "evidence ".repeat(1_300),
        },
    ];
    request.model_context_window = 4_096;

    let result = runtime
        .run(request, CancellationToken::new())
        .await
        .expect("agent should compact and finish");
    assert_eq!(result.final_message, "Final answer from compacted context.");
    assert_eq!(result.usage.input_tokens, 2_700);
    assert_eq!(result.usage.cached_input_tokens, 2_000);

    let requests = model.requests().expect("requests should be recorded");
    assert_eq!(requests.len(), 2);
    let expected_transport_key = agent_id.to_string();
    let compact_cache = requests[0]
        .prompt_cache
        .as_ref()
        .expect("compaction request should have a prompt-cache configuration");
    let final_cache = requests[1]
        .prompt_cache
        .as_ref()
        .expect("final request should have a prompt-cache configuration");
    assert!(compact_cache.key.ends_with("-agent"));
    assert_ne!(compact_cache.key, expected_transport_key);
    assert_eq!(compact_cache, final_cache);
    assert_eq!(
        requests[0].transport_session_key,
        requests[1].transport_session_key
    );
    assert_eq!(
        requests[0].transport_session_key.as_deref(),
        Some(expected_transport_key.as_str())
    );
    assert!(requests[0].tools.is_empty());
    assert!(requests[0].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::User, content } if content.contains("context checkpoint compaction"))
    }));
    assert!(requests[1].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::User, content } if content.contains("following checkpoint"))
    }));
    assert!(!requests[1].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::Assistant, content } if content.starts_with("evidence"))
    }));
    assert!(events.events().await.iter().any(|event| matches!(
        event,
        AgentEvent::ContextCompactionCompleted {
            before_tokens,
            after_tokens,
            ..
        } if before_tokens > after_tokens
    )));
}

#[derive(Clone, Default)]
struct RetryOnceModel {
    attempts: Arc<AtomicUsize>,
}

#[derive(Clone, Default)]
struct OutputLimitOnceModel {
    attempts: Arc<AtomicUsize>,
    requests: Arc<std::sync::Mutex<Vec<papermachine_protocol::ModelRequest>>>,
}

#[derive(Clone, Default)]
struct OutputLimitAlwaysModel;

#[async_trait]
impl ModelClient for OutputLimitAlwaysModel {
    async fn stream(
        &self,
        _request: papermachine_protocol::ModelRequest,
    ) -> Result<ModelStream, ModelError> {
        Ok(stream::iter([Err(ModelError::IncompleteResponse {
            reason: "max_output_tokens".to_string(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 100,
                cached_input_tokens: 4,
                cache_write_input_tokens: 0,
            },
        })])
        .boxed())
    }
}

#[async_trait]
impl ModelClient for OutputLimitOnceModel {
    async fn stream(
        &self,
        request: papermachine_protocol::ModelRequest,
    ) -> Result<ModelStream, ModelError> {
        self.requests
            .lock()
            .expect("request lock should not be poisoned")
            .push(request);
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return Ok(stream::iter([Err(ModelError::IncompleteResponse {
                reason: "max_output_tokens".to_string(),
                usage: TokenUsage {
                    input_tokens: 11,
                    output_tokens: 32_768,
                    cached_input_tokens: 7,
                    cache_write_input_tokens: 0,
                },
            })])
            .boxed());
        }
        Ok(stream::iter([
            Ok(ModelEvent::OutputTextDelta {
                delta: "concise complete response".to_string(),
            }),
            Ok(ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 4,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            }),
        ])
        .boxed())
    }
}

#[tokio::test]
async fn output_limit_retry_is_concise_and_preserves_failed_usage() {
    let model = OutputLimitOnceModel::default();
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(
        Arc::new(model.clone()),
        ToolRegistry::default(),
        Arc::new(events.clone()),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let mut request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Return valid JSON.",
        "Plan the research routes.",
    );
    request.reasoning_effort = Some(ReasoningEffort::High);

    let result = runtime
        .run(request, CancellationToken::new())
        .await
        .expect("agent should recover from output exhaustion");
    assert_eq!(result.final_message, "concise complete response");
    assert_eq!(result.usage.input_tokens, 15);
    assert_eq!(result.usage.output_tokens, 32_770);
    assert_eq!(result.usage.cached_input_tokens, 7);

    {
        let requests = model
            .requests
            .lock()
            .expect("request lock should not be poisoned");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].reasoning_effort, Some(ReasoningEffort::Low));
        assert!(requests[1].input.iter().any(|item| {
            matches!(item, ModelInputItem::Message { role: MessageRole::User, content } if content.contains("exhausted its output budget"))
        }));
    }
    assert!(events.events().await.iter().any(|event| matches!(
        event,
        AgentEvent::SamplingRetry { attempt: 1, error }
            if error.contains("max_output_tokens")
    )));
}

#[tokio::test]
async fn terminal_output_limit_failure_emits_all_consumed_usage() {
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(
        Arc::new(OutputLimitAlwaysModel),
        ToolRegistry::default(),
        Arc::new(events.clone()),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Return valid JSON.",
        "Plan the research routes.",
    );

    let error = runtime
        .run(request, CancellationToken::new())
        .await
        .expect_err("three output-limited samples should fail the turn");
    assert!(error.to_string().contains("max_output_tokens"));
    assert!(events.events().await.iter().any(|event| matches!(
        event,
        AgentEvent::ModelStepFailed { usage, .. }
            if *usage == TokenUsage {
                input_tokens: 30,
                output_tokens: 300,
                cached_input_tokens: 12,
                cache_write_input_tokens: 0,
            }
    )));
}

#[async_trait]
impl ModelClient for RetryOnceModel {
    async fn stream(
        &self,
        _request: papermachine_protocol::ModelRequest,
    ) -> Result<ModelStream, ModelError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return Ok(stream::iter([
                Ok(ModelEvent::OutputTextDelta {
                    delta: "discarded partial response".repeat(16),
                }),
                Err(ModelError::Stream("connection interrupted".to_string())),
            ])
            .boxed());
        }
        Ok(stream::iter([
            Ok(ModelEvent::OutputTextDelta {
                delta: "successful response".to_string(),
            }),
            Ok(ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 4,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            }),
        ])
        .boxed())
    }
}

#[tokio::test]
async fn retry_discards_partial_deltas_from_the_failed_attempt() {
    let model = RetryOnceModel::default();
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(
        Arc::new(model.clone()),
        ToolRegistry::default(),
        Arc::new(events.clone()),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Return a short response.",
        "Test stream recovery.",
    );

    let result = runtime
        .run(request, CancellationToken::new())
        .await
        .expect("agent should recover from one interrupted stream");
    assert_eq!(result.final_message, "successful response");
    assert_eq!(model.attempts.load(Ordering::SeqCst), 2);

    let events = events.events().await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::SamplingRetry { attempt: 1, .. }))
    );
    let mut visible = String::new();
    for event in events {
        match event {
            AgentEvent::MessageDelta { delta } => visible.push_str(&delta),
            AgentEvent::MessageReset => visible.clear(),
            _ => {}
        }
    }
    assert_eq!(visible, "successful response");
}

#[tokio::test]
async fn retry_recovers_when_provider_completes_with_reasoning_but_no_message() {
    let empty_usage = TokenUsage {
        input_tokens: 12,
        output_tokens: 7,
        cached_input_tokens: 8,
        cache_write_input_tokens: 0,
    };
    let model = ScriptedModelClient::new([
        vec![
            ModelEvent::ResponseItemCompleted {
                item: serde_json::json!({"type": "reasoning", "summary": []}),
            },
            ModelEvent::Completed { usage: empty_usage },
        ],
        vec![
            ModelEvent::OutputTextDelta {
                delta: "recovered response".to_string(),
            },
            ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 12,
                    output_tokens: 3,
                    cached_input_tokens: 12,
                    cache_write_input_tokens: 0,
                },
            },
        ],
    ]);
    let events = RecordingAgentEventSink::default();
    let runtime = AgentRuntime::new(
        Arc::new(model.clone()),
        ToolRegistry::default(),
        Arc::new(events.clone()),
    );
    let directory = tempdir().expect("temporary workspace should be created");
    let request = AgentTurnRequest::new(
        ProjectId::new(),
        SessionId::new(),
        AgentId::new(),
        TurnId::new(),
        turn_environment(directory.path(), AccessPreset::Workspace),
        directory.path().join("sandbox"),
        "test-model",
        "Return a short response.",
        "Test empty completion recovery.",
    );

    let result = runtime
        .run(request, CancellationToken::new())
        .await
        .expect("agent should retry an empty completion");
    assert_eq!(result.final_message, "recovered response");
    assert_eq!(result.usage.input_tokens, 24);
    assert_eq!(result.usage.output_tokens, 10);
    assert_eq!(result.usage.cached_input_tokens, 20);
    let requests = model.requests().expect("requests should be recorded");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].reasoning_effort, Some(ReasoningEffort::Low));
    assert!(requests[1].input.iter().any(|item| {
        matches!(
            item,
            ModelInputItem::Message { role: MessageRole::User, content }
                if content.contains("completed without emitting an answer")
        )
    }));
    assert!(events.events().await.iter().any(|event| {
        matches!(
            event,
            AgentEvent::SamplingRetry { attempt: 1, error }
                if error.contains("without a message or tool call")
        )
    }));
}
