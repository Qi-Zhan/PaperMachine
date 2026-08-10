use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use async_trait::async_trait;
use papermachine_protocol::AgentId;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::TurnId;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_store::StoreHandle;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::str::FromStr;

const DEFAULT_MAX_BYTES: usize = 64 * 1024;
const MAX_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct ReadResourceTool {
    store: StoreHandle,
}

impl ReadResourceTool {
    pub fn new(store: StoreHandle) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadResourceArgs {
    uri: String,
    offset: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
enum ProjectResource {
    Project,
    ProjectHome,
    Session(SessionId),
    Agent(AgentId),
    Turn(TurnId),
    Artifact(ArtifactId),
}

#[async_trait]
impl ToolExecutor for ReadResourceTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_resource".to_string(),
            description: "Read PaperMachine-managed content in the current Project. Start with pm://project to discover stable Session, Agent, Turn, Artifact, and Project-home URIs; that index omits the calling Session's own records. Continue large resources with next_offset.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "uri": {"type": "string", "description": "A pm:// resource URI"},
                    "offset": {"type": "integer", "minimum": 0},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_BYTES}
                },
                "required": ["uri"],
                "additionalProperties": false
            }),
            supports_parallel: true,
        }
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadResourceArgs =
            serde_json::from_value(arguments).map_err(|error| ToolError::InvalidArguments {
                tool: "read_resource".to_string(),
                message: error.to_string(),
            })?;
        let resource = parse_uri(&args.uri)?;
        let project_id = context.project_id;
        let calling_session_id = context.session_id;
        let content = self
            .store
            .call(move |store| render_resource(store, project_id, calling_session_id, resource))
            .await
            .map_err(store_error)?;
        let offset = args.offset.unwrap_or_default();
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1, MAX_BYTES);
        let (visible, next_offset) = chunk(&content, offset, max_bytes)?;
        let total_bytes = content.len();
        let returned_bytes = visible.len();
        Ok(ToolOutput {
            value: json!({
                "uri": args.uri,
                "content": visible,
                "offset": offset,
                "returned_bytes": returned_bytes,
                "total_bytes": total_bytes,
                "truncated": next_offset.is_some(),
                "next_offset": next_offset,
            }),
            summary: format!("read {returned_bytes} bytes from {}", args.uri),
        })
    }
}

fn parse_uri(uri: &str) -> Result<ProjectResource, ToolError> {
    let path = uri
        .strip_prefix("pm://")
        .ok_or_else(|| invalid_uri(uri, "URI must start with pm://"))?;
    if path == "project" {
        return Ok(ProjectResource::Project);
    }
    if path == "project-home" {
        return Ok(ProjectResource::ProjectHome);
    }
    let (kind, id) = path
        .split_once('/')
        .filter(|(_, id)| !id.is_empty() && !id.contains('/'))
        .ok_or_else(|| invalid_uri(uri, "unknown Project resource URI"))?;
    match kind {
        "session" => SessionId::from_str(id)
            .map(ProjectResource::Session)
            .map_err(|error| invalid_uri(uri, &error.to_string())),
        "agent" => AgentId::from_str(id)
            .map(ProjectResource::Agent)
            .map_err(|error| invalid_uri(uri, &error.to_string())),
        "turn" => TurnId::from_str(id)
            .map(ProjectResource::Turn)
            .map_err(|error| invalid_uri(uri, &error.to_string())),
        "artifact" => ArtifactId::from_str(id)
            .map(ProjectResource::Artifact)
            .map_err(|error| invalid_uri(uri, &error.to_string())),
        _ => Err(invalid_uri(uri, "unknown Project resource URI")),
    }
}

fn render_resource(
    store: &Store,
    project_id: ProjectId,
    calling_session_id: SessionId,
    resource: ProjectResource,
) -> Result<String, StoreError> {
    let value = match resource {
        ProjectResource::Project => {
            let project = store.get_project(project_id)?;
            let home = store.get_project_home(project_id)?;
            let sessions = store
                .list_project_sessions(project_id)?
                .into_iter()
                .filter(|session| session.id != calling_session_id)
                .map(|session| {
                    json!({
                        "uri": format!("pm://session/{}", session.id),
                        "id": session.id,
                        "title": session.title,
                        "program": session.program.manifest.name,
                        "program_slug": session.program.manifest.slug,
                        "request": session.request,
                        "status": session.status,
                        "attention_required": session.attention_required,
                        "updated_at": session.updated_at,
                    })
                })
                .collect::<Vec<_>>();
            let artifacts = store
                .list_project_artifacts(project_id)?
                .into_iter()
                .filter(|artifact| artifact.session_id != calling_session_id)
                .map(|artifact| {
                    json!({
                        "uri": format!("pm://artifact/{}", artifact.id),
                        "id": artifact.id,
                        "session_id": artifact.session_id,
                        "agent_id": artifact.agent_id,
                        "kind": artifact.kind,
                        "name": artifact.name,
                        "media_type": artifact.media_type,
                        "size_bytes": artifact.size_bytes,
                        "metadata": artifact.metadata,
                        "created_at": artifact.created_at,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "uri": "pm://project",
                "project": {"id": project.id, "name": project.name},
                "project_home": home.map(|home| json!({
                    "uri": "pm://project-home",
                    "revision": home.revision,
                    "updated_at": home.updated_at,
                })),
                "sessions": sessions,
                "artifacts": artifacts,
            })
        }
        ProjectResource::ProjectHome => {
            let Some(home) = store.get_project_home(project_id)? else {
                return Ok(serde_json::to_string_pretty(&json!({
                    "uri": "pm://project-home",
                    "exists": false,
                }))?);
            };
            let artifact = store.get_artifact(home.artifact_id)?;
            ensure_project(project_id, artifact.project_id, "Project home")?;
            let html = String::from_utf8(store.read_artifact(&artifact)?)
                .map_err(|error| StoreError::Invariant(error.to_string()))?;
            json!({
                "uri": "pm://project-home",
                "exists": true,
                "home": home,
                "html": html,
            })
        }
        ProjectResource::Session(session_id) => {
            let session = store.get_session(session_id)?;
            ensure_project(project_id, session.project_id, "Session")?;
            let agents = store
                .list_agents(session.id)?
                .into_iter()
                .map(|agent| {
                    json!({
                        "uri": format!("pm://agent/{}", agent.id),
                        "id": agent.id,
                        "name": agent.name,
                        "role": agent.role,
                        "model": agent.model,
                    })
                })
                .collect::<Vec<_>>();
            let actions = store.list_action_invocations(session.id)?;
            let attempts = store.list_session_action_attempts(session.id)?;
            let artifacts = store
                .list_artifacts(session.id)?
                .into_iter()
                .map(|artifact| {
                    json!({
                        "uri": format!("pm://artifact/{}", artifact.id),
                        "artifact": artifact,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "uri": format!("pm://session/{session_id}"),
                "session": session,
                "agents": agents,
                "actions": actions,
                "attempts": attempts,
                "artifacts": artifacts,
            })
        }
        ProjectResource::Agent(agent_id) => {
            let agent = store.get_agent(agent_id)?;
            let session = store.get_session(agent.session_id)?;
            ensure_project(project_id, session.project_id, "Agent")?;
            let turns = store
                .list_turns(agent.id)?
                .into_iter()
                .map(|turn| {
                    json!({
                        "uri": format!("pm://turn/{}", turn.id),
                        "id": turn.id,
                        "status": turn.status,
                        "input": turn.input,
                        "output": turn.output,
                        "usage": turn.usage,
                        "created_at": turn.created_at,
                        "updated_at": turn.updated_at,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "uri": format!("pm://agent/{agent_id}"),
                "agent": agent,
                "turns": turns,
            })
        }
        ProjectResource::Turn(turn_id) => {
            let turn = store.get_turn(turn_id)?;
            let agent = store.get_agent(turn.agent_id)?;
            let session = store.get_session(agent.session_id)?;
            ensure_project(project_id, session.project_id, "Turn")?;
            let steps = store.list_steps(turn.id)?;
            json!({
                "uri": format!("pm://turn/{turn_id}"),
                "turn": turn,
                "steps": steps,
            })
        }
        ProjectResource::Artifact(artifact_id) => {
            let artifact = store.get_artifact(artifact_id)?;
            ensure_project(project_id, artifact.project_id, "Artifact")?;
            let content = if is_textual(&artifact.media_type) {
                Some(
                    String::from_utf8(store.read_artifact(&artifact)?)
                        .map_err(|error| StoreError::Invariant(error.to_string()))?,
                )
            } else {
                None
            };
            let content_available = content.is_some();
            json!({
                "uri": format!("pm://artifact/{artifact_id}"),
                "artifact": artifact,
                "content": content,
                "content_available": content_available,
            })
        }
    };
    serde_json::to_string_pretty(&value).map_err(StoreError::from)
}

fn ensure_project(expected: ProjectId, actual: ProjectId, entity: &str) -> Result<(), StoreError> {
    if expected == actual {
        Ok(())
    } else {
        Err(StoreError::Invariant(format!(
            "{entity} belongs to another Project"
        )))
    }
}

fn is_textual(media_type: &str) -> bool {
    let media_type = media_type.to_ascii_lowercase();
    media_type.starts_with("text/")
        || media_type.contains("json")
        || media_type.contains("xml")
        || media_type.contains("yaml")
        || media_type.contains("toml")
}

fn chunk(
    content: &str,
    offset: usize,
    max_bytes: usize,
) -> Result<(&str, Option<usize>), ToolError> {
    if offset > content.len() || !content.is_char_boundary(offset) {
        return Err(ToolError::InvalidArguments {
            tool: "read_resource".to_string(),
            message: "offset is outside the resource or not a UTF-8 boundary".to_string(),
        });
    }
    let mut end = offset.saturating_add(max_bytes).min(content.len());
    while end > offset && !content.is_char_boundary(end) {
        end -= 1;
    }
    let next = (end < content.len()).then_some(end);
    Ok((&content[offset..end], next))
}

fn invalid_uri(uri: &str, message: &str) -> ToolError {
    ToolError::InvalidArguments {
        tool: "read_resource".to_string(),
        message: format!("{message}: {uri}"),
    }
}

fn store_error(error: StoreError) -> ToolError {
    ToolError::Execution(error.to_string())
}
