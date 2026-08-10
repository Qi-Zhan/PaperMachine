//! HTTP and realtime API for PaperMachine.

mod demo_model;
mod directory_picker;
pub mod paths;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::header::HeaderName;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::Event;
use axum::response::sse::KeepAlive;
use axum::response::sse::Sse;
use axum::routing::get;
use axum::routing::post;
use papermachine_model::ConfiguredModels;
use papermachine_model::DEFAULT_MODEL_CONTEXT_WINDOW;
use papermachine_model::ModelClient;
use papermachine_model::ModelProfile;
use papermachine_model::ModelProviderInfo;
use papermachine_protocol::*;
use papermachine_session::TurnRuntime;
use papermachine_session::TurnRuntimeConfig;
use papermachine_session::TurnRuntimeError;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_skills::SkillError;
use papermachine_store::NewSession;
use papermachine_store::ProjectCatalog;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_store::StoreHandle;
use papermachine_tools::ExecCommandTool;
use papermachine_tools::FetchUrlTool;
use papermachine_tools::ReadFileTool;
use papermachine_tools::ReadResourceTool;
use papermachine_tools::ToolCatalog;
use papermachine_tools::WriteFileTool;
use papermachine_workflow::PythonSessionExecutor;
use papermachine_workflow::SessionExecutor;
use papermachine_workflow::SessionScheduler;
use papermachine_workflow::SessionSchedulerError;
use papermachine_workflow::WorkflowGenerationRequest;
use papermachine_workflow::WorkflowGenerator;
use papermachine_workflow::WorkflowProgramCatalog;
use papermachine_workflow::WorkflowProgramCatalogError;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::services::ServeFile;
use tower_http::trace::TraceLayer;

pub use demo_model::DemoModelClient;

#[derive(Clone, Debug)]
pub enum ServerModelConfig {
    Demo,
    Providers(ConfiguredModels),
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub resource_root: PathBuf,
    pub data_dir: PathBuf,
    pub default_workspace_root: PathBuf,
    pub models: ServerModelConfig,
    pub max_concurrent_runs: usize,
    pub max_parallel_actions: usize,
}

type InitializedModels = (
    Arc<dyn ModelClient>,
    String,
    usize,
    &'static str,
    Vec<ModelProfile>,
    Vec<ModelProviderInfo>,
);

const LOCAL_TOOL_NAMES: [&str; 5] = [
    "read_file",
    "write_file",
    "exec_command",
    "fetch_url",
    "read_resource",
];

#[derive(Clone)]
pub struct AppState {
    catalog: Arc<ProjectCatalog>,
    default_workspace_root: PathBuf,
    projects: Arc<RwLock<HashMap<ProjectId, ProjectHandle>>>,
    runtime_factory: Arc<ProjectRuntimeFactory>,
    shutdown: CancellationToken,
    generator: WorkflowGenerator,
    default_model: String,
    model_context_window: usize,
    model_profiles: Vec<ModelProfile>,
    model_providers: Vec<ModelProviderInfo>,
    mode: &'static str,
}

impl AppState {
    pub fn mode(&self) -> &'static str {
        self.mode
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    async fn project_lease(&self, project_id: ProjectId) -> Result<ProjectReadLease, StoreError> {
        let projects = Arc::clone(&self.projects).read_owned().await;
        let project = projects
            .get(&project_id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound {
                entity: "Project runtime",
                id: project_id.to_string(),
            })?;
        Ok(ProjectReadLease {
            _projects: projects,
            project,
            project_id,
        })
    }

    async fn runtime_from_lease(
        &self,
        lease: ProjectReadLease,
    ) -> Result<ProjectRuntimeLease, StoreError> {
        let factory = Arc::clone(&self.runtime_factory);
        let runtime = lease.runtime(factory).await?;
        Ok(ProjectRuntimeLease {
            _project: lease,
            runtime,
        })
    }

    async fn project_runtime(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectRuntimeLease, StoreError> {
        let lease = self.project_lease(project_id).await?;
        self.runtime_from_lease(lease).await
    }

    async fn project_store(&self, project_id: ProjectId) -> Result<ProjectReadLease, StoreError> {
        self.project_lease(project_id).await
    }

    async fn project_write(
        &self,
        project_id: ProjectId,
    ) -> Result<OwnedRwLockWriteGuard<HashMap<ProjectId, ProjectHandle>>, StoreError> {
        let projects = Arc::clone(&self.projects).write_owned().await;
        if !projects.contains_key(&project_id) {
            return Err(StoreError::NotFound {
                entity: "Project",
                id: project_id.to_string(),
            });
        }
        Ok(projects)
    }
}

#[derive(Clone)]
struct ProjectHandle {
    store: StoreHandle,
    runtime: Arc<OnceCell<ProjectRuntime>>,
}

struct ProjectReadLease {
    _projects: OwnedRwLockReadGuard<HashMap<ProjectId, ProjectHandle>>,
    project: ProjectHandle,
    project_id: ProjectId,
}

impl ProjectReadLease {
    fn store(&self) -> StoreHandle {
        self.project.store.clone()
    }

    async fn runtime(
        &self,
        factory: Arc<ProjectRuntimeFactory>,
    ) -> Result<ProjectRuntime, StoreError> {
        let project_id = self.project_id;
        let store = self.project.store.clone();
        self.project
            .runtime
            .get_or_try_init(|| async move {
                let project = store
                    .call(move |store| store.get_project(project_id))
                    .await?;
                factory.build(&project, store).await.map_err(|error| {
                    StoreError::Invariant(format!(
                        "Project {project_id} runtime is unavailable: {error:#}"
                    ))
                })
            })
            .await
            .cloned()
    }
}

struct ProjectRuntimeLease {
    _project: ProjectReadLease,
    runtime: ProjectRuntime,
}

impl std::ops::Deref for ProjectRuntimeLease {
    type Target = ProjectRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl ProjectHandle {
    fn unloaded(store: StoreHandle) -> Self {
        Self {
            store,
            runtime: Arc::new(OnceCell::new()),
        }
    }

    fn loaded(store: StoreHandle, runtime: ProjectRuntime) -> Self {
        let cell = OnceCell::new();
        assert!(
            cell.set(runtime).is_ok(),
            "a fresh Project runtime cell must be empty"
        );
        Self {
            store,
            runtime: Arc::new(cell),
        }
    }
}

#[derive(Clone)]
struct ProjectRuntime {
    store: StoreHandle,
    catalog: Arc<RwLock<WorkflowProgramCatalog>>,
    scheduler: SessionScheduler,
    turns: TurnRuntime,
    skills: Arc<ProjectSkillCatalog>,
}

struct ProjectRuntimeFactory {
    base_catalog: WorkflowProgramCatalog,
    model: Arc<dyn ModelClient>,
    default_model: String,
    model_context_window: usize,
    turn_permits: Arc<Semaphore>,
    session_permits: Arc<Semaphore>,
}

impl ProjectRuntimeFactory {
    async fn build(&self, project: &Project, store: StoreHandle) -> anyhow::Result<ProjectRuntime> {
        let workflow_runtime_root = store.managed_root().join("workflow-runtime");
        let sandbox_root = store.managed_root().join("runtime/sandboxes");
        let runtime_root = workflow_runtime_root.clone();
        store
            .call::<_, anyhow::Error, _>(move |core| {
                core.reconcile_artifacts()
                    .context("failed to reconcile Artifact storage")?;
                reset_ephemeral_directory(&runtime_root)
                    .context("failed to reset Python WorkflowProgram runtime")?;
                reset_ephemeral_directory(&sandbox_root)
                    .context("failed to reset Agent sandboxes")?;
                Ok(())
            })
            .await?;
        let tools = ToolCatalog::builder()
            .register_workspace(ReadFileTool)
            .context("failed to register read_file")?
            .register_workspace(WriteFileTool)
            .context("failed to register write_file")?
            .register_workspace(FetchUrlTool)
            .context("failed to register fetch_url")?
            .register_workspace(ExecCommandTool)
            .context("failed to register exec_command")?
            .register_project(ReadResourceTool::new(store.clone()))
            .context("failed to register read_resource")?
            .build();
        let mut catalog = self.base_catalog.clone();
        let catalog_project = project.clone();
        catalog = store
            .call::<_, anyhow::Error, _>(move |core| {
                catalog
                    .load_project(&catalog_project, core)
                    .with_context(|| {
                        format!(
                            "failed to load WorkflowPrograms for Project {}",
                            catalog_project.id
                        )
                    })?;
                Ok(catalog)
            })
            .await?;
        let skills = Arc::new(ProjectSkillCatalog::new(store.clone()));
        skills.ensure_project(project.id).await?;
        let turns = TurnRuntime::new_with_permits(
            store.clone(),
            Arc::clone(&self.model),
            tools.clone(),
            Arc::clone(&skills),
            TurnRuntimeConfig {
                default_model: self.default_model.clone(),
                model_context_window: self.model_context_window,
                max_concurrent_turns: 1,
            },
            Arc::clone(&self.turn_permits),
        );
        let executor: Arc<dyn SessionExecutor> = Arc::new(PythonSessionExecutor::new(
            store.clone(),
            turns.clone(),
            catalog.python(),
            catalog.python_runtime_root(),
            workflow_runtime_root,
        ));
        let scheduler = SessionScheduler::new_with_permits(
            store.clone(),
            executor,
            Arc::clone(&self.session_permits),
        );
        scheduler
            .recover()
            .await
            .context("failed to recover unfinished Sessions")?;
        Ok(ProjectRuntime {
            store,
            catalog: Arc::new(RwLock::new(catalog)),
            scheduler,
            turns,
            skills,
        })
    }
}

fn reset_ephemeral_directory(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)?;
        }
        Ok(_) => anyhow::bail!("runtime path is not a real directory: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::create_dir_all(path)?;
    Ok(())
}

pub async fn initialize(config: &ServerConfig) -> anyhow::Result<AppState> {
    anyhow::ensure!(
        config.default_workspace_root.is_absolute(),
        "PaperMachine default Workspace root must be absolute"
    );
    let workflows_root = config.resource_root.join("workflows");
    let builtins_root = workflows_root.join("builtin");
    anyhow::ensure!(
        builtins_root.is_dir(),
        "PaperMachine built-in WorkflowProgram directory is missing: {}",
        builtins_root.display()
    );
    let python_runtime_root = config.resource_root.join("python");
    let validator = python_runtime_root.join("papermachine/_validate.py");
    anyhow::ensure!(
        validator.is_file(),
        "PaperMachine Python runtime is missing: {}",
        validator.display()
    );
    let base_catalog = WorkflowProgramCatalog::scan(
        &workflows_root,
        &python_runtime_root,
        LOCAL_TOOL_NAMES.into_iter().map(str::to_string),
    )
    .context("failed to load built-in WorkflowProgram catalog")?;
    let catalog = Arc::new(
        ProjectCatalog::open(&config.data_dir)
            .context("failed to open PaperMachine Project catalog")?,
    );
    catalog
        .quarantine_staging()
        .context("failed to quarantine incomplete Project staging state")?;
    let (model, default_model, model_context_window, mode, model_profiles, model_providers): InitializedModels = match &config.models {
        ServerModelConfig::Demo => (
            Arc::new(DemoModelClient),
            "demo-model".to_string(),
            DEFAULT_MODEL_CONTEXT_WINDOW,
            "demo",
            Vec::new(),
            Vec::new(),
        ),
        ServerModelConfig::Providers(configured) => {
            let model_context_window = configured
                .router
                .model_context_window(&configured.default_model)
                .context("configured default model has no context window")?;
            let model: Arc<dyn ModelClient> = Arc::new(configured.router.clone());
            (
                model,
                configured.default_model.clone(),
                model_context_window,
                "providers",
                configured.profiles.clone(),
                configured.providers.clone(),
            )
        }
    };

    let runtime_factory = Arc::new(ProjectRuntimeFactory {
        base_catalog,
        model: Arc::clone(&model),
        default_model: default_model.clone(),
        model_context_window,
        turn_permits: Arc::new(Semaphore::new(
            config
                .max_concurrent_runs
                .saturating_mul(config.max_parallel_actions)
                .max(1),
        )),
        session_permits: Arc::new(Semaphore::new(config.max_concurrent_runs.max(1))),
    });
    let (catalog_projects, failures) = catalog.scan_resilient()?;
    for failure in failures {
        tracing::warn!(
            path = %failure.path.display(),
            error = %failure.error,
            "skipping unavailable Project catalog entry"
        );
    }
    let mut projects = HashMap::new();
    let mut recoverable_projects = Vec::new();
    for entry in catalog_projects {
        let project = entry.project;
        let store = entry.store;
        match store.list_recoverable_sessions() {
            Ok(sessions) if !sessions.is_empty() => recoverable_projects.push(project.id),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    project_id = %project.id,
                    %error,
                    "could not inspect Project recovery state"
                );
            }
        }
        let store = StoreHandle::spawn(store)?;
        projects.insert(project.id, ProjectHandle::unloaded(store));
    }
    schedule_trash_cleanup(Arc::clone(&catalog), catalog.trash_entries()?);

    let state = AppState {
        catalog,
        default_workspace_root: config.default_workspace_root.clone(),
        projects: Arc::new(RwLock::new(projects)),
        runtime_factory,
        shutdown: CancellationToken::new(),
        generator: WorkflowGenerator::new(Arc::clone(&model), &default_model),
        default_model,
        model_context_window,
        model_profiles,
        model_providers,
        mode,
    };
    for project_id in recoverable_projects {
        if let Err(error) = state.project_runtime(project_id).await {
            tracing::error!(%project_id, %error, "failed to recover active Project runtime");
        }
    }
    Ok(state)
}

pub fn router(state: AppState, web_dist: PathBuf) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/workspaces/pick-directory", post(pick_workspace_directory))
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{project_id}",
            get(get_project_overview)
                .put(relocate_project)
                .delete(remove_project),
        )
        .route(
            "/projects/{project_id}/events/stream",
            get(stream_project_events),
        )
        .route(
            "/projects/{project_id}/system-prompt",
            get(get_project_system_prompt).put(update_project_system_prompt),
        )
        .route(
            "/projects/{project_id}/skills",
            get(list_project_skills).post(create_project_skill),
        )
        .route(
            "/projects/{project_id}/skills/{slug}",
            get(get_project_skill),
        )
        .route(
            "/projects/{project_id}/sessions",
            get(list_sessions).post(create_session),
        )
        .route(
            "/projects/{project_id}/sessions/{session_id}",
            get(get_session_view).delete(archive_session),
        )
        .route(
            "/projects/{project_id}/sessions/{session_id}/pause",
            post(pause_session),
        )
        .route(
            "/projects/{project_id}/sessions/{session_id}/resume",
            post(resume_session),
        )
        .route(
            "/projects/{project_id}/sessions/{session_id}/cancel",
            post(cancel_session),
        )
        .route(
            "/projects/{project_id}/sessions/{session_id}/agents/{agent_id}/control",
            post(create_control_message),
        )
        .route(
            "/projects/{project_id}/sessions/{session_id}/events",
            get(list_session_events),
        )
        .route(
            "/projects/{project_id}/sessions/{session_id}/events/stream",
            get(stream_session_events),
        )
        .route(
            "/projects/{project_id}/turns/{turn_id}/cancel",
            post(cancel_turn),
        )
        .route(
            "/projects/{project_id}/workflow-programs",
            get(list_workflow_programs).post(save_workflow_program),
        )
        .route(
            "/projects/{project_id}/workflow-programs/{slug}",
            get(get_workflow_program),
        )
        .route(
            "/projects/{project_id}/workflow-programs/validate",
            post(validate_workflow),
        )
        .route(
            "/projects/{project_id}/workflow-programs/generate",
            post(generate_workflow),
        )
        .route(
            "/projects/{project_id}/human-requests/{human_request_id}/answer",
            post(answer_human_request),
        )
        .route(
            "/projects/{project_id}/artifacts/{artifact_id}/content",
            get(artifact_content),
        );
    let index = web_dist.join("index.html");
    Router::new()
        .nest("/api", api)
        .fallback_service(ServeDir::new(web_dist).fallback(ServeFile::new(index)))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    model_mode: &'static str,
    default_model: String,
    model_context_window: usize,
    model_profiles: Vec<ModelProfile>,
    model_providers: Vec<ModelProviderInfo>,
    workflow_runtime: &'static str,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        model_mode: state.mode,
        default_model: state.default_model,
        model_context_window: state.model_context_window,
        model_profiles: state.model_profiles.clone(),
        model_providers: state.model_providers.clone(),
        workflow_runtime: "python_effect_dsl",
    })
}

#[derive(Serialize)]
struct ProjectCatalogEntry {
    #[serde(flatten)]
    project: Project,
    workspace_available: bool,
}

async fn list_projects(State(state): State<AppState>) -> ApiResult<Json<Vec<ProjectCatalogEntry>>> {
    let project_ids = state
        .projects
        .read()
        .await
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let mut entries = Vec::with_capacity(project_ids.len());
    for project_id in project_ids {
        let project = state
            .project_store(project_id)
            .await?
            .store()
            .call(move |store| store.get_project(project_id))
            .await?;
        entries.push(ProjectCatalogEntry {
            workspace_available: workspace_attachment_available(&project.workspace),
            project,
        });
    }
    entries.sort_by(|left, right| {
        right
            .project
            .updated_at
            .cmp(&left.project.updated_at)
            .then_with(|| left.project.id.cmp(&right.project.id))
    });
    Ok(Json(entries))
}

fn workspace_attachment_available(workspace: &WorkspaceAttachment) -> bool {
    let path = std::path::Path::new(&workspace.path);
    workspace.validate().is_ok()
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && path.canonicalize().is_ok_and(|canonical| canonical == path)
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectRequest {
    name: String,
    #[serde(default)]
    workspace: Option<WorkspaceSelection>,
}

async fn create_project(
    State(state): State<AppState>,
    Json(request): Json<CreateProjectRequest>,
) -> ApiResult<(StatusCode, Json<ProjectCatalogEntry>)> {
    if request.name.trim().is_empty() {
        return Err(ApiError::bad_request("Project name must not be empty"));
    }
    let name = request.name.trim();
    let workspace = match request.workspace.as_ref() {
        Some(selection) => canonical_workspace_selection(selection, true)?,
        None => next_default_workspace(&state, name).await?,
    };
    let catalog = Arc::clone(&state.catalog);
    let project_name = name.to_string();
    let catalog_workspace = workspace.clone();
    let entry = tokio::task::spawn_blocking(move || {
        catalog.create_project(&project_name, &catalog_workspace)
    })
    .await
    .map_err(|error| ApiError::internal(format!("Project creation task failed: {error}")))??;
    let project = entry.project;
    let store = StoreHandle::spawn(entry.store)?;
    let runtime = match state.runtime_factory.build(&project, store.clone()).await {
        Ok(runtime) => runtime,
        Err(error) => {
            store.shutdown().await?;
            let trash = retire_catalog_project(Arc::clone(&state.catalog), project.id).await?;
            schedule_trash_cleanup(Arc::clone(&state.catalog), vec![trash]);
            return Err(ApiError::internal(error.to_string()));
        }
    };
    state
        .projects
        .write()
        .await
        .insert(project.id, ProjectHandle::loaded(store, runtime));
    Ok((
        StatusCode::CREATED,
        Json(ProjectCatalogEntry {
            project,
            workspace_available: true,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PickWorkspaceDirectoryRequest {}

#[derive(Serialize)]
struct PickWorkspaceDirectoryResponse {
    path: Option<String>,
}

async fn pick_workspace_directory(
    Json(_request): Json<PickWorkspaceDirectoryRequest>,
) -> ApiResult<Json<PickWorkspaceDirectoryResponse>> {
    let selection = tokio::task::spawn_blocking(directory_picker::pick_directory)
        .await
        .map_err(|error| ApiError::internal(format!("Directory picker task failed: {error}")))?
        .map_err(ApiError::internal)?;
    Ok(Json(PickWorkspaceDirectoryResponse {
        path: selection.map(|path| path.to_string_lossy().into_owned()),
    }))
}

async fn next_default_workspace(state: &AppState, project_name: &str) -> ApiResult<PathBuf> {
    let catalog = Arc::clone(&state.catalog);
    let workspace_root = state.default_workspace_root.clone();
    let directory_name = default_workspace_directory_name(project_name);
    tokio::task::spawn_blocking(move || {
        let attached = catalog
            .scan()?
            .into_iter()
            .map(|entry| PathBuf::from(entry.project.workspace.path))
            .collect::<Vec<_>>();
        for index in 1_u64.. {
            let candidate = if index == 1 {
                workspace_root.join(&directory_name)
            } else {
                workspace_root.join(format!("{directory_name} {index}"))
            };
            if !candidate.exists() && !attached.iter().any(|path| path == &candidate) {
                return canonical_workspace(candidate.to_string_lossy().as_ref(), true);
            }
        }
        unreachable!("the default Workspace suffix space is unbounded")
    })
    .await
    .map_err(|error| ApiError::internal(format!("Workspace selection task failed: {error}")))?
}

async fn retire_catalog_project(
    catalog: Arc<ProjectCatalog>,
    project_id: ProjectId,
) -> ApiResult<PathBuf> {
    tokio::task::spawn_blocking(move || catalog.retire_project(project_id))
        .await
        .map_err(|error| ApiError::internal(format!("Project retirement task failed: {error}")))?
        .map_err(Into::into)
}

fn default_workspace_directory_name(project_name: &str) -> String {
    let mut name = String::new();
    for character in project_name.trim().chars().take(80) {
        if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
        {
            if !name.ends_with('-') {
                name.push('-');
            }
        } else {
            name.push(character);
        }
    }
    let name = name
        .trim_matches(|character: char| {
            character.is_whitespace() || character == '.' || character == '-'
        })
        .to_string();
    if name.is_empty() {
        "Project".to_string()
    } else {
        name
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectPathRequest {
    workspace: WorkspaceSelection,
}

async fn relocate_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<ProjectPathRequest>,
) -> ApiResult<Json<ProjectCatalogEntry>> {
    let project_id = parse_id(&project_id, "Project")?;
    let workspace = canonical_workspace_selection(&request.workspace, false)?;
    let projects = state.project_write(project_id).await?;
    let store = projects
        .get(&project_id)
        .expect("project_write verifies membership")
        .store
        .clone();
    let catalog = Arc::clone(&state.catalog);
    let project = store
        .call::<_, StoreError, _>(move |core| {
            ensure_project_can_detach(core)?;
            catalog.relocate_project(core, project_id, &workspace)
        })
        .await?;
    drop(projects);
    Ok(Json(ProjectCatalogEntry {
        project,
        workspace_available: true,
    }))
}

async fn remove_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<StatusCode> {
    let project_id = parse_id(&project_id, "Project")?;
    let mut projects = state.project_write(project_id).await?;
    let project = projects
        .get(&project_id)
        .expect("project_write verifies membership")
        .clone();
    project.store.call(ensure_project_can_detach).await?;
    let project = projects
        .remove(&project_id)
        .expect("verified Project must still exist");
    let managed_root = project.store.managed_root().to_path_buf();
    project.store.shutdown().await?;
    let trash = match retire_catalog_project(Arc::clone(&state.catalog), project_id).await {
        Ok(trash) => trash,
        Err(error) => {
            let reopened = StoreHandle::spawn(Store::open(&managed_root)?)?;
            projects.insert(project_id, ProjectHandle::unloaded(reopened));
            return Err(error);
        }
    };
    drop(projects);
    schedule_trash_cleanup(Arc::clone(&state.catalog), vec![trash]);
    Ok(StatusCode::NO_CONTENT)
}

fn schedule_trash_cleanup(catalog: Arc<ProjectCatalog>, paths: Vec<PathBuf>) {
    for path in paths {
        let catalog = Arc::clone(&catalog);
        tokio::task::spawn_blocking(move || {
            if let Err(error) = catalog.purge_trash_entry(&path) {
                tracing::warn!(path = %path.display(), %error, "failed to purge Project trash entry");
            }
        });
    }
}

fn canonical_workspace(value: &str, create: bool) -> ApiResult<PathBuf> {
    let workspace = PathBuf::from(value.trim());
    if !workspace.is_absolute() {
        return Err(ApiError::bad_request(
            "Project Workspace path must be absolute",
        ));
    }
    if create {
        std::fs::create_dir_all(&workspace).map_err(|error| StoreError::Io(error.to_string()))?;
    }
    workspace.canonicalize().map_err(|error| {
        ApiError::bad_request(format!("Project Workspace is unavailable: {error}"))
    })
}

fn canonical_workspace_selection(
    selection: &WorkspaceSelection,
    create: bool,
) -> ApiResult<PathBuf> {
    canonical_workspace(&selection.path, create)
}

fn ensure_project_can_detach(store: &Store) -> Result<(), StoreError> {
    if !store.list_recoverable_sessions()?.is_empty() {
        return Err(StoreError::Invariant(
            "Project has active work; finish or cancel it before changing its Workspace attachment"
                .to_string(),
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct ProjectOverview {
    project: Project,
    project_home: Option<ProjectHome>,
    project_home_artifact: Option<Artifact>,
    summary_session: Option<Session>,
}

async fn get_project_overview(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<ProjectOverview>> {
    let project_id = parse_id(&project_id, "Project")?;
    let store = state.project_store(project_id).await?.store();
    Ok(Json(
        store
            .call(move |core| {
                let project_home = core.get_project_home(project_id)?;
                let project_home_artifact = project_home
                    .as_ref()
                    .map(|home| core.get_artifact(home.artifact_id))
                    .transpose()?;
                Ok::<_, StoreError>(ProjectOverview {
                    project: core.get_project(project_id)?,
                    project_home,
                    project_home_artifact,
                    summary_session: core
                        .latest_project_session_for_program(project_id, "project-summary")?,
                })
            })
            .await?,
    ))
}

#[derive(Serialize)]
struct ProjectSessionUpdate {
    r#type: &'static str,
    session: Session,
}

#[derive(Serialize)]
struct StreamResyncUpdate {
    r#type: &'static str,
}

async fn stream_project_events(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let project_id = parse_id(&project_id, "Project")?;
    let store_handle = state.project_store(project_id).await?.store();
    let session_events = store_handle
        .call(move |core| {
            core.get_project(project_id)?;
            Ok::<_, StoreError>(core.subscribe())
        })
        .await?;
    let session_store = store_handle.clone();
    let sessions = BroadcastStream::new(session_events)
        .then(move |result| {
            let session_store = session_store.clone();
            async move {
                let event = match result {
                    Ok(event) => event,
                    Err(_) => return Some(stream_resync_sse_event("project_resync")),
                };
                if !matches!(
                    event.payload,
                    SessionEventPayload::SessionCreated { .. }
                        | SessionEventPayload::SessionChanged { .. }
                ) {
                    return None;
                }
                let session = session_store
                    .call(move |core| core.get_session(event.session_id))
                    .await
                    .ok()?;
                (session.project_id == project_id).then(|| {
                    project_update_sse_event(
                        "session_changed",
                        &ProjectSessionUpdate {
                            r#type: "session_changed",
                            session,
                        },
                    )
                })
            }
        })
        .filter_map(|event| event);
    Ok(event_stream(sessions, state.shutdown_token()))
}

fn event_stream(
    events: impl Stream<Item = Result<Event, Infallible>> + Send + 'static,
    shutdown: CancellationToken,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(futures::StreamExt::take_until(
        events,
        shutdown.cancelled_owned(),
    ))
    .keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

fn project_update_sse_event(
    event_type: &'static str,
    update: &impl Serialize,
) -> Result<Event, Infallible> {
    Ok(Event::default()
        .event(event_type)
        .data(serialize_sse_data(update)))
}

fn stream_resync_sse_event(event_type: &'static str) -> Result<Event, Infallible> {
    project_update_sse_event(event_type, &StreamResyncUpdate { r#type: event_type })
}

async fn get_project_system_prompt(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<ProjectSystemPrompt>> {
    let project_id = parse_id(&project_id, "Project")?;
    let store = state.project_store(project_id).await?.store();
    Ok(Json(
        store
            .call(move |core| core.get_project_system_prompt(project_id))
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemPromptRequest {
    #[serde(default)]
    system_prompt: String,
}

async fn update_project_system_prompt(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<SystemPromptRequest>,
) -> ApiResult<Json<ProjectSystemPrompt>> {
    let project_id = parse_id(&project_id, "Project")?;
    let store = state.project_store(project_id).await?.store();
    Ok(Json(
        store
            .call(move |core| core.set_project_system_prompt(project_id, request.system_prompt))
            .await?,
    ))
}

#[derive(Deserialize)]
struct SessionListQuery {
    #[serde(default = "default_session_list_limit")]
    limit: usize,
}

const fn default_session_list_limit() -> usize {
    100
}

async fn list_sessions(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<SessionListQuery>,
) -> ApiResult<Json<Vec<Session>>> {
    let id = parse_id(&project_id, "Project")?;
    if !(1..=200).contains(&query.limit) {
        return Err(ApiError::bad_request("Session list limit must be 1..=200"));
    }
    let store = state.project_store(id).await?.store();
    Ok(Json(
        store
            .call(move |core| core.list_recent_sessions(id, query.limit))
            .await?,
    ))
}

async fn list_project_skills(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<ProjectSkill>>> {
    let id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(id).await?;
    Ok(Json(runtime.skills.list(id).await?))
}

async fn get_project_skill(
    State(state): State<AppState>,
    Path((project_id, slug)): Path<(String, String)>,
) -> ApiResult<Json<ProjectSkill>> {
    let id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(id).await?;
    Ok(Json(runtime.skills.load(id, &slug).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectSkillRequest {
    slug: String,
    name: String,
    description: String,
    instructions: String,
}

async fn create_project_skill(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<CreateProjectSkillRequest>,
) -> ApiResult<(StatusCode, Json<ProjectSkill>)> {
    let id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(id).await?;
    let skill = runtime
        .skills
        .create(
            id,
            request.slug.trim(),
            request.name.trim(),
            request.description.trim(),
            request.instructions.trim(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(skill)))
}

#[derive(Serialize)]
struct SessionView {
    session: Session,
    agents: Vec<Agent>,
    turns: Vec<Turn>,
    steps: Vec<AgentStep>,
    rollouts: Vec<AgentRolloutView>,
    effects: Vec<SessionEffect>,
    actions: Vec<ActionInvocation>,
    attempts: Vec<ActionAttempt>,
    human_requests: Vec<HumanRequest>,
    control_messages: Vec<ControlMessage>,
    artifacts: Vec<Artifact>,
}

#[derive(Serialize)]
struct AgentRolloutView {
    agent_id: AgentId,
    status: AgentRolloutStatus,
}

#[derive(Serialize)]
struct SessionStreamUpdate {
    #[serde(flatten)]
    event: SessionEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<Session>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<Turn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<AgentStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<Agent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    human_request: Option<HumanRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<ActionInvocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<ActionAttempt>,
}

async fn get_session_view(
    State(state): State<AppState>,
    Path((project_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<SessionView>> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let runtime = state.project_runtime(project_id).await?;
    let session = runtime
        .store
        .call(move |store| store.get_session(session_id))
        .await?;
    if session.project_id != project_id {
        return Err(ApiError::not_found(format!("Session {session_id}")));
    }
    let (agents, turns, steps, rollouts, effects, actions, attempts, requests, controls, artifacts) =
        runtime
            .store
            .call(move |store| {
                let agents = store.list_agents(session_id)?;
                let rollouts = agents
                    .iter()
                    .map(|agent| {
                        Ok::<_, StoreError>(AgentRolloutView {
                            agent_id: agent.id,
                            status: store.agent_rollout_status(agent.id)?,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, StoreError>((
                    agents,
                    store.list_session_turns(session_id)?,
                    store.list_session_steps(session_id)?,
                    rollouts,
                    store.list_session_effects(session_id)?,
                    store.list_action_invocations(session_id)?,
                    store.list_session_action_attempts(session_id)?,
                    store.list_human_requests(session_id)?,
                    store.list_control_messages(session_id)?,
                    store.list_artifacts(session_id)?,
                ))
            })
            .await?;
    Ok(Json(SessionView {
        session,
        agents,
        turns,
        steps,
        rollouts,
        effects,
        actions,
        attempts,
        human_requests: requests,
        control_messages: controls,
        artifacts,
    }))
}

async fn archive_session(
    State(state): State<AppState>,
    Path((project_id, session_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let runtime = state.project_runtime(project_id).await?;
    let session = runtime
        .store
        .call(move |store| store.get_session(session_id))
        .await?;
    if session.archived_at.is_some() {
        return Ok(StatusCode::NO_CONTENT);
    }
    if !session.status.is_terminal() {
        match runtime.scheduler.cancel(session_id).await {
            Ok(()) | Err(SessionSchedulerError::TerminalSession { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    runtime
        .store
        .call(move |store| store.archive_session(session_id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel_turn(
    State(state): State<AppState>,
    Path((project_id, turn_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let turn_id: TurnId = parse_id(&turn_id, "Turn")?;
    let runtime = state.project_runtime(project_id).await?;
    runtime
        .store
        .call(move |store| store.get_turn(turn_id))
        .await?;
    runtime.turns.cancel(turn_id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn list_workflow_programs(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<WorkflowProgram>>> {
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    Ok(Json(
        runtime
            .catalog
            .read()
            .await
            .list(project_id)
            .into_iter()
            .map(|item| item.registration)
            .collect(),
    ))
}

#[derive(Serialize)]
struct WorkflowProgramSourceResponse {
    registration: WorkflowProgram,
    source: String,
    validation: WorkflowValidation,
}

async fn get_workflow_program(
    State(state): State<AppState>,
    Path((project_id, slug)): Path<(String, String)>,
) -> ApiResult<Json<WorkflowProgramSourceResponse>> {
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    let catalog = runtime.catalog.read().await;
    let program = catalog
        .get(project_id, &slug)
        .ok_or_else(|| ApiError::not_found(format!("WorkflowProgram {slug}")))?;
    Ok(Json(WorkflowProgramSourceResponse {
        registration: program.registration.clone(),
        source: program.source_code.clone(),
        validation: program.validation.clone(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowSourceRequest {
    source: String,
}

async fn validate_workflow(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<WorkflowSourceRequest>,
) -> ApiResult<Json<WorkflowValidation>> {
    let runtime = state
        .project_runtime(parse_id(&project_id, "Project")?)
        .await?;
    Ok(Json(
        runtime
            .catalog
            .read()
            .await
            .validate_source(&request.source)?,
    ))
}

#[derive(Serialize)]
struct GeneratedWorkflowResponse {
    source: String,
    validation: WorkflowValidation,
}

async fn generate_workflow(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<WorkflowGenerationRequest>,
) -> ApiResult<Json<GeneratedWorkflowResponse>> {
    let runtime = state
        .project_runtime(parse_id(&project_id, "Project")?)
        .await?;
    let source = state
        .generator
        .generate(request, CancellationToken::new())
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let validation = runtime.catalog.read().await.validate_source(&source)?;
    Ok(Json(GeneratedWorkflowResponse { source, validation }))
}

async fn save_workflow_program(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<WorkflowSourceRequest>,
) -> ApiResult<(StatusCode, Json<WorkflowProgram>)> {
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    let mut guard = Arc::clone(&runtime.catalog).write_owned().await;
    let mut catalog = guard.clone();
    let source = request.source;
    let (updated, loaded) = runtime
        .store
        .call(move |store| {
            let project = store.get_project(project_id)?;
            let loaded = catalog.save_user(&project, &source, store)?;
            Ok::<_, WorkflowProgramCatalogError>((catalog, loaded))
        })
        .await?;
    *guard = updated;
    Ok((StatusCode::CREATED, Json(loaded.registration)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSessionRequest {
    program_slug: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    instructions: String,
    #[serde(default = "empty_object")]
    params: Value,
    #[serde(default)]
    source_session_id: Option<SessionId>,
    model: String,
    access: AccessPreset,
    #[serde(default)]
    enabled_skills: Vec<String>,
    #[serde(default)]
    agent_access_overrides: BTreeMap<String, AccessPreset>,
}

fn empty_object() -> Value {
    json!({})
}

fn validate_model_profile(state: &AppState, model: &str) -> ApiResult<()> {
    let valid = if state.model_profiles.is_empty() {
        model == state.default_model
    } else {
        state
            .model_profiles
            .iter()
            .any(|profile| profile.id == model)
    };
    if !valid {
        let available = if state.model_profiles.is_empty() {
            state.default_model.clone()
        } else {
            state
                .model_profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(ApiError::bad_request(format!(
            "unknown model profile {model:?}; choose one of: {available}",
        )));
    }
    Ok(())
}

fn validate_model_profile_params(state: &AppState, schema: &Value, value: &Value) -> ApiResult<()> {
    if schema.get("format").and_then(Value::as_str) == Some("model-profile")
        && let Some(model) = value
            .as_str()
            .map(str::trim)
            .filter(|model| !model.is_empty())
    {
        validate_model_profile(state, model)?;
    }

    if let (Some(properties), Some(values)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (name, child_schema) in properties {
            if let Some(child_value) = values.get(name) {
                validate_model_profile_params(state, child_schema, child_value)?;
            }
        }
    }

    if let (Some(item_schema), Some(items)) = (schema.get("items"), value.as_array()) {
        for item in items {
            validate_model_profile_params(state, item_schema, item)?;
        }
    }
    Ok(())
}

async fn create_session(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    runtime
        .skills
        .validate_enabled(project_id, &request.enabled_skills)
        .await?;
    let model = request.model.trim().to_string();
    if model.is_empty() {
        return Err(ApiError::bad_request(
            "Session model must name an explicit model profile",
        ));
    }
    validate_model_profile(&state, &model)?;
    let (snapshot, declared_agent_classes) = {
        let catalog = runtime.catalog.read().await;
        let program = catalog
            .get(project_id, &request.program_slug)
            .ok_or_else(|| {
                ApiError::not_found(format!("WorkflowProgram {}", request.program_slug))
            })?;
        (
            program.snapshot(),
            program
                .validation
                .agents
                .iter()
                .map(|agent| agent.class_name.clone())
                .collect::<std::collections::HashSet<_>>(),
        )
    };
    let user_task = request
        .request
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    match snapshot.manifest.request_mode {
        WorkflowRequestMode::Required if user_task.is_empty() => {
            return Err(ApiError::bad_request(format!(
                "WorkflowProgram {:?} requires a user task",
                snapshot.manifest.slug
            )));
        }
        WorkflowRequestMode::None if !user_task.is_empty() => {
            return Err(ApiError::bad_request(format!(
                "WorkflowProgram {:?} starts without a user task",
                snapshot.manifest.slug
            )));
        }
        WorkflowRequestMode::Required | WorkflowRequestMode::None => {}
    }
    if let Some(unknown) = request
        .agent_access_overrides
        .keys()
        .find(|class_name| !declared_agent_classes.contains(*class_name))
    {
        return Err(ApiError::bad_request(format!(
            "Agent access override references unknown class {unknown:?}"
        )));
    }
    validate_schema_value(&snapshot.manifest.params_schema, &request.params, "params")
        .map_err(ApiError::bad_request)?;
    validate_model_profile_params(&state, &snapshot.manifest.params_schema, &request.params)?;
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| snapshot.manifest.name.clone());
    let session = runtime
        .store
        .call(move |store| {
            store.create_session(NewSession {
                project_id,
                program: snapshot,
                title,
                request: user_task,
                instructions: request.instructions.trim().to_string(),
                trigger: SessionTrigger {
                    kind: if request.source_session_id.is_some() {
                        SessionTriggerKind::User
                    } else {
                        SessionTriggerKind::Manual
                    },
                    source_session_id: request.source_session_id,
                },
                params: request.params,
                default_model: model,
                access: request.access,
                enabled_skills: request.enabled_skills,
                agent_access_overrides: request.agent_access_overrides,
            })
        })
        .await?;
    runtime.scheduler.start(session.id).await?;
    Ok((StatusCode::CREATED, Json(session)))
}
async fn pause_session(
    State(state): State<AppState>,
    Path((project_id, session_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let runtime = state.project_runtime(project_id).await?;
    runtime.scheduler.pause(session_id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn resume_session(
    State(state): State<AppState>,
    Path((project_id, session_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let runtime = state.project_runtime(project_id).await?;
    runtime.scheduler.resume(session_id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_session(
    State(state): State<AppState>,
    Path((project_id, session_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let runtime = state.project_runtime(project_id).await?;
    runtime.scheduler.cancel(session_id).await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    kind: ControlMessageKind,
    content: String,
    action_invocation_id: Option<ActionInvocationId>,
}

async fn create_control_message(
    State(state): State<AppState>,
    Path((project_id, session_id, agent_id)): Path<(String, String, String)>,
    Json(request): Json<ControlRequest>,
) -> ApiResult<(StatusCode, Json<ControlMessage>)> {
    if request.content.trim().is_empty() {
        return Err(ApiError::bad_request("control message must not be empty"));
    }
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let agent_id: AgentId = parse_id(&agent_id, "Agent")?;
    let runtime = state.project_runtime(project_id).await?;
    let content = request.content.trim().to_string();
    let message = runtime
        .store
        .call(move |store| {
            let session = store.get_session(session_id)?;
            if session.project_id != project_id {
                return Err(StoreError::Invariant(
                    "Session belongs to another Project".to_string(),
                ));
            }
            store.create_control_message(
                session_id,
                agent_id,
                request.action_invocation_id,
                request.kind,
                &content,
            )
        })
        .await?;
    Ok((StatusCode::CREATED, Json(message)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HumanAnswerRequest {
    answer: Value,
}

async fn answer_human_request(
    State(state): State<AppState>,
    Path((project_id, request_id)): Path<(String, String)>,
    Json(request): Json<HumanAnswerRequest>,
) -> ApiResult<Json<HumanRequest>> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let id: HumanRequestId = parse_id(&request_id, "HumanRequest")?;
    let runtime = state.project_runtime(project_id).await?;
    let current = runtime
        .store
        .call(move |store| store.get_human_request(id))
        .await?;
    validate_schema_value(&current.response_schema, &request.answer, "answer")
        .map_err(ApiError::bad_request)?;
    Ok(Json(
        runtime
            .store
            .call(move |store| store.answer_human_request(id, request.answer))
            .await?,
    ))
}

#[derive(Default, Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: u64,
}

async fn list_session_events(
    State(state): State<AppState>,
    Path((project_id, session_id)): Path<(String, String)>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Vec<SessionEvent>>> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let runtime = state.project_runtime(project_id).await?;
    Ok(Json(
        runtime
            .store
            .call(move |store| {
                store.get_session(session_id)?;
                store.list_session_events(session_id, query.after)
            })
            .await?,
    ))
}

async fn stream_session_events(
    State(state): State<AppState>,
    Path((project_id, session_id)): Path<(String, String)>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let runtime = state.project_runtime(project_id).await?;
    let (receiver, replay) = runtime
        .store
        .call(move |store| {
            store.get_session(session_id)?;
            Ok::<_, StoreError>((
                store.subscribe(),
                store.list_session_events(session_id, query.after)?,
            ))
        })
        .await?;
    let high_watermark = replay.last().map_or(query.after, |event| event.sequence);
    let replay_store = runtime.store.clone();
    let replay = tokio_stream::iter(replay).then(move |event| {
        let replay_store = replay_store.clone();
        async move { session_sse_event(&replay_store, event).await }
    });
    let live_store = runtime.store.clone();
    let live = BroadcastStream::new(receiver)
        .then(move |result| {
            let live_store = live_store.clone();
            async move {
                match result {
                    Ok(event)
                        if event.session_id == session_id
                            && (event.sequence == 0 || event.sequence > high_watermark) =>
                    {
                        Some(session_sse_event(&live_store, event).await)
                    }
                    Ok(_) => None,
                    Err(_) => Some(stream_resync_sse_event("session_resync")),
                }
            }
        })
        .filter_map(|event| event);
    Ok(event_stream(replay.chain(live), state.shutdown_token()))
}

async fn session_sse_event(store: &StoreHandle, event: SessionEvent) -> Result<Event, Infallible> {
    let event_type = event_type(&event.payload);
    let include_session = matches!(
        &event.payload,
        SessionEventPayload::SessionCreated { .. }
            | SessionEventPayload::SessionChanged { .. }
            | SessionEventPayload::AgentCreated { .. }
            | SessionEventPayload::UsageUpdated { .. }
            | SessionEventPayload::HumanRequestOpened { .. }
            | SessionEventPayload::HumanRequestResolved { .. }
    );
    let include_turn = matches!(
        &event.payload,
        SessionEventPayload::TurnCreated | SessionEventPayload::TurnStatusChanged { .. }
    );
    let include_step = matches!(
        &event.payload,
        SessionEventPayload::ModelStepStarted
            | SessionEventPayload::ModelStepCompleted
            | SessionEventPayload::ModelStepFailed
            | SessionEventPayload::ToolCallStarted
            | SessionEventPayload::ToolCallCompleted
            | SessionEventPayload::HostedToolCompleted
    );
    let human_request_id = match &event.payload {
        SessionEventPayload::HumanRequestOpened {
            human_request_id, ..
        }
        | SessionEventPayload::HumanRequestResolved {
            human_request_id, ..
        } => Some(*human_request_id),
        _ => None,
    };
    let include_agent = matches!(&event.payload, SessionEventPayload::AgentCreated { .. });
    let action_ids = match &event.payload {
        SessionEventPayload::ActionChanged {
            action_invocation_id,
            action_attempt_id,
            ..
        } => Some((*action_invocation_id, *action_attempt_id)),
        _ => None,
    };
    let update = match store
        .call(move |store| {
            Ok::<_, StoreError>(SessionStreamUpdate {
                session: include_session
                    .then(|| store.get_session(event.session_id).ok())
                    .flatten(),
                turn: include_turn
                    .then(|| event.turn_id.and_then(|id| store.get_turn(id).ok()))
                    .flatten(),
                step: include_step
                    .then(|| event.step_id.and_then(|id| store.get_step(id).ok()))
                    .flatten(),
                agent: include_agent
                    .then(|| event.agent_id.and_then(|id| store.get_agent(id).ok()))
                    .flatten(),
                human_request: human_request_id.and_then(|id| store.get_human_request(id).ok()),
                action: action_ids.and_then(|(id, _)| store.get_action_invocation(id).ok()),
                attempt: action_ids
                    .and_then(|(_, id)| id.and_then(|id| store.get_action_attempt(id).ok())),
                event,
            })
        })
        .await
    {
        Ok(update) => update,
        Err(_) => return stream_resync_sse_event("session_resync"),
    };
    let data = serialize_sse_data(&update);
    let mut result = Event::default().event(event_type).data(data);
    if update.event.sequence > 0 {
        result = result.id(update.event.sequence.to_string());
    }
    Ok(result)
}

fn event_type(payload: &impl Serialize) -> String {
    serde_json::to_value(payload)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "event".to_string())
}

fn serialize_sse_data(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|error| format!(r#"{{"error":"{error}"}}"#))
}

async fn artifact_content(
    State(state): State<AppState>,
    Path((project_id, artifact_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let project_id: ProjectId = parse_id(&project_id, "Project")?;
    let artifact_id: ArtifactId = parse_id(&artifact_id, "Artifact")?;
    let runtime = state.project_runtime(project_id).await?;
    let artifact = runtime
        .store
        .call(move |store| store.get_artifact(artifact_id))
        .await?;
    let stored_artifact = artifact.clone();
    let bytes = runtime
        .store
        .call(move |store| store.read_artifact(&stored_artifact))
        .await?;
    let media_type = HeaderValue::from_str(&artifact.media_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, media_type);
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    if artifact.media_type.starts_with("text/html") {
        response.headers_mut().insert(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:",
            ),
        );
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok(response)
}

fn validate_schema_value(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    if schema.as_object().is_none_or(serde_json::Map::is_empty) {
        return Ok(());
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{path} is not one of the allowed values"));
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value
                .as_object()
                .ok_or_else(|| format!("{path} must be an object"))?;
            let properties = schema.get("properties").and_then(Value::as_object);
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(key) {
                        return Err(format!("{path}.{key} is required"));
                    }
                }
            }
            if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
                for key in object.keys() {
                    if !properties.is_some_and(|properties| properties.contains_key(key)) {
                        return Err(format!("{path}.{key} is not an allowed field"));
                    }
                }
            }
            if let Some(properties) = properties {
                for (key, child) in object {
                    if let Some(child_schema) = properties.get(key) {
                        validate_schema_value(child_schema, child, &format!("{path}.{key}"))?;
                    }
                }
            }
        }
        Some("array") => {
            let array = value
                .as_array()
                .ok_or_else(|| format!("{path} must be an array"))?;
            if let Some(items) = schema.get("items") {
                for (index, item) in array.iter().enumerate() {
                    validate_schema_value(items, item, &format!("{path}[{index}]"))?;
                }
            }
        }
        Some("string") if !value.is_string() => return Err(format!("{path} must be a string")),
        Some("integer") if !value.is_i64() && !value.is_u64() => {
            return Err(format!("{path} must be an integer"));
        }
        Some("number") if !value.is_number() => return Err(format!("{path} must be a number")),
        Some("boolean") if !value.is_boolean() => return Err(format!("{path} must be a boolean")),
        Some("null") if !value.is_null() => return Err(format!("{path} must be null")),
        _ => {}
    }
    Ok(())
}

fn parse_id<T: FromStr>(value: &str, kind: &str) -> ApiResult<T> {
    value
        .parse()
        .map_err(|_| ApiError::bad_request(format!("invalid {kind} id: {value}")))
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn not_found(entity: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("{} not found", entity.into()),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        let status = match error {
            StoreError::NotFound { .. } => StatusCode::NOT_FOUND,
            StoreError::Invariant(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl From<SessionSchedulerError> for ApiError {
    fn from(error: SessionSchedulerError) -> Self {
        let status = match &error {
            SessionSchedulerError::Store(StoreError::NotFound { .. }) => StatusCode::NOT_FOUND,
            SessionSchedulerError::TerminalSession { .. }
            | SessionSchedulerError::NotScheduled(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl From<WorkflowProgramCatalogError> for ApiError {
    fn from(error: WorkflowProgramCatalogError) -> Self {
        let status = match error {
            WorkflowProgramCatalogError::Invalid(_)
            | WorkflowProgramCatalogError::InvalidFile { .. } => StatusCode::BAD_REQUEST,
            WorkflowProgramCatalogError::Duplicate { .. } => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl From<TurnRuntimeError> for ApiError {
    fn from(error: TurnRuntimeError) -> Self {
        let status = match &error {
            TurnRuntimeError::Store(StoreError::NotFound { .. }) => StatusCode::NOT_FOUND,
            TurnRuntimeError::Store(StoreError::Invariant(_))
            | TurnRuntimeError::TerminalTurn(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl From<SkillError> for ApiError {
    fn from(error: SkillError) -> Self {
        let status = match error {
            SkillError::Store(StoreError::NotFound { .. }) => StatusCode::NOT_FOUND,
            SkillError::Store(StoreError::Invariant(_)) => StatusCode::CONFLICT,
            SkillError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SkillError::NotFound(_) => StatusCode::NOT_FOUND,
            SkillError::AlreadyExists(_) => StatusCode::CONFLICT,
            SkillError::Invalid(_) | SkillError::Yaml(_) => StatusCode::BAD_REQUEST,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[tokio::test]
    async fn event_stream_closes_on_application_shutdown() {
        let shutdown = CancellationToken::new();
        let response = event_stream(stream::pending(), shutdown.clone()).into_response();
        let mut body = response.into_body().into_data_stream();

        shutdown.cancel();

        let next = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("SSE body should close promptly");
        assert!(next.is_none());
    }
}
