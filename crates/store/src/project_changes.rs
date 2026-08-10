use crate::ProjectChange;
use crate::Store;
use crate::StoreError;
use papermachine_protocol::ActionInvocation;
use papermachine_protocol::Artifact;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::str::FromStr;

const CURSOR_PREFIX: &str = "pc1_";
const PAGE_BYTES: usize = 1024 * 1024;
const PAGE_RESERVE_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProjectSnapshotPage {
    pub cursor: String,
    pub changed: bool,
    pub has_more: bool,
    pub resources: Vec<ProjectEntitySnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProjectEntitySnapshot {
    pub kind: String,
    pub id: String,
    pub session_id: Option<SessionId>,
    pub deleted: bool,
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChangeCursor {
    project_id: ProjectId,
    session_id: SessionId,
    sequence: u64,
    continuation: Option<ArtifactContinuation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArtifactContinuation {
    id: ArtifactId,
    sha256: String,
    offset: usize,
}

impl Store {
    pub fn project_snapshot_changes(
        &self,
        project_id: ProjectId,
        calling_session_id: SessionId,
        after_cursor: Option<&str>,
    ) -> Result<ProjectSnapshotPage, StoreError> {
        let calling_session = self.get_session(calling_session_id)?;
        ensure_project(
            project_id,
            calling_session.project_id,
            "Project change caller",
        )?;
        let mut cursor = match after_cursor {
            Some(cursor) => decode_cursor(cursor)?,
            None => ChangeCursor {
                project_id,
                session_id: calling_session_id,
                sequence: 0,
                continuation: None,
            },
        };
        if cursor.project_id != project_id || cursor.session_id != calling_session_id {
            return Err(StoreError::Invariant(
                "Project change cursor belongs to another Project or Session".to_string(),
            ));
        }

        if let Some(continuation) = cursor.continuation.take() {
            return self.continue_artifact(cursor, continuation);
        }

        let mut resources = Vec::new();
        let mut encoded_bytes = PAGE_RESERVE_BYTES;
        let captured_cursor = loop {
            let batch = self.project_changes_after(project_id, cursor.sequence)?;
            let captured_cursor = batch.captured_cursor;
            if batch.changes.is_empty() {
                cursor.sequence = captured_cursor;
                break captured_cursor;
            }
            let changes = latest_entity_changes(batch.changes, calling_session_id);
            let mut stopped_for_size = false;
            for change in changes {
                let snapshot = self.snapshot_change(project_id, &change)?;
                let size = serde_json::to_vec(&snapshot)?.len().saturating_add(1);
                if encoded_bytes.saturating_add(size) > PAGE_BYTES {
                    if resources.is_empty() && change.kind == "artifact" {
                        return self.start_artifact(cursor, change);
                    }
                    if resources.is_empty() {
                        return Err(StoreError::Invariant(format!(
                            "Project {} snapshot {} exceeds the Project change page limit",
                            change.kind, change.entity_id
                        )));
                    }
                    stopped_for_size = true;
                    break;
                }
                encoded_bytes = encoded_bytes.saturating_add(size);
                resources.push(snapshot);
                cursor.sequence = change.sequence;
            }
            if stopped_for_size || cursor.sequence >= captured_cursor {
                break captured_cursor;
            }
            if resources.is_empty() {
                cursor.sequence = batch.last_sequence.unwrap_or(captured_cursor);
                if cursor.sequence >= captured_cursor {
                    break captured_cursor;
                }
                continue;
            }
            break captured_cursor;
        };
        let has_more = cursor.sequence < captured_cursor;
        Ok(ProjectSnapshotPage {
            cursor: encode_cursor(&cursor)?,
            changed: !resources.is_empty(),
            has_more,
            resources,
        })
    }

    fn start_artifact(
        &self,
        cursor: ChangeCursor,
        change: ProjectChange,
    ) -> Result<ProjectSnapshotPage, StoreError> {
        let artifact_id = ArtifactId::from_str(&change.entity_id)
            .map_err(|error| StoreError::Invariant(error.to_string()))?;
        let artifact = match self.get_artifact(artifact_id) {
            Ok(artifact) => artifact,
            Err(StoreError::NotFound { .. }) => {
                let snapshot = tombstone(&change);
                let mut next = cursor;
                next.sequence = change.sequence;
                return Ok(ProjectSnapshotPage {
                    cursor: encode_cursor(&next)?,
                    changed: true,
                    has_more: true,
                    resources: vec![snapshot],
                });
            }
            Err(error) => return Err(error),
        };
        self.ensure_artifact_project(&artifact, cursor.project_id)?;
        if !is_textual(&artifact.media_type) {
            return Err(StoreError::Invariant(format!(
                "binary Artifact {} metadata exceeds the Project change page limit",
                artifact.id
            )));
        }
        self.artifact_page(cursor, change.sequence, artifact, 0)
    }

    fn continue_artifact(
        &self,
        cursor: ChangeCursor,
        continuation: ArtifactContinuation,
    ) -> Result<ProjectSnapshotPage, StoreError> {
        let artifact = self.get_artifact(continuation.id)?;
        self.ensure_artifact_project(&artifact, cursor.project_id)?;
        if artifact.sha256 != continuation.sha256 {
            return Err(StoreError::Invariant(format!(
                "Artifact {} changed while its Project snapshot was paged",
                artifact.id
            )));
        }
        let sequence = cursor.sequence;
        self.artifact_page(cursor, sequence, artifact, continuation.offset)
    }

    fn artifact_page(
        &self,
        mut cursor: ChangeCursor,
        sequence: u64,
        artifact: Artifact,
        offset: usize,
    ) -> Result<ProjectSnapshotPage, StoreError> {
        let bytes = self.read_artifact(&artifact)?;
        let content = std::str::from_utf8(&bytes)
            .map_err(|error| StoreError::Invariant(error.to_string()))?;
        if offset > content.len() || !content.is_char_boundary(offset) {
            return Err(StoreError::Invariant(format!(
                "Artifact continuation offset is invalid for {}",
                artifact.id
            )));
        }
        let end = fit_artifact_end(&artifact, content, offset)?;
        let complete = end == content.len();
        let snapshot = artifact_snapshot(&artifact, Some(&content[offset..end]), offset, complete);
        if complete {
            cursor.sequence = sequence;
            cursor.continuation = None;
        } else {
            cursor.sequence = sequence;
            cursor.continuation = Some(ArtifactContinuation {
                id: artifact.id,
                sha256: artifact.sha256.clone(),
                offset: end,
            });
        }
        Ok(ProjectSnapshotPage {
            cursor: encode_cursor(&cursor)?,
            changed: true,
            has_more: !complete
                || self
                    .project_changes_after(cursor.project_id, sequence)?
                    .captured_cursor
                    > sequence,
            resources: vec![snapshot],
        })
    }

    fn snapshot_change(
        &self,
        project_id: ProjectId,
        change: &ProjectChange,
    ) -> Result<ProjectEntitySnapshot, StoreError> {
        let result = match change.kind.as_str() {
            "project" => self.get_project(project_id).map(|project| {
                json!({
                    "id": project.id,
                    "name": project.name,
                    "created_at": project.created_at,
                    "updated_at": project.updated_at,
                })
            }),
            "project_home" => self
                .get_project_home(project_id)
                .and_then(|home| {
                    home.map(|home| serde_json::to_value(home).map_err(StoreError::from))
                        .transpose()
                })
                .and_then(|home| {
                    home.ok_or_else(|| StoreError::NotFound {
                        entity: "Project home",
                        id: project_id.to_string(),
                    })
                }),
            "session" => {
                let id = SessionId::from_str(&change.entity_id)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?;
                self.get_session(id).and_then(|session| {
                    ensure_project(project_id, session.project_id, "Session")?;
                    Ok(json!({
                        "id": session.id,
                        "title": session.title,
                        "program": {
                            "slug": session.program.manifest.slug,
                            "name": session.program.manifest.name,
                        },
                        "request": session.request,
                        "instructions": session.instructions,
                        "status": session.status,
                        "params": session.params,
                        "output": session.output,
                        "error": session.error,
                        "attention_required": session.attention_required,
                        "usage": session.usage,
                        "created_at": session.created_at,
                        "updated_at": session.updated_at,
                    }))
                })
            }
            "agent" => {
                let id = papermachine_protocol::AgentId::from_str(&change.entity_id)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?;
                self.get_agent(id).and_then(|agent| {
                    let session = self.get_session(agent.session_id)?;
                    ensure_project(project_id, session.project_id, "Agent")?;
                    Ok(json!({
                        "id": agent.id,
                        "session_id": agent.session_id,
                        "class_name": agent.class_name,
                        "name": agent.name,
                        "role": agent.role,
                        "model": agent.model,
                        "access": agent.access,
                        "skills": agent.skills,
                        "created_at": agent.created_at,
                    }))
                })
            }
            "turn" => {
                let id = papermachine_protocol::TurnId::from_str(&change.entity_id)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?;
                self.get_turn(id).and_then(|turn| {
                    let agent = self.get_agent(turn.agent_id)?;
                    let session = self.get_session(agent.session_id)?;
                    ensure_project(project_id, session.project_id, "Turn")?;
                    let action = self.action_for_turn(session.id, turn.id)?;
                    Ok(json!({
                        "turn": {
                            "id": turn.id,
                            "status": turn.status,
                            "input": turn.input,
                            "output": turn.output,
                            "usage": turn.usage,
                            "completed_model_steps": turn.completed_model_steps,
                            "hosted_search_calls_used": turn.hosted_search_calls_used,
                            "error": turn.error,
                            "created_at": turn.created_at,
                            "updated_at": turn.updated_at,
                        },
                        "session": {
                            "id": session.id,
                            "title": session.title,
                            "program_slug": session.program.manifest.slug,
                        },
                        "agent": {
                            "id": agent.id,
                            "name": agent.name,
                            "role": agent.role,
                            "class_name": agent.class_name,
                            "model": agent.model,
                        },
                        "action": action.map(action_provenance),
                    }))
                })
            }
            "artifact" => {
                let id = ArtifactId::from_str(&change.entity_id)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?;
                self.get_artifact(id).and_then(|artifact| {
                    self.ensure_artifact_project(&artifact, project_id)?;
                    if is_textual(&artifact.media_type) {
                        let bytes = self.read_artifact(&artifact)?;
                        let content = std::str::from_utf8(&bytes)
                            .map_err(|error| StoreError::Invariant(error.to_string()))?;
                        Ok(artifact_snapshot(&artifact, Some(content), 0, true).data)
                    } else {
                        Ok(artifact_snapshot(&artifact, None, 0, true).data)
                    }
                })
            }
            kind => {
                return Err(StoreError::Invariant(format!(
                    "unknown Project change kind: {kind}"
                )));
            }
        };
        match result {
            Ok(data) => Ok(ProjectEntitySnapshot {
                kind: change.kind.clone(),
                id: change.entity_id.clone(),
                session_id: change.session_id,
                deleted: false,
                data,
            }),
            Err(StoreError::NotFound { .. }) => Ok(tombstone(change)),
            Err(error) => Err(error),
        }
    }

    fn action_for_turn(
        &self,
        session_id: SessionId,
        turn_id: papermachine_protocol::TurnId,
    ) -> Result<Option<ActionInvocation>, StoreError> {
        let attempt = self
            .list_session_action_attempts(session_id)?
            .into_iter()
            .find(|attempt| attempt.turn_id == Some(turn_id));
        attempt
            .map(|attempt| self.get_action_invocation(attempt.invocation_id))
            .transpose()
    }

    fn ensure_artifact_project(
        &self,
        artifact: &Artifact,
        project_id: ProjectId,
    ) -> Result<(), StoreError> {
        ensure_project(project_id, artifact.project_id, "Artifact")
    }
}

fn latest_entity_changes(
    changes: Vec<ProjectChange>,
    calling_session_id: SessionId,
) -> Vec<ProjectChange> {
    let mut latest = HashMap::<(String, String), ProjectChange>::new();
    for change in changes {
        if change.session_id != Some(calling_session_id) {
            latest.insert((change.kind.clone(), change.entity_id.clone()), change);
        }
    }
    let mut changes = latest.into_values().collect::<Vec<_>>();
    changes.sort_by_key(|change| change.sequence);
    changes
}

fn artifact_snapshot(
    artifact: &Artifact,
    content: Option<&str>,
    offset: usize,
    complete: bool,
) -> ProjectEntitySnapshot {
    ProjectEntitySnapshot {
        kind: "artifact".to_string(),
        id: artifact.id.to_string(),
        session_id: Some(artifact.session_id),
        deleted: false,
        data: json!({
            "id": artifact.id,
            "session_id": artifact.session_id,
            "agent_id": artifact.agent_id,
            "action_invocation_id": artifact.action_invocation_id,
            "kind": artifact.kind,
            "name": artifact.name,
            "media_type": artifact.media_type,
            "sha256": artifact.sha256,
            "size_bytes": artifact.size_bytes,
            "metadata": artifact.metadata,
            "created_at": artifact.created_at,
            "content": content,
            "content_offset": offset,
            "content_complete": complete,
        }),
    }
}

fn action_provenance(action: ActionInvocation) -> Value {
    json!({
        "id": action.id,
        "name": action.action_name,
        "arguments": action.arguments,
        "source": action.source,
        "status": action.status,
    })
}

fn tombstone(change: &ProjectChange) -> ProjectEntitySnapshot {
    ProjectEntitySnapshot {
        kind: change.kind.clone(),
        id: change.entity_id.clone(),
        session_id: change.session_id,
        deleted: true,
        data: Value::Null,
    }
}

fn fit_artifact_end(
    artifact: &Artifact,
    content: &str,
    offset: usize,
) -> Result<usize, StoreError> {
    let mut low = offset;
    let mut high = content.len();
    while low < high {
        let mut middle = low + (high - low).div_ceil(2);
        while middle > offset && !content.is_char_boundary(middle) {
            middle -= 1;
        }
        if middle == low && middle < high {
            middle = next_char_boundary(content, middle);
        }
        let snapshot = artifact_snapshot(
            artifact,
            Some(&content[offset..middle]),
            offset,
            middle == content.len(),
        );
        if serde_json::to_vec(&snapshot)?
            .len()
            .saturating_add(PAGE_RESERVE_BYTES)
            <= PAGE_BYTES
        {
            low = middle;
        } else {
            high = middle.saturating_sub(1);
            while high > offset && !content.is_char_boundary(high) {
                high -= 1;
            }
        }
    }
    if low == offset && offset < content.len() {
        return Err(StoreError::Invariant(format!(
            "Artifact {} metadata leaves no room for content in a Project change page",
            artifact.id
        )));
    }
    Ok(low)
}

fn next_char_boundary(content: &str, offset: usize) -> usize {
    let mut next = offset.saturating_add(1).min(content.len());
    while next < content.len() && !content.is_char_boundary(next) {
        next += 1;
    }
    next
}

fn encode_cursor(cursor: &ChangeCursor) -> Result<String, StoreError> {
    Ok(format!(
        "{CURSOR_PREFIX}{}",
        hex::encode(serde_json::to_vec(cursor)?)
    ))
}

fn decode_cursor(cursor: &str) -> Result<ChangeCursor, StoreError> {
    let encoded = cursor
        .strip_prefix(CURSOR_PREFIX)
        .ok_or_else(|| StoreError::Invariant("invalid Project change cursor".to_string()))?;
    let bytes = hex::decode(encoded)
        .map_err(|_| StoreError::Invariant("invalid Project change cursor".to_string()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| StoreError::Invariant("invalid Project change cursor".to_string()))
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
