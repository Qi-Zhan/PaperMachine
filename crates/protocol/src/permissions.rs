//! Materialized filesystem and child-process authorization.
//!
//! The split between user-facing presets and a concrete per-Turn policy is
//! adapted from OpenAI Codex `codex-rs/protocol/src/permissions.rs` at commit
//! `b2dc8b3e4be4fe3a453d50e13835f707b258f15b`. PaperMachine keeps a smaller
//! policy surface and adds an immutable managed-state deny.

use crate::WorkspaceAttachment;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const PROTECTED_WORKSPACE_METADATA: [&str; 3] = [".git", ".agents", ".codex"];
const SENSITIVE_PATH_NAMES: [&str; 7] = [
    ".git-credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "application_default_credentials.json",
    "credentials",
    "credentials.json",
];
const USER_CREDENTIAL_PATHS: [&str; 8] = [
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".config/gh",
];

/// Stable user-facing choices. These values order Session ceilings, but all
/// enforcement consumes [`AuthorizationContext`] instead of this preset.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AccessPreset {
    ModelOnly,
    ReadOnly,
    Workspace,
    FullAccess,
}

impl AccessPreset {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelOnly => "model_only",
            Self::ReadOnly => "read_only",
            Self::Workspace => "workspace",
            Self::FullAccess => "full_access",
        }
    }

    pub fn allows_local_tool(self, name: &str) -> bool {
        match self {
            Self::ModelOnly => false,
            Self::ReadOnly => matches!(name, "exec_command" | "write_stdin"),
            Self::Workspace | Self::FullAccess => {
                matches!(name, "exec_command" | "write_stdin" | "apply_patch")
            }
        }
    }
}

impl std::fmt::Display for AccessPreset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemScope {
    None,
    Workspace,
    Host,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct FilesystemAuthorization {
    pub read: FilesystemScope,
    pub write: FilesystemScope,
    /// Absolute PaperMachine-owned roots denied before every other rule.
    pub managed_roots: Vec<String>,
    /// Root-relative metadata directories that remain read-only below full
    /// access, matching Codex workspace-write defaults.
    pub read_only_workspace_metadata: Vec<String>,
    /// Credential-bearing path-component names denied below full access.
    /// `.env` and `.env.*` are matched separately as a family.
    pub sensitive_path_names: Vec<String>,
    /// Absolute user credential roots denied below full access.
    pub sensitive_roots: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NetworkCapabilities {
    /// Network available to an untrusted child process.
    pub child_process: bool,
}

/// Environment variables available to untrusted child processes. The runtime
/// starts from an empty environment, copies only platform core variables, then
/// applies the case-insensitive deny fragments before its own HOME/TMP
/// overrides. This follows Codex's core-inherit plus default-secret-exclude
/// model without importing its configuration surface.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct EnvironmentAuthorization {
    pub inherit_core: bool,
    pub deny_name_fragments: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AuthorizationContext {
    pub preset: AccessPreset,
    pub workspace_root: String,
    pub cwd: String,
    pub filesystem: FilesystemAuthorization,
    pub network: NetworkCapabilities,
    pub environment: EnvironmentAuthorization,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathOperation {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathAuthorizationFailure {
    InvalidPath,
    ManagedState,
    OutsideWorkspace,
    ReadDenied,
    WriteDenied,
    SensitivePath,
    ProtectedWorkspaceMetadata,
}

impl AuthorizationContext {
    pub fn materialize(
        preset: AccessPreset,
        workspace_root: String,
        cwd: String,
        managed_roots: Vec<String>,
    ) -> Result<Self, String> {
        validate_absolute_path("Workspace", &workspace_root)?;
        validate_absolute_roots("managed", &managed_roots)?;
        if !Path::new(&cwd).is_absolute() {
            return Err("Turn cwd must be absolute".to_string());
        }
        if !Path::new(&cwd).starts_with(&workspace_root) {
            return Err("Turn cwd must stay inside the Workspace".to_string());
        }

        let (read, write, network) = match preset {
            AccessPreset::ModelOnly => (
                FilesystemScope::None,
                FilesystemScope::None,
                NetworkCapabilities::default(),
            ),
            AccessPreset::ReadOnly => (
                FilesystemScope::Host,
                FilesystemScope::None,
                NetworkCapabilities::default(),
            ),
            AccessPreset::Workspace => (
                FilesystemScope::Host,
                FilesystemScope::Workspace,
                NetworkCapabilities::default(),
            ),
            AccessPreset::FullAccess => (
                FilesystemScope::Host,
                FilesystemScope::Host,
                NetworkCapabilities {
                    child_process: true,
                },
            ),
        };

        Ok(Self {
            preset,
            workspace_root,
            cwd,
            filesystem: FilesystemAuthorization {
                read,
                write,
                managed_roots,
                read_only_workspace_metadata: PROTECTED_WORKSPACE_METADATA
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                sensitive_path_names: SENSITIVE_PATH_NAMES
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                sensitive_roots: if preset == AccessPreset::FullAccess {
                    Vec::new()
                } else {
                    user_credential_roots()
                },
            },
            network,
            environment: EnvironmentAuthorization {
                inherit_core: true,
                deny_name_fragments: ["KEY", "SECRET", "TOKEN"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        })
    }

    pub fn policy_sha256(&self) -> Result<String, serde_json::Error> {
        let document = serde_json::to_vec(self)?;
        Ok(hex::encode(Sha256::digest(document)))
    }

    pub fn authorize_path(
        &self,
        path: &Path,
        operation: PathOperation,
    ) -> Result<(), PathAuthorizationFailure> {
        if !path.is_absolute() {
            return Err(PathAuthorizationFailure::InvalidPath);
        }
        let candidate = normalize_absolute(path).ok_or(PathAuthorizationFailure::InvalidPath)?;
        if self
            .filesystem
            .managed_roots
            .iter()
            .map(Path::new)
            .any(|root| candidate.starts_with(root))
        {
            return Err(PathAuthorizationFailure::ManagedState);
        }

        let scope = match operation {
            PathOperation::Read => self.filesystem.read,
            PathOperation::Write => self.filesystem.write,
        };
        match scope {
            FilesystemScope::None => {
                return Err(match operation {
                    PathOperation::Read => PathAuthorizationFailure::ReadDenied,
                    PathOperation::Write => PathAuthorizationFailure::WriteDenied,
                });
            }
            FilesystemScope::Host | FilesystemScope::Workspace => {}
        }

        if self.preset != AccessPreset::FullAccess
            && (is_sensitive_path(&candidate, &self.filesystem.sensitive_path_names)
                || self
                    .filesystem
                    .sensitive_roots
                    .iter()
                    .map(Path::new)
                    .any(|root| candidate.starts_with(root)))
        {
            return Err(PathAuthorizationFailure::SensitivePath);
        }

        if scope == FilesystemScope::Host {
            return Ok(());
        }
        let relative = candidate
            .strip_prefix(&self.workspace_root)
            .map_err(|_| PathAuthorizationFailure::OutsideWorkspace)?;
        if operation == PathOperation::Write
            && first_normal_component(relative).is_some_and(|component| {
                self.filesystem
                    .read_only_workspace_metadata
                    .iter()
                    .any(|name| name == component)
            })
        {
            return Err(PathAuthorizationFailure::ProtectedWorkspaceMetadata);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct TurnEnvironmentSnapshot {
    pub workspace: WorkspaceAttachment,
    pub cwd: String,
    pub authorization: AuthorizationContext,
    pub authorization_sha256: String,
}

impl TurnEnvironmentSnapshot {
    pub fn materialize(
        workspace: WorkspaceAttachment,
        managed_root: impl Into<String>,
        preset: AccessPreset,
    ) -> Result<Self, String> {
        workspace.validate()?;
        let cwd = workspace.path.clone();
        let authorization = AuthorizationContext::materialize(
            preset,
            workspace.path.clone(),
            cwd.clone(),
            vec![managed_root.into()],
        )?;
        let authorization_sha256 = authorization
            .policy_sha256()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            workspace,
            cwd,
            authorization,
            authorization_sha256,
        })
    }
}

fn validate_absolute_roots(label: &str, roots: &[String]) -> Result<(), String> {
    if roots.is_empty() {
        return Err(format!("{label} roots must not be empty"));
    }
    for root in roots {
        if !Path::new(root).is_absolute() {
            return Err(format!("{label} root must be absolute: {root}"));
        }
    }
    Ok(())
}

fn validate_absolute_path(label: &str, path: &str) -> Result<(), String> {
    if !Path::new(path).is_absolute() {
        return Err(format!("{label} path must be absolute: {path}"));
    }
    Ok(())
}

fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized.is_absolute().then_some(normalized)
}

fn first_normal_component(path: &Path) -> Option<&str> {
    path.components().find_map(|component| match component {
        Component::Normal(value) => value.to_str(),
        _ => None,
    })
}

fn is_sensitive_path(path: &Path, exact_names: &[String]) -> bool {
    path.components().any(|component| {
        let Component::Normal(value) = component else {
            return false;
        };
        let Some(name) = value.to_str() else {
            return true;
        };
        name == ".env"
            || name.starts_with(".env.")
            || exact_names.iter().any(|candidate| candidate == name)
    })
}

fn user_credential_roots() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    if !home.is_absolute() {
        return Vec::new();
    }
    USER_CREDENTIAL_PATHS
        .into_iter()
        .map(|relative| home.join(relative).to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(preset: AccessPreset) -> AuthorizationContext {
        AuthorizationContext::materialize(
            preset,
            "/workspace".to_string(),
            "/workspace".to_string(),
            vec!["/managed/project".to_string()],
        )
        .expect("fixture policy should materialize")
    }

    #[test]
    fn workspace_policy_protects_metadata_credentials_and_managed_state() {
        let policy = policy(AccessPreset::Workspace);
        assert_eq!(
            policy.authorize_path(Path::new("/workspace/.git/config"), PathOperation::Write),
            Err(PathAuthorizationFailure::ProtectedWorkspaceMetadata)
        );
        assert_eq!(
            policy.authorize_path(
                Path::new("/workspace/config/.env.local"),
                PathOperation::Read
            ),
            Err(PathAuthorizationFailure::SensitivePath)
        );
        assert_eq!(
            policy.authorize_path(
                Path::new("/managed/project/state/project.db"),
                PathOperation::Read
            ),
            Err(PathAuthorizationFailure::ManagedState)
        );
        assert_eq!(
            policy.authorize_path(Path::new("/outside/ordinary"), PathOperation::Read),
            Ok(())
        );
        assert_eq!(
            policy.authorize_path(Path::new("/outside/.env.local"), PathOperation::Read),
            Err(PathAuthorizationFailure::SensitivePath)
        );
        assert_eq!(
            policy.authorize_path(Path::new("/outside/file"), PathOperation::Write),
            Err(PathAuthorizationFailure::OutsideWorkspace)
        );
        assert_eq!(
            policy.authorize_path(Path::new("/workspace/src/lib.rs"), PathOperation::Write),
            Ok(())
        );
    }

    #[test]
    fn full_access_still_denies_managed_state() {
        let policy = policy(AccessPreset::FullAccess);
        assert_eq!(
            policy.authorize_path(Path::new("/outside/secret"), PathOperation::Write),
            Ok(())
        );
        assert_eq!(
            policy.authorize_path(Path::new("/outside/.env"), PathOperation::Read),
            Ok(())
        );
        assert_eq!(
            policy.authorize_path(
                Path::new("/managed/project/state/project.db"),
                PathOperation::Write
            ),
            Err(PathAuthorizationFailure::ManagedState)
        );
    }

    #[test]
    fn policy_hash_is_stable_and_sensitive_to_the_workspace() {
        let first = policy(AccessPreset::Workspace);
        let second = policy(AccessPreset::Workspace);
        let other = AuthorizationContext::materialize(
            AccessPreset::Workspace,
            "/other".to_string(),
            "/other".to_string(),
            vec!["/managed/project".to_string()],
        )
        .expect("other policy should materialize");
        assert_eq!(
            first
                .policy_sha256()
                .expect("first policy should serialize"),
            second
                .policy_sha256()
                .expect("second policy should serialize")
        );
        assert_ne!(
            first
                .policy_sha256()
                .expect("first policy should serialize"),
            other
                .policy_sha256()
                .expect("other policy should serialize")
        );
    }
}
