//! Bubblewrap transformation adapted from Codex linux-sandbox/bwrap.rs.

use crate::ExecutionError;
use crate::FilesystemPolicy;
use crate::NetworkPolicy;
use crate::SandboxBackend;
use crate::manager::ResolvedSandboxPolicy;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use tokio::process::Command;

const PLATFORM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/etc",
    "/lib",
    "/lib64",
    "/nix/store",
    "/run/current-system/sw",
];

pub(crate) fn prepare(
    program: OsString,
    args: Vec<OsString>,
    cwd: &Path,
    policy: &ResolvedSandboxPolicy,
    _environment: &HashMap<OsString, OsString>,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    if is_wsl1() {
        return Err(ExecutionError::SandboxUnavailable(
            "bubblewrap requires WSL2; WSL1 cannot create user namespaces".to_string(),
        ));
    }
    let bwrap = find_bwrap(&policy.write_roots).ok_or_else(|| {
        ExecutionError::SandboxUnavailable(
            "bubblewrap was not found on a trusted PATH entry".to_string(),
        )
    })?;
    let mut command = Command::new(bwrap);
    command.args([
        "--new-session",
        "--die-with-parent",
        "--unshare-user",
        "--unshare-pid",
    ]);
    if policy.network == NetworkPolicy::Deny {
        command.arg("--unshare-net");
    }
    if policy.filesystem_read == FilesystemPolicy::Host {
        if policy.filesystem_write == FilesystemPolicy::Host {
            command.args(["--bind", "/", "/"]);
        } else {
            command.args(["--ro-bind", "/", "/"]);
        }
    } else {
        command.args(["--tmpfs", "/"]);
        for root in PLATFORM_READ_ROOTS
            .iter()
            .map(Path::new)
            .filter(|root| root.exists())
        {
            append_bind(&mut command, "--ro-bind", root);
        }
        for root in &policy.read_roots {
            if root.exists() {
                append_bind(&mut command, "--ro-bind", root);
            }
        }
    }
    command.args(["--dev", "/dev", "--proc", "/proc"]);
    if policy.filesystem_write == FilesystemPolicy::Scoped {
        for root in &policy.write_roots {
            if root.exists() {
                append_bind(&mut command, "--bind", root);
            }
        }
    }
    for root in &policy.read_only_roots {
        if root.exists() {
            append_bind(&mut command, "--ro-bind", root);
        }
    }
    for path in policy
        .unreadable_roots
        .iter()
        .chain(policy.sensitive_paths.iter())
    {
        append_mask(&mut command, path);
    }
    command
        .arg("--chdir")
        .arg(cwd)
        .arg("--")
        .arg(program)
        .args(args);
    Ok((command, SandboxBackend::LinuxBubblewrap))
}

fn append_bind(command: &mut Command, option: &str, path: &Path) {
    command.arg(option).arg(path).arg(path);
}

fn append_mask(command: &mut Command, path: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        command
            .arg("--tmpfs")
            .arg(path)
            .arg("--remount-ro")
            .arg(path);
    } else {
        command.arg("--ro-bind").arg("/dev/null").arg(path);
    }
}

fn find_bwrap(write_roots: &[PathBuf]) -> Option<PathBuf> {
    let search_path = std::env::var_os("PATH")?;
    std::env::split_paths(&search_path)
        .filter(|directory| directory.is_absolute())
        .filter_map(|directory| std::fs::canonicalize(directory).ok())
        .filter(|directory| !write_roots.iter().any(|root| directory.starts_with(root)))
        .map(|directory| directory.join("bwrap"))
        .find(|candidate| candidate.is_file())
}

fn is_wsl1() -> bool {
    std::fs::read_to_string("/proc/version").is_ok_and(|version| {
        let version = version.to_ascii_lowercase();
        version.contains("microsoft") && !version.contains("microsoft-standard")
    })
}
