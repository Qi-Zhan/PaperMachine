use chrono::Utc;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use papermachine_protocol::WorkflowId;
use papermachine_store::Store;
use papermachine_store::StoreError;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct ProjectSnapshotOptions {
    pub focus_session_id: Option<SessionId>,
    pub exclude_workflow_id: Option<WorkflowId>,
    pub max_sessions: usize,
    pub max_turns_per_session: usize,
    pub max_workflows: usize,
    pub max_artifacts: usize,
    pub include_artifact_content: bool,
    pub max_text_chars: usize,
}

impl Default for ProjectSnapshotOptions {
    fn default() -> Self {
        Self {
            focus_session_id: None,
            exclude_workflow_id: None,
            max_sessions: 50,
            max_turns_per_session: 12,
            max_workflows: 200,
            max_artifacts: 50,
            include_artifact_content: false,
            max_text_chars: 500_000,
        }
    }
}

pub fn build_project_snapshot(
    store: &Store,
    project_id: ProjectId,
    options: ProjectSnapshotOptions,
) -> Result<Value, StoreError> {
    let max_sessions = options.max_sessions.clamp(1, 200);
    let max_turns = options.max_turns_per_session.clamp(1, 100);
    let max_workflows = options.max_workflows.clamp(1, 200);
    let max_artifacts = options.max_artifacts.clamp(1, 200);
    let max_text_chars = options.max_text_chars.clamp(1_000, 2_000_000);
    let project = store.get_project(project_id)?;
    let workflows = store.list_project_workflows(project.id)?;
    let summary_workflow_ids = workflows
        .iter()
        .filter(|workflow| workflow.program.manifest.slug == "project-summary")
        .map(|workflow| workflow.id)
        .collect::<HashSet<_>>();
    let mut summary_session_ids = HashSet::new();
    for workflow_id in &summary_workflow_ids {
        for participant in store.list_participants(*workflow_id)? {
            summary_session_ids.insert(participant.session_id);
        }
    }

    let mut text = SnapshotTextBudget::new(max_text_chars);
    let (project_description, project_description_truncated) =
        text.take(&project.description, 12_000);
    let mut project_sessions = store
        .list_sessions(project.id)?
        .into_iter()
        .filter(|session| !summary_session_ids.contains(&session.id))
        .collect::<Vec<_>>();
    if let Some(focus_session_id) = options.focus_session_id {
        project_sessions.sort_by_key(|session| usize::from(session.id != focus_session_id));
    }
    let sessions = project_sessions
        .into_iter()
        .take(max_sessions)
        .map(|session| {
            let turns = store
                .list_turns(session.id)?
                .into_iter()
                .rev()
                .take(max_turns)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|turn| {
                    let (input, input_truncated) = text.take(&turn.input, 24_000);
                    let (output, output_truncated) = turn
                        .output
                        .as_deref()
                        .map(|value| text.take(value, 48_000))
                        .map_or((None, false), |(value, truncated)| (Some(value), truncated));
                    json!({
                        "id": turn.id,
                        "origin": turn.origin,
                        "status": turn.status,
                        "input": input,
                        "input_truncated": input_truncated,
                        "output": output,
                        "output_truncated": output_truncated,
                        "updated_at": turn.updated_at,
                    })
                })
                .collect::<Vec<_>>();
            Ok::<_, StoreError>(json!({
                "id": session.id,
                "title": session.title,
                "origin": session.origin,
                "status": session.status,
                "updated_at": session.updated_at,
                "turns": turns,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let workflow_summaries = workflows
        .into_iter()
        .filter(|workflow| {
            options.exclude_workflow_id != Some(workflow.id)
                && workflow.program.manifest.slug != "project-summary"
        })
        .take(max_workflows)
        .map(|workflow| {
            let (objective, objective_truncated) = text.take(&workflow.objective, 12_000);
            let output = workflow.output.as_ref().map(|value| {
                let serialized = serde_json::to_string(value).unwrap_or_default();
                let (content, truncated) = text.take(&serialized, 48_000);
                json!({"json": content, "truncated": truncated})
            });
            let (error, error_truncated) = workflow
                .error
                .as_deref()
                .map(|value| text.take(value, 12_000))
                .map_or((None, false), |(value, truncated)| (Some(value), truncated));
            json!({
                "id": workflow.id,
                "program": workflow.program.manifest.name,
                "program_slug": workflow.program.manifest.slug,
                "objective": objective,
                "objective_truncated": objective_truncated,
                "status": workflow.status,
                "attention_required": workflow.attention_required,
                "output": output,
                "error": error,
                "error_truncated": error_truncated,
                "updated_at": workflow.updated_at,
            })
        })
        .collect::<Vec<_>>();

    let artifacts = store
        .list_project_artifacts(project.id)?
        .into_iter()
        .filter(|artifact| {
            artifact.metadata.get("role").and_then(Value::as_str) != Some("project_summary")
        })
        .take(max_artifacts)
        .map(|artifact| {
            let (content, content_truncated) = if options.include_artifact_content
                && is_textual_media_type(&artifact.media_type)
            {
                store
                    .read_artifact(&artifact)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .map(|value| text.take(&value, 48_000))
                    .map_or((None, false), |(value, truncated)| (Some(value), truncated))
            } else {
                (None, false)
            };
            json!({
                "id": artifact.id,
                "workflow_id": artifact.workflow_id,
                "session_id": artifact.session_id,
                "kind": artifact.kind,
                "name": artifact.name,
                "media_type": artifact.media_type,
                "metadata": artifact.metadata,
                "content": content,
                "content_truncated": content_truncated,
                "created_at": artifact.created_at,
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "captured_at": Utc::now(),
        "project": {
            "id": project.id,
            "name": project.name,
            "description": project_description,
            "description_truncated": project_description_truncated,
        },
        "focus_session_id": options.focus_session_id,
        "sessions": sessions,
        "workflows": workflow_summaries,
        "artifacts": artifacts,
        "limits": {
            "max_sessions": max_sessions,
            "max_turns_per_session": max_turns,
            "max_workflows": max_workflows,
            "max_artifacts": max_artifacts,
            "include_artifact_content": options.include_artifact_content,
            "max_text_chars": max_text_chars,
            "text_budget_exhausted": text.exhausted(),
        },
    }))
}

fn is_textual_media_type(media_type: &str) -> bool {
    let media_type = media_type.to_ascii_lowercase();
    media_type.starts_with("text/")
        || media_type.contains("json")
        || media_type.contains("xml")
        || media_type.contains("yaml")
        || media_type.contains("toml")
}

struct SnapshotTextBudget {
    remaining: usize,
}

impl SnapshotTextBudget {
    const TRUNCATION_MARKER: &'static str = "\n\n[Truncated by PaperMachine project snapshot]";

    const fn new(max_chars: usize) -> Self {
        Self {
            remaining: max_chars,
        }
    }

    fn exhausted(&self) -> bool {
        self.remaining == 0
    }

    fn take(&mut self, value: &str, per_field_limit: usize) -> (String, bool) {
        let value_chars = value.chars().count();
        let allowed = self.remaining.min(per_field_limit);
        if value_chars <= allowed {
            self.remaining -= value_chars;
            return (value.to_string(), false);
        }
        if allowed == 0 {
            return (String::new(), true);
        }
        let marker_chars = Self::TRUNCATION_MARKER.chars().count();
        let content_chars = allowed.saturating_sub(marker_chars);
        let mut truncated = value.chars().take(content_chars).collect::<String>();
        if allowed >= marker_chars {
            truncated.push_str(Self::TRUNCATION_MARKER);
        }
        self.remaining -= allowed;
        (truncated, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermachine_protocol::AgentAccessProfile;
    use papermachine_protocol::ArtifactKind;
    use papermachine_protocol::Budget;
    use papermachine_protocol::WorkflowProgramId;
    use papermachine_protocol::WorkflowProgramManifest;
    use papermachine_protocol::WorkflowProgramSnapshot;
    use papermachine_protocol::WorkflowProgramSource;
    use papermachine_store::NewWorkflow;
    use serde_json::json;
    use tempfile::tempdir;

    fn program() -> WorkflowProgramSnapshot {
        WorkflowProgramSnapshot {
            project_id: None,
            manifest: WorkflowProgramManifest {
                id: WorkflowProgramId::new(),
                slug: "context-test".to_string(),
                name: "Context test".to_string(),
                description: "Capture prior Project state".to_string(),
                entrypoint: "main".to_string(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                default_budget: Budget::default(),
            },
            source: WorkflowProgramSource::Builtin,
            definition_path: "builtin/context-test/workflow.py".to_string(),
            sha256: "context-test".to_string(),
            source_code: "async def main(ctx): return {}\n".to_string(),
        }
    }

    #[test]
    fn text_budget_is_global_and_unicode_safe() {
        let mut budget = SnapshotTextBudget::new(1_000);
        let (first, first_truncated) = budget.take(&"研".repeat(1_200), 900);
        let (second, second_truncated) = budget.take(&"究".repeat(200), 900);

        assert!(first_truncated);
        assert!(second_truncated);
        assert!(first.is_char_boundary(first.len()));
        assert!(second.is_char_boundary(second.len()));
        assert!(budget.exhausted());
    }

    #[test]
    fn snapshot_prioritizes_the_origin_and_can_embed_text_artifacts() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Store::open_in_memory(directory.path().join("artifacts")).expect("store should open");
        let project = store
            .create_project(
                "Context Project",
                "A stable description",
                directory.path().join("project"),
            )
            .expect("Project should be created");
        let older = store
            .create_session(project.id, "Older Session", "", "test-model", Vec::new())
            .expect("older Session should be created");
        let focus = store
            .create_session(project.id, "Focused Session", "", "test-model", Vec::new())
            .expect("focused Session should be created");
        let workflow = store
            .create_workflow(NewWorkflow {
                project_id: project.id,
                started_from_session_id: Some(older.id),
                program: program(),
                objective: "Produce durable evidence".to_string(),
                system_prompt: String::new(),
                input: json!({}),
                budget: None,
                default_model: "test-model".to_string(),
                access: AgentAccessProfile::Research,
                enabled_skills: Vec::new(),
                launch_context: Default::default(),
                agent_access_overrides: Default::default(),
            })
            .expect("Workflow should be created");
        store
            .create_artifact(
                project.id,
                workflow.id,
                Some(older.id),
                None,
                ArtifactKind::Report,
                "evidence.md",
                "text/markdown",
                json!({"source": "primary"}),
                "原始证据内容".as_bytes(),
            )
            .expect("Artifact should be created");

        let snapshot = build_project_snapshot(
            &store,
            project.id,
            ProjectSnapshotOptions {
                focus_session_id: Some(focus.id),
                include_artifact_content: true,
                max_text_chars: 1_000,
                ..ProjectSnapshotOptions::default()
            },
        )
        .expect("Project snapshot should be built");

        assert_eq!(snapshot["focus_session_id"], focus.id.to_string());
        assert_eq!(snapshot["sessions"][0]["id"], focus.id.to_string());
        assert_eq!(snapshot["project"]["description"], "A stable description");
        assert_eq!(snapshot["artifacts"][0]["content"], "原始证据内容");
        assert_eq!(snapshot["artifacts"][0]["content_truncated"], false);
        assert_eq!(snapshot["limits"]["include_artifact_content"], true);
    }
}
