use crate::StoreError;
use hex::encode;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::ResearchId;
use papermachine_protocol::SessionId;
use papermachine_protocol::WorkflowRunId;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::path::PathBuf;

pub(crate) struct StoredArtifactFile {
    pub relative_path: String,
    pub sha256: String,
    pub size_bytes: u64,
}

pub(crate) fn store_artifact_file(
    artifact_root: &Path,
    research_id: ResearchId,
    workflow_run_id: WorkflowRunId,
    session_id: Option<SessionId>,
    artifact_id: ArtifactId,
    name: &str,
    bytes: &[u8],
) -> Result<StoredArtifactFile, StoreError> {
    let session_segment = session_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "run".to_string());
    let directory = artifact_root
        .join(research_id.to_string())
        .join(workflow_run_id.to_string())
        .join(session_segment);
    std::fs::create_dir_all(&directory).map_err(|error| StoreError::Io(error.to_string()))?;

    let file_name = format!("{}-{}", artifact_id, sanitize_name(name));
    let destination = directory.join(file_name);
    let temporary = temporary_path(&destination, artifact_id);
    std::fs::write(&temporary, bytes).map_err(|error| StoreError::Io(error.to_string()))?;
    std::fs::rename(&temporary, &destination).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        StoreError::Io(error.to_string())
    })?;

    let relative = destination
        .strip_prefix(artifact_root)
        .map_err(|error| StoreError::Invariant(error.to_string()))?;
    Ok(StoredArtifactFile {
        relative_path: relative.to_string_lossy().into_owned(),
        sha256: encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as u64,
    })
}

fn temporary_path(destination: &Path, artifact_id: ArtifactId) -> PathBuf {
    let mut name = destination
        .file_name()
        .map(|value| value.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{artifact_id}.tmp"));
    destination.with_file_name(name)
}

fn sanitize_name(name: &str) -> String {
    let value = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = value.trim_matches('_');
    if trimmed.is_empty() {
        "artifact".to_string()
    } else {
        trimmed.chars().take(120).collect()
    }
}
