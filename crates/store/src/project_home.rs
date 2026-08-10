use crate::Store;
use crate::StoreError;
use crate::artifact::store_artifact_file;
use chrono::Utc;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::Artifact;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::ArtifactKind;
use papermachine_protocol::ProjectHome;
use papermachine_protocol::SessionId;
use papermachine_protocol::WorkflowId;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

pub const PROJECT_HOME_ROLE: &str = "project_summary";
pub const PROJECT_HOME_SOURCE_ROLE: &str = "project_summary_source";
pub const PROJECT_HOME_MEDIA_TYPE: &str = "text/html; charset=utf-8";
pub const PROJECT_HOME_SOURCE_MEDIA_TYPE: &str = "application/vnd.papermachine.project-home+json";

const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct PublishedProjectHome {
    pub home: ProjectHome,
    pub artifact: Artifact,
    pub source_artifact: Artifact,
    pub changed: bool,
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn publish_project_home(
        &self,
        workflow_id: WorkflowId,
        action_invocation_id: ActionInvocationId,
        session_id: SessionId,
        source_artifact_id: ArtifactId,
        artifact_id: ArtifactId,
        html: String,
        metadata: Value,
    ) -> Result<PublishedProjectHome, StoreError> {
        let invocation = self.get_action_invocation(action_invocation_id)?;
        if invocation.workflow_id != workflow_id || invocation.session_id != session_id {
            return Err(StoreError::Invariant(
                "Project-home publication Action has invalid ownership".to_string(),
            ));
        }
        let workflow = self.get_workflow(workflow_id)?;
        let html = html.trim();
        if html.is_empty() {
            return Err(StoreError::Invariant(
                "Project-home Action returned an empty page".to_string(),
            ));
        }
        validate_html_fragment(html)?;
        let revision = hex::encode(Sha256::digest(html.as_bytes()));
        let current = self.get_project_home(workflow.project_id)?;
        if let Some(current) = current.as_ref()
            && current.revision == revision
        {
            return Ok(PublishedProjectHome {
                artifact: self.get_artifact(current.artifact_id)?,
                source_artifact: self.get_artifact(current.source_artifact_id)?,
                home: current.clone(),
                changed: false,
            });
        }

        let mut page_metadata = metadata.as_object().cloned().ok_or_else(|| {
            StoreError::Invariant("Project-home publication metadata must be an object".to_string())
        })?;
        let source_content = serde_json::to_vec(&json!({
            "revision": revision,
            "html": html,
        }))?;
        let page = format!("<article>\n{html}\n</article>");
        let now = Utc::now();
        let source_file = store_artifact_file(
            &self.managed_root().join("artifacts"),
            workflow_id,
            Some(session_id),
            source_artifact_id,
            "project-home.source.json",
            &source_content,
        )?;
        let source_artifact = Artifact {
            id: source_artifact_id,
            project_id: workflow.project_id,
            workflow_id,
            session_id: Some(session_id),
            action_invocation_id: Some(action_invocation_id),
            kind: ArtifactKind::Other,
            name: "project-home.source.json".to_string(),
            media_type: PROJECT_HOME_SOURCE_MEDIA_TYPE.to_string(),
            relative_path: source_file.relative_path,
            sha256: source_file.sha256,
            size_bytes: source_file.size_bytes,
            metadata: json!({
                "role": PROJECT_HOME_SOURCE_ROLE,
                "revision": revision,
            }),
            created_at: now,
        };
        let source_file_created = source_file.created;
        let page_file = match store_artifact_file(
            &self.managed_root().join("artifacts"),
            workflow_id,
            Some(session_id),
            artifact_id,
            "project-home.html",
            page.as_bytes(),
        ) {
            Ok(file) => file,
            Err(error) => {
                if source_file_created {
                    remove_artifact_file(self, &source_artifact);
                }
                return Err(error);
            }
        };
        let page_file_created = page_file.created;
        page_metadata.insert("role".to_string(), json!(PROJECT_HOME_ROLE));
        page_metadata.insert("revision".to_string(), json!(revision));
        page_metadata.insert("source_artifact_id".to_string(), json!(source_artifact.id));
        page_metadata.insert(
            "base_artifact_id".to_string(),
            json!(current.as_ref().map(|home| home.artifact_id)),
        );
        let artifact = Artifact {
            id: artifact_id,
            project_id: workflow.project_id,
            workflow_id,
            session_id: Some(session_id),
            action_invocation_id: Some(action_invocation_id),
            kind: ArtifactKind::Report,
            name: "project-home.html".to_string(),
            media_type: PROJECT_HOME_MEDIA_TYPE.to_string(),
            relative_path: page_file.relative_path,
            sha256: page_file.sha256,
            size_bytes: page_file.size_bytes,
            metadata: Value::Object(page_metadata),
            created_at: now,
        };
        let home = ProjectHome {
            project_id: workflow.project_id,
            artifact_id: artifact.id,
            source_artifact_id: source_artifact.id,
            revision,
            updated_at: now,
        };
        let base_artifact_id = current.map(|home| home.artifact_id);
        match self.commit_project_home(base_artifact_id, &home, &source_artifact, &artifact) {
            Ok(home) => Ok(PublishedProjectHome {
                home,
                artifact,
                source_artifact,
                changed: true,
            }),
            Err(error) => {
                if source_file_created {
                    remove_artifact_file(self, &source_artifact);
                }
                if page_file_created {
                    remove_artifact_file(self, &artifact);
                }
                Err(error)
            }
        }
    }
}

fn remove_artifact_file(store: &Store, artifact: &Artifact) {
    let _ = crate::artifact::remove_artifact_file(
        &store.managed_root().join("artifacts"),
        &artifact.relative_path,
    );
}

fn validate_html_fragment(html: &str) -> Result<(), StoreError> {
    if html.len() > MAX_PAGE_BYTES {
        return Err(StoreError::Invariant(format!(
            "Project home exceeds {MAX_PAGE_BYTES} bytes"
        )));
    }
    if html.contains('\0') {
        return Err(StoreError::Invariant(
            "Project-home HTML must not contain NUL bytes".to_string(),
        ));
    }
    if !html.starts_with('<') || !html.ends_with('>') || html.contains("```") {
        return Err(StoreError::Invariant(
            "Project-home output must be only an HTML fragment".to_string(),
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
            "Project-home HTML contains forbidden <{tag}> markup"
        )));
    }
    if lower.contains("style=")
        || lower.contains("srcdoc=")
        || lower.contains("javascript:")
        || contains_event_handler(&lower)
    {
        return Err(StoreError::Invariant(
            "Project-home HTML contains an unsafe attribute or URL".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_markup() {
        for html in [
            "<script>alert(1)</script>",
            "<img src=x>",
            "<p onclick=alert(1)>x</p>",
            "<a href=javascript:alert(1)>x</a>",
            "```html\n<h1>Result</h1>\n```",
            "Result without markup",
        ] {
            assert!(validate_html_fragment(html).is_err(), "accepted {html}");
        }
        assert!(
            validate_html_fragment("<h1>Result</h1><table><tr><td>1</td></tr></table>").is_ok()
        );
    }
}
