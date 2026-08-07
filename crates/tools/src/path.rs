use crate::ToolError;
use papermachine_protocol::AgentAccessProfile;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

pub(crate) async fn resolve_workspace_path(
    workspace_root: &Path,
    relative_path: &str,
) -> Result<PathBuf, ToolError> {
    let relative = Path::new(relative_path);
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolError::PathOutsideWorkspace(relative_path.to_string()));
    }

    let canonical_root = tokio::fs::canonicalize(workspace_root)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    let candidate = canonical_root.join(relative);

    let mut existing = candidate.as_path();
    while tokio::fs::symlink_metadata(existing).await.is_err() {
        existing = existing
            .parent()
            .ok_or_else(|| ToolError::PathOutsideWorkspace(relative_path.to_string()))?;
    }
    let canonical_existing = tokio::fs::canonicalize(existing)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    if !canonical_existing.starts_with(&canonical_root) {
        return Err(ToolError::PathOutsideWorkspace(relative_path.to_string()));
    }
    Ok(candidate)
}

pub(crate) async fn resolve_tool_path(
    workspace_root: &Path,
    protected_root: &Path,
    requested_path: &str,
    access: AgentAccessProfile,
) -> Result<PathBuf, ToolError> {
    if !access.is_unrestricted() {
        return resolve_workspace_path(workspace_root, requested_path).await;
    }
    let path = Path::new(requested_path);
    if path.as_os_str().is_empty() {
        return Err(ToolError::InvalidArguments {
            tool: "file".to_string(),
            message: "path must not be empty".to_string(),
        });
    }
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let workspace = tokio::fs::canonicalize(workspace_root)
            .await
            .map_err(|error| ToolError::Io(error.to_string()))?;
        workspace.join(path)
    };
    reject_managed_path(&candidate, protected_root, requested_path).await?;
    Ok(candidate)
}

async fn reject_managed_path(
    candidate: &Path,
    protected_root: &Path,
    original: &str,
) -> Result<(), ToolError> {
    let protected_root = tokio::fs::canonicalize(protected_root)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    let mut existing = candidate;
    while tokio::fs::symlink_metadata(existing).await.is_err() {
        existing = existing
            .parent()
            .ok_or_else(|| ToolError::PathInsideManagedState(original.to_string()))?;
    }
    let canonical_existing = tokio::fs::canonicalize(existing)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    if canonical_existing.starts_with(&protected_root) {
        return Err(ToolError::PathInsideManagedState(original.to_string()));
    }
    Ok(())
}
