use crate::DEFAULT_MAX_OUTPUT_BYTES;
use crate::ExecutionError;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::EnvironmentAuthorization;
use papermachine_protocol::FilesystemScope;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPolicy {
    Deny,
    Allow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemPolicy {
    Scoped,
    Host,
}

#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    pub filesystem_read: FilesystemPolicy,
    pub filesystem_write: FilesystemPolicy,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub workspace_roots: Vec<PathBuf>,
    pub unreadable_roots: Vec<PathBuf>,
    pub read_only_roots: Vec<PathBuf>,
    pub sensitive_path_names: Vec<String>,
    pub protected_workspace_metadata: Vec<String>,
    pub network: NetworkPolicy,
    pub environment: EnvironmentAuthorization,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl SandboxPolicy {
    pub fn from_authorization(
        authorization: &AuthorizationContext,
        timeout: Duration,
    ) -> Result<Self, ExecutionError> {
        let workspace_roots = vec![absolute_path(&authorization.workspace_root)?];
        let mut unreadable_roots = paths(&authorization.filesystem.managed_roots)?;
        unreadable_roots.extend(paths(&authorization.filesystem.sensitive_roots)?);
        let (filesystem_read, read_roots) =
            scope_roots(authorization.filesystem.read, &workspace_roots);
        let (filesystem_write, write_roots) =
            scope_roots(authorization.filesystem.write, &workspace_roots);
        let protects_workspace = filesystem_write != FilesystemPolicy::Host;
        let protects_credentials =
            authorization.preset != papermachine_protocol::AccessPreset::FullAccess;
        Ok(Self {
            filesystem_read,
            filesystem_write,
            read_roots,
            write_roots,
            workspace_roots: workspace_roots.clone(),
            unreadable_roots,
            read_only_roots: if protects_workspace {
                workspace_roots
                    .iter()
                    .flat_map(|root| {
                        authorization
                            .filesystem
                            .read_only_workspace_metadata
                            .iter()
                            .map(move |name| root.join(name))
                    })
                    .collect()
            } else {
                Vec::new()
            },
            sensitive_path_names: if protects_credentials {
                let mut names = authorization.filesystem.sensitive_path_names.clone();
                names.push(".env".to_string());
                names
            } else {
                Vec::new()
            },
            protected_workspace_metadata: if protects_workspace {
                authorization
                    .filesystem
                    .read_only_workspace_metadata
                    .clone()
            } else {
                Vec::new()
            },
            network: if authorization.network.child_process {
                NetworkPolicy::Allow
            } else {
                NetworkPolicy::Deny
            },
            environment: authorization.environment.clone(),
            timeout,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        })
    }

    pub fn workflow_runtime(workspace: &Path) -> Result<Self, ExecutionError> {
        if !workspace.is_absolute() {
            return Err(ExecutionError::InvalidPolicy(format!(
                "Workflow runtime root must be absolute: {}",
                workspace.display()
            )));
        }
        Ok(Self {
            filesystem_read: FilesystemPolicy::Scoped,
            filesystem_write: FilesystemPolicy::Scoped,
            read_roots: vec![workspace.to_path_buf()],
            write_roots: vec![workspace.to_path_buf()],
            workspace_roots: Vec::new(),
            unreadable_roots: Vec::new(),
            read_only_roots: Vec::new(),
            sensitive_path_names: Vec::new(),
            protected_workspace_metadata: Vec::new(),
            network: NetworkPolicy::Deny,
            environment: EnvironmentAuthorization {
                inherit_core: true,
                deny_name_fragments: ["KEY", "SECRET", "TOKEN"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            timeout: Duration::from_secs(60),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        })
    }

    pub fn unrestricted(timeout: Duration) -> Self {
        Self {
            filesystem_read: FilesystemPolicy::Host,
            filesystem_write: FilesystemPolicy::Host,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            workspace_roots: Vec::new(),
            unreadable_roots: Vec::new(),
            read_only_roots: Vec::new(),
            sensitive_path_names: Vec::new(),
            protected_workspace_metadata: Vec::new(),
            network: NetworkPolicy::Allow,
            environment: EnvironmentAuthorization {
                inherit_core: true,
                deny_name_fragments: ["KEY", "SECRET", "TOKEN"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            timeout,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    pub(crate) fn requires_platform_sandbox(&self) -> bool {
        self.filesystem_read != FilesystemPolicy::Host
            || self.filesystem_write != FilesystemPolicy::Host
            || self.network == NetworkPolicy::Deny
            || !self.unreadable_roots.is_empty()
            || !self.read_only_roots.is_empty()
            || !self.sensitive_path_names.is_empty()
    }
}

fn paths(values: &[String]) -> Result<Vec<PathBuf>, ExecutionError> {
    values.iter().map(|value| absolute_path(value)).collect()
}

fn absolute_path(value: &str) -> Result<PathBuf, ExecutionError> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ExecutionError::InvalidPolicy(format!(
            "sandbox root must be absolute: {value}"
        )))
    }
}

fn scope_roots(
    scope: FilesystemScope,
    workspace_roots: &[PathBuf],
) -> (FilesystemPolicy, Vec<PathBuf>) {
    match scope {
        FilesystemScope::None => (FilesystemPolicy::Scoped, Vec::new()),
        FilesystemScope::Workspace => (FilesystemPolicy::Scoped, workspace_roots.to_vec()),
        FilesystemScope::Host => (FilesystemPolicy::Host, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermachine_protocol::AccessPreset;

    #[test]
    fn materialized_authorization_maps_without_reinterpreting_the_preset() {
        let authorization = AuthorizationContext::materialize(
            AccessPreset::Research,
            "/workspace".to_string(),
            "/workspace".to_string(),
            vec!["/managed".to_string()],
        )
        .expect("authorization");
        let policy = SandboxPolicy::from_authorization(&authorization, Duration::from_secs(7))
            .expect("sandbox policy");
        assert_eq!(policy.filesystem_read, FilesystemPolicy::Host);
        assert_eq!(policy.write_roots, vec![PathBuf::from("/workspace")]);
        assert_eq!(
            policy.unreadable_roots.first(),
            Some(&PathBuf::from("/managed"))
        );
        assert!(
            policy
                .unreadable_roots
                .iter()
                .any(|path| path.ends_with(".ssh"))
        );
        assert_eq!(policy.network, NetworkPolicy::Deny);
        assert!(
            policy
                .read_only_roots
                .contains(&PathBuf::from("/workspace/.git"))
        );
        assert!(policy.sensitive_path_names.contains(&".env".to_string()));
    }
}
