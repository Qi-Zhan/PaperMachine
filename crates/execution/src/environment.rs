use papermachine_protocol::EnvironmentAuthorization;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

#[cfg(not(target_os = "windows"))]
const CORE_VARIABLES: &[&str] = &[
    "PATH", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "USER",
];

#[cfg(target_os = "windows")]
const CORE_VARIABLES: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SHELL",
    "COMSPEC",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "USERNAME",
    "USERDOMAIN",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PROGRAMDATA",
    "POWERSHELL",
    "PWSH",
];

pub(crate) fn child_environment(
    policy: &EnvironmentAuthorization,
    sandbox_home: &Path,
    sandbox_tmp: &Path,
    overrides: &[(OsString, OsString)],
) -> HashMap<OsString, OsString> {
    let mut environment = HashMap::new();
    if policy.inherit_core {
        for (key, value) in std::env::vars_os() {
            let key_text = key.to_string_lossy();
            if CORE_VARIABLES
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&key_text))
                && !denied_name(&key_text, &policy.deny_name_fragments)
            {
                insert_case_insensitive(&mut environment, key, value);
            }
        }
    }

    insert_case_insensitive(
        &mut environment,
        OsString::from("HOME"),
        sandbox_home.as_os_str().to_owned(),
    );
    insert_case_insensitive(
        &mut environment,
        OsString::from("TMPDIR"),
        sandbox_tmp.as_os_str().to_owned(),
    );
    insert_case_insensitive(
        &mut environment,
        OsString::from("TMP"),
        sandbox_tmp.as_os_str().to_owned(),
    );
    insert_case_insensitive(
        &mut environment,
        OsString::from("TEMP"),
        sandbox_tmp.as_os_str().to_owned(),
    );
    #[cfg(unix)]
    insert_case_insensitive(
        &mut environment,
        OsString::from("TMPPREFIX"),
        sandbox_tmp.join("shell").into_os_string(),
    );
    #[cfg(target_os = "windows")]
    insert_case_insensitive(
        &mut environment,
        OsString::from("USERPROFILE"),
        sandbox_home.as_os_str().to_owned(),
    );
    if !environment
        .keys()
        .any(|key| key.to_string_lossy().eq_ignore_ascii_case("LANG"))
    {
        environment.insert(OsString::from("LANG"), OsString::from("C.UTF-8"));
    }
    for (key, value) in overrides {
        if !denied_name(&key.to_string_lossy(), &policy.deny_name_fragments) {
            insert_case_insensitive(&mut environment, key.clone(), value.clone());
        }
    }
    environment
}

fn denied_name(name: &str, deny_fragments: &[String]) -> bool {
    let name = name.to_ascii_uppercase();
    deny_fragments
        .iter()
        .any(|fragment| name.contains(&fragment.to_ascii_uppercase()))
}

fn insert_case_insensitive(
    environment: &mut HashMap<OsString, OsString>,
    key: OsString,
    value: OsString,
) {
    if let Some(existing) = environment
        .keys()
        .find(|existing| {
            existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&key.to_string_lossy())
        })
        .cloned()
    {
        environment.remove(&existing);
    }
    environment.insert(key, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_secret_override_is_rejected() {
        let root = Path::new("/sandbox");
        let policy = EnvironmentAuthorization {
            inherit_core: false,
            deny_name_fragments: vec!["TOKEN".to_string(), "KEY".to_string()],
        };
        let environment = child_environment(
            &policy,
            root,
            root,
            &[
                (OsString::from("API_TOKEN"), OsString::from("secret")),
                (OsString::from("SAFE"), OsString::from("yes")),
            ],
        );
        assert!(!environment.contains_key(&OsString::from("API_TOKEN")));
        assert_eq!(
            environment.get(&OsString::from("SAFE")),
            Some(&OsString::from("yes"))
        );
    }

    #[test]
    fn inherited_environment_is_core_only_and_secret_free() {
        let root = Path::new("/sandbox");
        let policy = EnvironmentAuthorization {
            inherit_core: true,
            deny_name_fragments: vec!["KEY".to_string(), "SECRET".to_string(), "TOKEN".to_string()],
        };
        let environment = child_environment(&policy, root, root, &[]);
        assert!(environment.keys().all(|key| {
            let key = key.to_string_lossy().to_ascii_uppercase();
            !key.contains("KEY") && !key.contains("SECRET") && !key.contains("TOKEN")
        }));
        if let Some(host_path) = std::env::var_os("PATH") {
            assert_eq!(environment.get(&OsString::from("PATH")), Some(&host_path));
        }
    }
}
