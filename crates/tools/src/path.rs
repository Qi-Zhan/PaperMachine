use crate::ToolError;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::PathAuthorizationFailure;
use papermachine_protocol::PathOperation;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

pub(crate) async fn resolve_tool_path(
    authorization: &AuthorizationContext,
    requested_path: &str,
    operation: PathOperation,
) -> Result<PathBuf, ToolError> {
    let requested = Path::new(requested_path);
    if requested.as_os_str().is_empty()
        || requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolError::PathOutsideWorkspace(requested_path.to_string()));
    }
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        Path::new(&authorization.cwd).join(requested)
    };
    let mut existing = candidate.as_path();
    let mut missing = Vec::new();
    while tokio::fs::symlink_metadata(existing).await.is_err() {
        let name = existing
            .file_name()
            .ok_or_else(|| ToolError::PathOutsideWorkspace(requested_path.to_string()))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| ToolError::PathOutsideWorkspace(requested_path.to_string()))?;
    }
    let mut resolved = tokio::fs::canonicalize(existing)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    authorization
        .authorize_path(&resolved, operation)
        .map_err(|failure| map_authorization_failure(failure, requested_path))?;
    Ok(resolved)
}

fn map_authorization_failure(failure: PathAuthorizationFailure, requested_path: &str) -> ToolError {
    match failure {
        PathAuthorizationFailure::ManagedState => {
            ToolError::PathInsideManagedState(requested_path.to_string())
        }
        PathAuthorizationFailure::SensitiveWorkspacePath => {
            ToolError::SensitiveWorkspacePath(requested_path.to_string())
        }
        PathAuthorizationFailure::ProtectedWorkspaceMetadata => {
            ToolError::ProtectedWorkspaceMetadata(requested_path.to_string())
        }
        PathAuthorizationFailure::InvalidPath
        | PathAuthorizationFailure::OutsideWorkspace
        | PathAuthorizationFailure::ReadDenied
        | PathAuthorizationFailure::WriteDenied => {
            ToolError::PathOutsideWorkspace(requested_path.to_string())
        }
    }
}
