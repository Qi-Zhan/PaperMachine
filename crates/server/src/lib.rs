//! HTTP and realtime API for PaperMachine.

mod demo_model;
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
use axum::routing::put;
use papermachine_model::ConfiguredModels;
use papermachine_model::DEFAULT_MODEL_CONTEXT_WINDOW;
use papermachine_model::ModelClient;
use papermachine_model::ModelProfile;
use papermachine_model::ModelProviderInfo;
use papermachine_protocol::*;
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeConfig;
use papermachine_session::SessionRuntimeError;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_skills::SkillError;
use papermachine_store::NewWorkflow;
use papermachine_store::ProjectLibrary;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_tools::ExecCommandTool;
use papermachine_tools::FetchUrlTool;
use papermachine_tools::ReadFileTool;
use papermachine_tools::ToolRegistry;
use papermachine_tools::WriteFileTool;
use papermachine_workflow::ProjectSnapshotOptions;
use papermachine_workflow::PythonWorkflowRuntime;
use papermachine_workflow::WorkflowGenerationRequest;
use papermachine_workflow::WorkflowGenerator;
use papermachine_workflow::WorkflowProgramCatalog;
use papermachine_workflow::WorkflowProgramCatalogError;
use papermachine_workflow::WorkflowRuntime;
use papermachine_workflow::WorkflowScheduler;
use papermachine_workflow::WorkflowSchedulerError;
use papermachine_workflow::build_project_snapshot;
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
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio_stream::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tower_http::cors::Any;
use tower_http::cors::CorsLayer;
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

#[derive(Clone)]
pub struct AppState {
    library: Arc<ProjectLibrary>,
    projects: Arc<RwLock<HashMap<ProjectId, ProjectRuntime>>>,
    runtime_factory: Arc<ProjectRuntimeFactory>,
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

    async fn project_runtime(&self, project_id: ProjectId) -> Result<ProjectRuntime, StoreError> {
        self.projects
            .read()
            .await
            .get(&project_id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound {
                entity: "available project",
                id: project_id.to_string(),
            })
    }

    async fn locate<T>(
        &self,
        entity: &'static str,
        id: &str,
        lookup: impl Fn(&Store) -> Result<T, StoreError>,
    ) -> Result<(ProjectRuntime, T), StoreError> {
        let projects = self
            .projects
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for project in projects {
            match lookup(&project.store) {
                Ok(value) => return Ok((project, value)),
                Err(StoreError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Err(StoreError::NotFound {
            entity,
            id: id.to_string(),
        })
    }
}

#[derive(Clone)]
struct ProjectRuntime {
    store: Arc<Store>,
    catalog: Arc<RwLock<WorkflowProgramCatalog>>,
    scheduler: WorkflowScheduler,
    sessions: SessionRuntime,
    skills: Arc<ProjectSkillCatalog>,
}

struct ProjectRuntimeFactory {
    workflows_root: PathBuf,
    python_runtime_root: PathBuf,
    model: Arc<dyn ModelClient>,
    tools: ToolRegistry,
    default_model: String,
    model_context_window: usize,
    turn_permits: Arc<Semaphore>,
    workflow_permits: Arc<Semaphore>,
}

impl ProjectRuntimeFactory {
    fn open_store(&self, project: &Project) -> Result<Arc<Store>, StoreError> {
        let metadata_root = PathBuf::from(&project.root_path).join(".papermachine");
        Ok(Arc::new(Store::open(
            metadata_root.join("state/project.db"),
            metadata_root.join("artifacts"),
        )?))
    }

    async fn build(&self, project: &Project, store: Arc<Store>) -> anyhow::Result<ProjectRuntime> {
        let mut catalog =
            WorkflowProgramCatalog::scan(&self.workflows_root, &self.python_runtime_root, &store)
                .context("failed to load built-in Workflow catalog")?;
        catalog
            .load_project(project, &store)
            .with_context(|| format!("failed to load Workflows for Project {}", project.id))?;
        let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
        skills.ensure_project(project.id)?;
        let sessions = SessionRuntime::new_with_permits(
            Arc::clone(&store),
            Arc::clone(&self.model),
            self.tools.clone(),
            Arc::clone(&skills),
            SessionRuntimeConfig {
                default_model: self.default_model.clone(),
                model_context_window: self.model_context_window,
                max_concurrent_turns: 1,
            },
            Arc::clone(&self.turn_permits),
        );
        let executor: Arc<dyn WorkflowRuntime> = Arc::new(PythonWorkflowRuntime::new(
            Arc::clone(&store),
            sessions.clone(),
            catalog.python(),
            catalog.python_runtime_root(),
            PathBuf::from(&project.root_path).join(".papermachine/workflow-runtime"),
        ));
        let scheduler = WorkflowScheduler::new_with_permits(
            Arc::clone(&store),
            executor,
            Arc::clone(&self.workflow_permits),
        );
        sessions
            .recover()
            .await
            .context("failed to recover unfinished standalone Session Turns")?;
        scheduler
            .recover()
            .await
            .context("failed to recover unfinished Workflows")?;
        Ok(ProjectRuntime {
            store,
            catalog: Arc::new(RwLock::new(catalog)),
            scheduler,
            sessions,
            skills,
        })
    }
}

pub async fn initialize(config: &ServerConfig) -> anyhow::Result<AppState> {
    let workflows_root = config.resource_root.join("workflows");
    let builtins_root = workflows_root.join("builtin");
    anyhow::ensure!(
        builtins_root.is_dir(),
        "PaperMachine built-in Workflow directory is missing: {}",
        builtins_root.display()
    );
    let python_runtime_root = config.resource_root.join("python");
    let validator = python_runtime_root.join("papermachine/_validate.py");
    anyhow::ensure!(
        validator.is_file(),
        "PaperMachine Python runtime is missing: {}",
        validator.display()
    );
    let library = Arc::new(
        ProjectLibrary::open(config.data_dir.join("library.db"))
            .context("failed to open PaperMachine Project library")?,
    );
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

    let tools = ToolRegistry::builder()
        .register(ReadFileTool)
        .context("failed to register read_file")?
        .register(WriteFileTool)
        .context("failed to register write_file")?
        .register(FetchUrlTool)
        .context("failed to register fetch_url")?
        .register(ExecCommandTool)
        .context("failed to register exec_command")?
        .build();
    let runtime_factory = Arc::new(ProjectRuntimeFactory {
        workflows_root,
        python_runtime_root,
        model: Arc::clone(&model),
        tools,
        default_model: default_model.clone(),
        model_context_window,
        turn_permits: Arc::new(Semaphore::new(
            config
                .max_concurrent_runs
                .saturating_mul(config.max_parallel_actions)
                .max(1),
        )),
        workflow_permits: Arc::new(Semaphore::new(config.max_concurrent_runs.max(1))),
    });
    let mut projects = HashMap::new();
    for project in library.list()? {
        let database = PathBuf::from(&project.root_path).join(".papermachine/state/project.db");
        if !database.is_file() {
            continue;
        }
        let store = runtime_factory.open_store(&project)?;
        let stored_project = store.get_project(project.id)?;
        projects.insert(
            project.id,
            runtime_factory.build(&stored_project, store).await?,
        );
    }

    Ok(AppState {
        library,
        projects: Arc::new(RwLock::new(projects)),
        runtime_factory,
        generator: WorkflowGenerator::new(Arc::clone(&model), &default_model),
        default_model,
        model_context_window,
        model_profiles,
        model_providers,
        mode,
    })
}

pub fn router(state: AppState, web_dist: PathBuf) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/open", post(open_project))
        .route(
            "/projects/{project_id}",
            get(get_project_overview)
                .put(relocate_project)
                .delete(remove_project),
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
        .route("/projects/{project_id}/sessions", get(list_sessions))
        .route("/sessions/{session_id}", get(get_session_view))
        .route("/sessions/{session_id}/turns", post(create_turn))
        .route("/sessions/{session_id}/skills", put(update_session_skills))
        .route(
            "/sessions/{session_id}/system-prompt",
            put(update_session_system_prompt),
        )
        .route("/sessions/{session_id}/access", put(update_session_access))
        .route(
            "/sessions/{session_id}/workflows",
            get(list_session_workflows),
        )
        .route("/sessions/{session_id}/events", get(list_session_events))
        .route(
            "/sessions/{session_id}/events/stream",
            get(stream_session_events),
        )
        .route("/turns/{turn_id}/cancel", post(cancel_turn))
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
            "/projects/{project_id}/workflows",
            get(list_project_workflows).post(create_workflow),
        )
        .route("/workflows/{workflow_id}", get(get_workflow_view))
        .route("/workflows/{workflow_id}/pause", post(pause_workflow))
        .route("/workflows/{workflow_id}/resume", post(resume_workflow))
        .route("/workflows/{workflow_id}/cancel", post(cancel_workflow))
        .route(
            "/workflows/{workflow_id}/sessions/{session_id}/control",
            post(create_control_message),
        )
        .route("/workflows/{workflow_id}/events", get(list_workflow_events))
        .route(
            "/workflows/{workflow_id}/events/stream",
            get(stream_workflow_events),
        )
        .route(
            "/human-requests/{human_request_id}/answer",
            post(answer_human_request),
        )
        .route("/artifacts/{artifact_id}/content", get(artifact_content));
    let index = web_dist.join("index.html");
    Router::new()
        .nest("/api", api)
        .fallback_service(ServeDir::new(web_dist).fallback(ServeFile::new(index)))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(Any)
                .allow_methods(Any),
        )
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
struct ProjectLibraryEntry {
    #[serde(flatten)]
    project: Project,
    available: bool,
}

async fn list_projects(State(state): State<AppState>) -> ApiResult<Json<Vec<ProjectLibraryEntry>>> {
    let available = state
        .projects
        .read()
        .await
        .keys()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    Ok(Json(
        state
            .library
            .list()?
            .into_iter()
            .map(|project| ProjectLibraryEntry {
                available: available.contains(&project.id),
                project,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProjectRequest {
    name: String,
    #[serde(default)]
    description: String,
    root_path: String,
}

async fn create_project(
    State(state): State<AppState>,
    Json(request): Json<CreateProjectRequest>,
) -> ApiResult<(StatusCode, Json<ProjectLibraryEntry>)> {
    if request.name.trim().is_empty() {
        return Err(ApiError::bad_request("Project name must not be empty"));
    }
    if request.root_path.trim().is_empty() {
        return Err(ApiError::bad_request("Project root path must not be empty"));
    }
    let requested_root = PathBuf::from(request.root_path.trim());
    if !requested_root.is_absolute() {
        return Err(ApiError::bad_request("Project root path must be absolute"));
    }
    std::fs::create_dir_all(&requested_root).map_err(|error| StoreError::Io(error.to_string()))?;
    let root = requested_root
        .canonicalize()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if state
        .library
        .list()?
        .iter()
        .any(|project| PathBuf::from(&project.root_path) == root)
    {
        return Err(StoreError::Invariant(format!(
            "Project directory is already registered: {}",
            root.display()
        ))
        .into());
    }
    let metadata_root = root.join(".papermachine");
    let store = Arc::new(Store::open(
        metadata_root.join("state/project.db"),
        metadata_root.join("artifacts"),
    )?);
    let project = store.create_project(request.name.trim(), request.description.trim(), &root)?;
    state.library.register(&project)?;
    let runtime = state
        .runtime_factory
        .build(&project, store)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    state.projects.write().await.insert(project.id, runtime);
    Ok((
        StatusCode::CREATED,
        Json(ProjectLibraryEntry {
            project,
            available: true,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectPathRequest {
    root_path: String,
}

async fn open_project(
    State(state): State<AppState>,
    Json(request): Json<ProjectPathRequest>,
) -> ApiResult<(StatusCode, Json<ProjectLibraryEntry>)> {
    let root = canonical_project_root(&request.root_path)?;
    let (stored_project, store) = inspect_project_store(&root)?;
    if state.projects.read().await.contains_key(&stored_project.id) {
        let registered = state.library.get(stored_project.id)?;
        if PathBuf::from(&registered.root_path) != root {
            return Err(ApiError::bad_request(
                "Project is already open at another directory; use relocate",
            ));
        }
        let project = store.relocate_project(stored_project.id, &root)?;
        return Ok((
            StatusCode::OK,
            Json(ProjectLibraryEntry {
                project,
                available: true,
            }),
        ));
    }
    let project = store.relocate_project(stored_project.id, &root)?;
    state.library.register(&project)?;
    let runtime = state
        .runtime_factory
        .build(&project, store)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    state.projects.write().await.insert(project.id, runtime);
    Ok((
        StatusCode::CREATED,
        Json(ProjectLibraryEntry {
            project,
            available: true,
        }),
    ))
}

async fn relocate_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<ProjectPathRequest>,
) -> ApiResult<Json<ProjectLibraryEntry>> {
    let project_id = parse_id(&project_id, "Project")?;
    state.library.get(project_id)?;
    if let Some(runtime) = state.projects.read().await.get(&project_id).cloned() {
        ensure_project_can_detach(&runtime)?;
    }
    let root = canonical_project_root(&request.root_path)?;
    let (project, store) = load_project_store(&root, Some(project_id))?;
    state.library.register(&project)?;
    let runtime = state
        .runtime_factory
        .build(&project, store)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    state.projects.write().await.insert(project.id, runtime);
    Ok(Json(ProjectLibraryEntry {
        project,
        available: true,
    }))
}

async fn remove_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<StatusCode> {
    let project_id = parse_id(&project_id, "Project")?;
    if let Some(runtime) = state.projects.read().await.get(&project_id).cloned() {
        ensure_project_can_detach(&runtime)?;
    }
    state.library.remove(project_id)?;
    state.projects.write().await.remove(&project_id);
    Ok(StatusCode::NO_CONTENT)
}

fn canonical_project_root(value: &str) -> ApiResult<PathBuf> {
    let root = PathBuf::from(value.trim());
    if !root.is_absolute() {
        return Err(ApiError::bad_request("Project root path must be absolute"));
    }
    root.canonicalize().map_err(|error| {
        ApiError::bad_request(format!("Project directory is unavailable: {error}"))
    })
}

fn load_project_store(
    root: &std::path::Path,
    expected_id: Option<ProjectId>,
) -> ApiResult<(Project, Arc<Store>)> {
    let (project, store) = inspect_project_store(root)?;
    if expected_id.is_some_and(|expected_id| expected_id != project.id) {
        return Err(ApiError::bad_request(
            "Selected directory belongs to a different Project",
        ));
    }
    let project = store.relocate_project(project.id, root)?;
    Ok((project, store))
}

fn inspect_project_store(root: &std::path::Path) -> ApiResult<(Project, Arc<Store>)> {
    let metadata_root = root.join(".papermachine");
    let database = metadata_root.join("state/project.db");
    if !database.is_file() || !metadata_root.join("project.toml").is_file() {
        return Err(ApiError::bad_request(
            "Selected directory is not a PaperMachine Project",
        ));
    }
    let store = Arc::new(Store::open(database, metadata_root.join("artifacts"))?);
    let projects = store.list_projects()?;
    if projects.len() != 1 {
        return Err(StoreError::Invariant(
            "Project database must contain exactly one Project".to_string(),
        )
        .into());
    }
    Ok((projects[0].clone(), store))
}

fn ensure_project_can_detach(runtime: &ProjectRuntime) -> ApiResult<()> {
    if !runtime.store.list_recoverable_workflows()?.is_empty()
        || !runtime.store.list_resumable_standalone_turns()?.is_empty()
    {
        return Err(StoreError::Invariant(
            "Project has active work; finish or cancel it before changing its library entry"
                .to_string(),
        )
        .into());
    }
    Ok(())
}

#[derive(Serialize)]
struct ProjectOverview {
    project: Project,
    system_prompt: ProjectSystemPrompt,
    sessions: Vec<Session>,
    workflows: Vec<Workflow>,
    workflow_participants: Vec<WorkflowParticipant>,
    human_requests: Vec<HumanRequest>,
    artifacts: Vec<Artifact>,
}

async fn get_project_overview(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<ProjectOverview>> {
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    let workflows = runtime.store.list_project_workflows(project_id)?;
    let mut participants = Vec::new();
    let mut requests = Vec::new();
    for workflow in &workflows {
        participants.extend(runtime.store.list_participants(workflow.id)?);
        requests.extend(runtime.store.list_human_requests(workflow.id)?);
    }
    Ok(Json(ProjectOverview {
        project: runtime.store.get_project(project_id)?,
        system_prompt: runtime.store.get_project_system_prompt(project_id)?,
        sessions: runtime.store.list_sessions(project_id)?,
        workflows,
        workflow_participants: participants,
        human_requests: requests,
        artifacts: runtime.store.list_project_artifacts(project_id)?,
    }))
}

async fn get_project_system_prompt(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<ProjectSystemPrompt>> {
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    Ok(Json(runtime.store.get_project_system_prompt(project_id)?))
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
    let runtime = state.project_runtime(project_id).await?;
    Ok(Json(runtime.store.set_project_system_prompt(
        project_id,
        request.system_prompt,
    )?))
}

async fn list_sessions(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<Session>>> {
    let id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(id).await?;
    Ok(Json(runtime.store.list_sessions(id)?))
}

async fn list_project_skills(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<ProjectSkill>>> {
    let id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(id).await?;
    Ok(Json(runtime.skills.list(id)?))
}

async fn get_project_skill(
    State(state): State<AppState>,
    Path((project_id, slug)): Path<(String, String)>,
) -> ApiResult<Json<ProjectSkill>> {
    let id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(id).await?;
    Ok(Json(runtime.skills.load(id, &slug)?))
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
    let skill = runtime.skills.create(
        id,
        request.slug.trim(),
        request.name.trim(),
        request.description.trim(),
        request.instructions.trim(),
    )?;
    Ok((StatusCode::CREATED, Json(skill)))
}

#[derive(Serialize)]
struct SessionView {
    session: Session,
    turns: Vec<Turn>,
    steps: Vec<AgentStep>,
    workflows: Vec<Workflow>,
    workflow_memberships: Vec<WorkflowParticipant>,
    human_requests: Vec<HumanRequest>,
}

async fn get_session_view(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<SessionView>> {
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, session) = state
        .locate("session", &session_id.to_string(), |store| {
            store.get_session(session_id)
        })
        .await?;
    let turns = runtime.store.list_turns(session_id)?;
    let mut steps = Vec::new();
    for turn in &turns {
        steps.extend(runtime.store.list_steps(turn.id)?);
    }
    let workflows = runtime.store.list_session_workflows(session_id)?;
    let mut memberships = Vec::new();
    let mut requests = Vec::new();
    for workflow in &workflows {
        memberships.extend(
            runtime
                .store
                .list_participants(workflow.id)?
                .into_iter()
                .filter(|item| item.session_id == session_id),
        );
        requests.extend(
            runtime
                .store
                .list_human_requests(workflow.id)?
                .into_iter()
                .filter(|item| item.session_id == session_id),
        );
    }
    Ok(Json(SessionView {
        session,
        turns,
        steps,
        workflows,
        workflow_memberships: memberships,
        human_requests: requests,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTurnRequest {
    input: String,
}

async fn create_turn(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<CreateTurnRequest>,
) -> ApiResult<(StatusCode, Json<Turn>)> {
    if request.input.trim().is_empty() {
        return Err(ApiError::bad_request("Turn input must not be empty"));
    }
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, _) = state
        .locate("session", &session_id.to_string(), |store| {
            store.get_session(session_id)
        })
        .await?;
    let turn = runtime
        .sessions
        .submit(session_id, request.input.trim())
        .await?;
    Ok((StatusCode::CREATED, Json(turn)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateSessionSkillsRequest {
    enabled_skills: Vec<String>,
}

async fn update_session_skills(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<UpdateSessionSkillsRequest>,
) -> ApiResult<Json<Session>> {
    let id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, session) = state
        .locate("session", &id.to_string(), |store| store.get_session(id))
        .await?;
    runtime
        .skills
        .validate_enabled(session.project_id, &request.enabled_skills)?;
    Ok(Json(
        runtime
            .store
            .set_session_enabled_skills(id, request.enabled_skills)?,
    ))
}

async fn update_session_system_prompt(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<SystemPromptRequest>,
) -> ApiResult<Json<Session>> {
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, _) = state
        .locate("session", &session_id.to_string(), |store| {
            store.get_session(session_id)
        })
        .await?;
    Ok(Json(runtime.store.set_session_system_prompt(
        session_id,
        request.system_prompt,
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateSessionAccessRequest {
    access: AgentAccessProfile,
}

async fn update_session_access(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<UpdateSessionAccessRequest>,
) -> ApiResult<Json<Session>> {
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, _) = state
        .locate("session", &session_id.to_string(), |store| {
            store.get_session(session_id)
        })
        .await?;
    Ok(Json(
        runtime
            .store
            .set_session_access(session_id, request.access)?,
    ))
}

async fn cancel_turn(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
) -> ApiResult<StatusCode> {
    let turn_id: TurnId = parse_id(&turn_id, "Turn")?;
    let (runtime, _) = state
        .locate("turn", &turn_id.to_string(), |store| {
            store.get_turn(turn_id)
        })
        .await?;
    runtime.sessions.cancel(turn_id).await?;
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
    let project = runtime.store.get_project(project_id)?;
    let loaded =
        runtime
            .catalog
            .write()
            .await
            .save_user(&project, &request.source, &runtime.store)?;
    Ok((StatusCode::CREATED, Json(loaded.registration)))
}

async fn list_session_workflows(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Vec<Workflow>>> {
    let id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, _) = state
        .locate("session", &id.to_string(), |store| store.get_session(id))
        .await?;
    Ok(Json(runtime.store.list_session_workflows(id)?))
}

async fn list_project_workflows(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<Workflow>>> {
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    Ok(Json(runtime.store.list_project_workflows(project_id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateWorkflowRequest {
    program_slug: String,
    request: String,
    #[serde(default)]
    instructions: String,
    #[serde(default = "empty_object")]
    params: Value,
    #[serde(default)]
    started_from_session_id: Option<SessionId>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    access: AgentAccessProfile,
    #[serde(default)]
    enabled_skills: Vec<String>,
    #[serde(default)]
    context_mode: WorkflowContextMode,
    #[serde(default)]
    agent_access_overrides: BTreeMap<String, AgentAccessProfile>,
}

fn empty_object() -> Value {
    json!({})
}

fn validate_model_profile(state: &AppState, model: &str) -> ApiResult<()> {
    if !state.model_profiles.is_empty()
        && !state
            .model_profiles
            .iter()
            .any(|profile| profile.id == model)
    {
        return Err(ApiError::bad_request(format!(
            "unknown model profile {model:?}; choose one of: {}",
            state
                .model_profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

fn validate_model_profile_params(state: &AppState, schema: &Value, value: &Value) -> ApiResult<()> {
    if schema.get("format").and_then(Value::as_str) == Some("model-profile") {
        if let Some(model) = value
            .as_str()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            validate_model_profile(state, model)?;
        }
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

async fn create_workflow(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<CreateWorkflowRequest>,
) -> ApiResult<(StatusCode, Json<Workflow>)> {
    if request.request.trim().is_empty() {
        return Err(ApiError::bad_request("Workflow request must not be empty"));
    }
    let project_id = parse_id(&project_id, "Project")?;
    let runtime = state.project_runtime(project_id).await?;
    runtime
        .skills
        .validate_enabled(project_id, &request.enabled_skills)?;
    let model = if request.model.trim().is_empty() {
        state.default_model.as_str()
    } else {
        request.model.trim()
    };
    validate_model_profile(&state, model)?;
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
    let launch_context = match request.context_mode {
        WorkflowContextMode::Fresh => WorkflowLaunchContext::default(),
        WorkflowContextMode::ProjectSnapshot => WorkflowLaunchContext {
            mode: WorkflowContextMode::ProjectSnapshot,
            snapshot: Some(build_project_snapshot(
                &runtime.store,
                project_id,
                ProjectSnapshotOptions {
                    focus_session_id: request.started_from_session_id,
                    max_sessions: 20,
                    max_turns_per_session: 8,
                    max_workflows: 100,
                    max_artifacts: 30,
                    include_artifact_content: true,
                    max_text_chars: 300_000,
                    ..ProjectSnapshotOptions::default()
                },
            )?),
        },
    };
    let workflow = runtime.store.create_workflow(NewWorkflow {
        project_id,
        started_from_session_id: request.started_from_session_id,
        program: snapshot,
        request: request.request.trim().to_string(),
        instructions: request.instructions.trim().to_string(),
        trigger: WorkflowTrigger {
            kind: if request.started_from_session_id.is_some() {
                WorkflowTriggerKind::User
            } else {
                WorkflowTriggerKind::Manual
            },
            source_workflow_id: None,
            source_session_id: request.started_from_session_id,
            source_timer_id: None,
        },
        params: request.params,
        default_model: model.to_string(),
        access: request.access,
        enabled_skills: request.enabled_skills,
        launch_context,
        agent_access_overrides: request.agent_access_overrides,
    })?;
    runtime.scheduler.start(workflow.id).await?;
    Ok((StatusCode::CREATED, Json(workflow)))
}

#[derive(Serialize)]
struct WorkflowView {
    workflow: Workflow,
    effects: Vec<WorkflowEffect>,
    participants: Vec<WorkflowParticipant>,
    sessions: Vec<Session>,
    actions: Vec<ActionInvocation>,
    attempts: Vec<ActionAttempt>,
    teams: Vec<WorkflowTeam>,
    relations: Vec<AgentRelation>,
    task_scopes: Vec<TaskScope>,
    timers: Vec<WorkflowTimer>,
    channels: Vec<WorkflowChannel>,
    signals: Vec<WorkflowSignal>,
    human_requests: Vec<HumanRequest>,
    control_messages: Vec<ControlMessage>,
    artifacts: Vec<Artifact>,
}

async fn get_workflow_view(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> ApiResult<Json<WorkflowView>> {
    let workflow_id: WorkflowId = parse_id(&workflow_id, "Workflow")?;
    let (runtime, run) = state
        .locate("workflow", &workflow_id.to_string(), |store| {
            store.get_workflow(workflow_id)
        })
        .await?;
    let participants = runtime.store.list_participants(workflow_id)?;
    let mut sessions = Vec::new();
    for participant in &participants {
        sessions.push(runtime.store.get_session(participant.session_id)?);
    }
    let actions = runtime.store.list_action_invocations(workflow_id)?;
    let mut attempts = Vec::new();
    for action in &actions {
        attempts.extend(runtime.store.list_action_attempts(action.id)?);
    }
    let channels = runtime.store.list_channels(workflow_id)?;
    let mut signals = Vec::new();
    for channel in &channels {
        signals.extend(runtime.store.list_signals(channel.id, 0)?);
    }
    Ok(Json(WorkflowView {
        workflow: run,
        effects: runtime.store.list_workflow_effects(workflow_id)?,
        participants,
        sessions,
        actions,
        attempts,
        teams: runtime.store.list_teams(workflow_id)?,
        relations: runtime.store.list_relations(workflow_id)?,
        task_scopes: runtime.store.list_task_scopes(workflow_id)?,
        timers: runtime.store.list_timers(workflow_id)?,
        channels,
        signals,
        human_requests: runtime.store.list_human_requests(workflow_id)?,
        control_messages: runtime.store.list_control_messages(workflow_id)?,
        artifacts: runtime.store.list_artifacts(workflow_id)?,
    }))
}

async fn pause_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> ApiResult<StatusCode> {
    let workflow_id: WorkflowId = parse_id(&workflow_id, "Workflow")?;
    let (runtime, _) = state
        .locate("workflow", &workflow_id.to_string(), |store| {
            store.get_workflow(workflow_id)
        })
        .await?;
    runtime.scheduler.pause(workflow_id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn resume_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> ApiResult<StatusCode> {
    let workflow_id: WorkflowId = parse_id(&workflow_id, "Workflow")?;
    let (runtime, _) = state
        .locate("workflow", &workflow_id.to_string(), |store| {
            store.get_workflow(workflow_id)
        })
        .await?;
    runtime.scheduler.resume(workflow_id).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> ApiResult<StatusCode> {
    let workflow_id: WorkflowId = parse_id(&workflow_id, "Workflow")?;
    let (runtime, _) = state
        .locate("workflow", &workflow_id.to_string(), |store| {
            store.get_workflow(workflow_id)
        })
        .await?;
    runtime.scheduler.cancel(workflow_id).await?;
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
    Path((workflow_id, session_id)): Path<(String, String)>,
    Json(request): Json<ControlRequest>,
) -> ApiResult<(StatusCode, Json<ControlMessage>)> {
    if request.content.trim().is_empty() {
        return Err(ApiError::bad_request("control message must not be empty"));
    }
    let workflow_id: WorkflowId = parse_id(&workflow_id, "Workflow")?;
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, run) = state
        .locate("workflow", &workflow_id.to_string(), |store| {
            store.get_workflow(workflow_id)
        })
        .await?;
    let is_member = runtime
        .store
        .list_participants(workflow_id)?
        .iter()
        .any(|item| item.session_id == session_id);
    if !is_member && run.started_from_session_id != Some(session_id) {
        return Err(ApiError::bad_request(
            "Session does not participate in this Workflow",
        ));
    }
    let message = runtime.store.create_control_message(
        workflow_id,
        session_id,
        request.action_invocation_id,
        request.kind,
        request.content.trim(),
    )?;
    Ok((StatusCode::CREATED, Json(message)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HumanAnswerRequest {
    answer: Value,
}

async fn answer_human_request(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
    Json(request): Json<HumanAnswerRequest>,
) -> ApiResult<Json<HumanRequest>> {
    let id: HumanRequestId = parse_id(&request_id, "HumanRequest")?;
    let (runtime, current) = state
        .locate("human request", &id.to_string(), |store| {
            store.get_human_request(id)
        })
        .await?;
    validate_schema_value(&current.response_schema, &request.answer, "answer")
        .map_err(ApiError::bad_request)?;
    Ok(Json(
        runtime.store.answer_human_request(id, request.answer)?,
    ))
}

#[derive(Default, Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: u64,
}

async fn list_workflow_events(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Vec<WorkflowEvent>>> {
    let workflow_id: WorkflowId = parse_id(&workflow_id, "Workflow")?;
    let (runtime, _) = state
        .locate("workflow", &workflow_id.to_string(), |store| {
            store.get_workflow(workflow_id)
        })
        .await?;
    Ok(Json(
        runtime
            .store
            .list_workflow_events(workflow_id, query.after)?,
    ))
}

async fn stream_workflow_events(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let workflow_id: WorkflowId = parse_id(&workflow_id, "Workflow")?;
    let (runtime, _) = state
        .locate("workflow", &workflow_id.to_string(), |store| {
            store.get_workflow(workflow_id)
        })
        .await?;
    let receiver = runtime.store.subscribe();
    let replay = runtime
        .store
        .list_workflow_events(workflow_id, query.after)?;
    let high_watermark = replay.last().map_or(query.after, |event| event.sequence);
    let replay = tokio_stream::iter(replay.into_iter().map(run_sse_event));
    let live = BroadcastStream::new(receiver).filter_map(move |result| match result {
        Ok(event) if event.workflow_id == workflow_id && event.sequence > high_watermark => {
            Some(run_sse_event(event))
        }
        _ => None,
    });
    Ok(Sse::new(replay.chain(live)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

async fn list_session_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Vec<SessionEvent>>> {
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, _) = state
        .locate("session", &session_id.to_string(), |store| {
            store.get_session(session_id)
        })
        .await?;
    Ok(Json(
        runtime.store.list_session_events(session_id, query.after)?,
    ))
}

async fn stream_session_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let session_id: SessionId = parse_id(&session_id, "Session")?;
    let (runtime, _) = state
        .locate("session", &session_id.to_string(), |store| {
            store.get_session(session_id)
        })
        .await?;
    let receiver = runtime.store.subscribe_sessions();
    let replay = runtime.store.list_session_events(session_id, query.after)?;
    let high_watermark = replay.last().map_or(query.after, |event| event.sequence);
    let replay = tokio_stream::iter(replay.into_iter().map(session_sse_event));
    let live = BroadcastStream::new(receiver).filter_map(move |result| match result {
        Ok(event) if event.session_id == session_id && event.sequence > high_watermark => {
            Some(session_sse_event(event))
        }
        _ => None,
    });
    Ok(Sse::new(replay.chain(live)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

fn run_sse_event(event: WorkflowEvent) -> Result<Event, Infallible> {
    sse_event(event.sequence, &event.payload, &event)
}

fn session_sse_event(event: SessionEvent) -> Result<Event, Infallible> {
    sse_event(event.sequence, &event.payload, &event)
}

fn sse_event<P: Serialize, E: Serialize>(
    sequence: u64,
    payload: &P,
    event: &E,
) -> Result<Event, Infallible> {
    let event_type = serde_json::to_value(payload)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "event".to_string());
    let data =
        serde_json::to_string(event).unwrap_or_else(|error| format!(r#"{{"error":"{error}"}}"#));
    Ok(Event::default()
        .id(sequence.to_string())
        .event(event_type)
        .data(data))
}

async fn artifact_content(
    State(state): State<AppState>,
    Path(artifact_id): Path<String>,
) -> ApiResult<Response> {
    let artifact_id: ArtifactId = parse_id(&artifact_id, "Artifact")?;
    let (runtime, artifact) = state
        .locate("artifact", &artifact_id.to_string(), |store| {
            store.get_artifact(artifact_id)
        })
        .await?;
    let bytes = runtime.store.read_artifact(&artifact)?;
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

impl From<WorkflowSchedulerError> for ApiError {
    fn from(error: WorkflowSchedulerError) -> Self {
        let status = match &error {
            WorkflowSchedulerError::Store(StoreError::NotFound { .. }) => StatusCode::NOT_FOUND,
            WorkflowSchedulerError::TerminalWorkflow { .. }
            | WorkflowSchedulerError::NotScheduled(_) => StatusCode::CONFLICT,
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

impl From<SessionRuntimeError> for ApiError {
    fn from(error: SessionRuntimeError) -> Self {
        let status = match &error {
            SessionRuntimeError::Store(StoreError::NotFound { .. }) => StatusCode::NOT_FOUND,
            SessionRuntimeError::Store(StoreError::Invariant(_))
            | SessionRuntimeError::TerminalTurn(_) => StatusCode::CONFLICT,
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
            SkillError::Invalid(_) | SkillError::SnapshotChanged(_) | SkillError::Yaml(_) => {
                StatusCode::BAD_REQUEST
            }
            SkillError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
