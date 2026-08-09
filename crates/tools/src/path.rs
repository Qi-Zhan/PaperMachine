use crate::ToolError;
use cap_fs_ext::FollowSymlinks;
use cap_fs_ext::OpenOptionsFollowExt;
use cap_fs_ext::OpenOptionsMaybeDirExt;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use cap_std::fs::OpenOptions;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::PathAuthorizationFailure;
use papermachine_protocol::PathOperation;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
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
    loop {
        match tokio::fs::symlink_metadata(existing).await {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .ok_or_else(|| ToolError::PathOutsideWorkspace(requested_path.to_string()))?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| ToolError::PathOutsideWorkspace(requested_path.to_string()))?;
            }
            Err(error) => return Err(ToolError::Io(error.to_string())),
        }
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

/// Read through directory handles after authorization. Every path component is
/// opened without following symlinks, so a post-authorization rename cannot
/// redirect the operation to another filesystem object.
pub(crate) fn read_resolved_file(
    path: &Path,
    max_bytes: usize,
) -> Result<(Vec<u8>, u64, bool), ToolError> {
    let (parent, name) = open_parent(path, false)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(false);
    let mut file = parent
        .open_with(&name, &options)
        .map_err(map_capability_error)?;
    let metadata = file.metadata().map_err(map_capability_error)?;
    if !metadata.is_file() {
        return Err(ToolError::Io(format!(
            "path is not a regular file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(max_bytes.min(metadata.len() as usize));
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(map_capability_error)?;
    let observed_size = bytes.len() as u64;
    let truncated = bytes.len() > max_bytes || metadata.len() > max_bytes as u64;
    bytes.truncate(max_bytes);
    let size = metadata.len().max(observed_size);
    Ok((bytes, size, truncated))
}

/// Write through the same no-follow directory walk used by reads.
pub(crate) fn write_resolved_file(
    path: &Path,
    content: &[u8],
    create_parents: bool,
) -> Result<(), ToolError> {
    let (parent, name) = open_parent(path, create_parents)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(false);
    let mut file = parent
        .open_with(&name, &options)
        .map_err(map_capability_error)?;
    file.write_all(content).map_err(map_capability_error)?;
    file.sync_all().map_err(map_capability_error)
}

fn open_parent(path: &Path, create_parents: bool) -> Result<(Dir, OsString), ToolError> {
    let (root, mut components) = split_absolute(path)?;
    let name = components
        .pop()
        .ok_or_else(|| ToolError::Io(format!("path has no filename: {}", path.display())))?;
    let mut directory =
        Dir::open_ambient_dir(root, ambient_authority()).map_err(map_capability_error)?;
    for component in components {
        directory = match open_child_dir(&directory, &component) {
            Ok(child) => child,
            Err(error) if create_parents && error.kind() == std::io::ErrorKind::NotFound => {
                match directory.create_dir(&component) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(map_capability_error(error)),
                }
                open_child_dir(&directory, &component).map_err(map_capability_error)?
            }
            Err(error) => return Err(map_capability_error(error)),
        };
    }
    Ok((directory, name))
}

fn open_child_dir(parent: &Dir, name: &OsString) -> std::io::Result<Dir> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!(
                "path component is not a directory: {}",
                Path::new(name).display()
            ),
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

fn split_absolute(path: &Path) -> Result<(PathBuf, Vec<OsString>), ToolError> {
    if !path.is_absolute() {
        return Err(ToolError::Io(format!(
            "authorized path is not absolute: {}",
            path.display()
        )));
    }
    let mut root = PathBuf::new();
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => root.push(component.as_os_str()),
            Component::Normal(name) => components.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(ToolError::Io(format!(
                    "authorized path is not normalized: {}",
                    path.display()
                )));
            }
        }
    }
    Ok((root, components))
}

fn map_capability_error(error: std::io::Error) -> ToolError {
    ToolError::Io(error.to_string())
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[tokio::test]
    async fn authorized_path_cannot_be_redirected_by_a_later_symlink_swap() {
        let fixture = tempdir().expect("fixture should be created");
        let workspace = fixture.path().join("workspace");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(workspace.join("slot")).expect("slot should be created");
        std::fs::create_dir_all(&outside).expect("outside should be created");
        std::fs::write(workspace.join("slot/value.txt"), "inside")
            .expect("inside fixture should be written");
        std::fs::write(outside.join("value.txt"), "outside")
            .expect("outside fixture should be written");
        let workspace = workspace
            .canonicalize()
            .expect("workspace should canonicalize");
        let authorization = AuthorizationContext::materialize(
            papermachine_protocol::AccessPreset::Research,
            vec![workspace.to_string_lossy().into_owned()],
            workspace.to_string_lossy().into_owned(),
            vec![
                fixture
                    .path()
                    .join("managed")
                    .to_string_lossy()
                    .into_owned(),
            ],
        )
        .expect("authorization should materialize");
        let resolved = resolve_tool_path(&authorization, "slot/value.txt", PathOperation::Read)
            .await
            .expect("initial path should authorize");

        std::fs::rename(workspace.join("slot"), workspace.join("old-slot"))
            .expect("slot should move");
        symlink(&outside, workspace.join("slot")).expect("redirecting symlink should be created");

        assert!(read_resolved_file(&resolved, 1024).is_err());
    }
}
