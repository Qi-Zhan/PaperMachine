use chrono::Utc;
use hex::encode;
use papermachine_protocol::WorkflowRegistration;
use papermachine_protocol::WorkflowSnapshot;
use papermachine_protocol::WorkflowSource;
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

type WorkflowKey = (String, String);

#[derive(Clone, Debug)]
pub struct LoadedWorkflow {
    pub registration: WorkflowRegistration,
    pub source_code: String,
    pub path: PathBuf,
}

impl LoadedWorkflow {
    pub fn snapshot(&self) -> WorkflowSnapshot {
        WorkflowSnapshot {
            manifest: self.registration.manifest.clone(),
            source: self.registration.source,
            definition_path: self.registration.definition_path.clone(),
            sha256: self.registration.sha256.clone(),
            source_code: self.source_code.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowCatalog {
    workflows_root: PathBuf,
    python_runtime_root: PathBuf,
    python: PathBuf,
    entries: BTreeMap<WorkflowKey, LoadedWorkflow>,
}

impl WorkflowCatalog {
    pub fn scan(
        workflows_root: impl AsRef<Path>,
        python_runtime_root: impl AsRef<Path>,
        store: &Store,
    ) -> Result<Self, WorkflowCatalogError> {
        let workflows_root = workflows_root.as_ref().to_path_buf();
        let python_runtime_root = python_runtime_root.as_ref().to_path_buf();
        fs::create_dir_all(workflows_root.join("builtin"))?;
        fs::create_dir_all(workflows_root.join("user"))?;
        let python = find_python()?;
        let mut catalog = Self {
            workflows_root,
            python_runtime_root,
            python,
            entries: BTreeMap::new(),
        };
        for path in workflow_files(&catalog.workflows_root)? {
            let loaded = catalog.load_file(&path)?;
            let key = (
                loaded.registration.manifest.slug.clone(),
                loaded.registration.manifest.version.clone(),
            );
            if catalog
                .entries
                .insert(key.clone(), loaded.clone())
                .is_some()
            {
                return Err(WorkflowCatalogError::Duplicate {
                    slug: key.0,
                    version: key.1,
                });
            }
            store.register_workflow(&loaded.registration)?;
        }
        Ok(catalog)
    }

    pub fn list(&self) -> Vec<LoadedWorkflow> {
        self.entries.values().cloned().collect()
    }

    pub fn get(&self, slug: &str, version: &str) -> Option<&LoadedWorkflow> {
        self.entries.get(&(slug.to_string(), version.to_string()))
    }

    pub fn validate_source(
        &self,
        source: &str,
    ) -> Result<WorkflowValidation, WorkflowCatalogError> {
        validate_with_python(&self.python, &self.python_runtime_root, source)
    }

    pub fn save_user(
        &mut self,
        source: &str,
        store: &Store,
    ) -> Result<LoadedWorkflow, WorkflowCatalogError> {
        let validation = self.validate_source(source)?;
        if !validation.valid {
            return Err(WorkflowCatalogError::Invalid(Box::new(validation)));
        }
        let manifest = validation.manifest.ok_or_else(|| {
            WorkflowCatalogError::Validator("valid source did not return a manifest".to_string())
        })?;
        let key = (manifest.slug.clone(), manifest.version.clone());
        let sha256 = encode(Sha256::digest(source.as_bytes()));
        if let Some(existing) = self.entries.get(&key) {
            if existing.registration.sha256 == sha256 {
                return Ok(existing.clone());
            }
            return Err(WorkflowCatalogError::Immutable {
                slug: key.0,
                version: key.1,
            });
        }
        let directory = self
            .workflows_root
            .join("user")
            .join(&manifest.slug)
            .join(&manifest.version);
        fs::create_dir_all(&directory)?;
        let path = directory.join("workflow.py");
        fs::write(&path, source)?;
        let relative = path
            .strip_prefix(&self.workflows_root)
            .map_err(|error| WorkflowCatalogError::Path(error.to_string()))?;
        let loaded = LoadedWorkflow {
            registration: WorkflowRegistration {
                manifest,
                source: WorkflowSource::User,
                definition_path: relative.to_string_lossy().into_owned(),
                sha256,
                updated_at: Utc::now(),
            },
            source_code: source.to_string(),
            path,
        };
        store.register_workflow(&loaded.registration)?;
        self.entries.insert(key, loaded.clone());
        Ok(loaded)
    }

    fn load_file(&self, path: &Path) -> Result<LoadedWorkflow, WorkflowCatalogError> {
        let source_code = fs::read_to_string(path)?;
        let validation = self.validate_source(&source_code)?;
        if !validation.valid {
            return Err(WorkflowCatalogError::InvalidFile {
                path: path.to_path_buf(),
                validation: Box::new(validation),
            });
        }
        let manifest = validation.manifest.ok_or_else(|| {
            WorkflowCatalogError::Validator("valid source did not return a manifest".to_string())
        })?;
        let relative = path
            .strip_prefix(&self.workflows_root)
            .map_err(|error| WorkflowCatalogError::Path(error.to_string()))?;
        let owner = relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .ok_or_else(|| WorkflowCatalogError::Path(path.display().to_string()))?;
        let source = match owner {
            "builtin" => WorkflowSource::Builtin,
            "user" => WorkflowSource::User,
            _ => return Err(WorkflowCatalogError::UnknownOwner(owner.to_string())),
        };
        Ok(LoadedWorkflow {
            registration: WorkflowRegistration {
                manifest,
                source,
                definition_path: relative.to_string_lossy().into_owned(),
                sha256: encode(Sha256::digest(source_code.as_bytes())),
                updated_at: fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .map(chrono::DateTime::<Utc>::from)
                    .unwrap_or_else(|_| Utc::now()),
            },
            source_code,
            path: path.to_path_buf(),
        })
    }

    pub fn python(&self) -> &Path {
        &self.python
    }

    pub fn python_runtime_root(&self) -> &Path {
        &self.python_runtime_root
    }
}

fn validate_with_python(
    python: &Path,
    runtime_root: &Path,
    source: &str,
) -> Result<WorkflowValidation, WorkflowCatalogError> {
    let validator = runtime_root.join("papermachine").join("_validate.py");
    if !validator.is_file() {
        return Err(WorkflowCatalogError::Validator(format!(
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
        .map_err(|error| WorkflowCatalogError::Validator(error.to_string()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| {
            WorkflowCatalogError::Validator("validator stdin is unavailable".to_string())
        })?
        .write_all(source.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(WorkflowCatalogError::Validator(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| WorkflowCatalogError::Validator(error.to_string()))
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
    for owner in ["builtin", "user"] {
        visit(&root.join(owner), &mut files)?;
    }
    files.sort();
    Ok(files)
}

fn find_python() -> Result<PathBuf, WorkflowCatalogError> {
    if let Some(path) = std::env::var_os("PAPERMACHINE_PYTHON") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(WorkflowCatalogError::PythonUnavailable(
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
    Err(WorkflowCatalogError::PythonUnavailable(
        "Python 3.11 or newer was not found".to_string(),
    ))
}

#[derive(Debug, Error)]
pub enum WorkflowCatalogError {
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
    #[error("duplicate workflow {slug}@{version}")]
    Duplicate { slug: String, version: String },
    #[error("workflow {slug}@{version} is immutable; publish a new version")]
    Immutable { slug: String, version: String },
    #[error("workflow path is invalid: {0}")]
    Path(String),
    #[error("workflow must live under builtin/ or user/, found {0}")]
    UnknownOwner(String),
    #[error("Python runtime unavailable: {0}")]
    PythonUnavailable(String),
}
