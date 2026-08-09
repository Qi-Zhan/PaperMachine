//! Materialized filesystem, tool, and network authorization.
//!
//! The split between user-facing presets and a concrete per-Turn policy is
//! adapted from OpenAI Codex `codex-rs/protocol/src/permissions.rs` at commit
//! `b2dc8b3e4be4fe3a453d50e13835f707b258f15b`. PaperMachine keeps a smaller
//! research-specific policy surface and adds an immutable managed-state deny.

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
const SENSITIVE_WORKSPACE_FILES: [&str; 3] = [".git-credentials", ".npmrc", ".pypirc"];

/// Stable user-facing choices. These values order Workflow ceilings, but all
/// enforcement consumes [`AuthorizationContext`] instead of this preset.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AccessPreset {
    ModelOnly,
    ReadOnly,
    Workspace,
    Research,
    FullAccess,
}

impl AccessPreset {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelOnly => "model_only",
            Self::ReadOnly => "read_only",
            Self::Workspace => "workspace",
            Self::Research => "research",
            Self::FullAccess => "full_access",
        }
    }

    pub fn tool_capabilities(self) -> ToolCapabilities {
        match self {
            Self::ModelOnly => ToolCapabilities::default(),
            Self::ReadOnly => ToolCapabilities {
                read_file: true,
                ..ToolCapabilities::default()
            },
            Self::Workspace => ToolCapabilities {
                read_file: true,
                write_file: true,
                exec_command: true,
                fetch_url: false,
            },
            Self::Research | Self::FullAccess => ToolCapabilities {
                read_file: true,
                write_file: true,
                exec_command: true,
                fetch_url: true,
            },
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
    /// Credential-bearing filenames denied for reads and writes below full
    /// access. `.env` and `.env.*` are matched separately as a family.
    pub sensitive_workspace_files: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ToolCapabilities {
    pub read_file: bool,
    pub write_file: bool,
    pub exec_command: bool,
    pub fetch_url: bool,
}

impl ToolCapabilities {
    pub fn allows(&self, name: &str) -> bool {
        match name {
            "read_file" => self.read_file,
            "write_file" => self.write_file,
            "exec_command" => self.exec_command,
            "fetch_url" => self.fetch_url,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NetworkCapabilities {
    /// Network available to an untrusted child process.
    pub child_process: bool,
    /// Controlled Rust-owned HTTPS fetch tool.
    pub controlled_fetch: bool,
    /// Provider-hosted web search, further bounded by provider capability.
    pub hosted_web_search: bool,
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
    pub workspace_roots: Vec<String>,
    pub cwd: String,
    pub filesystem: FilesystemAuthorization,
    pub tools: ToolCapabilities,
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
    SensitiveWorkspacePath,
    ProtectedWorkspaceMetadata,
}

impl AuthorizationContext {
    pub fn materialize(
        preset: AccessPreset,
        workspace_roots: Vec<String>,
        cwd: String,
        managed_roots: Vec<String>,
    ) -> Result<Self, String> {
        validate_absolute_roots("Workspace", &workspace_roots)?;
        validate_absolute_roots("managed", &managed_roots)?;
        if !Path::new(&cwd).is_absolute() {
            return Err("Turn cwd must be absolute".to_string());
        }
        if !workspace_roots
            .iter()
            .any(|root| Path::new(&cwd).starts_with(root))
        {
            return Err("Turn cwd must stay inside a Workspace root".to_string());
        }

        let (read, write, tools, network) = match preset {
            AccessPreset::ModelOnly => (
                FilesystemScope::None,
                FilesystemScope::None,
                preset.tool_capabilities(),
                NetworkCapabilities::default(),
            ),
            AccessPreset::ReadOnly => (
                FilesystemScope::Workspace,
                FilesystemScope::None,
                preset.tool_capabilities(),
                NetworkCapabilities::default(),
            ),
            AccessPreset::Workspace => (
                FilesystemScope::Workspace,
                FilesystemScope::Workspace,
                preset.tool_capabilities(),
                NetworkCapabilities::default(),
            ),
            AccessPreset::Research => (
                FilesystemScope::Workspace,
                FilesystemScope::Workspace,
                preset.tool_capabilities(),
                NetworkCapabilities {
                    child_process: false,
                    controlled_fetch: true,
                    hosted_web_search: true,
                },
            ),
            AccessPreset::FullAccess => (
                FilesystemScope::Host,
                FilesystemScope::Host,
                preset.tool_capabilities(),
                NetworkCapabilities {
                    child_process: true,
                    controlled_fetch: true,
                    hosted_web_search: true,
                },
            ),
        };

        Ok(Self {
            preset,
            workspace_roots,
            cwd,
            filesystem: FilesystemAuthorization {
                read,
                write,
                managed_roots,
                read_only_workspace_metadata: PROTECTED_WORKSPACE_METADATA
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                sensitive_workspace_files: SENSITIVE_WORKSPACE_FILES
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            tools,
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
            FilesystemScope::Host => return Ok(()),
            FilesystemScope::Workspace => {}
        }

        let relative = self
            .workspace_roots
            .iter()
            .filter_map(|root| candidate.strip_prefix(root).ok())
            .min_by_key(|relative| relative.components().count())
            .ok_or(PathAuthorizationFailure::OutsideWorkspace)?;
        if is_sensitive_workspace_path(relative, &self.filesystem.sensitive_workspace_files) {
            return Err(PathAuthorizationFailure::SensitiveWorkspacePath);
        }
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
        let cwd = workspace
            .primary_path()
            .ok_or_else(|| "Workspace primary root is missing".to_string())?
            .to_string();
        let authorization = AuthorizationContext::materialize(
            preset,
            workspace.roots.clone(),
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

fn is_sensitive_workspace_path(path: &Path, exact_names: &[String]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(preset: AccessPreset) -> AuthorizationContext {
        AuthorizationContext::materialize(
            preset,
            vec!["/workspace".to_string()],
            "/workspace".to_string(),
            vec!["/managed/project".to_string()],
        )
        .expect("fixture policy should materialize")
    }

    #[test]
    fn workspace_policy_protects_metadata_credentials_and_managed_state() {
        let policy = policy(AccessPreset::Research);
        assert_eq!(
            policy.authorize_path(Path::new("/workspace/.git/config"), PathOperation::Write),
            Err(PathAuthorizationFailure::ProtectedWorkspaceMetadata)
        );
        assert_eq!(
            policy.authorize_path(
                Path::new("/workspace/config/.env.local"),
                PathOperation::Read
            ),
            Err(PathAuthorizationFailure::SensitiveWorkspacePath)
        );
        assert_eq!(
            policy.authorize_path(
                Path::new("/managed/project/state/project.db"),
                PathOperation::Read
            ),
            Err(PathAuthorizationFailure::ManagedState)
        );
        assert_eq!(
            policy.authorize_path(Path::new("/outside/secret"), PathOperation::Read),
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
            policy.authorize_path(
                Path::new("/managed/project/state/project.db"),
                PathOperation::Write
            ),
            Err(PathAuthorizationFailure::ManagedState)
        );
    }

    #[test]
    fn policy_hash_is_stable_and_sensitive_to_the_workspace() {
        let first = policy(AccessPreset::Research);
        let second = policy(AccessPreset::Research);
        let other = AuthorizationContext::materialize(
            AccessPreset::Research,
            vec!["/other".to_string()],
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
