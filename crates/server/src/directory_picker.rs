use std::path::PathBuf;
use std::process::Command;

pub fn pick_directory() -> Result<Option<PathBuf>, String> {
    platform_picker().and_then(normalize_selection)
}

fn normalize_selection(selection: Option<String>) -> Result<Option<PathBuf>, String> {
    let Some(selection) = selection else {
        return Ok(None);
    };
    let path = PathBuf::from(selection.trim());
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Selected directory is unavailable: {error}"))?;
    if !canonical.is_dir() {
        return Err("Selected path is not a directory".to_string());
    }
    Ok(Some(canonical))
}

#[cfg(target_os = "macos")]
fn platform_picker() -> Result<Option<String>, String> {
    let script = r#"try
set selectedFolder to choose folder with prompt "Choose a Workspace folder"
return POSIX path of selectedFolder
on error number -128
return ""
end try"#;
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", script])
        .output()
        .map_err(|error| format!("Could not open the directory picker: {error}"))?;
    command_selection(output)
}

#[cfg(target_os = "linux")]
fn platform_picker() -> Result<Option<String>, String> {
    match Command::new("zenity")
        .args([
            "--file-selection",
            "--directory",
            "--title=Choose a Workspace folder",
        ])
        .output()
    {
        Ok(output) => dialog_selection(output),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match Command::new("kdialog")
                .args(["--getexistingdirectory", "."])
                .output()
            {
                Ok(output) => dialog_selection(output),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(
                    "No native directory picker is available; enter the path manually".to_string(),
                ),
                Err(error) => Err(format!("Could not open the directory picker: {error}")),
            }
        }
        Err(error) => Err(format!("Could not open the directory picker: {error}")),
    }
}

#[cfg(target_os = "windows")]
fn platform_picker() -> Result<Option<String>, String> {
    let script = r#"Add-Type -AssemblyName System.Windows.Forms
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog
$dialog.Description = 'Choose a Workspace folder'
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    [Console]::Out.Write($dialog.SelectedPath)
}"#;
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|error| format!("Could not open the directory picker: {error}"))?;
    command_selection(output)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_picker() -> Result<Option<String>, String> {
    Err("Native directory selection is unavailable on this platform".to_string())
}

#[cfg(target_os = "linux")]
fn dialog_selection(output: std::process::Output) -> Result<Option<String>, String> {
    if output.status.success() {
        return output_text(output.stdout).map(Some);
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    command_error(output.stderr)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn command_selection(output: std::process::Output) -> Result<Option<String>, String> {
    if output.status.success() {
        let selection = output_text(output.stdout)?;
        return Ok((!selection.trim().is_empty()).then_some(selection));
    }
    command_error(output.stderr)
}

fn output_text(output: Vec<u8>) -> Result<String, String> {
    String::from_utf8(output)
        .map(|value| value.trim().to_string())
        .map_err(|_| "The directory picker returned an invalid path".to_string())
}

fn command_error(stderr: Vec<u8>) -> Result<Option<String>, String> {
    let message = String::from_utf8_lossy(&stderr).trim().to_string();
    Err(if message.is_empty() {
        "The directory picker failed".to_string()
    } else {
        format!("The directory picker failed: {message}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn normalization_returns_canonical_directories_and_preserves_cancel() {
        let directory = tempdir().expect("temporary directory should be created");
        assert_eq!(
            normalize_selection(Some(directory.path().to_string_lossy().into_owned()))
                .expect("directory should normalize"),
            Some(
                directory
                    .path()
                    .canonicalize()
                    .expect("temporary directory should canonicalize")
            )
        );
        assert_eq!(
            normalize_selection(None).expect("cancel should be accepted"),
            None
        );
        assert_eq!(
            normalize_selection(Some("  ".to_string())).expect("empty output is cancel"),
            None
        );
    }
}
