use crate::StoreError;
use crate::filesystem::sync_directory;
use crate::filesystem::write_atomic;
use hex::encode;
use papermachine_protocol::Artifact;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::SessionId;
use papermachine_protocol::WorkflowId;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

pub(crate) struct StoredArtifactFile {
    pub relative_path: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub created: bool,
}

pub(crate) fn store_artifact_file(
    artifact_root: &Path,
    workflow_id: WorkflowId,
    session_id: Option<SessionId>,
    artifact_id: ArtifactId,
    name: &str,
    bytes: &[u8],
) -> Result<StoredArtifactFile, StoreError> {
    let session_segment = session_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "workflow".to_string());
    let directory = artifact_root
        .join(workflow_id.to_string())
        .join(session_segment);
    std::fs::create_dir_all(&directory).map_err(|error| StoreError::Io(error.to_string()))?;

    let file_name = format!("{}-{}", artifact_id, sanitize_name(name));
    let destination = directory.join(file_name);
    let sha256 = encode(Sha256::digest(bytes));
    let created = if destination.exists() {
        let metadata = regular_file_metadata(&destination)?;
        let existing =
            std::fs::read(&destination).map_err(|error| StoreError::Io(error.to_string()))?;
        if metadata.len() != bytes.len() as u64 || encode(Sha256::digest(&existing)) != sha256 {
            return Err(StoreError::Invariant(format!(
                "Artifact file already exists with different content: {}",
                destination.display()
            )));
        }
        false
    } else {
        write_atomic(&destination, bytes)?;
        true
    };

    let relative = destination
        .strip_prefix(artifact_root)
        .map_err(|error| StoreError::Invariant(error.to_string()))?;
    Ok(StoredArtifactFile {
        relative_path: relative.to_string_lossy().into_owned(),
        sha256,
        size_bytes: bytes.len() as u64,
        created,
    })
}

pub(crate) fn remove_artifact_file(
    artifact_root: &Path,
    relative_path: &str,
) -> Result<(), StoreError> {
    let path = artifact_path(artifact_root, relative_path)?;
    match std::fs::remove_file(&path) {
        Ok(()) => path.parent().map(sync_directory).transpose().map(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::Io(error.to_string())),
    }
}

pub(crate) fn read_artifact_file(
    artifact_root: &Path,
    artifact: &Artifact,
) -> Result<Vec<u8>, StoreError> {
    let path = artifact_path(artifact_root, &artifact.relative_path)?;
    let metadata = regular_file_metadata(&path)?;
    if metadata.len() != artifact.size_bytes {
        return Err(StoreError::Invariant(format!(
            "Artifact {} size does not match its durable record",
            artifact.id
        )));
    }
    let bytes = std::fs::read(&path).map_err(|error| StoreError::Io(error.to_string()))?;
    if encode(Sha256::digest(&bytes)) != artifact.sha256 {
        return Err(StoreError::Invariant(format!(
            "Artifact {} hash does not match its durable record",
            artifact.id
        )));
    }
    Ok(bytes)
}

pub(crate) fn reconcile_artifact_files(
    artifact_root: &Path,
    artifacts: &[Artifact],
) -> Result<(), StoreError> {
    let mut expected = BTreeSet::new();
    for artifact in artifacts {
        let relative = validated_relative_path(&artifact.relative_path)?;
        if !expected.insert(relative.clone()) {
            return Err(StoreError::Invariant(format!(
                "multiple Artifacts reference {}",
                relative.display()
            )));
        }
        let metadata = regular_file_metadata(&artifact_root.join(&relative))?;
        if metadata.len() != artifact.size_bytes {
            return Err(StoreError::Invariant(format!(
                "Artifact {} size does not match its durable record",
                artifact.id
            )));
        }
        read_artifact_file(artifact_root, artifact)?;
    }

    let mut directories = Vec::new();
    let mut pending = vec![artifact_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = std::fs::read_dir(&directory)
            .map_err(|error| StoreError::Io(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry
                .file_type()
                .map_err(|error| StoreError::Io(error.to_string()))?;
            if file_type.is_symlink() {
                return Err(StoreError::Invariant(format!(
                    "Artifact storage contains a symlink: {}",
                    entry.path().display()
                )));
            }
            if file_type.is_dir() {
                directories.push(entry.path());
                pending.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                return Err(StoreError::Invariant(format!(
                    "Artifact storage contains a non-file entry: {}",
                    entry.path().display()
                )));
            }
            let relative = entry
                .path()
                .strip_prefix(artifact_root)
                .map_err(|error| StoreError::Invariant(error.to_string()))?
                .to_path_buf();
            if !expected.contains(&relative) {
                std::fs::remove_file(entry.path())
                    .map_err(|error| StoreError::Io(error.to_string()))?;
            }
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        if std::fs::read_dir(&directory)
            .map_err(|error| StoreError::Io(error.to_string()))?
            .next()
            .is_none()
        {
            std::fs::remove_dir(&directory).map_err(|error| StoreError::Io(error.to_string()))?;
        }
    }
    Ok(())
}

fn artifact_path(artifact_root: &Path, relative_path: &str) -> Result<PathBuf, StoreError> {
    Ok(artifact_root.join(validated_relative_path(relative_path)?))
}

fn validated_relative_path(relative_path: &str) -> Result<PathBuf, StoreError> {
    let path = Path::new(relative_path);
    if relative_path.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(StoreError::Invariant(format!(
            "Artifact has an invalid relative path: {relative_path}"
        )));
    }
    Ok(path.to_path_buf())
}

fn regular_file_metadata(path: &Path) -> Result<std::fs::Metadata, StoreError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        StoreError::Invariant(format!(
            "Artifact file is unavailable: {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(StoreError::Invariant(format!(
            "Artifact path is not a regular file: {}",
            path.display()
        )));
    }
    Ok(metadata)
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
