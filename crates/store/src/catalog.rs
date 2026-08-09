//! Filesystem-backed Project catalog.
//!
//! The catalog contains no database of its own. A directory name identifies a
//! Project and the single Project row inside that directory's current-schema
//! database is authoritative.

use crate::Store;
use crate::StoreError;
use papermachine_protocol::Project;
use papermachine_protocol::ProjectId;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone)]
pub struct ProjectCatalog {
    data_root: Arc<PathBuf>,
    projects_root: Arc<PathBuf>,
    staging_root: Arc<PathBuf>,
    trash_root: Arc<PathBuf>,
    mutation: Arc<Mutex<()>>,
}

pub struct CatalogProject {
    pub project: Project,
    pub store: Store,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogFailure {
    pub path: PathBuf,
    pub error: String,
}

impl ProjectCatalog {
    pub fn open(data_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        std::fs::create_dir_all(data_root.as_ref())
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let data_root = data_root
            .as_ref()
            .canonicalize()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let projects_root = data_root.join("projects");
        let staging_root = data_root.join("staging");
        let trash_root = data_root.join("trash");
        for root in [&projects_root, &staging_root, &trash_root] {
            std::fs::create_dir_all(root).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        Ok(Self {
            data_root: Arc::new(data_root),
            projects_root: Arc::new(projects_root),
            staging_root: Arc::new(staging_root),
            trash_root: Arc::new(trash_root),
            mutation: Arc::new(Mutex::new(())),
        })
    }

    pub fn data_root(&self) -> &Path {
        self.data_root.as_ref()
    }

    pub fn managed_root(&self, project_id: ProjectId) -> PathBuf {
        self.projects_root.join(project_id.to_string())
    }

    pub fn scan(&self) -> Result<Vec<CatalogProject>, StoreError> {
        let _guard = self.mutation.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.scan_unlocked()
    }

    pub fn scan_resilient(&self) -> Result<(Vec<CatalogProject>, Vec<CatalogFailure>), StoreError> {
        let _guard = self.mutation.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.scan_resilient_unlocked()
    }

    pub fn create_project(
        &self,
        name: impl Into<String>,
        workspace: impl AsRef<Path>,
    ) -> Result<CatalogProject, StoreError> {
        let _guard = self.mutation.lock().map_err(|_| StoreError::LockPoisoned)?;
        let workspace = canonical_workspace(workspace.as_ref(), true)?;
        ensure_workspace_is_external(&workspace, &self.data_root)?;
        ensure_workspace_is_unique(&workspace, None, &self.scan_unlocked()?)?;

        let project_id = ProjectId::new();
        let staging = self
            .staging_root
            .join(format!("{}-{}", project_id, uuid::Uuid::now_v7()));
        let destination = self.managed_root(project_id);
        if destination.exists() {
            return Err(StoreError::Invariant(format!(
                "Project managed directory already exists: {}",
                destination.display()
            )));
        }

        let mut published = false;
        let result = (|| {
            let store = Store::create(&staging)?;
            let project = store.create_project_with_id(project_id, name, workspace.as_path())?;
            drop(store);
            std::fs::rename(&staging, &destination)
                .map_err(|error| StoreError::Io(error.to_string()))?;
            published = true;
            sync_directory(&self.staging_root)?;
            sync_directory(&self.projects_root)?;
            let store = Store::open(&destination)?;
            validate_project_identity(&store, project_id)?;
            Ok(CatalogProject { project, store })
        })();
        if result.is_err() && staging.exists() {
            remove_entry(&staging)?;
        }
        if result.is_err() && published && destination.exists() {
            let quarantine = self
                .trash_root
                .join(format!("failed-project-{}", uuid::Uuid::now_v7()));
            std::fs::rename(&destination, &quarantine)
                .map_err(|error| StoreError::Io(error.to_string()))?;
            sync_directory(&self.projects_root)?;
            sync_directory(&self.trash_root)?;
        }
        result
    }

    pub fn relocate_project(
        &self,
        store: &Store,
        project_id: ProjectId,
        workspace: impl AsRef<Path>,
    ) -> Result<Project, StoreError> {
        let _guard = self.mutation.lock().map_err(|_| StoreError::LockPoisoned)?;
        if store.managed_root() != self.managed_root(project_id) {
            return Err(StoreError::Invariant(
                "Project Store is outside the catalog entry".to_string(),
            ));
        }
        let workspace = canonical_workspace(workspace.as_ref(), false)?;
        ensure_workspace_is_external(&workspace, &self.data_root)?;
        ensure_workspace_is_unique(&workspace, Some(project_id), &self.scan_unlocked()?)?;
        store.relocate_project(project_id, workspace)
    }

    /// Atomically remove a Project from the live catalog. The returned path is
    /// inside `trash/` and can be deleted asynchronously after live references
    /// to the Project runtime have drained.
    pub fn retire_project(&self, project_id: ProjectId) -> Result<PathBuf, StoreError> {
        let _guard = self.mutation.lock().map_err(|_| StoreError::LockPoisoned)?;
        let source = self.managed_root(project_id);
        if !source.is_dir() {
            return Err(StoreError::NotFound {
                entity: "project",
                id: project_id.to_string(),
            });
        }
        let destination =
            self.trash_root
                .join(format!("project-{}-{}", project_id, uuid::Uuid::now_v7()));
        std::fs::rename(&source, &destination)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        sync_directory(&self.projects_root)?;
        sync_directory(&self.trash_root)?;
        Ok(destination)
    }

    /// Move abandoned pre-publish directories out of `staging/`. They were
    /// never catalog entries and are safe to purge as managed state.
    pub fn quarantine_staging(&self) -> Result<Vec<PathBuf>, StoreError> {
        let _guard = self.mutation.lock().map_err(|_| StoreError::LockPoisoned)?;
        let mut quarantined = Vec::new();
        for entry in sorted_entries(&self.staging_root)? {
            let destination = self
                .trash_root
                .join(format!("staging-{}", uuid::Uuid::now_v7()));
            std::fs::rename(entry.path(), &destination)
                .map_err(|error| StoreError::Io(error.to_string()))?;
            quarantined.push(destination);
        }
        if !quarantined.is_empty() {
            sync_directory(&self.staging_root)?;
            sync_directory(&self.trash_root)?;
        }
        Ok(quarantined)
    }

    pub fn trash_entries(&self) -> Result<Vec<PathBuf>, StoreError> {
        sorted_entries(&self.trash_root)
            .map(|entries| entries.into_iter().map(|entry| entry.path()).collect())
    }

    pub fn purge_trash_entry(&self, path: &Path) -> Result<(), StoreError> {
        if path.parent() != Some(self.trash_root.as_ref()) || path.file_name().is_none() {
            return Err(StoreError::Invariant(format!(
                "refusing to purge a path outside the Project trash: {}",
                path.display()
            )));
        }
        if !path.exists() && std::fs::symlink_metadata(path).is_err() {
            return Ok(());
        }
        remove_entry(path)
    }

    fn scan_unlocked(&self) -> Result<Vec<CatalogProject>, StoreError> {
        let (projects, failures) = self.scan_resilient_unlocked()?;
        if let Some(failure) = failures.into_iter().next() {
            return Err(StoreError::Invariant(format!(
                "invalid Project catalog entry {}: {}",
                failure.path.display(),
                failure.error
            )));
        }
        Ok(projects)
    }

    fn scan_resilient_unlocked(
        &self,
    ) -> Result<(Vec<CatalogProject>, Vec<CatalogFailure>), StoreError> {
        let mut projects = Vec::new();
        let mut failures = Vec::new();
        let mut workspace_owners = BTreeMap::<PathBuf, ProjectId>::new();
        for entry in sorted_entries(&self.projects_root)? {
            let path = entry.path();
            let candidate = load_catalog_project(entry).and_then(|candidate| {
                for root in &candidate.project.workspace.roots {
                    let root = PathBuf::from(root);
                    if let Some(owner) = workspace_owners.get(&root) {
                        return Err(StoreError::Invariant(format!(
                            "Workspace {} is attached to Projects {owner} and {}",
                            root.display(),
                            candidate.project.id
                        )));
                    }
                }
                Ok(candidate)
            });
            let candidate = match candidate {
                Ok(candidate) => candidate,
                Err(error) => {
                    failures.push(CatalogFailure {
                        path,
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            for root in &candidate.project.workspace.roots {
                workspace_owners.insert(PathBuf::from(root), candidate.project.id);
            }
            projects.push(candidate);
        }
        projects.sort_by(|left, right| {
            right
                .project
                .updated_at
                .cmp(&left.project.updated_at)
                .then_with(|| left.project.id.cmp(&right.project.id))
        });
        Ok((projects, failures))
    }
}

fn load_catalog_project(entry: std::fs::DirEntry) -> Result<CatalogProject, StoreError> {
    let file_type = entry
        .file_type()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if !file_type.is_dir() || file_type.is_symlink() {
        return Err(StoreError::Invariant(format!(
            "catalog entry is not a real directory: {}",
            entry.path().display()
        )));
    }
    let name = entry
        .file_name()
        .into_string()
        .map_err(|_| StoreError::Invariant("Project directory name is not Unicode".to_string()))?;
    let project_id = ProjectId::from_str(&name).map_err(|error| {
        StoreError::Invariant(format!("invalid Project directory {name}: {error}"))
    })?;
    let store = Store::open(entry.path())?;
    let project = validate_project_identity(&store, project_id)?;
    Ok(CatalogProject { project, store })
}

fn validate_project_identity(
    store: &Store,
    directory_id: ProjectId,
) -> Result<Project, StoreError> {
    let projects = store.list_projects()?;
    if projects.len() != 1 {
        return Err(StoreError::Invariant(format!(
            "Project database must contain exactly one Project row; found {}",
            projects.len()
        )));
    }
    let project = projects
        .into_iter()
        .next()
        .ok_or_else(|| StoreError::Invariant("Project database has no Project row".to_string()))?;
    if project.id != directory_id {
        return Err(StoreError::Invariant(format!(
            "Project directory id {directory_id} does not match database id {}",
            project.id
        )));
    }
    Ok(project)
}

fn ensure_workspace_is_unique(
    workspace: &Path,
    except: Option<ProjectId>,
    projects: &[CatalogProject],
) -> Result<(), StoreError> {
    if projects.iter().any(|candidate| {
        Some(candidate.project.id) != except
            && candidate
                .project
                .workspace
                .roots
                .iter()
                .any(|root| Path::new(root) == workspace)
    }) {
        return Err(StoreError::Invariant(format!(
            "Workspace is already attached to another Project: {}",
            workspace.display()
        )));
    }
    Ok(())
}

fn canonical_workspace(path: &Path, create: bool) -> Result<PathBuf, StoreError> {
    if !path.is_absolute() {
        return Err(StoreError::Invariant(
            "Project Workspace must be an absolute path".to_string(),
        ));
    }
    if create {
        std::fs::create_dir_all(path).map_err(|error| StoreError::Io(error.to_string()))?;
    }
    path.canonicalize()
        .map_err(|error| StoreError::Io(error.to_string()))
}

fn ensure_workspace_is_external(workspace: &Path, data_root: &Path) -> Result<(), StoreError> {
    if workspace.starts_with(data_root) || data_root.starts_with(workspace) {
        return Err(StoreError::Invariant(
            "Project Workspace must be separate from all PaperMachine managed state".to_string(),
        ));
    }
    Ok(())
}

fn sorted_entries(root: &Path) -> Result<Vec<std::fs::DirEntry>, StoreError> {
    let mut entries = std::fs::read_dir(root)
        .map_err(|error| StoreError::Io(error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn remove_entry(path: &Path) -> Result<(), StoreError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| StoreError::Io(error.to_string()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path).map_err(|error| StoreError::Io(error.to_string()))
    } else {
        std::fs::remove_file(path).map_err(|error| StoreError::Io(error.to_string()))
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| StoreError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}
