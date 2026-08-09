use crate::StoreError;
use cap_fs_ext::FollowSymlinks;
use cap_fs_ext::OpenOptionsFollowExt;
use cap_fs_ext::OpenOptionsMaybeDirExt;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use cap_std::fs::OpenOptions;
use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

const MAX_MANAGED_DIRECTORY_ENTRIES: usize = 100_000;

/// A capability rooted at one Project's managed directory. Every operation
/// accepts only normalized relative paths and opens every component without
/// following symlinks.
#[derive(Clone)]
pub struct ManagedFs {
    root: Arc<Dir>,
    root_path: Arc<PathBuf>,
}

impl ManagedFs {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let requested = root.as_ref();
        let metadata = std::fs::symlink_metadata(requested).map_err(io_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(StoreError::Invariant(format!(
                "managed root is not a real directory: {}",
                requested.display()
            )));
        }
        let root = requested.canonicalize().map_err(io_error)?;
        let directory = Dir::open_ambient_dir(&root, ambient_authority()).map_err(io_error)?;
        Ok(Self {
            root: Arc::new(directory),
            root_path: Arc::new(root),
        })
    }

    pub fn absolute_path(&self, relative: impl AsRef<Path>) -> Result<PathBuf, StoreError> {
        Ok(self.root_path.join(validated_relative(relative.as_ref())?))
    }

    pub fn ensure_dir(&self, relative: impl AsRef<Path>) -> Result<(), StoreError> {
        let components = relative_components(relative.as_ref())?;
        self.open_directory(&components, true)?;
        Ok(())
    }

    pub fn read(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StoreError> {
        let (parent, name) = self.open_parent(relative.as_ref(), false)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .follow(FollowSymlinks::No)
            .maybe_dir(false);
        let mut file = parent.open_with(&name, &options).map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(StoreError::Invariant(format!(
                "managed path is not a regular file: {}",
                relative.as_ref().display()
            )));
        }
        if metadata.len() > max_bytes as u64 {
            return Err(StoreError::Invariant(format!(
                "managed file exceeds {max_bytes} bytes: {}",
                relative.as_ref().display()
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > max_bytes {
            return Err(StoreError::Invariant(format!(
                "managed file exceeds {max_bytes} bytes: {}",
                relative.as_ref().display()
            )));
        }
        Ok(bytes)
    }

    pub fn read_string(
        &self,
        relative: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<String, StoreError> {
        String::from_utf8(self.read(relative.as_ref(), max_bytes)?).map_err(|error| {
            StoreError::Invariant(format!(
                "managed file is not UTF-8: {}: {error}",
                relative.as_ref().display()
            ))
        })
    }

    pub fn write_atomic(
        &self,
        relative: impl AsRef<Path>,
        content: &[u8],
    ) -> Result<PathBuf, StoreError> {
        let relative = validated_relative(relative.as_ref())?;
        let (parent, name) = self.open_parent(&relative, true)?;
        match parent.symlink_metadata(&name) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(StoreError::Invariant(format!(
                    "managed destination is not a regular file: {}",
                    relative.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        let temporary = OsString::from(format!(
            ".{}.{}.tmp",
            Path::new(&name)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("file"),
            uuid::Uuid::now_v7()
        ));
        let result = (|| {
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No)
                .maybe_dir(false);
            let mut file = parent.open_with(&temporary, &options).map_err(io_error)?;
            file.write_all(content).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            parent
                .rename(&temporary, &parent, &name)
                .map_err(io_error)?;
            sync_cap_directory(&parent)
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temporary);
        }
        result?;
        Ok(self.root_path.join(relative))
    }

    pub fn is_regular_file(&self, relative: impl AsRef<Path>) -> Result<bool, StoreError> {
        let mut components = relative_components(relative.as_ref())?;
        let name = components.pop().ok_or_else(|| {
            StoreError::Invariant("managed file path has no filename".to_string())
        })?;
        let mut parent = self.root.try_clone().map_err(io_error)?;
        for component in components {
            parent = match open_child_dir(&parent, &component) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(io_error(error)),
            };
        }
        match parent.symlink_metadata(name) {
            Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(error)),
        }
    }

    pub fn list_directories(&self, relative: impl AsRef<Path>) -> Result<Vec<String>, StoreError> {
        let components = relative_components(relative.as_ref())?;
        let directory = self.open_directory(&components, false)?;
        let mut names = Vec::new();
        let mut visited = 0_usize;
        for entry in directory.entries().map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            visited = visited.saturating_add(1);
            if visited > MAX_MANAGED_DIRECTORY_ENTRIES {
                return Err(StoreError::Invariant(format!(
                    "managed directory exceeds {MAX_MANAGED_DIRECTORY_ENTRIES} entries: {}",
                    relative.as_ref().display()
                )));
            }
            let file_type = entry.file_type().map_err(io_error)?;
            if file_type.is_symlink() {
                return Err(StoreError::Invariant(format!(
                    "managed directory contains a symlink: {}",
                    relative.as_ref().join(entry.file_name()).display()
                )));
            }
            if file_type.is_dir() {
                names.push(entry.file_name().into_string().map_err(|_| {
                    StoreError::Invariant("managed directory name is not UTF-8".to_string())
                })?);
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn remove(&self, relative: impl AsRef<Path>) -> Result<(), StoreError> {
        let (parent, name) = self.open_parent(relative.as_ref(), false)?;
        let mut visited = 0_usize;
        remove_child(&parent, &name, &mut visited)?;
        sync_cap_directory(&parent)
    }

    fn open_parent(&self, relative: &Path, create: bool) -> Result<(Dir, OsString), StoreError> {
        let mut components = relative_components(relative)?;
        let name = components.pop().ok_or_else(|| {
            StoreError::Invariant("managed file path has no filename".to_string())
        })?;
        Ok((self.open_directory(&components, create)?, name))
    }

    fn open_directory(&self, components: &[OsString], create: bool) -> Result<Dir, StoreError> {
        let mut directory = self.root.try_clone().map_err(io_error)?;
        for component in components {
            directory = match open_child_dir(&directory, component) {
                Ok(child) => child,
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    match directory.create_dir(component) {
                        Ok(()) => sync_cap_directory(&directory)?,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(io_error(error)),
                    }
                    open_child_dir(&directory, component).map_err(io_error)?
                }
                Err(error) => return Err(io_error(error)),
            };
        }
        Ok(directory)
    }
}

fn validated_relative(path: &Path) -> Result<PathBuf, StoreError> {
    let components = relative_components(path)?;
    Ok(components.iter().collect())
}

fn relative_components(path: &Path) -> Result<Vec<OsString>, StoreError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(StoreError::Invariant(format!(
            "managed path must be relative and non-empty: {}",
            path.display()
        )));
    }
    path.components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_os_string()),
            _ => Err(StoreError::Invariant(format!(
                "managed path must be normalized: {}",
                path.display()
            ))),
        })
        .collect()
}

fn open_child_dir(parent: &Dir, name: &OsString) -> std::io::Result<Dir> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!(
                "managed path component is not a directory: {}",
                Path::new(name).display()
            ),
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

fn remove_child(parent: &Dir, name: &OsString, visited: &mut usize) -> Result<(), StoreError> {
    *visited = visited.saturating_add(1);
    if *visited > MAX_MANAGED_DIRECTORY_ENTRIES {
        return Err(StoreError::Invariant(format!(
            "managed deletion exceeds {MAX_MANAGED_DIRECTORY_ENTRIES} entries"
        )));
    }
    let metadata = parent.symlink_metadata(name).map_err(io_error)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let directory = open_child_dir(parent, name).map_err(io_error)?;
        let entries = directory
            .entries()
            .map_err(io_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_error)?;
        for entry in entries {
            remove_child(&directory, &entry.file_name(), visited)?;
        }
        parent.remove_dir(name).map_err(io_error)
    } else {
        parent.remove_file(name).map_err(io_error)
    }
}

fn sync_cap_directory(directory: &Dir) -> Result<(), StoreError> {
    directory
        .try_clone()
        .and_then(|directory| directory.into_std_file().sync_all())
        .map_err(io_error)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn managed_fs_atomically_replaces_bounded_regular_files() {
        let fixture = tempdir().expect("fixture should be created");
        let managed = ManagedFs::open(fixture.path()).expect("managed fs should open");
        managed
            .write_atomic("prompts/system.md", b"first")
            .expect("first write should succeed");
        managed
            .write_atomic("prompts/system.md", b"second")
            .expect("replacement should succeed");
        assert_eq!(
            managed
                .read_string("prompts/system.md", 6)
                .expect("bounded read should succeed"),
            "second"
        );
        assert!(managed.read("prompts/system.md", 5).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn managed_fs_never_follows_symlinks_and_safe_delete_stays_beneath_root() {
        use std::os::unix::fs::symlink;

        let fixture = tempdir().expect("fixture should be created");
        let root = fixture.path().join("managed");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(root.join("tree")).expect("managed tree should be created");
        std::fs::create_dir(&outside).expect("outside directory should be created");
        std::fs::write(outside.join("secret.txt"), "outside")
            .expect("outside fixture should be written");
        symlink(&outside, root.join("tree/link")).expect("symlink fixture should be created");
        let managed = ManagedFs::open(&root).expect("managed fs should open");

        assert!(managed.read("tree/link/secret.txt", 100).is_err());
        assert!(
            managed
                .write_atomic("tree/link/new.txt", b"escape")
                .is_err()
        );
        managed.remove("tree").expect("managed tree should delete");
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.txt"))
                .expect("outside file should remain"),
            "outside"
        );
        assert!(!outside.join("new.txt").exists());
    }
}
