use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use papermachine_protocol::WorkflowId;
use papermachine_store::ProjectChangeBatch;
use papermachine_store::Store;
use papermachine_store::StoreError;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct ProjectSnapshotOptions {
    pub focus_session_id: Option<SessionId>,
    pub exclude_workflow_id: Option<WorkflowId>,
    pub after_cursor: Option<u64>,
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
            after_cursor: None,
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
    let excluded_session_ids = match options.exclude_workflow_id {
        Some(workflow_id) => store
            .list_participants(workflow_id)?
            .into_iter()
            .map(|participant| participant.session_id.to_string())
            .collect::<HashSet<_>>(),
        None => HashSet::new(),
    };
    let changes = store.project_changes_after(project.id, options.after_cursor)?;
    let selection = select_project_changes(
        changes,
        options.after_cursor,
        options.exclude_workflow_id,
        &excluded_session_ids,
        max_sessions,
        max_workflows,
        max_artifacts,
    )?;

    let mut text = SnapshotTextBudget::new(max_text_chars);
    let project_home = match store
        .get_project_home(project.id)?
        .filter(|_| options.after_cursor.is_none() || selection.project_home)
    {
        Some(home) => {
            let artifact = store.get_artifact(home.artifact_id)?;
            let (content, content_truncated) = if options.include_artifact_content {
                let content = String::from_utf8(store.read_artifact(&artifact)?)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?;
                let (content, truncated) =
                    text.take(SnapshotTextSection::ProjectHome, &content, 48_000);
                (Some(content), truncated)
            } else {
                (None, false)
            };
            Some(json!({
                "artifact_id": home.artifact_id,
                "source_artifact_id": home.source_artifact_id,
                "revision": home.revision,
                "updated_at": home.updated_at,
                "content": content,
                "content_truncated": content_truncated,
            }))
        }
        None => None,
    };
    let mut project_sessions = store
        .list_project_sessions(project.id)?
        .into_iter()
        .filter(|session| !excluded_session_ids.contains(&session.id.to_string()))
        .filter(|session| {
            options.after_cursor.is_none() || selection.sessions.contains(&session.id.to_string())
        })
        .collect::<Vec<_>>();
    project_sessions.sort_by_key(|session| {
        (
            usize::from(
                options
                    .focus_session_id
                    .is_some_and(|focus| session.id != focus),
            ),
            usize::from(session.status == papermachine_protocol::SessionStatus::Archived),
        )
    });
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
                    let (input, input_truncated) =
                        text.take(SnapshotTextSection::Sessions, &turn.input, 24_000);
                    let (output, output_truncated) = turn
                        .output
                        .as_deref()
                        .map(|value| text.take(SnapshotTextSection::Sessions, value, 48_000))
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
                && (options.after_cursor.is_none()
                    || selection.workflows.contains(&workflow.id.to_string()))
        })
        .take(max_workflows)
        .map(|workflow| {
            let (request, request_truncated) =
                text.take(SnapshotTextSection::Workflows, &workflow.request, 12_000);
            let output = workflow.output.as_ref().map(|value| {
                let serialized = serde_json::to_string(value).unwrap_or_default();
                let (content, truncated) =
                    text.take(SnapshotTextSection::Workflows, &serialized, 48_000);
                json!({"json": content, "truncated": truncated})
            });
            let (error, error_truncated) = workflow
                .error
                .as_deref()
                .map(|value| text.take(SnapshotTextSection::Workflows, value, 12_000))
                .map_or((None, false), |(value, truncated)| (Some(value), truncated));
            json!({
                "id": workflow.id,
                "program": workflow.program.manifest.name,
                "program_slug": workflow.program.manifest.slug,
                "request": request,
                "request_truncated": request_truncated,
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
            !matches!(
                artifact.metadata.get("role").and_then(Value::as_str),
                Some("project_summary" | "project_summary_source")
            ) && options.exclude_workflow_id != Some(artifact.workflow_id)
                && (options.after_cursor.is_none()
                    || selection.artifacts.contains(&artifact.id.to_string()))
        })
        .take(max_artifacts)
        .map(|artifact| {
            let (content, content_truncated) = if options.include_artifact_content
                && is_textual_media_type(&artifact.media_type)
            {
                let content = String::from_utf8(store.read_artifact(&artifact)?)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?;
                let (content, truncated) =
                    text.take(SnapshotTextSection::Artifacts, &content, 48_000);
                (Some(content), truncated)
            } else {
                (None, false)
            };
            Ok::<_, StoreError>(json!({
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
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let changed = options.after_cursor.is_none()
        || selection.project
        || project_home.is_some()
        || !sessions.is_empty()
        || !workflow_summaries.is_empty()
        || !artifacts.is_empty();
    Ok(json!({
        "cursor": selection.cursor,
        "has_more": selection.has_more,
        "changed": changed,
        "mode": if options.after_cursor.is_some() { "delta" } else { "full" },
        "after_cursor": options.after_cursor,
        "project": {
            "id": project.id,
            "name": project.name,
        },
        "project_home": project_home,
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

#[derive(Default)]
struct SnapshotSelection {
    cursor: u64,
    captured_cursor: u64,
    has_more: bool,
    project: bool,
    project_home: bool,
    sessions: HashSet<String>,
    workflows: HashSet<String>,
    artifacts: HashSet<String>,
}

#[allow(clippy::too_many_arguments)]
fn select_project_changes(
    batch: ProjectChangeBatch,
    after_cursor: Option<u64>,
    exclude_workflow_id: Option<WorkflowId>,
    excluded_session_ids: &HashSet<String>,
    max_sessions: usize,
    max_workflows: usize,
    max_artifacts: usize,
) -> Result<SnapshotSelection, StoreError> {
    let mut selection = SnapshotSelection {
        cursor: batch.captured_cursor,
        captured_cursor: batch.captured_cursor,
        ..SnapshotSelection::default()
    };
    let Some(after_cursor) = after_cursor else {
        return Ok(selection);
    };
    selection.cursor = after_cursor;
    for change in batch.changes {
        if exclude_workflow_id.is_some_and(|id| change.workflow_id == Some(id))
            || (change.kind == "session" && excluded_session_ids.contains(&change.entity_id))
        {
            selection.cursor = change.sequence;
            continue;
        }
        let accepted = match change.kind.as_str() {
            "project" => {
                selection.project = true;
                true
            }
            "project_home" => {
                selection.project_home = true;
                true
            }
            "session" => insert_bounded(&mut selection.sessions, change.entity_id, max_sessions),
            "workflow" => insert_bounded(&mut selection.workflows, change.entity_id, max_workflows),
            "artifact" => insert_bounded(&mut selection.artifacts, change.entity_id, max_artifacts),
            kind => {
                return Err(StoreError::Invariant(format!(
                    "unknown Project change kind: {kind}"
                )));
            }
        };
        if !accepted {
            selection.has_more = true;
            break;
        }
        selection.cursor = change.sequence;
    }
    selection.has_more |= selection.cursor < selection.captured_cursor;
    Ok(selection)
}

fn insert_bounded(values: &mut HashSet<String>, value: String, limit: usize) -> bool {
    values.contains(&value) || values.len() < limit && values.insert(value)
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
    total: usize,
    remaining: usize,
    truncated: bool,
}

#[derive(Clone, Copy)]
enum SnapshotTextSection {
    ProjectHome,
    Sessions,
    Workflows,
    Artifacts,
}

impl SnapshotTextBudget {
    const TRUNCATION_MARKER: &'static str = "\n\n[Truncated by PaperMachine project snapshot]";

    const fn new(max_chars: usize) -> Self {
        Self {
            total: max_chars,
            remaining: max_chars,
            truncated: false,
        }
    }

    fn exhausted(&self) -> bool {
        self.truncated
    }

    fn take(
        &mut self,
        section: SnapshotTextSection,
        value: &str,
        per_field_limit: usize,
    ) -> (String, bool) {
        let value_chars = value.chars().count();
        let reserve = match section {
            SnapshotTextSection::ProjectHome => self.total * 9 / 10,
            SnapshotTextSection::Sessions => self.total / 2,
            SnapshotTextSection::Workflows => self.total * 3 / 10,
            SnapshotTextSection::Artifacts => 0,
        };
        let allowed = self.remaining.saturating_sub(reserve).min(per_field_limit);
        if value_chars <= allowed {
            self.remaining -= value_chars;
            return (value.to_string(), false);
        }
        self.truncated = true;
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
    use papermachine_protocol::AccessPreset;
    use papermachine_protocol::ArtifactKind;
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
                request_mode: Default::default(),
                params_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
            },
            source: WorkflowProgramSource::Builtin,
            definition_path: "builtin/context-test/workflow.py".to_string(),
            sha256: "context-test".to_string(),
            runtime_sha256: "test-runtime".to_string(),
            source_code: "async def main(ctx): return {}\n".to_string(),
        }
    }

    #[test]
    fn text_budget_is_unicode_safe_and_reserves_later_sections() {
        let mut budget = SnapshotTextBudget::new(1_000);
        let (first, first_truncated) =
            budget.take(SnapshotTextSection::Sessions, &"研".repeat(1_200), 900);
        let (second, second_truncated) =
            budget.take(SnapshotTextSection::Artifacts, &"究".repeat(200), 900);

        assert!(first_truncated);
        assert!(!second_truncated);
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
            .create_project("Context Project", directory.path().join("project"))
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
                request: "Produce durable evidence".to_string(),
                instructions: String::new(),
                trigger: Default::default(),
                params: json!({}),
                default_model: "test-model".to_string(),
                access: AccessPreset::Research,
                enabled_skills: Vec::new(),
                launch_context: Default::default(),
                agent_access_overrides: Default::default(),
            })
            .expect("Workflow should be created");
        let evidence = store
            .create_artifact(
                project.id,
                workflow.id,
                Some(older.id),
                None,
                ArtifactKind::Report,
                "evidence.html",
                "text/html",
                json!({"role": "evidence"}),
                "<h1>原始证据内容</h1>".as_bytes(),
            )
            .expect("Artifact should be created");
        for role in ["project_summary", "project_summary_source"] {
            store
                .create_artifact(
                    project.id,
                    workflow.id,
                    Some(older.id),
                    None,
                    ArtifactKind::Other,
                    format!("forged-{role}.txt"),
                    "text/plain",
                    json!({"role": role}),
                    b"must not become canonical Project-home context",
                )
                .expect("reserved-role fixture should be created");
        }
        store
            .set_session_status(
                older.id,
                papermachine_protocol::SessionStatus::Archived,
                None,
            )
            .expect("historical Session should be archived");

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
        assert!(snapshot["sessions"].as_array().is_some_and(|sessions| {
            sessions.iter().any(|session| {
                session["id"] == older.id.to_string() && session["status"] == "archived"
            })
        }));
        assert_eq!(snapshot["project"]["name"], "Context Project");
        assert_eq!(snapshot["project_home"], Value::Null);
        assert_eq!(snapshot["artifacts"].as_array().map(Vec::len), Some(1));
        assert_eq!(snapshot["artifacts"][0]["content"], "<h1>原始证据内容</h1>");
        assert_eq!(snapshot["artifacts"][0]["content_truncated"], false);
        assert_eq!(snapshot["limits"]["include_artifact_content"], true);

        let without_current = build_project_snapshot(
            &store,
            project.id,
            ProjectSnapshotOptions {
                exclude_workflow_id: Some(workflow.id),
                ..ProjectSnapshotOptions::default()
            },
        )
        .expect("calling Workflow state should be excluded");
        assert_eq!(without_current["workflows"], json!([]));
        assert_eq!(without_current["artifacts"], json!([]));

        std::fs::remove_file(
            store
                .managed_root()
                .join("artifacts")
                .join(evidence.relative_path),
        )
        .expect("Artifact fixture should be removable");
        let error = build_project_snapshot(
            &store,
            project.id,
            ProjectSnapshotOptions {
                include_artifact_content: true,
                ..ProjectSnapshotOptions::default()
            },
        )
        .expect_err("missing Artifact content must fail the snapshot");
        assert!(error.to_string().contains("Artifact file is unavailable"));
    }

    #[test]
    fn snapshot_cursor_returns_only_later_project_changes() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Store::open_in_memory(directory.path().join("artifacts")).expect("store should open");
        let project = store
            .create_project("Incremental context", directory.path().join("project"))
            .expect("Project should be created");
        store
            .create_session(project.id, "Before cursor", "", "test-model", Vec::new())
            .expect("initial Session should be created");

        let full = build_project_snapshot(&store, project.id, ProjectSnapshotOptions::default())
            .expect("full snapshot should be built");
        let cursor = full["cursor"].as_u64().expect("cursor should be numeric");
        let empty_delta = build_project_snapshot(
            &store,
            project.id,
            ProjectSnapshotOptions {
                after_cursor: Some(cursor),
                ..ProjectSnapshotOptions::default()
            },
        )
        .expect("empty delta should be built");
        assert_eq!(empty_delta["mode"], "delta");
        assert_eq!(empty_delta["sessions"], json!([]));
        assert_eq!(empty_delta["changed"], false);
        assert_eq!(empty_delta["has_more"], false);

        let first = store
            .create_session(
                project.id,
                "First after cursor",
                "",
                "test-model",
                Vec::new(),
            )
            .expect("first later Session should be created");
        let second = store
            .create_session(
                project.id,
                "Second after cursor",
                "",
                "test-model",
                Vec::new(),
            )
            .expect("second later Session should be created");
        let changed = build_project_snapshot(
            &store,
            project.id,
            ProjectSnapshotOptions {
                after_cursor: Some(cursor),
                max_sessions: 1,
                ..ProjectSnapshotOptions::default()
            },
        )
        .expect("delta snapshot should be built");
        assert_eq!(changed["sessions"].as_array().map(Vec::len), Some(1));
        assert_eq!(changed["sessions"][0]["id"], first.id.to_string());
        assert_eq!(changed["changed"], true);
        assert_eq!(changed["has_more"], true);

        let next = build_project_snapshot(
            &store,
            project.id,
            ProjectSnapshotOptions {
                after_cursor: Some(
                    changed["cursor"]
                        .as_u64()
                        .expect("cursor should be numeric"),
                ),
                max_sessions: 1,
                ..ProjectSnapshotOptions::default()
            },
        )
        .expect("remaining delta should be built");
        assert_eq!(next["sessions"].as_array().map(Vec::len), Some(1));
        assert_eq!(next["sessions"][0]["id"], second.id.to_string());
        assert_eq!(next["has_more"], false);
    }
}
