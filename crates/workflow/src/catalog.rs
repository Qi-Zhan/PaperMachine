use chrono::Utc;
use hex::encode;
use papermachine_protocol::Project;
use papermachine_protocol::ProjectId;
use papermachine_protocol::WorkflowProgram;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_protocol::WorkflowValidation;
use papermachine_store::Store;
use papermachine_store::StoreError;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use thiserror::Error;

type WorkflowKey = (Option<ProjectId>, String);

#[derive(Clone, Debug)]
pub struct LoadedWorkflowProgram {
    pub registration: WorkflowProgram,
    pub source_code: String,
    pub path: PathBuf,
    pub validation: WorkflowValidation,
}

impl LoadedWorkflowProgram {
    pub fn snapshot(&self) -> WorkflowProgramSnapshot {
        WorkflowProgramSnapshot {
            project_id: self.registration.project_id,
            manifest: self.registration.manifest.clone(),
            source: self.registration.source,
            definition_path: self.registration.definition_path.clone(),
            sha256: self.registration.sha256.clone(),
            source_code: self.source_code.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowProgramCatalog {
    builtins_root: PathBuf,
    python_runtime_root: PathBuf,
    python: PathBuf,
    entries: BTreeMap<WorkflowKey, LoadedWorkflowProgram>,
}

impl WorkflowProgramCatalog {
    pub fn scan(
        workflows_root: impl AsRef<Path>,
        python_runtime_root: impl AsRef<Path>,
        store: &Store,
    ) -> Result<Self, WorkflowProgramCatalogError> {
        let builtins_root = workflows_root.as_ref().join("builtin");
        let python_runtime_root = python_runtime_root.as_ref().to_path_buf();
        fs::create_dir_all(&builtins_root)?;
        let python = find_python()?;
        let mut catalog = Self {
            builtins_root,
            python_runtime_root,
            python,
            entries: BTreeMap::new(),
        };
        for path in workflow_files(&catalog.builtins_root)? {
            let loaded = catalog.load_file(&path, None, WorkflowProgramSource::Builtin)?;
            catalog.insert(loaded, store)?;
        }
        Ok(catalog)
    }

    /// Load or refresh the workflow programs owned by one Project directory.
    pub fn load_project(
        &mut self,
        project: &Project,
        store: &Store,
    ) -> Result<(), WorkflowProgramCatalogError> {
        self.entries
            .retain(|(owner, _), _| *owner != Some(project.id));
        let root = project_workflows_root(project);
        fs::create_dir_all(&root)?;
        for path in workflow_files(&root)? {
            let loaded = self.load_file(&path, Some(project), WorkflowProgramSource::User)?;
            self.insert(loaded, store)?;
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
        validate_with_python(&self.python, &self.python_runtime_root, source)
    }

    pub fn save_user(
        &mut self,
        project: &Project,
        source: &str,
        store: &Store,
    ) -> Result<LoadedWorkflowProgram, WorkflowProgramCatalogError> {
        let validation = self.validate_source(source)?;
        if !validation.valid {
            return Err(WorkflowProgramCatalogError::Invalid(Box::new(validation)));
        }
        let manifest = validation.manifest.clone().ok_or_else(|| {
            WorkflowProgramCatalogError::Validator(
                "valid source did not return a manifest".to_string(),
            )
        })?;
        let key = (Some(project.id), manifest.slug.clone());
        let sha256 = encode(Sha256::digest(source.as_bytes()));
        if let Some(existing) = self.entries.get(&key) {
            if existing.registration.sha256 == sha256 {
                return Ok(existing.clone());
            }
        }
        let directory = project_workflows_root(project).join(&manifest.slug);
        fs::create_dir_all(&directory)?;
        let path = directory.join("workflow.py");
        let temporary = directory.join("workflow.py.tmp");
        fs::write(&temporary, source)?;
        fs::rename(&temporary, &path)?;
        let loaded = LoadedWorkflowProgram {
            registration: WorkflowProgram {
                project_id: Some(project.id),
                manifest,
                source: WorkflowProgramSource::User,
                definition_path: project_definition_path(&path, project)?,
                sha256,
                updated_at: Utc::now(),
            },
            source_code: source.to_string(),
            path,
            validation,
        };
        store.register_workflow_program(&loaded.registration)?;
        self.entries.insert(key, loaded.clone());
        Ok(loaded)
    }

    fn insert(
        &mut self,
        loaded: LoadedWorkflowProgram,
        store: &Store,
    ) -> Result<(), WorkflowProgramCatalogError> {
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
        store.register_workflow_program(&loaded.registration)?;
        Ok(())
    }

    fn load_file(
        &self,
        path: &Path,
        project: Option<&Project>,
        source: WorkflowProgramSource,
    ) -> Result<LoadedWorkflowProgram, WorkflowProgramCatalogError> {
        let source_code = fs::read_to_string(path)?;
        let validation = self.validate_source(&source_code)?;
        if !validation.valid {
            return Err(WorkflowProgramCatalogError::InvalidFile {
                path: path.to_path_buf(),
                validation: Box::new(validation),
            });
        }
        let manifest = validation.manifest.clone().ok_or_else(|| {
            WorkflowProgramCatalogError::Validator(
                "valid source did not return a manifest".to_string(),
            )
        })?;
        let definition_path = match project {
            Some(project) => project_definition_path(path, project)?,
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
                manifest,
                source,
                definition_path,
                sha256: encode(Sha256::digest(source_code.as_bytes())),
                updated_at: fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .map(chrono::DateTime::<Utc>::from)
                    .unwrap_or_else(|_| Utc::now()),
            },
            source_code,
            path: path.to_path_buf(),
            validation,
        })
    }

    pub fn python(&self) -> &Path {
        &self.python
    }

    pub fn python_runtime_root(&self) -> &Path {
        &self.python_runtime_root
    }
}

fn project_workflows_root(project: &Project) -> PathBuf {
    PathBuf::from(&project.root_path)
        .join(".papermachine")
        .join("workflows")
}

fn project_definition_path(
    path: &Path,
    project: &Project,
) -> Result<String, WorkflowProgramCatalogError> {
    path.strip_prefix(&project.root_path)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| WorkflowProgramCatalogError::Path(error.to_string()))
}

fn validate_with_python(
    python: &Path,
    runtime_root: &Path,
    source: &str,
) -> Result<WorkflowValidation, WorkflowProgramCatalogError> {
    let validator = runtime_root.join("papermachine").join("_validate.py");
    if !validator.is_file() {
        return Err(WorkflowProgramCatalogError::Validator(format!(
            "validator is missing: {}",
            validator.display()
        )));
    }
    let mut child = Command::new(python)
        .arg(&validator)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| WorkflowProgramCatalogError::Validator(error.to_string()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| {
            WorkflowProgramCatalogError::Validator("validator stdin is unavailable".to_string())
        })?
        .write_all(source.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(WorkflowProgramCatalogError::Validator(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| WorkflowProgramCatalogError::Validator(error.to_string()))
}

fn workflow_files(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    fn visit(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
                visit(&path, output)?;
            } else if path.file_name().is_some_and(|name| name == "workflow.py") {
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

fn find_python() -> Result<PathBuf, WorkflowProgramCatalogError> {
    if let Some(path) = std::env::var_os("PAPERMACHINE_PYTHON") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(WorkflowProgramCatalogError::PythonUnavailable(
            path.display().to_string(),
        ));
    }
    for path in [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/usr/bin/python3",
    ] {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            let output = Command::new(&candidate)
                .arg("-c")
                .arg("import sys; print(sys.version_info[:2] >= (3, 11))")
                .output();
            if output
                .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"True"))
            {
                return Ok(candidate);
            }
        }
    }
    Err(WorkflowProgramCatalogError::PythonUnavailable(
        "Python 3.11 or newer was not found".to_string(),
    ))
}

#[derive(Debug, Error)]
pub enum WorkflowProgramCatalogError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("workflow validator failed: {0}")]
    Validator(String),
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
    #[error("Python runtime unavailable: {0}")]
    PythonUnavailable(String),
}
