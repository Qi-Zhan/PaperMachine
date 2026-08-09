use crate::ExecutionError;
use crate::SandboxBackend;
use crate::configure_process_group;
use crate::environment::child_environment;
use crate::platform;
use crate::policy::SandboxPolicy;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use tokio::process::Command;

const MAX_SENSITIVE_SCAN_ENTRIES: usize = 250_000;
const MAX_SENSITIVE_MATCHES: usize = 8_192;

pub struct SandboxRequest {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub sandbox_root: PathBuf,
    pub policy: SandboxPolicy,
    pub environment_overrides: Vec<(OsString, OsString)>,
}

impl SandboxRequest {
    pub fn new(
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = OsString>,
        cwd: impl Into<PathBuf>,
        sandbox_root: impl Into<PathBuf>,
        policy: SandboxPolicy,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            cwd: cwd.into(),
            sandbox_root: sandbox_root.into(),
            policy,
            environment_overrides: Vec::new(),
        }
    }

    pub fn with_environment_override(
        mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Self {
        self.environment_overrides.push((key.into(), value.into()));
        self
    }
}

pub struct PreparedSandboxCommand {
    command: Command,
    backend: SandboxBackend,
}

impl PreparedSandboxCommand {
    pub const fn backend(&self) -> SandboxBackend {
        self.backend
    }

    pub fn into_command(self) -> Command {
        self.command
    }
}

#[derive(Clone, Copy, Default)]
pub struct SandboxManager;

impl SandboxManager {
    pub async fn prepare(
        &self,
        request: SandboxRequest,
    ) -> Result<PreparedSandboxCommand, ExecutionError> {
        let cwd = tokio::fs::canonicalize(&request.cwd).await?;
        if !cwd.is_dir() {
            return Err(ExecutionError::InvalidWorkspace(cwd.display().to_string()));
        }
        tokio::fs::create_dir_all(&request.sandbox_root).await?;
        let sandbox_root = tokio::fs::canonicalize(&request.sandbox_root).await?;
        let sandbox_home = sandbox_root.join("home");
        let sandbox_tmp = sandbox_root.join("tmp");
        tokio::fs::create_dir_all(&sandbox_home).await?;
        tokio::fs::create_dir_all(&sandbox_tmp).await?;

        let policy = tokio::task::spawn_blocking({
            let program = request.program.clone();
            let sandbox_root = sandbox_root.clone();
            move || resolve_policy(request.policy, &program, &sandbox_root)
        })
        .await
        .map_err(|error| ExecutionError::InvalidPolicy(error.to_string()))??;
        let environment = child_environment(
            &policy.environment,
            &sandbox_home,
            &sandbox_tmp,
            &request.environment_overrides,
        );
        let (mut command, backend) =
            platform::prepare(request.program, request.args, &cwd, &policy, &environment)?;
        command.current_dir(&cwd).env_clear().envs(environment);
        configure_process_group(&mut command);
        Ok(PreparedSandboxCommand { command, backend })
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) struct ResolvedSandboxPolicy {
    pub filesystem_read: crate::FilesystemPolicy,
    pub filesystem_write: crate::FilesystemPolicy,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub unreadable_roots: Vec<PathBuf>,
    pub read_only_roots: Vec<PathBuf>,
    pub sensitive_paths: Vec<PathBuf>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub sensitive_path_names: Vec<String>,
    pub network: crate::NetworkPolicy,
    pub environment: papermachine_protocol::EnvironmentAuthorization,
    pub requires_platform_sandbox: bool,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub platform_state_root: PathBuf,
}

fn resolve_policy(
    policy: SandboxPolicy,
    program: &OsString,
    sandbox_root: &Path,
) -> Result<ResolvedSandboxPolicy, ExecutionError> {
    let requires_platform_sandbox = policy.requires_platform_sandbox();
    let mut read_roots = resolve_paths(policy.read_roots)?;
    let mut write_roots = resolve_paths(policy.write_roots)?;
    let workspace_roots = resolve_paths(policy.workspace_roots)?;
    let unreadable_roots = resolve_paths(policy.unreadable_roots)?;
    let read_only_roots = resolve_paths(policy.read_only_roots)?;
    let sandbox_root = resolve_path(sandbox_root)?;
    read_roots.push(sandbox_root.clone());
    write_roots.push(sandbox_root.clone());
    read_roots.extend(executable_read_roots(program));
    read_roots.extend(path_read_roots());
    deduplicate_paths(&mut read_roots);
    deduplicate_paths(&mut write_roots);

    let sensitive_names = policy.sensitive_path_names;
    let sensitive_paths = scan_sensitive_paths(&workspace_roots, &sensitive_names)?;
    let platform_state_root = unreadable_roots
        .first()
        .map(|root| root.join("runtime/windows-sandbox"))
        .unwrap_or_else(|| sandbox_root.join("windows-sandbox"));
    Ok(ResolvedSandboxPolicy {
        filesystem_read: policy.filesystem_read,
        filesystem_write: policy.filesystem_write,
        read_roots,
        write_roots,
        unreadable_roots,
        read_only_roots,
        sensitive_paths,
        sensitive_path_names: sensitive_names,
        network: policy.network,
        environment: policy.environment,
        requires_platform_sandbox,
        platform_state_root,
    })
}

fn resolve_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, ExecutionError> {
    paths.into_iter().map(|path| resolve_path(&path)).collect()
}

fn resolve_path(path: &Path) -> Result<PathBuf, ExecutionError> {
    if !path.is_absolute() {
        return Err(ExecutionError::InvalidPolicy(format!(
            "sandbox path must be absolute: {}",
            path.display()
        )));
    }
    let normalized = normalize_absolute(path).ok_or_else(|| {
        ExecutionError::InvalidPolicy(format!("invalid sandbox path: {}", path.display()))
    })?;
    let mut existing = normalized.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            ExecutionError::InvalidPolicy(format!(
                "sandbox path has no existing ancestor: {}",
                path.display()
            ))
        })?;
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            ExecutionError::InvalidPolicy(format!(
                "sandbox path has no existing ancestor: {}",
                path.display()
            ))
        })?;
    }
    let mut resolved = std::fs::canonicalize(existing)?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
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

fn executable_read_roots(program: &OsString) -> Vec<PathBuf> {
    let path = PathBuf::from(program);
    if !path.is_absolute() || !path.exists() {
        return Vec::new();
    }
    let canonical = std::fs::canonicalize(path).ok();
    let Some(parent) = canonical.as_deref().and_then(Path::parent) else {
        return Vec::new();
    };
    let root = parent
        .parent()
        .filter(|candidate| candidate.parent().is_some())
        .unwrap_or(parent);
    vec![root.to_path_buf()]
}

fn path_read_roots() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| path.is_absolute() && path.is_dir())
                .filter_map(|path| std::fs::canonicalize(path).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

fn scan_sensitive_paths(
    roots: &[PathBuf],
    names: &[String],
) -> Result<Vec<PathBuf>, ExecutionError> {
    if roots.is_empty() || names.is_empty() {
        return Ok(Vec::new());
    }
    let mut matches = BTreeSet::new();
    let mut stack = roots.to_vec();
    let mut visited = 0_usize;
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)?;
        for entry in entries {
            let entry = entry?;
            visited += 1;
            if visited > MAX_SENSITIVE_SCAN_ENTRIES {
                return Err(ExecutionError::InvalidPolicy(format!(
                    "credential-path scan exceeded {MAX_SENSITIVE_SCAN_ENTRIES} entries"
                )));
            }
            let file_type = entry.file_type()?;
            let path = entry.path();
            let name = entry.file_name();
            if sensitive_name(&name.to_string_lossy(), names) {
                matches.insert(resolve_path(&path)?);
                if matches.len() > MAX_SENSITIVE_MATCHES {
                    return Err(ExecutionError::InvalidPolicy(format!(
                        "credential-path scan matched more than {MAX_SENSITIVE_MATCHES} paths"
                    )));
                }
                continue;
            }
            if file_type.is_dir() && !file_type.is_symlink() {
                stack.push(path);
            }
        }
    }
    Ok(matches.into_iter().collect())
}

fn sensitive_name(name: &str, names: &[String]) -> bool {
    name == ".env" || name.starts_with(".env.") || names.iter().any(|candidate| candidate == name)
}
