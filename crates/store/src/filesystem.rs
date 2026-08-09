use crate::StoreError;
use std::io::Write;
use std::path::Path;

pub(crate) fn write_atomic(path: &Path, content: &[u8]) -> Result<(), StoreError> {
    let parent = path.parent().ok_or_else(|| {
        StoreError::Invariant(format!("path has no parent directory: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(io_error)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file"),
        uuid::Uuid::now_v7()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io_error)?;
        file.write_all(content).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        std::fs::rename(&temporary, path).map_err(io_error)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn remove_entry(path: &Path) -> Result<(), StoreError> {
    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path).map_err(io_error)?;
    } else {
        std::fs::remove_file(path).map_err(io_error)?;
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), StoreError> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Io(error.to_string())
}
