//! Seatbelt request transformation adapted from Codex sandboxing/seatbelt.rs.

use crate::ExecutionError;
use crate::FilesystemPolicy;
use crate::NetworkPolicy;
use crate::SandboxBackend;
use crate::manager::ResolvedSandboxPolicy;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use tokio::process::Command;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub(crate) fn prepare(
    program: OsString,
    args: Vec<OsString>,
    _cwd: &Path,
    policy: &ResolvedSandboxPolicy,
    _environment: &HashMap<OsString, OsString>,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    if !Path::new(SANDBOX_EXEC).is_file() {
        return Err(ExecutionError::SandboxUnavailable(
            "macOS sandbox-exec is not installed".to_string(),
        ));
    }
    let profile = seatbelt_profile(policy);
    let mut command = Command::new(SANDBOX_EXEC);
    command.arg("-p").arg(profile).arg(program).args(args);
    Ok((command, SandboxBackend::MacOsSeatbelt))
}

fn seatbelt_profile(policy: &ResolvedSandboxPolicy) -> String {
    let mut rules = vec!["(version 1)".to_string(), "(allow default)".to_string()];
    if policy.network == NetworkPolicy::Deny {
        rules.push("(deny network*)".to_string());
    }
    if policy.filesystem_write == FilesystemPolicy::Scoped {
        rules.push("(deny file-write*)".to_string());
        for root in &policy.write_roots {
            rules.push(format!(
                "(allow file-write* (subpath \"{}\"))",
                seatbelt_literal(root)
            ));
        }
        rules.push("(allow file-write* (literal \"/dev/null\"))".to_string());
    }
    if policy.filesystem_read == FilesystemPolicy::Scoped {
        let mut denied = BTreeSet::from([
            "/Volumes".to_string(),
            "/private/tmp".to_string(),
            "/tmp".to_string(),
            "/var/tmp".to_string(),
            "/var/folders".to_string(),
            "/private/var/folders".to_string(),
        ]);
        if let Some(home) = std::env::var_os("HOME") {
            denied.insert(seatbelt_literal(Path::new(&home)));
        }
        for root in denied {
            rules.push(format!("(deny file-read* (subpath \"{root}\"))"));
        }
        for root in &policy.read_roots {
            rules.push(format!(
                "(allow file-read* (subpath \"{}\"))",
                seatbelt_literal(root)
            ));
        }
    }
    for root in &policy.unreadable_roots {
        let root = seatbelt_literal(root);
        rules.push(format!("(deny file-read* (subpath \"{root}\"))"));
        rules.push(format!("(deny file-write* (subpath \"{root}\"))"));
    }
    for root in &policy.read_only_roots {
        rules.push(format!(
            "(deny file-write* (subpath \"{}\"))",
            seatbelt_literal(root)
        ));
    }
    for path in &policy.sensitive_paths {
        let path = seatbelt_literal(path);
        rules.push(format!("(deny file-read* (subpath \"{path}\"))"));
        rules.push(format!("(deny file-write* (subpath \"{path}\"))"));
    }
    if let Some(regex) = sensitive_path_regex(&policy.sensitive_path_names) {
        rules.push(format!("(deny file-read* (regex #\"{regex}\"))"));
        rules.push(format!("(deny file-write* (regex #\"{regex}\"))"));
    }
    rules.join("\n")
}

fn sensitive_path_regex(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let mut alternatives = BTreeSet::new();
    for name in names {
        if name == ".env" {
            alternatives.insert(r"\.env(\.[^/]*)?".to_string());
        } else {
            alternatives.insert(regex_escape(name));
        }
    }
    Some(format!(
        "^/(.*/)?({})(/.*)?$",
        alternatives.into_iter().collect::<Vec<_>>().join("|")
    ))
}

fn regex_escape(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if matches!(
            character,
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '*' | '?' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.replace('"', "\\\"")
}

fn seatbelt_literal(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermachine_protocol::EnvironmentAuthorization;

    #[test]
    fn profile_contains_managed_metadata_and_credential_denies() {
        let policy = ResolvedSandboxPolicy {
            filesystem_read: FilesystemPolicy::Scoped,
            filesystem_write: FilesystemPolicy::Scoped,
            read_roots: vec!["/workspace".into()],
            write_roots: vec!["/workspace".into()],
            unreadable_roots: vec!["/managed".into()],
            read_only_roots: vec!["/workspace/.git".into()],
            sensitive_paths: vec!["/workspace/.env".into()],
            sensitive_path_names: vec![".env".to_string(), ".npmrc".to_string()],
            network: NetworkPolicy::Deny,
            environment: EnvironmentAuthorization {
                inherit_core: true,
                deny_name_fragments: vec![],
            },
            requires_platform_sandbox: true,
            platform_state_root: "/managed/runtime/windows-sandbox".into(),
        };
        let profile = seatbelt_profile(&policy);
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("/managed"));
        assert!(profile.contains("/workspace/.git"));
        assert!(profile.contains(r"\.env(\.[^/]*)?"));
    }
}
