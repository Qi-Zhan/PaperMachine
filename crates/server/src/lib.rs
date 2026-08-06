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
use papermachine_research::PythonWorkflowExecutor;
use papermachine_research::StoreHumanRequestBroker;
use papermachine_research::WorkflowCatalog;
use papermachine_research::WorkflowCatalogError;
use papermachine_research::WorkflowGenerationRequest;
use papermachine_research::WorkflowGenerator;
use papermachine_research::WorkflowRunExecutor;
use papermachine_research::WorkflowRunScheduler;
use papermachine_research::WorkflowSchedulerError;
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeConfig;
use papermachine_session::SessionRuntimeError;
use papermachine_skills::ResearchSkillCatalog;
use papermachine_skills::SkillError;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_tools::AskHumanTool;
use papermachine_tools::ExecCommandTool;
use papermachine_tools::FetchUrlTool;
use papermachine_tools::ReadFileTool;
use papermachine_tools::ToolRegistry;
use papermachine_tools::WriteFileTool;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
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
    catalog: Arc<RwLock<WorkflowCatalog>>,
    scheduler: WorkflowRunScheduler,
    sessions: SessionRuntime,
    skills: Arc<ResearchSkillCatalog>,
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
    let store = Arc::new(
        Store::open(
            state_root.join("papermachine-v3.db"),
            state_root.join("artifacts-v3"),
        )
        .context("failed to open PaperMachine v3 store")?,
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
    let catalog = WorkflowCatalog::scan(&workflows_root, &python_runtime_root, &store)
        .context("failed to load Python workflow catalog")?;

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
    let skills = Arc::new(ResearchSkillCatalog::new(state_root.join("researches")));
    let sessions = SessionRuntime::new(
        Arc::clone(&store),
        Arc::clone(&model),
        tools,
        Arc::clone(&skills),
        SessionRuntimeConfig {
            workspace_root: state_root.join("session-workspaces"),
            default_model: config.default_model.clone(),
            model_context_window: config.model_context_window,
            max_concurrent_turns: config
                .max_concurrent_runs
                .saturating_mul(config.max_parallel_actions)
                .max(1),
        },
    );
    let executor: Arc<dyn WorkflowRunExecutor> = Arc::new(PythonWorkflowExecutor::new(
        Arc::clone(&store),
        sessions.clone(),
        catalog.python(),
        catalog.python_runtime_root(),
        state_root.join("workflow-runtime"),
    ));
    let scheduler =
        WorkflowRunScheduler::new(Arc::clone(&store), executor, config.max_concurrent_runs);
    scheduler
        .reconcile_process_restart()
        .context("failed to reconcile WorkflowRuns interrupted by process restart")?;
    sessions
        .recover()
        .await
        .context("failed to recover unfinished standalone Session Turns")?;
    scheduler
        .recover()
        .await
        .context("failed to start WorkflowRuns created before process restart")?;

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
        .route("/researches", get(list_researches).post(create_research))
        .route("/researches/{research_id}", get(get_research_overview))
        .route(
            "/researches/{research_id}/skills",
            get(list_research_skills).post(create_research_skill),
        )
        .route(
            "/researches/{research_id}/skills/{slug}",
            get(get_research_skill),
        )
        .route(
            "/researches/{research_id}/sessions",
            get(list_sessions).post(create_session),
        )
        .route("/sessions/{session_id}", get(get_session_view))
        .route("/sessions/{session_id}/turns", post(create_turn))
        .route("/sessions/{session_id}/skills", put(update_session_skills))
        .route("/sessions/{session_id}/access", put(update_session_access))
        .route(
            "/sessions/{session_id}/workflow-runs",
            get(list_workflow_runs).post(create_workflow_run),
        )
        .route("/sessions/{session_id}/events", get(list_session_events))
        .route(
            "/sessions/{session_id}/events/stream",
            get(stream_session_events),
        )
        .route("/turns/{turn_id}/cancel", post(cancel_turn))
        .route("/workflows", get(list_workflows).post(save_workflow))
        .route("/workflows/validate", post(validate_workflow))
        .route("/workflows/generate", post(generate_workflow))
        .route("/workflows/{slug}/{version}", get(get_workflow))
        .route(
            "/workflow-runs/{workflow_run_id}",
            get(get_workflow_run_view),
        )
        .route(
            "/workflow-runs/{workflow_run_id}/pause",
            post(pause_workflow_run),
        )
        .route(
            "/workflow-runs/{workflow_run_id}/resume",
            post(resume_workflow_run),
        )
        .route(
            "/workflow-runs/{workflow_run_id}/cancel",
            post(cancel_workflow_run),
        )
        .route(
            "/workflow-runs/{workflow_run_id}/sessions/{session_id}/control",
            post(create_control_message),
        )
        .route(
            "/workflow-runs/{workflow_run_id}/events",
            get(list_workflow_run_events),
        )
        .route(
            "/workflow-runs/{workflow_run_id}/events/stream",
            get(stream_workflow_run_events),
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
        workflow_runtime: "python_effect_dsl_v1",
    })
}

async fn list_researches(State(state): State<AppState>) -> ApiResult<Json<Vec<Research>>> {
    Ok(Json(state.store.list_researches()?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateResearchRequest {
    name: String,
    #[serde(default)]
    description: String,
}

async fn create_research(
    State(state): State<AppState>,
    Json(request): Json<CreateResearchRequest>,
) -> ApiResult<(StatusCode, Json<Research>)> {
    if request.name.trim().is_empty() {
        return Err(ApiError::bad_request("Research name must not be empty"));
    }
    let research = state
        .store
        .create_research(request.name.trim(), request.description.trim())?;
    state.skills.ensure_research(research.id)?;
    Ok((StatusCode::CREATED, Json(research)))
}

#[derive(Serialize)]
struct ResearchOverview {
    research: Research,
    sessions: Vec<Session>,
    workflow_runs: Vec<WorkflowRun>,
    workflow_participants: Vec<WorkflowParticipant>,
    human_requests: Vec<HumanRequest>,
    artifacts: Vec<Artifact>,
}

async fn get_research_overview(
    State(state): State<AppState>,
    Path(research_id): Path<String>,
) -> ApiResult<Json<ResearchOverview>> {
    let research_id = parse_id(&research_id, "Research")?;
    let workflow_runs = state.store.list_research_workflow_runs(research_id)?;
    let mut participants = Vec::new();
    let mut requests = Vec::new();
    for run in &workflow_runs {
        participants.extend(state.store.list_participants(run.id)?);
        requests.extend(state.store.list_human_requests(run.id)?);
    }
    Ok(Json(ResearchOverview {
        research: state.store.get_research(research_id)?,
        sessions: state.store.list_sessions(research_id)?,
        workflow_runs,
        workflow_participants: participants,
        human_requests: requests,
        artifacts: state.store.list_research_artifacts(research_id)?,
    }))
}

async fn list_sessions(
    State(state): State<AppState>,
    Path(research_id): Path<String>,
) -> ApiResult<Json<Vec<Session>>> {
    let id = parse_id(&research_id, "Research")?;
    state.store.get_research(id)?;
    Ok(Json(state.store.list_sessions(id)?))
}

async fn list_research_skills(
    State(state): State<AppState>,
    Path(research_id): Path<String>,
) -> ApiResult<Json<Vec<ResearchSkill>>> {
    let id = parse_id(&research_id, "Research")?;
    state.store.get_research(id)?;
    Ok(Json(state.skills.list(id)?))
}

async fn get_research_skill(
    State(state): State<AppState>,
    Path((research_id, slug)): Path<(String, String)>,
) -> ApiResult<Json<ResearchSkill>> {
    let id = parse_id(&research_id, "Research")?;
    state.store.get_research(id)?;
    Ok(Json(state.skills.load(id, &slug)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateResearchSkillRequest {
    slug: String,
    name: String,
    description: String,
    instructions: String,
}

async fn create_research_skill(
    State(state): State<AppState>,
    Path(research_id): Path<String>,
    Json(request): Json<CreateResearchSkillRequest>,
) -> ApiResult<(StatusCode, Json<ResearchSkill>)> {
    let id = parse_id(&research_id, "Research")?;
    state.store.get_research(id)?;
    let skill = state.skills.create(
        id,
        request.slug.trim(),
        request.name.trim(),
        request.description.trim(),
        request.instructions.trim(),
    )?;
    Ok((StatusCode::CREATED, Json(skill)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSessionRequest {
    #[serde(default)]
    title: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    enabled_skills: Vec<String>,
    #[serde(default)]
    access: AgentAccessProfile,
}

async fn create_session(
    State(state): State<AppState>,
    Path(research_id): Path<String>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    let research_id = parse_id(&research_id, "Research")?;
    state
        .skills
        .validate_enabled(research_id, &request.enabled_skills)?;
    let title = if request.title.trim().is_empty() {
        "New research Session"
    } else {
        request.title.trim()
    };
    let model = if request.model.trim().is_empty() {
        &state.default_model
    } else {
        request.model.trim()
    };
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
    let session = state.store.create_session_with_access(
        research_id,
        title,
        request.instructions.trim(),
        model,
        request.enabled_skills,
        request.access,
    )?;
    Ok((StatusCode::CREATED, Json(session)))
}

#[derive(Serialize)]
struct SessionView {
    session: Session,
    turns: Vec<Turn>,
    steps: Vec<AgentStep>,
    workflow_runs: Vec<WorkflowRun>,
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
    let workflow_runs = state.store.list_session_workflow_runs(session_id)?;
    let mut memberships = Vec::new();
    let mut requests = Vec::new();
    for run in &workflow_runs {
        memberships.extend(
            state
                .store
                .list_participants(run.id)?
                .into_iter()
                .filter(|item| item.session_id == session_id),
        );
        requests.extend(
            state
                .store
                .list_human_requests(run.id)?
                .into_iter()
                .filter(|item| item.session_id == session_id),
        );
    }
    Ok(Json(SessionView {
        session,
        turns,
        steps,
        workflow_runs,
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
        .validate_enabled(session.research_id, &request.enabled_skills)?;
    Ok(Json(
        state
            .store
            .set_session_enabled_skills(id, request.enabled_skills)?,
    ))
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

async fn list_workflows(State(state): State<AppState>) -> Json<Vec<WorkflowRegistration>> {
    Json(
        state
            .catalog
            .read()
            .await
            .list()
            .into_iter()
            .map(|item| item.registration)
            .collect(),
    )
}

#[derive(Serialize)]
struct WorkflowSourceResponse {
    registration: WorkflowRegistration,
    source: String,
}

async fn get_workflow(
    State(state): State<AppState>,
    Path((slug, version)): Path<(String, String)>,
) -> ApiResult<Json<WorkflowSourceResponse>> {
    let catalog = state.catalog.read().await;
    let workflow = catalog
        .get(&slug, &version)
        .ok_or_else(|| ApiError::not_found(format!("Workflow {slug}@{version}")))?;
    Ok(Json(WorkflowSourceResponse {
        registration: workflow.registration.clone(),
        source: workflow.source_code.clone(),
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

async fn save_workflow(
    State(state): State<AppState>,
    Json(request): Json<WorkflowSourceRequest>,
) -> ApiResult<(StatusCode, Json<WorkflowRegistration>)> {
    let loaded = state
        .catalog
        .write()
        .await
        .save_user(&request.source, &state.store)?;
    Ok((StatusCode::CREATED, Json(loaded.registration)))
}

async fn list_workflow_runs(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Vec<WorkflowRun>>> {
    let id = parse_id(&session_id, "Session")?;
    state.store.get_session(id)?;
    Ok(Json(state.store.list_session_workflow_runs(id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateWorkflowRunRequest {
    workflow_slug: String,
    workflow_version: String,
    objective: String,
    #[serde(default = "empty_object")]
    input: Value,
}

fn empty_object() -> Value {
    json!({})
}

async fn create_workflow_run(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(request): Json<CreateWorkflowRunRequest>,
) -> ApiResult<(StatusCode, Json<WorkflowRun>)> {
    if request.objective.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Research objective must not be empty",
        ));
    }
    let snapshot = {
        let catalog = state.catalog.read().await;
        catalog
            .get(&request.workflow_slug, &request.workflow_version)
            .map(|workflow| workflow.snapshot())
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "Workflow {}@{}",
                    request.workflow_slug, request.workflow_version
                ))
            })?
    };
    validate_schema_value(&snapshot.manifest.input_schema, &request.input, "input")
        .map_err(ApiError::bad_request)?;
    let run = state.store.create_workflow_run(
        parse_id(&session_id, "Session")?,
        snapshot,
        request.objective.trim(),
        request.input,
        None,
    )?;
    state.scheduler.start(run.id).await?;
    Ok((StatusCode::CREATED, Json(run)))
}

#[derive(Serialize)]
struct WorkflowRunView {
    workflow_run: WorkflowRun,
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

async fn get_workflow_run_view(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<WorkflowRunView>> {
    let run_id = parse_id(&run_id, "WorkflowRun")?;
    let run = state.store.get_workflow_run(run_id)?;
    let participants = state.store.list_participants(run_id)?;
    let mut sessions = Vec::new();
    for participant in &participants {
        sessions.push(state.store.get_session(participant.session_id)?);
    }
    let actions = state.store.list_action_invocations(run_id)?;
    let mut attempts = Vec::new();
    for action in &actions {
        attempts.extend(state.store.list_action_attempts(action.id)?);
    }
    let channels = state.store.list_channels(run_id)?;
    let mut signals = Vec::new();
    for channel in &channels {
        signals.extend(state.store.list_signals(channel.id, 0)?);
    }
    Ok(Json(WorkflowRunView {
        workflow_run: run,
        participants,
        sessions,
        actions,
        attempts,
        teams: state.store.list_teams(run_id)?,
        relations: state.store.list_relations(run_id)?,
        task_scopes: state.store.list_task_scopes(run_id)?,
        timers: state.store.list_timers(run_id)?,
        channels,
        signals,
        human_requests: state.store.list_human_requests(run_id)?,
        control_messages: state.store.list_control_messages(run_id)?,
        artifacts: state.store.list_artifacts(run_id)?,
    }))
}

async fn pause_workflow_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .scheduler
        .pause(parse_id(&run_id, "WorkflowRun")?)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

async fn resume_workflow_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .scheduler
        .resume(parse_id(&run_id, "WorkflowRun")?)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_workflow_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .scheduler
        .cancel(parse_id(&run_id, "WorkflowRun")?)
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
    Path((run_id, session_id)): Path<(String, String)>,
    Json(request): Json<ControlRequest>,
) -> ApiResult<(StatusCode, Json<ControlMessage>)> {
    if request.content.trim().is_empty() {
        return Err(ApiError::bad_request("control message must not be empty"));
    }
    let run_id = parse_id(&run_id, "WorkflowRun")?;
    let session_id = parse_id(&session_id, "Session")?;
    let run = state.store.get_workflow_run(run_id)?;
    let is_member = state
        .store
        .list_participants(run_id)?
        .iter()
        .any(|item| item.session_id == session_id);
    if !is_member && run.origin_session_id != session_id {
        return Err(ApiError::bad_request(
            "Session does not participate in this WorkflowRun",
        ));
    }
    let message = state.store.create_control_message(
        run_id,
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

async fn list_workflow_run_events(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Vec<WorkflowRunEvent>>> {
    Ok(Json(state.store.list_workflow_run_events(
        parse_id(&run_id, "WorkflowRun")?,
        query.after,
    )?))
}

async fn stream_workflow_run_events(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let run_id = parse_id(&run_id, "WorkflowRun")?;
    state.store.get_workflow_run(run_id)?;
    let receiver = state.store.subscribe();
    let replay = state.store.list_workflow_run_events(run_id, query.after)?;
    let high_watermark = replay.last().map_or(query.after, |event| event.sequence);
    let replay = tokio_stream::iter(replay.into_iter().map(run_sse_event));
    let live = BroadcastStream::new(receiver).filter_map(move |result| match result {
        Ok(event) if event.workflow_run_id == run_id && event.sequence > high_watermark => {
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

fn run_sse_event(event: WorkflowRunEvent) -> Result<Event, Infallible> {
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
            WorkflowSchedulerError::TerminalRun { .. }
            | WorkflowSchedulerError::NotScheduled(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl From<WorkflowCatalogError> for ApiError {
    fn from(error: WorkflowCatalogError) -> Self {
        let status = match error {
            WorkflowCatalogError::Invalid(_) | WorkflowCatalogError::InvalidFile { .. } => {
                StatusCode::BAD_REQUEST
            }
            WorkflowCatalogError::Immutable { .. } | WorkflowCatalogError::Duplicate { .. } => {
                StatusCode::CONFLICT
            }
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
