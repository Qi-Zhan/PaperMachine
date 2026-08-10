use crate::Store;
use crate::StoreError;
use crate::artifact::store_artifact_file;
use chrono::Utc;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::AgentId;
use papermachine_protocol::Artifact;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::ArtifactKind;
use papermachine_protocol::ProjectHome;
use papermachine_protocol::SessionId;
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
        session_id: SessionId,
        action_invocation_id: ActionInvocationId,
        agent_id: AgentId,
        source_artifact_id: ArtifactId,
        artifact_id: ArtifactId,
        html: String,
        metadata: Value,
    ) -> Result<PublishedProjectHome, StoreError> {
        let invocation = self.get_action_invocation(action_invocation_id)?;
        if invocation.session_id != session_id || invocation.agent_id != agent_id {
            return Err(StoreError::Invariant(
                "Project-home publication Action has invalid ownership".to_string(),
            ));
        }
        let session = self.get_session(session_id)?;
        let html = extract_html_document(&html)?;
        let revision = hex::encode(Sha256::digest(html.as_bytes()));
        let current = self.get_project_home(session.project_id)?;
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
        let now = Utc::now();
        let source_file = store_artifact_file(
            &self.managed_root().join("artifacts"),
            session_id,
            Some(agent_id),
            source_artifact_id,
            "project-home.source.json",
            &source_content,
        )?;
        let source_artifact = Artifact {
            id: source_artifact_id,
            project_id: session.project_id,
            session_id,
            agent_id: Some(agent_id),
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
            session_id,
            Some(agent_id),
            artifact_id,
            "project-home.html",
            html.as_bytes(),
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
            project_id: session.project_id,
            session_id,
            agent_id: Some(agent_id),
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
            project_id: session.project_id,
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

fn extract_html_document(output: &str) -> Result<&str, StoreError> {
    let output = output.trim();
    if output.len() > MAX_PAGE_BYTES {
        return Err(StoreError::Invariant(format!(
            "Project home exceeds {MAX_PAGE_BYTES} bytes"
        )));
    }
    if output.contains('\0') {
        return Err(StoreError::Invariant(
            "Project-home HTML must not contain NUL bytes".to_string(),
        ));
    }

    let lowercase = output.to_ascii_lowercase();
    let start = lowercase
        .find("<!doctype html")
        .or_else(|| lowercase.find("<html"));
    let end = lowercase
        .rfind("</html>")
        .map(|index| index + "</html>".len());
    let Some((start, end)) = start.zip(end).filter(|(start, end)| start < end) else {
        return Err(StoreError::Invariant(
            "Project-home Action must return a complete standalone HTML document".to_string(),
        ));
    };
    Ok(&output[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_bounds_html_documents() {
        assert!(extract_html_document("Result without markup").is_err());
        assert!(extract_html_document("<html><body>Result\0</body></html>").is_err());
        assert_eq!(
            extract_html_document(
                "Done.\n\n```html\n<!DOCTYPE html><html><body><h1>Result</h1></body></html>\n```"
            )
            .expect("fenced document should be extracted"),
            "<!DOCTYPE html><html><body><h1>Result</h1></body></html>"
        );
        assert_eq!(
            extract_html_document("<HTML><body>Result</body></HTML>")
                .expect("HTML matching should be case insensitive"),
            "<HTML><body>Result</body></HTML>"
        );
    }
}
