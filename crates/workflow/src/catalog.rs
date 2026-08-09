use chrono::Utc;
use hex::encode;
use papermachine_protocol::DiagnosticSeverity;
use papermachine_protocol::Project;
use papermachine_protocol::ProjectId;
use papermachine_protocol::WorkflowDiagnostic;
use papermachine_protocol::WorkflowProgram;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_protocol::WorkflowValidation;
use papermachine_store::Store;
use papermachine_store::StoreError;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
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
    known_tools: BTreeSet<String>,
    entries: BTreeMap<WorkflowKey, LoadedWorkflowProgram>,
}

impl WorkflowProgramCatalog {
    pub fn scan(
        workflows_root: impl AsRef<Path>,
        python_runtime_root: impl AsRef<Path>,
        store: &Store,
        known_tools: impl IntoIterator<Item = String>,
    ) -> Result<Self, WorkflowProgramCatalogError> {
        let builtins_root = workflows_root.as_ref().join("builtin");
        let python_runtime_root = python_runtime_root.as_ref().to_path_buf();
        if !builtins_root.is_dir() {
            return Err(WorkflowProgramCatalogError::Path(format!(
                "built-in Workflow directory is missing: {}",
                builtins_root.display()
            )));
        }
        let python = resolve_python_executable()?;
        let mut catalog = Self {
            builtins_root,
            python_runtime_root,
            python,
            known_tools: known_tools.into_iter().collect(),
            entries: BTreeMap::new(),
        };
        for path in workflow_files(&catalog.builtins_root)? {
            let loaded = catalog.load_file(&path, None, WorkflowProgramSource::Builtin, store)?;
            catalog.insert(loaded, store)?;
        }
        Ok(catalog)
    }

    /// Load or refresh the workflow programs owned by one managed Project.
    pub fn load_project(
        &mut self,
        project: &Project,
        store: &Store,
    ) -> Result<(), WorkflowProgramCatalogError> {
        self.entries
            .retain(|(owner, _), _| *owner != Some(project.id));
        let root = project_workflows_root(store);
        fs::create_dir_all(&root)?;
        for path in workflow_files(&root)? {
            let loaded =
                self.load_file(&path, Some(project), WorkflowProgramSource::User, store)?;
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
        let mut validation = validate_with_python(&self.python, &self.python_runtime_root, source)?;
        for agent in &validation.agents {
            for action in &agent.actions {
                let mut seen = BTreeSet::new();
                for tool in &action.tools {
                    let message = if tool.trim().is_empty() {
                        Some(format!(
                            "Action {}.{} declares an empty tool name",
                            agent.class_name, action.name
                        ))
                    } else if !seen.insert(tool.as_str()) {
                        Some(format!(
                            "Action {}.{} declares duplicate tool {tool:?}",
                            agent.class_name, action.name
                        ))
                    } else if !self.known_tools.contains(tool) {
                        Some(format!(
                            "Action {}.{} declares unknown tool {tool:?}",
                            agent.class_name, action.name
                        ))
                    } else {
                        None
                    };
                    if let Some(message) = message {
                        validation.diagnostics.push(WorkflowDiagnostic {
                            severity: DiagnosticSeverity::Error,
                            message,
                            line: None,
                            column: None,
                        });
                    }
                }
            }
        }
        validation.valid = !validation
            .diagnostics
            .iter()
            .any(|item| item.severity == DiagnosticSeverity::Error);
        Ok(validation)
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
        if let Some(existing) = self.entries.get(&key)
            && existing.registration.sha256 == sha256
        {
            return Ok(existing.clone());
        }
        let directory = project_workflows_root(store).join(&manifest.slug);
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
                definition_path: project_definition_path(&path, store)?,
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
        store: &Store,
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
            Some(_) => project_definition_path(path, store)?,
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

fn project_workflows_root(store: &Store) -> PathBuf {
    store.managed_root().join("workflows")
}

fn project_definition_path(
    path: &Path,
    store: &Store,
) -> Result<String, WorkflowProgramCatalogError> {
    path.strip_prefix(store.managed_root())
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

pub fn resolve_python_executable() -> Result<PathBuf, WorkflowProgramCatalogError> {
    if let Some(configured) = env::var_os("PAPERMACHINE_PYTHON") {
        let configured = PathBuf::from(configured);
        let path = resolve_executable(&configured).ok_or_else(|| {
            WorkflowProgramCatalogError::PythonUnavailable(configured.display().to_string())
        })?;
        if supported_python(&path) {
            return Ok(path);
        }
        return Err(WorkflowProgramCatalogError::PythonUnavailable(format!(
            "{} is not Python 3.11 or newer",
            path.display()
        )));
    }
    let names: &[&str] = if cfg!(windows) {
        &["python3.exe", "python.exe"]
    } else {
        &["python3", "python"]
    };
    for name in names {
        let candidate = PathBuf::from(name);
        if let Some(path) = resolve_executable(&candidate)
            && supported_python(&path)
        {
            return Ok(path);
        }
    }
    Err(WorkflowProgramCatalogError::PythonUnavailable(
        "Python 3.11 or newer was not found on PATH; set PAPERMACHINE_PYTHON".to_string(),
    ))
}

fn resolve_executable(value: &Path) -> Option<PathBuf> {
    if value.is_absolute() || value.components().count() > 1 {
        return value.is_file().then(|| value.to_path_buf());
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(value))
        .find(|candidate| candidate.is_file())
}

fn supported_python(path: &Path) -> bool {
    Command::new(path)
        .arg("-c")
        .arg("import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)")
        .output()
        .is_ok_and(|output| output.status.success())
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
