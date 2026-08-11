use crate::language::{compile_source, validate_source};
use chrono::Utc;
use papermachine_protocol::{
    Project, ProjectId, WorkflowProgram, WorkflowProgramSnapshot, WorkflowProgramSource,
    WorkflowValidation,
};
use papermachine_store::{Store, StoreError};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

type WorkflowKey = (Option<ProjectId>, String);

#[derive(Clone, Debug)]
pub struct LoadedWorkflowProgram {
    pub registration: WorkflowProgram,
    pub source_code: String,
    pub path: PathBuf,
    pub validation: WorkflowValidation,
    pub ir_sha256: String,
}

impl LoadedWorkflowProgram {
    pub fn snapshot(&self) -> WorkflowProgramSnapshot {
        WorkflowProgramSnapshot {
            project_id: self.registration.project_id,
            manifest: self.registration.manifest.clone(),
            source: self.registration.source,
            definition_path: self.registration.definition_path.clone(),
            sha256: self.registration.sha256.clone(),
            ir_sha256: self.ir_sha256.clone(),
            source_code: self.source_code.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowProgramCatalog {
    builtins_root: PathBuf,
    known_tools: BTreeSet<String>,
    entries: BTreeMap<WorkflowKey, LoadedWorkflowProgram>,
}

impl WorkflowProgramCatalog {
    pub fn scan(
        workflows_root: impl AsRef<Path>,
        known_tools: impl IntoIterator<Item = String>,
    ) -> Result<Self, WorkflowProgramCatalogError> {
        let builtins_root = workflows_root.as_ref().join("builtin");
        if !builtins_root.is_dir() {
            return Err(WorkflowProgramCatalogError::Path(format!(
                "built-in Workflow directory is missing: {}",
                builtins_root.display()
            )));
        }
        let mut catalog = Self {
            builtins_root,
            known_tools: known_tools.into_iter().collect(),
            entries: BTreeMap::new(),
        };
        for path in workflow_files(&catalog.builtins_root)? {
            let loaded = catalog.load_file(&path)?;
            catalog.insert(loaded)?;
        }
        Ok(catalog)
    }

    /// Load or refresh Workflow programs owned by one managed Project.
    pub fn load_project(
        &mut self,
        project: &Project,
        store: &Store,
    ) -> Result<(), WorkflowProgramCatalogError> {
        self.entries
            .retain(|(owner, _), _| *owner != Some(project.id));
        store.ensure_managed_directory("workflows")?;
        for slug in store.list_managed_directories("workflows")? {
            let relative = PathBuf::from("workflows").join(slug).join("workflow.pm");
            if !store.managed_file_exists(&relative)? {
                continue;
            }
            let source_code = store.read_managed_text(&relative, 128 * 1024)?;
            let path = store.managed_path(&relative)?;
            let loaded = self.load_source(
                &path,
                source_code,
                Some(project),
                WorkflowProgramSource::User,
                Some(store.managed_root()),
                Utc::now(),
            )?;
            self.insert(loaded)?;
        }
        Ok(())
    }

    pub fn list(&self, project_id: ProjectId) -> Vec<LoadedWorkflowProgram> {
        let mut visible = BTreeMap::<String, LoadedWorkflowProgram>::new();
        for ((owner, slug), program) in &self.entries {
            if owner.is_none() {
                visible.insert(slug.clone(), program.clone());
            }
        }
        for ((owner, slug), program) in &self.entries {
            if *owner == Some(project_id) {
                visible.insert(slug.clone(), program.clone());
            }
        }
        visible.into_values().collect()
    }

    pub fn get(&self, project_id: ProjectId, slug: &str) -> Option<&LoadedWorkflowProgram> {
        self.entries
            .get(&(Some(project_id), slug.to_string()))
            .or_else(|| self.entries.get(&(None, slug.to_string())))
    }

    pub fn validate_source(
        &self,
        source: &str,
    ) -> Result<WorkflowValidation, WorkflowProgramCatalogError> {
        Ok(validate_source(source, &self.known_tools))
    }

    pub fn save_user(
        &mut self,
        project: &Project,
        source: &str,
        store: &Store,
    ) -> Result<LoadedWorkflowProgram, WorkflowProgramCatalogError> {
        let compiled = compile_source(source, &self.known_tools)
            .map_err(WorkflowProgramCatalogError::Invalid)?;
        let manifest = compiled.manifest;
        let validation = compiled.validation;
        let ir_sha256 = compiled.ir_sha256;
        let key = (Some(project.id), manifest.slug.clone());
        let sha256 = hex::encode(Sha256::digest(source.as_bytes()));
        if let Some(existing) = self.entries.get(&key)
            && existing.registration.sha256 == sha256
        {
            return Ok(existing.clone());
        }
        let path = store.write_managed_file(
            PathBuf::from("workflows")
                .join(&manifest.slug)
                .join("workflow.pm"),
            source.as_bytes(),
        )?;
        let loaded = LoadedWorkflowProgram {
            registration: WorkflowProgram {
                project_id: Some(project.id),
                manifest,
                source: WorkflowProgramSource::User,
                definition_path: project_definition_path(&path, store.managed_root())?,
                sha256,
                updated_at: Utc::now(),
            },
            source_code: source.to_string(),
            path,
            validation,
            ir_sha256,
        };
        self.entries.insert(key, loaded.clone());
        Ok(loaded)
    }

    fn insert(&mut self, loaded: LoadedWorkflowProgram) -> Result<(), WorkflowProgramCatalogError> {
        let key = (
            loaded.registration.project_id,
            loaded.registration.manifest.slug.clone(),
        );
        if self.entries.insert(key.clone(), loaded.clone()).is_some() {
            return Err(WorkflowProgramCatalogError::Duplicate {
                project_id: key.0,
                slug: key.1,
            });
        }
        Ok(())
    }

    fn load_file(&self, path: &Path) -> Result<LoadedWorkflowProgram, WorkflowProgramCatalogError> {
        let source_code = fs::read_to_string(path)?;
        let updated_at = fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .map(chrono::DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        self.load_source(
            path,
            source_code,
            None,
            WorkflowProgramSource::Builtin,
            None,
            updated_at,
        )
    }

    fn load_source(
        &self,
        path: &Path,
        source_code: String,
        project: Option<&Project>,
        source: WorkflowProgramSource,
        managed_root: Option<&Path>,
        updated_at: chrono::DateTime<Utc>,
    ) -> Result<LoadedWorkflowProgram, WorkflowProgramCatalogError> {
        let compiled = compile_source(&source_code, &self.known_tools).map_err(|validation| {
            WorkflowProgramCatalogError::InvalidFile {
                path: path.to_path_buf(),
                validation,
            }
        })?;
        let definition_path = match project {
            Some(_) => project_definition_path(
                path,
                managed_root.ok_or_else(|| {
                    WorkflowProgramCatalogError::Path(
                        "Project Workflow has no managed root".to_string(),
                    )
                })?,
            )?,
            None => {
                let relative = path
                    .strip_prefix(&self.builtins_root)
                    .map_err(|error| WorkflowProgramCatalogError::Path(error.to_string()))?;
                PathBuf::from("builtin")
                    .join(relative)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        Ok(LoadedWorkflowProgram {
            registration: WorkflowProgram {
                project_id: project.map(|project| project.id),
                manifest: compiled.manifest,
                source,
                definition_path,
                sha256: hex::encode(Sha256::digest(source_code.as_bytes())),
                updated_at,
            },
            source_code,
            path: path.to_path_buf(),
            validation: compiled.validation,
            ir_sha256: compiled.ir_sha256,
        })
    }
}

fn project_definition_path(
    path: &Path,
    managed_root: &Path,
) -> Result<String, WorkflowProgramCatalogError> {
    path.strip_prefix(managed_root)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| WorkflowProgramCatalogError::Path(error.to_string()))
}

fn workflow_files(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    fn visit(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
                visit(&path, output)?;
            } else if path.file_name().is_some_and(|name| name == "workflow.pm") {
                output.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, &mut files)?;
    files.sort();
    Ok(files)
}

#[derive(Debug, Error)]
pub enum WorkflowProgramCatalogError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid workflow source")]
    Invalid(Box<WorkflowValidation>),
    #[error("invalid workflow file {path}: {validation:?}")]
    InvalidFile {
        path: PathBuf,
        validation: Box<WorkflowValidation>,
    },
    #[error("duplicate workflow program {slug} for Project {project_id:?}")]
    Duplicate {
        project_id: Option<ProjectId>,
        slug: String,
    },
    #[error("workflow path is invalid: {0}")]
    Path(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtin_workflows_compile_through_the_public_language() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("workflows");
        let catalog = WorkflowProgramCatalog::scan(root, Vec::<String>::new())
            .expect("built-in workflows should compile");
        assert_eq!(catalog.entries.len(), 6);
        assert!(
            catalog
                .entries
                .values()
                .all(|entry| entry.path.extension().is_some_and(|value| value == "pm"))
        );
    }
}
