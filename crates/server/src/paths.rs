use anyhow::Context;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Platform {
    MacOs,
    Linux,
    Windows,
}

pub fn default_data_dir() -> anyhow::Result<PathBuf> {
    let platform = if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    };
    resolve_data_dir(
        platform,
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
    )
    .context("could not determine the PaperMachine user data directory")
}

fn resolve_data_dir(
    platform: Platform,
    home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    local_app_data: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    match platform {
        Platform::MacOs => Ok(home
            .context("HOME is not set")?
            .join("Library/Application Support/PaperMachine")),
        Platform::Linux => Ok(match xdg_data_home {
            Some(root) => root.join("papermachine"),
            None => home
                .context("neither XDG_DATA_HOME nor HOME is set")?
                .join(".local/share/papermachine"),
        }),
        Platform::Windows => Ok(local_app_data
            .context("LOCALAPPDATA is not set")?
            .join("PaperMachine")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_application_support() {
        assert_eq!(
            resolve_data_dir(
                Platform::MacOs,
                Some(PathBuf::from("/Users/researcher")),
                None,
                None,
            )
            .expect("macOS data directory should resolve"),
            PathBuf::from("/Users/researcher/Library/Application Support/PaperMachine")
        );
    }

    #[test]
    fn linux_prefers_xdg_and_falls_back_to_local_share() {
        assert_eq!(
            resolve_data_dir(
                Platform::Linux,
                Some(PathBuf::from("/home/researcher")),
                Some(PathBuf::from("/data")),
                None,
            )
            .expect("XDG data directory should resolve"),
            PathBuf::from("/data/papermachine")
        );
        assert_eq!(
            resolve_data_dir(
                Platform::Linux,
                Some(PathBuf::from("/home/researcher")),
                None,
                None,
            )
            .expect("Linux fallback data directory should resolve"),
            PathBuf::from("/home/researcher/.local/share/papermachine")
        );
    }

    #[test]
    fn windows_uses_local_app_data() {
        assert_eq!(
            resolve_data_dir(
                Platform::Windows,
                None,
                None,
                Some(PathBuf::from("C:/Users/researcher/AppData/Local")),
            )
            .expect("Windows data directory should resolve"),
            PathBuf::from("C:/Users/researcher/AppData/Local/PaperMachine")
        );
    }
}
