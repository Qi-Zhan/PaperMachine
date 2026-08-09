use crate::Store;
use crate::StoreError;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::WorkflowId;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

pub const PROJECT_HOME_ROLE: &str = "project_summary";
pub const PROJECT_HOME_SOURCE_ROLE: &str = "project_summary_source";
pub const PROJECT_HOME_MEDIA_TYPE: &str = "text/html; charset=utf-8";
pub const PROJECT_HOME_SOURCE_MEDIA_TYPE: &str = "application/vnd.papermachine.project-home+json";

const MAX_BLOCKS: usize = 128;
const MAX_BLOCK_BYTES: usize = 256 * 1024;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATCH_OPERATIONS: usize = 128;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectHomeBlock {
    pub id: String,
    pub html: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectHomeSource {
    pub revision: String,
    pub blocks: Vec<ProjectHomeBlock>,
}

impl ProjectHomeSource {
    pub fn html(&self) -> String {
        materialize_html(&self.blocks)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectHomeDraft {
    pub action_invocation_id: ActionInvocationId,
    pub base_artifact_id: Option<ArtifactId>,
    pub revision: String,
    pub blocks: Vec<ProjectHomeBlock>,
    #[serde(default)]
    applied_effects: Vec<String>,
}

impl ProjectHomeDraft {
    pub fn source(&self) -> ProjectHomeSource {
        ProjectHomeSource {
            revision: self.revision.clone(),
            blocks: self.blocks.clone(),
        }
    }

    pub fn html(&self) -> String {
        materialize_html(&self.blocks)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectHomePatchOperation {
    Upsert { id: String, html: String },
    Remove { id: String },
    Reorder { order: Vec<String> },
}

impl Store {
    pub fn read_project_home_draft(
        &self,
        workflow_id: WorkflowId,
        action_invocation_id: ActionInvocationId,
    ) -> Result<ProjectHomeDraft, StoreError> {
        self.validate_project_home_action(workflow_id, action_invocation_id)?;
        let path = self.project_home_draft_path(workflow_id, action_invocation_id);
        if path.is_file() {
            let draft: ProjectHomeDraft = serde_json::from_slice(
                &std::fs::read(&path).map_err(|error| StoreError::Io(error.to_string()))?,
            )?;
            if draft.action_invocation_id == action_invocation_id {
                validate_draft(&draft)?;
                return Ok(draft);
            }
        }

        let workflow = self.get_workflow(workflow_id)?;
        let (base_artifact_id, source) = self.latest_project_home_source(workflow.project_id)?;
        let draft = ProjectHomeDraft {
            action_invocation_id,
            base_artifact_id,
            revision: source.revision,
            blocks: source.blocks,
            applied_effects: Vec::new(),
        };
        validate_draft(&draft)?;
        write_draft(&path, &draft)?;
        Ok(draft)
    }

    pub fn patch_project_home_draft(
        &self,
        workflow_id: WorkflowId,
        action_invocation_id: ActionInvocationId,
        effect_id: &str,
        base_revision: &str,
        operations: Vec<ProjectHomePatchOperation>,
    ) -> Result<ProjectHomeDraft, StoreError> {
        if effect_id.trim().is_empty() {
            return Err(StoreError::Invariant(
                "Project-home patch effect ID must not be empty".to_string(),
            ));
        }
        if operations.is_empty() || operations.len() > MAX_PATCH_OPERATIONS {
            return Err(StoreError::Invariant(format!(
                "Project-home patch must contain 1 to {MAX_PATCH_OPERATIONS} operations"
            )));
        }
        let mut draft = self.read_project_home_draft(workflow_id, action_invocation_id)?;
        if draft
            .applied_effects
            .iter()
            .any(|applied| applied == effect_id)
        {
            return Ok(draft);
        }
        if draft.revision != base_revision {
            return Err(StoreError::Invariant(format!(
                "Project-home revision conflict: expected {}, received {base_revision}",
                draft.revision
            )));
        }

        let previous_blocks = draft.blocks.clone();
        for operation in operations {
            apply_operation(&mut draft.blocks, operation)?;
        }
        validate_blocks(&draft.blocks)?;
        if draft.blocks == previous_blocks {
            return Err(StoreError::Invariant(
                "Project-home patch made no changes".to_string(),
            ));
        }
        draft.revision = revision_for(&draft.blocks)?;
        draft.applied_effects.push(effect_id.to_string());
        write_draft(
            &self.project_home_draft_path(workflow_id, action_invocation_id),
            &draft,
        )?;
        Ok(draft)
    }

    pub fn project_home_source_for_publish(
        &self,
        workflow_id: WorkflowId,
        action_invocation_id: ActionInvocationId,
    ) -> Result<(Option<ArtifactId>, ProjectHomeSource), StoreError> {
        let draft = self.read_project_home_draft(workflow_id, action_invocation_id)?;
        if draft.blocks.is_empty() {
            return Err(StoreError::Invariant(
                "Project-home Agent finished without creating any page blocks".to_string(),
            ));
        }
        Ok((draft.base_artifact_id, draft.source()))
    }

    fn validate_project_home_action(
        &self,
        workflow_id: WorkflowId,
        action_invocation_id: ActionInvocationId,
    ) -> Result<(), StoreError> {
        let invocation = self.get_action_invocation(action_invocation_id)?;
        if invocation.workflow_id != workflow_id {
            return Err(StoreError::Invariant(
                "Project-home Action belongs to another Workflow".to_string(),
            ));
        }
        Ok(())
    }

    fn latest_project_home_source(
        &self,
        project_id: papermachine_protocol::ProjectId,
    ) -> Result<(Option<ArtifactId>, ProjectHomeSource), StoreError> {
        let Some(page) = self
            .list_project_artifacts(project_id)?
            .into_iter()
            .find(|artifact| {
                artifact
                    .metadata
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    == Some(PROJECT_HOME_ROLE)
            })
        else {
            return Ok((None, empty_source()?));
        };
        let Some(source_id) = page
            .metadata
            .get("source_artifact_id")
            .and_then(serde_json::Value::as_str)
        else {
            // Pre-tool Project-home artifacts intentionally start a fresh block
            // model. The published page remains visible until the new draft is
            // atomically committed.
            return Ok((Some(page.id), empty_source()?));
        };
        let source_id = ArtifactId::from_str(source_id)
            .map_err(|error| StoreError::Invariant(error.to_string()))?;
        let source_artifact = self.get_artifact(source_id)?;
        if source_artifact.project_id != project_id
            || source_artifact
                .metadata
                .get("role")
                .and_then(serde_json::Value::as_str)
                != Some(PROJECT_HOME_SOURCE_ROLE)
        {
            return Err(StoreError::Invariant(
                "Project-home source Artifact has invalid ownership or role".to_string(),
            ));
        }
        let source: ProjectHomeSource =
            serde_json::from_slice(&self.read_artifact(&source_artifact)?)?;
        validate_source(&source)?;
        Ok((Some(page.id), source))
    }

    fn project_home_draft_path(
        &self,
        workflow_id: WorkflowId,
        action_invocation_id: ActionInvocationId,
    ) -> PathBuf {
        self.managed_root()
            .join("runtime/project-home-drafts")
            .join(workflow_id.to_string())
            .join(format!("{action_invocation_id}.json"))
    }
}

fn empty_source() -> Result<ProjectHomeSource, StoreError> {
    let blocks = Vec::new();
    Ok(ProjectHomeSource {
        revision: revision_for(&blocks)?,
        blocks,
    })
}

fn validate_draft(draft: &ProjectHomeDraft) -> Result<(), StoreError> {
    validate_blocks(&draft.blocks)?;
    let revision = revision_for(&draft.blocks)?;
    if draft.revision != revision {
        return Err(StoreError::Invariant(
            "Project-home draft revision does not match its blocks".to_string(),
        ));
    }
    Ok(())
}

fn validate_source(source: &ProjectHomeSource) -> Result<(), StoreError> {
    validate_blocks(&source.blocks)?;
    let revision = revision_for(&source.blocks)?;
    if source.revision != revision {
        return Err(StoreError::Invariant(
            "Project-home source revision does not match its blocks".to_string(),
        ));
    }
    Ok(())
}

fn apply_operation(
    blocks: &mut Vec<ProjectHomeBlock>,
    operation: ProjectHomePatchOperation,
) -> Result<(), StoreError> {
    match operation {
        ProjectHomePatchOperation::Upsert { id, html } => {
            validate_block_id(&id)?;
            validate_html_fragment(&html)?;
            if let Some(block) = blocks.iter_mut().find(|block| block.id == id) {
                block.html = html.trim().to_string();
            } else {
                blocks.push(ProjectHomeBlock {
                    id,
                    html: html.trim().to_string(),
                });
            }
        }
        ProjectHomePatchOperation::Remove { id } => {
            validate_block_id(&id)?;
            let before = blocks.len();
            blocks.retain(|block| block.id != id);
            if blocks.len() == before {
                return Err(StoreError::Invariant(format!(
                    "Project-home block does not exist: {id}"
                )));
            }
        }
        ProjectHomePatchOperation::Reorder { order } => {
            if order.len() != blocks.len() {
                return Err(StoreError::Invariant(
                    "Project-home reorder must name every current block exactly once".to_string(),
                ));
            }
            let requested = order.iter().cloned().collect::<BTreeSet<_>>();
            let current = blocks
                .iter()
                .map(|block| block.id.clone())
                .collect::<BTreeSet<_>>();
            if requested.len() != order.len() || requested != current {
                return Err(StoreError::Invariant(
                    "Project-home reorder must name every current block exactly once".to_string(),
                ));
            }
            let mut reordered = Vec::with_capacity(blocks.len());
            for id in order {
                let index = blocks
                    .iter()
                    .position(|block| block.id == id)
                    .ok_or_else(|| StoreError::Invariant(format!("unknown block: {id}")))?;
                reordered.push(blocks.remove(index));
            }
            *blocks = reordered;
        }
    }
    Ok(())
}

fn validate_blocks(blocks: &[ProjectHomeBlock]) -> Result<(), StoreError> {
    if blocks.len() > MAX_BLOCKS {
        return Err(StoreError::Invariant(format!(
            "Project home exceeds {MAX_BLOCKS} blocks"
        )));
    }
    let mut ids = BTreeSet::new();
    let mut total = 0_usize;
    for block in blocks {
        validate_block_id(&block.id)?;
        validate_html_fragment(&block.html)?;
        if !ids.insert(block.id.as_str()) {
            return Err(StoreError::Invariant(format!(
                "duplicate Project-home block ID: {}",
                block.id
            )));
        }
        total = total.saturating_add(block.html.len());
    }
    if total > MAX_PAGE_BYTES {
        return Err(StoreError::Invariant(format!(
            "Project home exceeds {MAX_PAGE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_block_id(id: &str) -> Result<(), StoreError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(StoreError::Invariant(
            "Project-home block IDs must use 1-64 ASCII letters, digits, '.', '_' or '-'"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_html_fragment(html: &str) -> Result<(), StoreError> {
    let html = html.trim();
    if html.is_empty() {
        return Err(StoreError::Invariant(
            "Project-home block HTML must not be empty".to_string(),
        ));
    }
    if html.len() > MAX_BLOCK_BYTES {
        return Err(StoreError::Invariant(format!(
            "Project-home block exceeds {MAX_BLOCK_BYTES} bytes"
        )));
    }
    if html.contains('\0') {
        return Err(StoreError::Invariant(
            "Project-home block HTML must not contain NUL bytes".to_string(),
        ));
    }
    let lower = html.to_ascii_lowercase();
    let forbidden_tags = [
        "article", "audio", "base", "button", "canvas", "embed", "form", "head", "html", "iframe",
        "img", "input", "link", "math", "meta", "object", "option", "script", "select", "style",
        "svg", "template", "textarea", "video",
    ];
    if let Some(tag) = forbidden_tags
        .iter()
        .find(|tag| contains_html_tag(&lower, tag))
    {
        return Err(StoreError::Invariant(format!(
            "Project-home block contains forbidden <{tag}> markup"
        )));
    }
    if lower.contains("style=")
        || lower.contains("srcdoc=")
        || lower.contains("javascript:")
        || contains_event_handler(&lower)
    {
        return Err(StoreError::Invariant(
            "Project-home block contains an unsafe attribute or URL".to_string(),
        ));
    }
    Ok(())
}

fn contains_html_tag(html: &str, tag: &str) -> bool {
    let prefix = format!("<{tag}");
    html.match_indices(&prefix).any(|(index, _)| {
        html.as_bytes()
            .get(index + prefix.len())
            .is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
    })
}

fn contains_event_handler(html: &str) -> bool {
    let bytes = html.as_bytes();
    for index in 0..bytes.len().saturating_sub(3) {
        if !bytes[index].is_ascii_whitespace()
            || bytes[index + 1] != b'o'
            || bytes[index + 2] != b'n'
        {
            continue;
        }
        let mut cursor = index + 3;
        while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] == b'=' {
            return true;
        }
    }
    false
}

fn revision_for(blocks: &[ProjectHomeBlock]) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(blocks)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn materialize_html(blocks: &[ProjectHomeBlock]) -> String {
    let mut html = String::from("<article>\n");
    for block in blocks {
        html.push_str(block.html.trim());
        html.push('\n');
    }
    html.push_str("</article>");
    html
}

fn write_draft(path: &Path, draft: &ProjectHomeDraft) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::Invariant("Project-home draft has no parent".to_string()))?;
    std::fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
    let temporary = parent.join(format!(".project-home-{}.tmp", uuid::Uuid::now_v7()));
    let bytes = serde_json::to_vec(draft)?;
    let result = (|| {
        let mut file =
            std::fs::File::create(&temporary).map_err(|error| StoreError::Io(error.to_string()))?;
        std::io::Write::write_all(&mut file, &bytes)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        std::fs::rename(&temporary, path).map_err(|error| StoreError::Io(error.to_string()))
    })();
    if result.is_err() && temporary.exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
