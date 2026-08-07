//! HTTP and realtime API for PaperMachine.

mod demo_model;

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
use papermachine_model::ModelClient;
use papermachine_model::ModelProfile;
use papermachine_model::ModelProviderInfo;
use papermachine_model::OpenAiResponsesClient;
use papermachine_model::OpenAiResponsesConfig;
use papermachine_protocol::*;
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeConfig;
use papermachine_session::SessionRuntimeError;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_skills::SkillError;
use papermachine_store::NewWorkflow;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_tools::AskHumanTool;
use papermachine_tools::ExecCommandTool;
use papermachine_tools::FetchUrlTool;
use papermachine_tools::ReadFileTool;
use papermachine_tools::ToolRegistry;
use papermachine_tools::WriteFileTool;
use papermachine_workflow::ProjectSnapshotOptions;
use papermachine_workflow::PythonWorkflowRuntime;
use papermachine_workflow::StoreHumanRequestBroker;
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
use std::convert::Infallible;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
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
pub struct ServerConfig {
    pub root: PathBuf,
    pub default_model: String,
    pub demo: bool,
    pub configured_models: Option<ConfiguredModels>,
    pub openai_config: Option<OpenAiResponsesConfig>,
    pub model_context_window: usize,
    pub max_concurrent_runs: usize,
    pub max_parallel_actions: usize,
}

#[derive(Clone)]
pub struct AppState {
    store: Arc<Store>,
    catalog: Arc<RwLock<WorkflowProgramCatalog>>,
    scheduler: WorkflowScheduler,
    sessions: SessionRuntime,
    skills: Arc<ProjectSkillCatalog>,
    generator: WorkflowGenerator,
    default_model: String,
    model_context_window: usize,
    model_profiles: Vec<ModelProfile>,
    model_providers: Vec<ModelProviderInfo>,
    mode: &'static str,
}

impl AppState {
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn mode(&self) -> &'static str {
        self.mode
    }
}

pub async fn initialize(config: &ServerConfig) -> anyhow::Result<AppState> {
    anyhow::ensure!(
        config.model_context_window >= 4_096,
        "model context window must be at least 4096 tokens"
    );
    let state_root = config.root.join(".papermachine");
    let durable_state_root = state_root.join("state");
    let store = Arc::new(
        Store::open(
            durable_state_root.join("papermachine.db"),
            durable_state_root.join("artifacts"),
        )
        .context("failed to open PaperMachine store")?,
    );
    let python_runtime_root = {
        let local = config.root.join("python");
        if local.is_dir() {
            local
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python")
        }
    };
    let workflows_root = config.root.join("workflows");
    let mut catalog = WorkflowProgramCatalog::scan(&workflows_root, &python_runtime_root, &store)
        .context("failed to load Python workflow catalog")?;
    for project in store.list_projects()? {
        catalog
            .load_project(&project, &store)
            .with_context(|| format!("failed to load workflows for Project {}", project.id))?;
    }

    let (model, mode, model_profiles, model_providers): (
        Arc<dyn ModelClient>,
        &'static str,
        Vec<ModelProfile>,
        Vec<ModelProviderInfo>,
    ) = if config.demo {
        (Arc::new(DemoModelClient), "demo", Vec::new(), Vec::new())
    } else if let Some(configured) = config.configured_models.as_ref() {
        let model: Arc<dyn ModelClient> = Arc::new(configured.router.clone());
        (
            model,
            "providers",
            configured.profiles.clone(),
            configured.providers.clone(),
        )
    } else {
        let openai_config = config
            .openai_config
            .clone()
            .map(Ok)
            .unwrap_or_else(OpenAiResponsesConfig::from_env)
            .context("failed to configure OpenAI model")?;
        if openai_config.endpoint.scheme() != "https" {
            tracing::warn!(
                endpoint = %openai_config.endpoint,
                "model endpoint is not protected by HTTPS"
            );
        }
        let profile = ModelProfile {
            id: config.default_model.clone(),
            provider: openai_config.provider_id.clone(),
            model: config.default_model.clone(),
            context_window: config.model_context_window,
        };
        let provider = ModelProviderInfo {
            id: openai_config.provider_id.clone(),
            kind: "openai_responses".to_string(),
            endpoint: openai_config.endpoint.to_string(),
            max_request_retries: openai_config.max_request_retries,
            request_timeout_seconds: openai_config.request_timeout.as_secs(),
            stream_idle_timeout_seconds: openai_config.stream_idle_timeout.as_secs(),
            responses_websockets: openai_config.responses_websockets,
            prompt_cache_mode: format!("{:?}", openai_config.prompt_cache_mode)
                .to_ascii_lowercase(),
        };
        let model = OpenAiResponsesClient::new(openai_config)
            .context("failed to create OpenAI model client")?;
        (Arc::new(model), "openai", vec![profile], vec![provider])
    };

    let human_broker = Arc::new(StoreHumanRequestBroker::new(Arc::clone(&store)));
    let tools = ToolRegistry::builder()
        .register(ReadFileTool)
        .context("failed to register read_file")?
        .register(WriteFileTool)
        .context("failed to register write_file")?
        .register(FetchUrlTool)
        .context("failed to register fetch_url")?
        .register(ExecCommandTool)
        .context("failed to register exec_command")?
        .register(AskHumanTool::new(human_broker))
        .context("failed to register ask_human")?
        .build();
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    let sessions = SessionRuntime::new(
        Arc::clone(&store),
        Arc::clone(&model),
        tools,
        Arc::clone(&skills),
        SessionRuntimeConfig {
            default_model: config.default_model.clone(),
            model_context_window: config.model_context_window,
            max_concurrent_turns: config
                .max_concurrent_runs
                .saturating_mul(config.max_parallel_actions)
                .max(1),
        },
    );
    let executor: Arc<dyn WorkflowRuntime> = Arc::new(PythonWorkflowRuntime::new(
        Arc::clone(&store),
        sessions.clone(),
        catalog.python(),
        catalog.python_runtime_root(),
        durable_state_root.join("workflow-runtime"),
    ));
    let scheduler =
        WorkflowScheduler::new(Arc::clone(&store), executor, config.max_concurrent_runs);
    sessions
        .recover()
        .await
        .context("failed to recover unfinished standalone Session Turns")?;
    scheduler
        .recover()
        .await
        .context("failed to recover unfinished Workflows")?;

    Ok(AppState {
        store,
        catalog: Arc::new(RwLock::new(catalog)),
        scheduler,
        sessions,
        skills,
        generator: WorkflowGenerator::new(Arc::clone(&model), &config.default_model),
        default_model: config.default_model.clone(),
        model_context_window: config.model_context_window,
        model_profiles,
        model_providers,
        mode,
    })
}

pub fn router(state: AppState, web_dist: PathBuf) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/{project_id}", get(get_project_overview))
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
            "/projects/{project_id}/workflows",
            get(list_project_workflows).post(create_workflow),
        )
        .route("/workflow-programs/validate", post(validate_workflow))
        .route("/workflow-programs/generate", post(generate_workflow))
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

async fn list_projects(State(state): State<AppState>) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(state.store.list_projects()?))
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
) -> ApiResult<(StatusCode, Json<Project>)> {
    if request.name.trim().is_empty() {
        return Err(ApiError::bad_request("Project name must not be empty"));
    }
    if request.root_path.trim().is_empty() {
        return Err(ApiError::bad_request("Project root path must not be empty"));
    }
    let project = state.store.create_project(
        request.name.trim(),
        request.description.trim(),
        request.root_path.trim(),
    )?;
    state.skills.ensure_project(project.id)?;
    state
        .catalog
        .write()
        .await
        .load_project(&project, &state.store)?;
    Ok((StatusCode::CREATED, Json(project)))
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
    let workflows = state.store.list_project_workflows(project_id)?;
    let mut participants = Vec::new();
    let mut requests = Vec::new();
    for workflow in &workflows {
        participants.extend(state.store.list_participants(workflow.id)?);
        requests.extend(state.store.list_human_requests(workflow.id)?);
    }
    Ok(Json(ProjectOverview {
        project: state.store.get_project(project_id)?,
        system_prompt: state.store.get_project_system_prompt(project_id)?,
        sessions: state.store.list_sessions(project_id)?,
        workflows,
        workflow_participants: participants,
        human_requests: requests,
        artifacts: state.store.list_project_artifacts(project_id)?,
    }))
}

async fn get_project_system_prompt(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<ProjectSystemPrompt>> {
    Ok(Json(state.store.get_project_system_prompt(parse_id(
        &project_id,
        "Project",
    )?)?))
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
    Ok(Json(state.store.set_project_system_prompt(
        parse_id(&project_id, "Project")?,
        request.system_prompt,
    )?))
}

async fn list_sessions(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<Session>>> {
    let id = parse_id(&project_id, "Project")?;
    state.store.get_project(id)?;
    Ok(Json(state.store.list_sessions(id)?))
}

async fn list_project_skills(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<ProjectSkill>>> {
    let id = parse_id(&project_id, "Project")?;
    state.store.get_project(id)?;
    Ok(Json(state.skills.list(id)?))
}

async fn get_project_skill(
    State(state): State<AppState>,
    Path((project_id, slug)): Path<(String, String)>,
) -> ApiResult<Json<ProjectSkill>> {
    let id = parse_id(&project_id, "Project")?;
    state.store.get_project(id)?;
    Ok(Json(state.skills.load(id, &slug)?))
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
    state.store.get_project(id)?;
    let skill = state.skills.create(
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
    let session_id = parse_id(&session_id, "Session")?;
    let session = state.store.get_session(session_id)?;
    let turns = state.store.list_turns(session_id)?;
    let mut steps = Vec::new();
    for turn in &turns {
        steps.extend(state.store.list_steps(turn.id)?);
    }
    let workflows = state.store.list_session_workflows(session_id)?;
    let mut memberships = Vec::new();
    let mut requests = Vec::new();
    for workflow in &workflows {
        memberships.extend(
            state
                .store
                .list_participants(workflow.id)?
                .into_iter()
                .filter(|item| item.session_id == session_id),
        );
        requests.extend(
            state
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
    let turn = state
        .sessions
        .submit(parse_id(&session_id, "Session")?, request.input.trim())
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
    let id = parse_id(&session_id, "Session")?;
    let session = state.store.get_session(id)?;
    state
        .skills
        .validate_enabled(session.project_id, &request.enabled_skills)?;
    Ok(Json(
        state
            .store
            .set_session_enabled_skills(id, request.enabled_skills)?,
    ))
}

async fn update_session_system_prompt(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<SystemPromptRequest>,
) -> ApiResult<Json<Session>> {
    Ok(Json(state.store.set_session_system_prompt(
        parse_id(&session_id, "Session")?,
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
    Ok(Json(state.store.set_session_access(
        parse_id(&session_id, "Session")?,
        request.access,
    )?))
}

async fn cancel_turn(
    State(state): State<AppState>,
    Path(turn_id): Path<String>,
) -> ApiResult<StatusCode> {
    state.sessions.cancel(parse_id(&turn_id, "Turn")?).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn list_workflow_programs(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<WorkflowProgram>>> {
    let project_id = parse_id(&project_id, "Project")?;
    state.store.get_project(project_id)?;
    Ok(Json(
        state
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
    state.store.get_project(project_id)?;
    let catalog = state.catalog.read().await;
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
    Json(request): Json<WorkflowSourceRequest>,
) -> ApiResult<Json<WorkflowValidation>> {
    Ok(Json(
        state
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
    Json(request): Json<WorkflowGenerationRequest>,
) -> ApiResult<Json<GeneratedWorkflowResponse>> {
    let source = state
        .generator
        .generate(request, CancellationToken::new())
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let validation = state.catalog.read().await.validate_source(&source)?;
    Ok(Json(GeneratedWorkflowResponse { source, validation }))
}

async fn save_workflow_program(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(request): Json<WorkflowSourceRequest>,
) -> ApiResult<(StatusCode, Json<WorkflowProgram>)> {
    let project = state.store.get_project(parse_id(&project_id, "Project")?)?;
    let loaded = state
        .catalog
        .write()
        .await
        .save_user(&project, &request.source, &state.store)?;
    Ok((StatusCode::CREATED, Json(loaded.registration)))
}

async fn list_session_workflows(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Vec<Workflow>>> {
    let id = parse_id(&session_id, "Session")?;
    state.store.get_session(id)?;
    Ok(Json(state.store.list_session_workflows(id)?))
}

async fn list_project_workflows(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<Workflow>>> {
    let project_id = parse_id(&project_id, "Project")?;
    state.store.get_project(project_id)?;
    Ok(Json(state.store.list_project_workflows(project_id)?))
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
    state.store.get_project(project_id)?;
    state
        .skills
        .validate_enabled(project_id, &request.enabled_skills)?;
    let model = if request.model.trim().is_empty() {
        state.default_model.as_str()
    } else {
        request.model.trim()
    };
    validate_model_profile(&state, model)?;
    let (snapshot, declared_agent_classes) = {
        let catalog = state.catalog.read().await;
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
                &state.store,
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
    let workflow = state.store.create_workflow(NewWorkflow {
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
    state.scheduler.start(workflow.id).await?;
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
    let workflow_id = parse_id(&workflow_id, "Workflow")?;
    let run = state.store.get_workflow(workflow_id)?;
    let participants = state.store.list_participants(workflow_id)?;
    let mut sessions = Vec::new();
    for participant in &participants {
        sessions.push(state.store.get_session(participant.session_id)?);
    }
    let actions = state.store.list_action_invocations(workflow_id)?;
    let mut attempts = Vec::new();
    for action in &actions {
        attempts.extend(state.store.list_action_attempts(action.id)?);
    }
    let channels = state.store.list_channels(workflow_id)?;
    let mut signals = Vec::new();
    for channel in &channels {
        signals.extend(state.store.list_signals(channel.id, 0)?);
    }
    Ok(Json(WorkflowView {
        workflow: run,
        effects: state.store.list_workflow_effects(workflow_id)?,
        participants,
        sessions,
        actions,
        attempts,
        teams: state.store.list_teams(workflow_id)?,
        relations: state.store.list_relations(workflow_id)?,
        task_scopes: state.store.list_task_scopes(workflow_id)?,
        timers: state.store.list_timers(workflow_id)?,
        channels,
        signals,
        human_requests: state.store.list_human_requests(workflow_id)?,
        control_messages: state.store.list_control_messages(workflow_id)?,
        artifacts: state.store.list_artifacts(workflow_id)?,
    }))
}

async fn pause_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .scheduler
        .pause(parse_id(&workflow_id, "Workflow")?)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

async fn resume_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .scheduler
        .resume(parse_id(&workflow_id, "Workflow")?)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .scheduler
        .cancel(parse_id(&workflow_id, "Workflow")?)
        .await?;
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
    let workflow_id = parse_id(&workflow_id, "Workflow")?;
    let session_id = parse_id(&session_id, "Session")?;
    let run = state.store.get_workflow(workflow_id)?;
    let is_member = state
        .store
        .list_participants(workflow_id)?
        .iter()
        .any(|item| item.session_id == session_id);
    if !is_member && run.started_from_session_id != Some(session_id) {
        return Err(ApiError::bad_request(
            "Session does not participate in this Workflow",
        ));
    }
    let message = state.store.create_control_message(
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
    let id = parse_id(&request_id, "HumanRequest")?;
    let current = state.store.get_human_request(id)?;
    validate_schema_value(&current.response_schema, &request.answer, "answer")
        .map_err(ApiError::bad_request)?;
    Ok(Json(state.store.answer_human_request(id, request.answer)?))
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
    Ok(Json(state.store.list_workflow_events(
        parse_id(&workflow_id, "Workflow")?,
        query.after,
    )?))
}

async fn stream_workflow_events(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let workflow_id = parse_id(&workflow_id, "Workflow")?;
    state.store.get_workflow(workflow_id)?;
    let receiver = state.store.subscribe();
    let replay = state.store.list_workflow_events(workflow_id, query.after)?;
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
    Ok(Json(state.store.list_session_events(
        parse_id(&session_id, "Session")?,
        query.after,
    )?))
}

async fn stream_session_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let session_id = parse_id(&session_id, "Session")?;
    state.store.get_session(session_id)?;
    let receiver = state.store.subscribe_sessions();
    let replay = state.store.list_session_events(session_id, query.after)?;
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
    let artifact = state
        .store
        .get_artifact(parse_id(&artifact_id, "Artifact")?)?;
    let bytes = state.store.read_artifact(&artifact)?;
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
