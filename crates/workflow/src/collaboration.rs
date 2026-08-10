use crate::ActionControl;
use async_trait::async_trait;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::ActionSource;
use papermachine_protocol::ActionStatus;
use papermachine_protocol::AgentId;
use papermachine_protocol::AgentInputId;
use papermachine_protocol::AgentInputKind;
use papermachine_protocol::AgentInputSource;
use papermachine_protocol::ToolDefinition;
use papermachine_store::NewActionInvocation;
use papermachine_store::StoreError;
use papermachine_store::StoreHandle;
use papermachine_tools::ToolCatalogBuilder;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use papermachine_tools::ToolExecutor;
use papermachine_tools::ToolOutput;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use std::str::FromStr;
use std::time::Duration;
use tokio::time::Instant;

const DEFAULT_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 30_000;
const MAX_WAIT_ACTIONS: usize = 64;
pub const COLLABORATION_TOOL_NAMES: [&str; 5] = [
    "list_agents",
    "send_message",
    "wait_agent",
    "spawn_agent",
    "interrupt_agent",
];

#[derive(Clone)]
pub struct CollaborationTools {
    store: StoreHandle,
    actions: ActionControl,
    max_children: usize,
}

impl CollaborationTools {
    pub fn new(store: StoreHandle, actions: ActionControl, max_children: usize) -> Self {
        Self {
            store,
            actions,
            max_children: max_children.max(1),
        }
    }

    pub fn register(&self, builder: ToolCatalogBuilder) -> Result<ToolCatalogBuilder, ToolError> {
        builder
            .register_collaboration(ListAgentsTool(self.clone()))?
            .register_collaboration(SendMessageTool(self.clone()))?
            .register_collaboration(WaitAgentTool(self.clone()))?
            .register_collaboration(SpawnAgentTool(self.clone()))?
            .register_collaboration(InterruptAgentTool(self.clone()))
    }
}

#[derive(Clone)]
struct ListAgentsTool(CollaborationTools);

#[async_trait]
impl ToolExecutor for ListAgentsTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "list_agents",
            "List the Agents in this Project, their Session tree, current activity, and most recent Action outcome.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            true,
        )
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        parse_empty("list_agents", arguments)?;
        let project_id = context.project_id;
        let caller_id = context.agent_id;
        let agents = self
            .0
            .store
            .call(move |store| {
                let caller = store.get_agent(caller_id)?;
                let caller_session = store.get_session(caller.session_id)?;
                if caller_session.project_id != project_id {
                    return Err(StoreError::Invariant(
                        "tool caller does not belong to the active Project".to_string(),
                    ));
                }
                let mut result = Vec::new();
                for session in store.list_project_sessions(project_id)? {
                    let actions = store.list_action_invocations(session.id)?;
                    for agent in store.list_agents(session.id)? {
                        let agent_actions = actions
                            .iter()
                            .filter(|action| action.agent_id == agent.id)
                            .collect::<Vec<_>>();
                        let activity =
                            if session.archived_at.is_some() || !session.status.accepts_actions() {
                                "unavailable"
                            } else if agent_actions
                                .iter()
                                .any(|action| action.status == ActionStatus::Running)
                            {
                                "running"
                            } else if agent_actions.iter().any(|action| {
                                matches!(
                                    action.status,
                                    ActionStatus::Scheduled | ActionStatus::Interrupted
                                )
                            }) {
                                "queued"
                            } else {
                                "idle"
                            };
                        let last_outcome = agent_actions
                            .iter()
                            .rev()
                            .find(|action| {
                                matches!(
                                    action.status,
                                    ActionStatus::Completed
                                        | ActionStatus::Failed
                                        | ActionStatus::Cancelled
                                )
                            })
                            .map(|action| {
                                json!({
                                    "action_invocation_id": action.id,
                                    "action_name": action.action_name,
                                    "status": action.status,
                                    "error": action.error,
                                })
                            });
                        result.push(json!({
                            "agent_id": agent.id,
                            "session_id": session.id,
                            "parent_agent_id": agent.parent_agent_id,
                            "name": agent.name,
                            "role": agent.role,
                            "access": agent.access,
                            "activity": activity,
                            "last_outcome": last_outcome,
                            "caller": agent.id == caller_id,
                        }));
                    }
                }
                Ok::<_, StoreError>(result)
            })
            .await
            .map_err(store_error)?;
        Ok(ToolOutput {
            summary: format!("listed {} Agent(s)", agents.len()),
            value: json!({"agents": agents}),
        })
    }
}

#[derive(Clone)]
struct SendMessageTool(CollaborationTools);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendMessageArgs {
    agent_id: String,
    message: String,
    #[serde(default)]
    start_turn: bool,
}

#[async_trait]
impl ToolExecutor for SendMessageTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "send_message",
            "Send durable input to another Agent. Set start_turn=true to schedule the message as a normal agent_task Action.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string"},
                    "message": {"type": "string"},
                    "start_turn": {"type": "boolean"}
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: SendMessageArgs = parse("send_message", arguments)?;
        let target_id = parse_agent_id("send_message", &args.agent_id)?;
        if target_id == context.agent_id {
            return Err(invalid("send_message", "target must be another Agent"));
        }
        if args.message.trim().is_empty() {
            return Err(invalid("send_message", "message must not be empty"));
        }
        let sender_id = context.agent_id;
        let project_id = context.project_id;
        if args.start_turn {
            let invocation_id =
                ActionInvocationId::from_uuid(tool_resource_uuid(&context, "send-message-action"));
            let message = args.message;
            let action = self
                .0
                .store
                .call(move |store| {
                    let sender = store.get_agent(sender_id)?;
                    let sender_session = store.get_session(sender.session_id)?;
                    let target = store.get_agent(target_id)?;
                    let target_session = store.get_session(target.session_id)?;
                    if sender_session.project_id != project_id
                        || target_session.project_id != project_id
                    {
                        return Err(StoreError::Invariant(
                            "Agent collaboration cannot cross a Project boundary".to_string(),
                        ));
                    }
                    let input =
                        format!("Task from {} ({}):\n{}", sender.name, sender.id, message);
                    store.create_action_invocation_with_id(
                        invocation_id,
                        NewActionInvocation {
                            session_id: target.session_id,
                            agent_id: target.id,
                            action_name: "agent_task".to_string(),
                            contract: "Complete the task sent by another Agent and return a concise, self-contained result.".to_string(),
                            arguments: json!({
                                "message": message,
                                "sender_agent_id": sender.id,
                            }),
                            input,
                            source: ActionSource::Agent {
                                sender_agent_id: sender.id,
                            },
                            tool_policy: None,
                            web_search_context_size: None,
                            reasoning_effort: None,
                            response_format: None,
                        },
                    )
                })
                .await
                .map_err(store_error)?;
            Ok(ToolOutput {
                summary: format!("scheduled agent_task {}", action.id),
                value: json!({
                    "agent_id": target_id,
                    "action_invocation_id": action.id,
                    "status": action.status,
                }),
            })
        } else {
            let input_id =
                AgentInputId::from_uuid(tool_resource_uuid(&context, "send-message-input"));
            let message = self
                .0
                .store
                .call(move |store| {
                    let target = store.get_agent(target_id)?;
                    let target_session = store.get_session(target.session_id)?;
                    if target_session.project_id != project_id {
                        return Err(StoreError::Invariant(
                            "Agent collaboration cannot cross a Project boundary".to_string(),
                        ));
                    }
                    store.create_agent_input_with_id(
                        input_id,
                        target.session_id,
                        target.id,
                        None,
                        AgentInputSource::Agent {
                            sender_agent_id: sender_id,
                        },
                        AgentInputKind::Message,
                        args.message,
                    )
                })
                .await
                .map_err(store_error)?;
            Ok(ToolOutput {
                summary: format!("queued input for Agent {target_id}"),
                value: json!({
                    "agent_id": target_id,
                    "agent_input_id": message.id,
                    "status": message.status,
                }),
            })
        }
    }
}

#[derive(Clone)]
struct WaitAgentTool(CollaborationTools);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitAgentArgs {
    action_invocation_ids: Vec<String>,
    timeout_ms: Option<u64>,
}

#[async_trait]
impl ToolExecutor for WaitAgentTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "wait_agent",
            "Wait for delegated Action invocations and return their durable statuses and outputs.",
            json!({
                "type": "object",
                "properties": {
                    "action_invocation_ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_WAIT_ACTIONS
                    },
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_MS}
                },
                "required": ["action_invocation_ids"],
                "additionalProperties": false
            }),
            true,
        )
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: WaitAgentArgs = parse("wait_agent", arguments)?;
        if args.action_invocation_ids.is_empty()
            || args.action_invocation_ids.len() > MAX_WAIT_ACTIONS
        {
            return Err(invalid(
                "wait_agent",
                "action_invocation_ids must contain between 1 and 64 ids",
            ));
        }
        let mut seen = HashSet::new();
        let mut ids = Vec::with_capacity(args.action_invocation_ids.len());
        for value in args.action_invocation_ids {
            let id = ActionInvocationId::from_str(&value)
                .map_err(|error| invalid("wait_agent", &error.to_string()))?;
            if !seen.insert(id) {
                return Err(invalid(
                    "wait_agent",
                    "action_invocation_ids contains a duplicate",
                ));
            }
            ids.push(id);
        }
        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut events = self
            .0
            .store
            .call::<_, StoreError, _>(|store| Ok(store.subscribe()))
            .await
            .map_err(store_error)?;
        loop {
            let query_ids = ids.clone();
            let project_id = context.project_id;
            let actions = self
                .0
                .store
                .call(move |store| {
                    query_ids
                        .into_iter()
                        .map(|id| {
                            let action = store.get_action_invocation(id)?;
                            let session = store.get_session(action.session_id)?;
                            if session.project_id != project_id {
                                return Err(StoreError::Invariant(
                                    "wait_agent cannot cross a Project boundary".to_string(),
                                ));
                            }
                            Ok(action)
                        })
                        .collect::<Result<Vec<_>, StoreError>>()
                })
                .await
                .map_err(store_error)?;
            let complete = actions.iter().all(|action| {
                matches!(
                    action.status,
                    ActionStatus::Completed | ActionStatus::Failed | ActionStatus::Cancelled
                )
            });
            if complete || Instant::now() >= deadline {
                return Ok(ToolOutput {
                    summary: if complete {
                        "delegated Actions finished".to_string()
                    } else {
                        "wait timed out".to_string()
                    },
                    value: json!({
                        "timed_out": !complete,
                        "actions": actions.into_iter().map(|action| json!({
                            "action_invocation_id": action.id,
                            "agent_id": action.agent_id,
                            "status": action.status,
                            "output": action.output,
                            "error": action.error,
                        })).collect::<Vec<_>>(),
                    }),
                });
            }
            tokio::select! {
                _ = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {},
                received = events.recv() => {
                    if received.is_err() {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct SpawnAgentTool(CollaborationTools);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArgs {
    task: String,
    name: Option<String>,
    access: Option<AccessPreset>,
}

#[async_trait]
impl ToolExecutor for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "spawn_agent",
            "Create one child Agent in this Session and schedule its first agent_task. The child inherits this Agent and may receive the same or lower access.",
            json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    "name": {"type": "string"},
                    "access": {
                        "type": "string",
                        "enum": ["model_only", "read_only", "workspace", "full_access"]
                    }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
            false,
        )
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: SpawnAgentArgs = parse("spawn_agent", arguments)?;
        let child_id = AgentId::from_uuid(tool_resource_uuid(&context, "spawn-agent"));
        let invocation_id =
            ActionInvocationId::from_uuid(tool_resource_uuid(&context, "spawn-agent-action"));
        let parent_id = context.agent_id;
        let max_children = self.0.max_children;
        let (agent, action) = self
            .0
            .store
            .call(move |store| {
                store.spawn_child_agent_task(
                    parent_id,
                    child_id,
                    invocation_id,
                    args.name,
                    args.task,
                    args.access,
                    max_children,
                )
            })
            .await
            .map_err(store_error)?;
        Ok(ToolOutput {
            summary: format!("spawned Agent {} as Action {}", agent.id, action.id),
            value: json!({
                "agent_id": agent.id,
                "name": agent.name,
                "access": agent.access,
                "action_invocation_id": action.id,
                "status": action.status,
            }),
        })
    }
}

#[derive(Clone)]
struct InterruptAgentTool(CollaborationTools);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InterruptAgentArgs {
    agent_id: String,
}

#[async_trait]
impl ToolExecutor for InterruptAgentTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "interrupt_agent",
            "Cancel queued work and interrupt running work for one descendant Agent.",
            json!({
                "type": "object",
                "properties": {"agent_id": {"type": "string"}},
                "required": ["agent_id"],
                "additionalProperties": false
            }),
            false,
        )
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: InterruptAgentArgs = parse("interrupt_agent", arguments)?;
        let target_id = parse_agent_id("interrupt_agent", &args.agent_id)?;
        let caller_id = context.agent_id;
        let input_id =
            AgentInputId::from_uuid(tool_resource_uuid(&context, "interrupt-agent-input"));
        let interrupted = self
            .0
            .store
            .call(move |store| {
                store.interrupt_descendant_agent(
                    caller_id,
                    target_id,
                    input_id,
                    format!("Interrupted by parent Agent {caller_id}"),
                )
            })
            .await
            .map_err(store_error)?;
        for action_id in &interrupted.running_action_ids {
            self.0.actions.cancel(*action_id).await;
        }
        Ok(ToolOutput {
            summary: format!("interrupted descendant Agent {target_id}"),
            value: json!({
                "agent_id": target_id,
                "cancelled_action_ids": interrupted.cancelled_action_ids,
                "running_action_ids": interrupted.running_action_ids,
            }),
        })
    }
}

fn definition(
    name: &str,
    description: &str,
    input_schema: Value,
    supports_parallel: bool,
) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
        supports_parallel,
    }
}

fn parse<T>(tool: &str, arguments: Value) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments).map_err(|error| invalid(tool, &error.to_string()))
}

fn parse_empty(tool: &str, arguments: Value) -> Result<(), ToolError> {
    match arguments {
        Value::Object(object) if object.is_empty() => Ok(()),
        _ => Err(invalid(tool, "expected an empty object")),
    }
}

fn parse_agent_id(tool: &str, value: &str) -> Result<AgentId, ToolError> {
    AgentId::from_str(value).map_err(|error| invalid(tool, &error.to_string()))
}

fn invalid(tool: &str, message: &str) -> ToolError {
    ToolError::InvalidArguments {
        tool: tool.to_string(),
        message: message.to_string(),
    }
}

fn store_error(error: StoreError) -> ToolError {
    ToolError::Execution(error.to_string())
}

fn tool_resource_uuid(context: &ToolContext, resource: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(
        context.session_id.as_uuid(),
        format!(
            "agent:{}:tool-call:{}:{resource}",
            context.agent_id, context.tool_call_id
        )
        .as_bytes(),
    )
}
