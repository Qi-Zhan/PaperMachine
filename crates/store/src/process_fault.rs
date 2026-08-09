//! Debug-build process fault injection for durability integration tests.
//!
//! Release builds compile every boundary to a no-op. A debug server may arm
//! exactly one named boundary before it starts serving; reaching that boundary
//! creates a marker and parks the current thread until an external test process
//! terminates the server.

pub const ROLLOUT_APPENDED_BEFORE_PROJECTION: &str = "rollout_appended_before_projection";
pub const FUNCTION_CALL_COMMITTED_BEFORE_DISPATCH: &str = "function_call_committed_before_dispatch";
pub const TURN_TERMINAL_CHECKPOINTED_BEFORE_COMMIT: &str =
    "turn_terminal_checkpointed_before_commit";

#[cfg(debug_assertions)]
mod debug {
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    struct ProcessFault {
        boundary: String,
        marker: PathBuf,
        triggered: AtomicBool,
    }

    static PROCESS_FAULT: OnceLock<ProcessFault> = OnceLock::new();

    pub fn install(boundary: String, marker: PathBuf) -> Result<(), String> {
        if boundary.trim().is_empty() {
            return Err("process fault boundary must not be empty".to_string());
        }
        if !marker.is_absolute() {
            return Err("process fault marker must be absolute".to_string());
        }
        PROCESS_FAULT
            .set(ProcessFault {
                boundary,
                marker,
                triggered: AtomicBool::new(false),
            })
            .map_err(|_| "process fault injection was already installed".to_string())
    }

    pub fn reach(boundary: &str) {
        let Some(fault) = PROCESS_FAULT.get() else {
            return;
        };
        if fault.boundary != boundary || fault.triggered.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(parent) = fault.marker.parent() {
            std::fs::create_dir_all(parent)
                .expect("process fault marker parent should be creatable");
        }
        std::fs::write(&fault.marker, format!("{boundary}\n"))
            .expect("process fault marker should be writable");
        loop {
            std::thread::park_timeout(Duration::from_secs(60));
        }
    }
}

#[cfg(debug_assertions)]
pub fn install_process_fault_injection(
    boundary: impl Into<String>,
    marker: impl Into<std::path::PathBuf>,
) -> Result<(), String> {
    debug::install(boundary.into(), marker.into())
}

pub fn reach_process_fault_boundary(boundary: &str) {
    #[cfg(debug_assertions)]
    debug::reach(boundary);
    #[cfg(not(debug_assertions))]
    let _ = boundary;
}
