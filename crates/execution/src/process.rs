use std::process::Stdio;
use tokio::process::Child;
use tokio::process::Command;

#[cfg(unix)]
pub fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(target_os = "windows")]
pub fn configure_process_group(command: &mut Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, target_os = "windows")))]
pub fn configure_process_group(_command: &mut Command) {}

pub async fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(process_id) = child.id() {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &format!("-{process_id}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(target_os = "windows")]
    if let Some(process_id) = child.id() {
        let _ = Command::new("taskkill.exe")
            .args(["/PID", &process_id.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
