use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::http::Request;
use axum::http::StatusCode;
use papermachine_model::ConfiguredModels;
use papermachine_model::ModelClient;
use papermachine_model::ModelProfile;
use papermachine_model::ModelRouter;
use papermachine_model::ScriptedModelClient;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::Project;
use papermachine_protocol::Session;
use papermachine_protocol::SessionId;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::WorkflowEvent;
use papermachine_server::ServerConfig;
use papermachine_server::ServerModelConfig;
use papermachine_server::initialize;
use papermachine_server::router;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tempfile::tempdir;
use tower::ServiceExt;

fn json_request(method: &str, uri: &str, value: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .expect("request should build")
}

fn empty_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request should build")
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("response body should load");
    serde_json::from_slice(&bytes).expect("response should contain JSON")
}

#[tokio::test]
async fn initialization_validates_resources_before_opening_application_data() {
    let directory = tempdir().expect("temporary directory should be created");
    let data_dir = directory.path().join("app-data");
    let error = initialize(&ServerConfig {
        resource_root: directory.path().join("incomplete-resources"),
        data_dir: data_dir.clone(),
        default_workspace_root: directory.path().join("workspaces"),
        models: ServerModelConfig::Demo,
        max_concurrent_runs: 1,
        max_parallel_actions: 1,
    })
    .await
    .err()
    .expect("an incomplete resource tree must be rejected");

    assert!(error.to_string().contains("built-in Workflow directory"));
    assert!(!data_dir.exists());
}

#[tokio::test]
async fn inactive_project_runtime_failures_are_lazy_and_isolated() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Lazy runtime").await;
    let broken_workflow = directory
        .path()
        .join("app-data/projects")
        .join(project.id.to_string())
        .join("workflows/broken/workflow.py");
    std::fs::create_dir_all(
        broken_workflow
            .parent()
            .expect("Workflow fixture should have a parent"),
    )
    .expect("Workflow fixture directory should be created");
    std::fs::write(&broken_workflow, "this is not valid Python (")
        .expect("invalid Workflow fixture should be written");
    drop(app);

    let restarted = test_app(&directory).await;
    let projects = restarted
        .clone()
        .oneshot(empty_request("GET", "/api/projects"))
        .await
        .expect("Project list should load");
    assert_eq!(projects.status(), StatusCode::OK);
    assert_eq!(
        response_json(projects).await.as_array().map(Vec::len),
        Some(1)
    );
    let overview = restarted
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project index should not require its runtime");
    assert_eq!(overview.status(), StatusCode::OK);

    let runtime_endpoint = restarted
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}/workflow-programs", project.id),
        ))
        .await
        .expect("runtime endpoint should report the local failure");
    assert_eq!(runtime_endpoint.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn managed_project_state_is_separate_from_the_user_workspace() {
    let directory = tempdir().expect("temporary directory should be created");
    let workspace = directory.path().join("research/portable-paper");
    let app = test_app(&directory).await;
    let response = app
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({
                "name": "Portable paper",
                "workspace": {"path": workspace},
            }),
        ))
        .await
        .expect("Project request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let project = response_json(response).await;

    let managed = directory
        .path()
        .join("app-data/projects")
        .join(project["id"].as_str().expect("Project id should exist"));
    assert!(directory.path().join("app-data/staging").is_dir());
    assert!(directory.path().join("app-data/trash").is_dir());
    assert!(managed.join("state/project.db").is_file());
    assert!(managed.join("artifacts").is_dir());
    assert!(managed.join("workflow-runtime").is_dir());
    assert!(managed.join("prompts/system.md").is_file());
    assert!(workspace.is_dir());
    assert!(
        std::fs::read_dir(&workspace)
            .expect("Workspace should list")
            .next()
            .is_none()
    );

    let broken_catalog_entry = directory.path().join("app-data/projects/not-a-project-id");
    std::fs::create_dir(&broken_catalog_entry).expect("broken catalog fixture should be created");
    let restarted = test_app(&directory).await;
    let projects = restarted
        .clone()
        .oneshot(empty_request("GET", "/api/projects"))
        .await
        .expect("Project catalog should load after restart");
    let projects = response_json(projects).await;
    assert_eq!(projects.as_array().map(Vec::len), Some(1));
    assert_eq!(projects[0]["id"], project["id"]);
    assert!(broken_catalog_entry.is_dir());

    let overview = restarted
        .oneshot(empty_request(
            "GET",
            &format!(
                "/api/projects/{}",
                project["id"].as_str().unwrap_or_default()
            ),
        ))
        .await
        .expect("Project state should reopen after restart");
    assert_eq!(overview.status(), StatusCode::OK);
}

#[tokio::test]
async fn project_without_a_workspace_uses_a_unique_default_directory() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;

    let first = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({"name": "Default project"}),
        ))
        .await
        .expect("first Project request should complete");
    assert_eq!(first.status(), StatusCode::CREATED);
    let first = response_json(first).await;
    let first_workspace = directory
        .path()
        .join("default-workspaces")
        .join("Default project")
        .canonicalize()
        .expect("default Workspace should be created");
    assert_eq!(
        first["workspace"]["path"],
        first_workspace.to_string_lossy().as_ref()
    );
    assert!(first.get("description").is_none());

    let second = app
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({"name": "Default project"}),
        ))
        .await
        .expect("second Project request should complete");
    assert_eq!(second.status(), StatusCode::CREATED);
    let second = response_json(second).await;
    let second_workspace = directory
        .path()
        .join("default-workspaces")
        .join("Default project 2")
        .canonicalize()
        .expect("suffixed default Workspace should be created");
    assert_eq!(
        second["workspace"]["path"],
        second_workspace.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn project_workspace_relocates_without_moving_managed_state_and_delete_preserves_workspace() {
    let directory = tempdir().expect("temporary directory should be created");
    let original_root = directory.path().join("research/original");
    let relocated_root = directory.path().join("research/relocated");
    let app = test_app(&directory).await;
    let created = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({
                "name": "Movable project",
                "workspace": {"path": original_root},
            }),
        ))
        .await
        .expect("Project request should complete");
    let project = response_json(created).await;
    let project_id = project["id"].as_str().expect("Project id should exist");
    assert_eq!(project["workspace_available"], true);

    let managed = directory.path().join("app-data/projects").join(project_id);
    assert!(managed.join("state/project.db").is_file());

    std::fs::rename(&original_root, &relocated_root)
        .expect("Project Workspace should move while the server is stopped");
    let restarted = test_app(&directory).await;
    let projects = restarted
        .clone()
        .oneshot(empty_request("GET", "/api/projects"))
        .await
        .expect("Project catalog should load");
    let projects = response_json(projects).await;
    assert_eq!(projects[0]["workspace_available"], false);

    let relocated = restarted
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/projects/{project_id}"),
            json!({"workspace": {"path": relocated_root}}),
        ))
        .await
        .expect("Project relocation should complete");
    assert_eq!(relocated.status(), StatusCode::OK);
    let relocated = response_json(relocated).await;
    assert_eq!(relocated["workspace_available"], true);
    assert_eq!(
        relocated["workspace"]["path"],
        relocated_root
            .canonicalize()
            .expect("relocated root should resolve")
            .to_string_lossy()
            .as_ref()
    );

    let removed = restarted
        .clone()
        .oneshot(empty_request(
            "DELETE",
            &format!("/api/projects/{project_id}"),
        ))
        .await
        .expect("Project removal should complete");
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert!(relocated_root.is_dir());
    assert!(!managed.exists());
}

#[tokio::test]
async fn project_removal_unloads_an_initialized_runtime_before_retiring_state() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Loaded runtime removal").await;
    let managed = directory
        .path()
        .join("app-data/projects")
        .join(project.id.to_string());

    let removed = app
        .oneshot(empty_request(
            "DELETE",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project removal should complete");
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert!(!managed.exists());
}

fn copy_directory(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("destination should be created");
    for entry in std::fs::read_dir(source).expect("source directory should be readable") {
        let entry = entry.expect("source entry should be readable");
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory(&source_path, &destination_path);
        } else {
            std::fs::copy(&source_path, &destination_path).expect("source file should be copied");
        }
    }
}

fn prepare_root(root: &Path) {
    let builtins = [
        "goal",
        "interactive-agent",
        "parallel-discovery",
        "project-summary",
    ];
    for slug in builtins {
        let builtin = root.join("workflows/builtin").join(slug);
        std::fs::create_dir_all(&builtin).expect("builtin directory should be created");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../workflows/builtin")
            .join(slug)
            .join("workflow.py");
        std::fs::copy(source, builtin.join("workflow.py"))
            .expect("builtin workflow should be copied");
    }
    let python_runtime = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python");
    copy_directory(&python_runtime, &root.join("python"));
}

async fn test_app(directory: &TempDir) -> Router {
    prepare_root(directory.path());
    let state = initialize(&ServerConfig {
        resource_root: directory.path().to_path_buf(),
        data_dir: directory.path().join("app-data"),
        default_workspace_root: directory.path().join("default-workspaces"),
        models: ServerModelConfig::Demo,
        max_concurrent_runs: 2,
        max_parallel_actions: 4,
    })
    .await
    .expect("server should initialize");
    router(state, directory.path().join("dist"))
}

async fn test_app_with_model_profiles(
    directory: &TempDir,
    scripted: ScriptedModelClient,
) -> Router {
    prepare_root(directory.path());
    let profiles = vec![
        ModelProfile {
            id: "research-model".to_string(),
            provider: "scripted".to_string(),
            model: "research-upstream".to_string(),
            context_window: 128_000,
            capabilities: Vec::new(),
            default_reasoning_effort: None,
            config_sha256: String::new(),
        },
        ModelProfile {
            id: "review-model".to_string(),
            provider: "scripted".to_string(),
            model: "review-upstream".to_string(),
            context_window: 128_000,
            capabilities: Vec::new(),
            default_reasoning_effort: None,
            config_sha256: String::new(),
        },
    ];
    let providers = HashMap::from([(
        "scripted".to_string(),
        Arc::new(scripted) as Arc<dyn ModelClient>,
    )]);
    let configured_models = ConfiguredModels {
        default_model: "research-model".to_string(),
        profiles: profiles.clone(),
        providers: Vec::new(),
        router: ModelRouter::new(profiles, providers).expect("model router should be valid"),
    };
    let state = initialize(&ServerConfig {
        resource_root: directory.path().to_path_buf(),
        data_dir: directory.path().join("app-data"),
        default_workspace_root: directory.path().join("default-workspaces"),
        models: ServerModelConfig::Providers(configured_models),
        max_concurrent_runs: 2,
        max_parallel_actions: 4,
    })
    .await
    .expect("server should initialize with model profiles");
    router(state, directory.path().join("dist"))
}

fn scripted_text(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::OutputTextDelta {
            delta: text.to_string(),
        },
        ModelEvent::Completed {
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 5,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
        },
    ]
}

async fn create_project(app: &Router, base: &Path, name: &str) -> Project {
    let directory_name = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let workspace_root = base.join("projects").join(directory_name);
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({
                "name": name,
                "workspace": {"path": workspace_root.to_string_lossy()}
            }),
        ))
        .await
        .expect("project request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    serde_json::from_value(response_json(response).await).expect("project should deserialize")
}

async fn create_project_and_session(app: &Router, base: &Path, name: &str) -> (Project, Session) {
    let project = create_project(app, base, name).await;
    let session = start_interactive_session(app, project.id, "Origin Session", "research").await;
    (project, session)
}

async fn start_interactive_session(
    app: &Router,
    project_id: papermachine_protocol::ProjectId,
    title: &str,
    access: &str,
) -> Session {
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{project_id}/workflows"),
            json!({
                "program_slug": "interactive-agent",
                "params": {
                    "session_title": title,
                    "agent_system_prompt": "",
                    "agent_access": access,
                },
                "model": "demo-model",
                "access": access,
            }),
        ))
        .await
        .expect("interactive Workflow request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let workflow = response_json(response).await;
    assert_eq!(workflow["request"], "");
    let workflow_id = workflow["id"]
        .as_str()
        .expect("interactive Workflow id should exist");
    for _ in 0..400 {
        let view = get_workflow_view(app, workflow_id).await;
        if let Some(session) = view["sessions"]
            .as_array()
            .and_then(|sessions| sessions.first())
        {
            return serde_json::from_value(session.clone())
                .expect("interactive Session should deserialize");
        }
        if matches!(
            view["workflow"]["status"].as_str(),
            Some("failed" | "cancelled")
        ) {
            panic!(
                "interactive Workflow terminated unexpectedly: {}",
                view["workflow"]
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("interactive Workflow did not create its Session")
}

async fn send_interactive_message(app: &Router, session_id: SessionId, message: &str) -> Value {
    let initial = app
        .clone()
        .oneshot(empty_request("GET", &format!("/api/sessions/{session_id}")))
        .await
        .expect("Session view should load before answering");
    let initial = response_json(initial).await;
    let prior_turns = initial["turns"].as_array().map_or(0, Vec::len);

    let request_id = loop {
        let response = app
            .clone()
            .oneshot(empty_request("GET", &format!("/api/sessions/{session_id}")))
            .await
            .expect("Session view should load while waiting for human input");
        let view = response_json(response).await;
        if let Some(request_id) = view["human_requests"]
            .as_array()
            .and_then(|requests| {
                requests.iter().find(|request| {
                    request["status"] == "open" && request["session_id"] == session_id.to_string()
                })
            })
            .and_then(|request| request["id"].as_str())
        {
            break request_id.to_string();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let answered = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/human-requests/{request_id}/answer"),
            json!({"answer": message}),
        ))
        .await
        .expect("human answer should complete");
    assert_eq!(answered.status(), StatusCode::OK);

    for _ in 0..400 {
        let response = app
            .clone()
            .oneshot(empty_request("GET", &format!("/api/sessions/{session_id}")))
            .await
            .expect("Session view should load while waiting for the Turn");
        let view = response_json(response).await;
        if let Some(turn) = view["turns"]
            .as_array()
            .filter(|turns| turns.len() > prior_turns)
            .and_then(|turns| turns.last())
            && turn["status"] == "completed"
        {
            return turn.clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("interactive message did not produce a completed Turn")
}

#[tokio::test]
async fn workflow_request_mode_matches_the_program_contract() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Request modes").await;

    let interactive_with_task = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "interactive-agent",
                "request": "This must be a Session message instead.",
                "params": {},
                "model": "demo-model",
                "access": "research"
            }),
        ))
        .await
        .expect("interactive request should be validated");
    assert_eq!(interactive_with_task.status(), StatusCode::BAD_REQUEST);

    let research_without_task = app
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "parallel-discovery",
                "params": {},
                "model": "demo-model",
                "access": "research"
            }),
        ))
        .await
        .expect("research request should be validated");
    assert_eq!(research_without_task.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn api_creates_and_updates_session_access_profiles() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, origin) = create_project_and_session(&app, directory.path(), "Access API").await;
    assert_eq!(origin.access.as_str(), "research");

    let created = start_interactive_session(&app, project.id, "No tools", "model_only").await;
    assert_eq!(created.access.as_str(), "model_only");

    let updated = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/sessions/{}/access", created.id),
            json!({"access": "workspace"}),
        ))
        .await
        .expect("access update should complete");
    assert_eq!(updated.status(), StatusCode::OK);
    let updated: Session = serde_json::from_value(response_json(updated).await)
        .expect("updated Session should deserialize");
    assert_eq!(updated.access.as_str(), "workspace");

    let invalid = app
        .oneshot(json_request(
            "PUT",
            &format!("/api/sessions/{}/access", created.id),
            json!({"access": "superuser"}),
        ))
        .await
        .expect("invalid access request should complete");
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn api_layers_project_and_session_system_prompts_into_user_turns() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, session) = create_project_and_session(&app, directory.path(), "Prompt API").await;

    let project_prompt = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/projects/{}/system-prompt", project.id),
            json!({"system_prompt": "Use only primary evidence."}),
        ))
        .await
        .expect("Project prompt request should complete");
    assert_eq!(project_prompt.status(), StatusCode::OK);
    let project_prompt = response_json(project_prompt).await;
    assert_eq!(project_prompt["content"], "Use only primary evidence.");
    assert_eq!(project_prompt["relative_path"], "prompts/system.md");

    let session_prompt = app
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/sessions/{}/system-prompt", session.id),
            json!({"system_prompt": "Answer in a compact evidence table."}),
        ))
        .await
        .expect("Session prompt request should complete");
    assert_eq!(session_prompt.status(), StatusCode::OK);
    let session_prompt: Session = serde_json::from_value(response_json(session_prompt).await)
        .expect("Session should deserialize");
    assert_eq!(
        session_prompt.system_prompt,
        "Answer in a compact evidence table."
    );

    let turn = send_interactive_message(&app, session.id, "Compare the sources.").await;
    assert_eq!(turn["origin"], "user");
    let kinds = turn["prompt"]["layers"]
        .as_array()
        .expect("prompt layers should be an array")
        .iter()
        .map(|layer| layer["kind"].as_str().expect("kind should be a string"))
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["runtime", "project", "workflow", "agent"]);

    let project_prompt = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}/system-prompt", project.id),
        ))
        .await
        .expect("Project prompt should load");
    assert_eq!(project_prompt.status(), StatusCode::OK);
    let project_prompt = response_json(project_prompt).await;
    assert_eq!(project_prompt["content"], "Use only primary evidence.");
}

async fn get_workflow_view(app: &Router, workflow_id: &str) -> Value {
    let response = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/workflows/{workflow_id}"),
        ))
        .await
        .expect("run view request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    response_json(response).await
}

async fn wait_for_workflow_status(app: &Router, workflow_id: &str, expected: &str) -> Value {
    let mut last_view = Value::Null;
    for _ in 0..400 {
        let view = get_workflow_view(app, workflow_id).await;
        if view["workflow"]["status"] == expected {
            return view;
        }
        if matches!(
            view["workflow"]["status"].as_str(),
            Some("failed" | "cancelled")
        ) {
            panic!("Workflow terminated unexpectedly: {}", view["workflow"]);
        }
        last_view = view;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Workflow did not reach {expected}: {last_view}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn interactive_session_turns_preserve_exact_human_message_provenance() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (_, session) =
        create_project_and_session(&app, directory.path(), "Interactive project").await;

    let session_view = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{}", session.id),
        ))
        .await
        .expect("Session view should complete");
    let session_view = response_json(session_view).await;
    let workflow_id = session_view["workflows"][0]["id"]
        .as_str()
        .expect("interactive Workflow should own the Session")
        .to_string();

    let (request_id, final_view) = {
        let mut request_id = None;
        let mut final_view = None;
        for _ in 0..400 {
            let view = get_workflow_view(&app, &workflow_id).await;
            if request_id.is_none() {
                request_id = view["human_requests"]
                    .as_array()
                    .and_then(|requests| {
                        requests.iter().find(|request| request["status"] == "open")
                    })
                    .and_then(|request| request["id"].as_str())
                    .map(str::to_string);
                if let Some(request_id) = request_id.as_ref() {
                    let answer = app
                        .clone()
                        .oneshot(json_request(
                            "POST",
                            &format!("/api/human-requests/{request_id}/answer"),
                            json!({"answer": "Inspect the prompt cache behavior."}),
                        ))
                        .await
                        .expect("human answer request should complete");
                    assert_eq!(answer.status(), StatusCode::OK);
                }
            } else if view["actions"]
                .as_array()
                .is_some_and(|actions| actions.len() == 1 && actions[0]["status"] == "completed")
                && view["human_requests"]
                    .as_array()
                    .is_some_and(|requests| requests.len() == 2)
            {
                final_view = Some(view);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (
            request_id.expect("interactive Workflow should ask for a message"),
            final_view.expect("interactive Workflow should complete one Turn and wait again"),
        )
    };

    assert_eq!(
        final_view["actions"][0]["source_human_request_id"],
        request_id
    );
    let session_view = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{}", session.id),
        ))
        .await
        .expect("updated Session view should complete");
    let session_view = response_json(session_view).await;
    assert_eq!(session_view["turns"].as_array().map(Vec::len), Some(1));
    assert_eq!(session_view["turns"][0]["origin"], "user");
    assert_eq!(
        session_view["turns"][0]["input"],
        "Inspect the prompt cache behavior."
    );
    assert!(
        session_view["turns"][0]["prompt"]["layers"]
            .as_array()
            .is_some_and(|layers| layers
                .iter()
                .any(|layer| layer["name"] == "Action contract"))
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn closing_a_session_archives_it_and_cancels_its_interactive_workflow() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, session) =
        create_project_and_session(&app, directory.path(), "Closable Session").await;
    let session_view = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{}", session.id),
        ))
        .await
        .expect("Session view should complete");
    let session_view = response_json(session_view).await;
    let workflow_id = session_view["workflows"][0]["id"]
        .as_str()
        .expect("interactive Workflow should exist")
        .to_string();

    let direct_cancel = app
        .clone()
        .oneshot(empty_request(
            "POST",
            &format!("/api/workflows/{workflow_id}/cancel"),
        ))
        .await
        .expect("interactive Workflow cancellation should be validated");
    assert_eq!(direct_cancel.status(), StatusCode::ACCEPTED);

    let close = app
        .clone()
        .oneshot(empty_request(
            "DELETE",
            &format!("/api/sessions/{}", session.id),
        ))
        .await
        .expect("Session close should complete");
    assert_eq!(close.status(), StatusCode::NO_CONTENT);

    let archived = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{}", session.id),
        ))
        .await
        .expect("archived Session should remain inspectable");
    assert_eq!(
        response_json(archived).await["session"]["status"],
        "archived"
    );

    let project_sessions = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}/sessions", project.id),
        ))
        .await
        .expect("Project Sessions should load");
    assert!(
        response_json(project_sessions)
            .await
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    let workflow = get_workflow_view(&app, &workflow_id).await;
    assert_eq!(workflow["workflow"]["status"], "cancelled");
    assert!(
        workflow["human_requests"]
            .as_array()
            .is_some_and(|requests| requests.iter().all(|request| request["status"] != "open"))
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn api_runs_python_workflow_as_project_owned_sessions() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;

    let health = app
        .clone()
        .oneshot(empty_request("GET", "/api/health"))
        .await
        .expect("health request should complete");
    assert_eq!(health.status(), StatusCode::OK);
    let health = response_json(health).await;
    assert_eq!(health["model_mode"], "demo");
    assert_eq!(health["workflow_runtime"], "python_effect_dsl");

    let (project, origin) =
        create_project_and_session(&app, directory.path(), "Parallel project").await;
    send_interactive_message(&app, origin.id, "Frame the comparison.").await;

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "parallel-discovery",
                "request": "Compare two implementation approaches.",
                "instructions": "Prefer directly comparable implementation evidence.",
                "params": {"perspectives": ["primary evidence", "failure modes"]},
                "started_from_session_id": origin.id,
                "model": "demo-model",
                "access": "research"
            }),
        ))
        .await
        .expect("run request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    let workflow_id = run["id"].as_str().expect("run id should be present");

    let view = wait_for_workflow_status(&app, workflow_id, "completed").await;
    assert_eq!(
        view["workflow"]["instructions"],
        "Prefer directly comparable implementation evidence."
    );
    assert_eq!(view["participants"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["sessions"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["actions"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["attempts"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["teams"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["relations"].as_array().map(Vec::len), Some(2));
    assert_eq!(view["task_scopes"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        view["workflow"]["request"],
        "Compare two implementation approaches."
    );
    assert_eq!(view["workflow"]["trigger"]["kind"], "user");
    assert_eq!(
        view["workflow"]["trigger"]["source_session_id"],
        origin.id.to_string()
    );
    assert!(
        view["workflow"]["output"]["summary"]
            .as_str()
            .is_some_and(|value| value.contains("Demo result"))
    );

    let response = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/workflows/{workflow_id}/events"),
        ))
        .await
        .expect("events request should complete");
    let events: Vec<WorkflowEvent> =
        serde_json::from_value(response_json(response).await).expect("events should deserialize");
    assert!(events.len() > 12);
    assert_eq!(events.first().map(|event| event.sequence), Some(1));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );

    let project_sessions = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}/sessions", project.id),
        ))
        .await
        .expect("Project Sessions should load");
    let project_sessions = response_json(project_sessions).await;
    assert_eq!(project_sessions.as_array().map(Vec::len), Some(4));

    let research_participant = view["participants"]
        .as_array()
        .expect("participants should exist")
        .iter()
        .find(|participant| participant["class_name"] == "Researcher")
        .expect("a Researcher participant should exist");
    let participant_session_id = research_participant["session_id"]
        .as_str()
        .expect("participant Session id should exist");
    let participant_view = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{participant_session_id}"),
        ))
        .await
        .expect("participant Session should load");
    let participant_view = response_json(participant_view).await;
    assert_eq!(participant_view["turns"].as_array().map(Vec::len), Some(1));
    assert_eq!(participant_view["turns"][0]["origin"], "workflow");
    let layers = participant_view["turns"][0]["prompt"]["layers"]
        .as_array()
        .expect("Workflow Turn prompt layers should exist");
    let prompt_kinds = layers
        .iter()
        .filter_map(|layer| layer["kind"].as_str())
        .collect::<Vec<_>>();
    assert!(prompt_kinds.contains(&"runtime"));
    assert!(prompt_kinds.contains(&"workflow"));
    assert!(prompt_kinds.contains(&"agent"));
    assert!(
        layers
            .iter()
            .any(|layer| layer["name"] == "Workflow run instructions")
    );
    assert!(
        layers
            .iter()
            .any(|layer| layer["name"] == "Action contract")
    );
    assert!(layers.iter().all(|layer| {
        layer["name"] != "Workflow objective"
            && layer["name"] != "Workflow launch context"
            && !layer["content"]
                .as_str()
                .is_some_and(|content| content.contains("Compare two implementation approaches."))
    }));
    let turn_input = participant_view["turns"][0]["input"]
        .as_str()
        .expect("Action Turn input should be text");
    assert!(turn_input.starts_with("Action arguments (Workflow-provided data):"));
    assert_eq!(
        turn_input
            .matches("Compare two implementation approaches.")
            .count(),
        1
    );
    let research_action = view["actions"]
        .as_array()
        .expect("actions should exist")
        .iter()
        .find(|action| action["agent_instance_id"] == research_participant["id"])
        .expect("Researcher Action should exist");
    assert!(
        research_action["contract"]
            .as_str()
            .is_some_and(|contract| contract.contains("Investigate the question"))
    );
    assert_eq!(
        research_action["arguments"]["question"],
        "Compare two implementation approaches."
    );
    assert_eq!(
        participant_view["workflow_memberships"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    let stream = app
        .oneshot(empty_request(
            "GET",
            &format!(
                "/api/workflows/{workflow_id}/events/stream?after={}",
                events.len()
            ),
        ))
        .await
        .expect("SSE request should connect");
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(
        stream
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn goal_reuses_one_tool_capable_session_and_stops_on_same_turn_completion() {
    let directory = tempdir().expect("temporary directory should be created");
    let scripted = ScriptedModelClient::new([
        scripted_text("Inspected the objective.\n<!-- papermachine-goal:active -->"),
        scripted_text("Implemented and verified it.\n<!-- papermachine-goal:complete -->"),
    ]);
    let app = test_app_with_model_profiles(&directory, scripted.clone()).await;
    let project = create_project(&app, directory.path(), "Goal loop").await;

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "goal",
                "request": "Implement and verify the requested change.",
                "params": {
                    "session_title": "Persistent Goal",
                    "agent_model": "research-model",
                    "agent_access": "research"
                },
                "model": "research-model",
                "access": "research"
            }),
        ))
        .await
        .expect("Goal Workflow request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let workflow = response_json(response).await;
    let workflow_id = workflow["id"].as_str().expect("Workflow id should exist");
    let view = wait_for_workflow_status(&app, workflow_id, "completed").await;

    assert_eq!(view["participants"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["actions"].as_array().map(Vec::len), Some(2));
    assert_eq!(view["attempts"].as_array().map(Vec::len), Some(2));
    assert_eq!(view["workflow"]["output"]["status"], "complete");
    assert_eq!(view["workflow"]["output"]["iterations"], 2);
    assert_eq!(
        view["workflow"]["output"]["result"],
        "Implemented and verified it."
    );
    assert!(
        view["actions"]
            .as_array()
            .expect("Actions should exist")
            .iter()
            .all(|action| action["action_name"] == "work")
    );

    let requests = scripted
        .requests()
        .expect("scripted model requests should load");
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.response_format.is_none())
    );
    assert!(requests.iter().all(|request| !request.tools.is_empty()));
    assert!(requests.iter().all(|request| {
        request
            .instructions
            .contains("<!-- papermachine-goal:complete -->")
    }));

    let session_id = view["sessions"][0]["id"]
        .as_str()
        .expect("Goal Session id should exist");
    let session = app
        .oneshot(empty_request("GET", &format!("/api/sessions/{session_id}")))
        .await
        .expect("Goal Session should load");
    let session = response_json(session).await;
    assert_eq!(session["turns"].as_array().map(Vec::len), Some(2));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn workflow_model_params_bind_each_agent_session_to_a_profile() {
    let directory = tempdir().expect("temporary directory should be created");
    let scripted = ScriptedModelClient::new([
        scripted_text("first route"),
        scripted_text("second route"),
        scripted_text("combined result"),
    ]);
    let app = test_app_with_model_profiles(&directory, scripted.clone()).await;
    let project = create_project(&app, directory.path(), "Per-Agent models").await;

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "parallel-discovery",
                "request": "Research in parallel and review with another model.",
                "params": {
                    "perspectives": ["first route", "second route"],
                    "research_model": "research-model",
                    "synthesis_model": "review-model"
                },
                "model": "research-model",
                "access": "research"
            }),
        ))
        .await
        .expect("multi-model Workflow request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let workflow = response_json(response).await;
    let workflow_id = workflow["id"].as_str().expect("Workflow id should exist");
    let view = wait_for_workflow_status(&app, workflow_id, "completed").await;

    for participant in view["participants"]
        .as_array()
        .expect("participants should exist")
    {
        let session_id = &participant["session_id"];
        let session = view["sessions"]
            .as_array()
            .expect("participant Sessions should exist")
            .iter()
            .find(|session| session["id"] == *session_id)
            .expect("participant Session should exist");
        let expected = match participant["class_name"].as_str() {
            Some("Researcher") => "research-model",
            Some("Synthesizer") => "review-model",
            other => panic!("unexpected Agent class {other:?}"),
        };
        assert_eq!(session["model"], expected);
        let session_id = session_id
            .as_str()
            .expect("participant Session id should be a string");
        let session_view = app
            .clone()
            .oneshot(empty_request("GET", &format!("/api/sessions/{session_id}")))
            .await
            .expect("participant Session should load");
        let session_view = response_json(session_view).await;
        let route = &session_view["turns"][0]["model_route"];
        assert_eq!(route["profile"], expected);
        assert_eq!(route["provider"], "scripted");
        assert_eq!(
            route["upstream_model"],
            if expected == "research-model" {
                "research-upstream"
            } else {
                "review-upstream"
            }
        );
        assert_eq!(route["context_window"], 128_000);
        assert_eq!(route["config_sha256"].as_str().map(str::len), Some(64));
    }

    let upstream_models = scripted
        .requests()
        .expect("scripted requests should load")
        .into_iter()
        .map(|request| request.model)
        .collect::<Vec<_>>();
    assert_eq!(
        upstream_models
            .iter()
            .filter(|model| model.as_str() == "research-upstream")
            .count(),
        2
    );
    assert_eq!(
        upstream_models
            .iter()
            .filter(|model| model.as_str() == "review-upstream")
            .count(),
        1
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn workflow_launch_configuration_captures_context_and_enforces_access_bounds() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, origin) =
        create_project_and_session(&app, directory.path(), "Configured launch").await;
    send_interactive_message(
        &app,
        origin.id,
        "Reuse this prior evidence instead of restarting.",
    )
    .await;

    let above_origin = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "parallel-discovery",
                "request": "Reject access above the origin Session.",
                "started_from_session_id": origin.id,
                "model": "demo-model",
                "access": "full_access"
            }),
        ))
        .await
        .expect("origin ceiling request should complete");
    assert_eq!(above_origin.status(), StatusCode::CONFLICT);

    let above_workflow = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "parallel-discovery",
                "request": "Reject an Agent override above the Workflow.",
                "started_from_session_id": origin.id,
                "model": "demo-model",
                "access": "model_only",
                "agent_access_overrides": {"Researcher": "research"}
            }),
        ))
        .await
        .expect("Workflow ceiling request should complete");
    assert_eq!(above_workflow.status(), StatusCode::CONFLICT);

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "parallel-discovery",
                "request": "Continue from the existing Project evidence.",
                "params": {"perspectives": ["prior primary evidence"]},
                "started_from_session_id": origin.id,
                "context_mode": "project_snapshot",
                "model": "demo-model",
                "access": "research",
                "agent_access_overrides": {
                    "Researcher": "read_only",
                    "Synthesizer": "model_only"
                }
            }),
        ))
        .await
        .expect("configured launch should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    assert_eq!(run["launch_context"]["mode"], "project_snapshot");
    assert_eq!(
        run["launch_context"]["snapshot"]["focus_session_id"],
        origin.id.to_string()
    );
    assert!(
        run["launch_context"]["snapshot"]["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions.iter().any(|session| {
                session["id"] == origin.id.to_string()
                    && session["turns"].as_array().is_some_and(|turns| {
                        turns.iter().any(|turn| {
                            turn["input"] == "Reuse this prior evidence instead of restarting."
                        })
                    })
            }))
    );
    assert_eq!(run["agent_access_overrides"]["Researcher"], "read_only");
    let workflow_id = run["id"].as_str().expect("Workflow id should exist");

    let completed = wait_for_workflow_status(&app, workflow_id, "completed").await;
    for participant in completed["participants"]
        .as_array()
        .expect("participants should be present")
    {
        let expected_access = match participant["class_name"].as_str() {
            Some("Researcher") => "read_only",
            Some("Synthesizer") => "model_only",
            Some("ContextAnalyst") => "model_only",
            other => panic!("unexpected Agent class {other:?}"),
        };
        let session_id = &participant["session_id"];
        let session = completed["sessions"]
            .as_array()
            .expect("participant Sessions should be present")
            .iter()
            .find(|session| session["id"] == *session_id)
            .expect("participant Session should be present");
        assert_eq!(session["access"], expected_access);
    }

    let context_session_id = completed["participants"]
        .as_array()
        .expect("participants should be present")
        .iter()
        .find(|participant| participant["class_name"] == "ContextAnalyst")
        .and_then(|participant| participant["session_id"].as_str())
        .expect("ContextAnalyst Session id should exist");
    let context_session = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{context_session_id}"),
        ))
        .await
        .expect("ContextAnalyst Session should load");
    let context_session = response_json(context_session).await;
    assert!(
        context_session["turns"][0]["input"]
            .as_str()
            .is_some_and(|input| input.contains("Reuse this prior evidence instead of restarting.")),
        "the Workflow must explicitly pass selected Project context to its context analyst"
    );

    let participant_session_id = completed["participants"]
        .as_array()
        .expect("participants should be present")
        .iter()
        .find(|participant| participant["class_name"] == "Researcher")
        .and_then(|participant| participant["session_id"].as_str())
        .expect("Researcher Session id should exist");
    let participant = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{participant_session_id}"),
        ))
        .await
        .expect("participant Session should load");
    let participant = response_json(participant).await;
    let layers = participant["turns"][0]["prompt"]["layers"]
        .as_array()
        .expect("Workflow Turn prompt layers should exist");
    assert!(
        layers
            .iter()
            .all(|layer| layer["name"] != "Workflow launch context"),
        "launch context is data and must only enter a Turn when the Workflow passes it"
    );
    assert!(
        participant["turns"][0]["input"].as_str().is_some_and(
            |input| !input.contains("Reuse this prior evidence instead of restarting.")
        ),
        "Researcher receives the compact brief rather than the raw Project snapshot"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn project_summary_publishes_an_html_home_page_fragment() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, _origin) =
        create_project_and_session(&app, directory.path(), "Summary project").await;

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "project-summary",
                "request": "Refresh the Project home page now.",
                "instructions": "Lead with verified progress and unresolved blockers.",
                "params": {
                    "interval_minutes": 0,
                    "max_sessions": 50,
                    "turns_per_session": 12,
                    "max_artifacts": 50
                },
                "model": "demo-model",
                "access": "model_only"
            }),
        ))
        .await
        .expect("summary Workflow request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let workflow = response_json(response).await;
    let workflow_id = workflow["id"]
        .as_str()
        .expect("summary Workflow id should exist");
    let view = wait_for_workflow_status(&app, workflow_id, "completed").await;
    let artifacts = view["artifacts"]
        .as_array()
        .expect("summary Artifacts should be present");
    assert_eq!(artifacts.len(), 2);
    let summary = artifacts
        .iter()
        .find(|artifact| artifact["metadata"]["role"] == "project_summary")
        .expect("published Project-home Artifact should exist");
    let source = artifacts
        .iter()
        .find(|artifact| artifact["metadata"]["role"] == "project_summary_source")
        .expect("Project-home block source Artifact should exist");
    let action_id = view["actions"]
        .as_array()
        .expect("summary Actions should exist")
        .iter()
        .find(|action| action["action_name"] == "maintain_project_home")
        .and_then(|action| action["id"].as_str())
        .expect("Project-home Action should exist");
    assert_eq!(summary["media_type"], "text/html; charset=utf-8");
    assert_eq!(summary["metadata"]["source_artifact_id"], source["id"]);
    assert_eq!(summary["action_invocation_id"], action_id);
    assert_eq!(source["action_invocation_id"], action_id);

    let overview = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project overview should load its canonical home");
    let overview = response_json(overview).await;
    assert_eq!(overview["project_home"]["artifact_id"], summary["id"]);
    assert_eq!(overview["project_home"]["source_artifact_id"], source["id"]);
    assert_eq!(overview["project_home_artifact"]["id"], summary["id"]);

    let artifact_id = summary["id"]
        .as_str()
        .expect("summary Artifact id should exist");
    let response = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/artifacts/{artifact_id}/content"),
        ))
        .await
        .expect("summary Artifact should load");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok()),
        Some("sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:")
    );
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("summary HTML should load");
    let html = String::from_utf8(bytes.to_vec()).expect("summary should be UTF-8 HTML");
    assert!(html.to_ascii_lowercase().contains("<article"));
    assert!(!html.to_ascii_lowercase().contains("<iframe"));

    let session_id = view["participants"][0]["session_id"]
        .as_str()
        .expect("summary Agent Session should exist");
    let session = app
        .clone()
        .oneshot(empty_request("GET", &format!("/api/sessions/{session_id}")))
        .await
        .expect("summary Agent Session should load");
    let session = response_json(session).await;
    let turn_tool_set = &session["turns"][0]["tool_set"];
    assert_eq!(
        turn_tool_set["definitions"]
            .as_array()
            .expect("summary Turn tool definitions should exist")
            .iter()
            .filter_map(|definition| definition["name"].as_str())
            .collect::<Vec<_>>(),
        vec![
            "patch_project_home",
            "preview_project_home",
            "read_project_home"
        ]
    );
    assert_eq!(turn_tool_set["sha256"].as_str().map(str::len), Some(64));
    let tool_names = session["steps"]
        .as_array()
        .expect("summary Agent Steps should exist")
        .iter()
        .filter(|step| step["kind"] == "tool")
        .filter_map(|step| step["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        tool_names,
        vec![
            "read_project_home",
            "patch_project_home",
            "preview_project_home"
        ]
    );

    let overview = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project overview should load");
    let overview = response_json(overview).await;
    assert_eq!(overview["project_home_artifact"]["id"], artifact_id);
    assert!(overview.get("artifacts").is_none());
}

#[tokio::test]
async fn api_generates_validates_and_publishes_python_workflows() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, _origin) =
        create_project_and_session(&app, directory.path(), "Workflow authoring").await;
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs/generate", project.id),
            json!({
                "name": "Claim challenge",
                "slug": "claim-challenge",
                "description": "Map evidence for a claim, then challenge it with counterevidence."
            }),
        ))
        .await
        .expect("generation request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    let generated = response_json(response).await;
    let source = generated["source"]
        .as_str()
        .expect("generated source should exist");
    assert_eq!(generated["validation"]["valid"], true);
    assert_eq!(
        generated["validation"]["manifest"]["slug"],
        "claim-challenge"
    );
    assert_eq!(
        generated["validation"]["agents"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(generated["validation"]["agents"][0]["access"], "research");
    assert_eq!(generated["validation"]["agents"][1]["access"], "model_only");
    assert_eq!(generated["validation"]["features"]["parallel_blocks"], 1);

    let unknown_tool_source = r#"from papermachine import Agent, action, workflow

class Worker(Agent):
    access = "research"

    @action(tools=["unknown_local_tool"])
    async def work(self):
        """Do work."""

@workflow(slug="unknown-tool", name="Unknown tool", description="Reject unknown tools.")
async def main(ctx):
    await Worker().work()
"#;
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs/validate", project.id),
            json!({"source": unknown_tool_source}),
        ))
        .await
        .expect("unknown Action tool should be validated");
    assert_eq!(response.status(), StatusCode::OK);
    let unknown_tool = response_json(response).await;
    assert_eq!(unknown_tool["valid"], false);
    assert!(
        unknown_tool["diagnostics"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("unknown tool"))
            }))
    );

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs/validate", project.id),
            json!({"source": source}),
        ))
        .await
        .expect("validation request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["valid"], true);

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs", project.id),
            json!({"source": source}),
        ))
        .await
        .expect("publish request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(
        directory
            .path()
            .join("app-data/projects")
            .join(project.id.to_string())
            .join("workflows/claim-challenge/workflow.py")
            .is_file()
    );
    assert!(
        std::fs::read_dir(&project.workspace.path)
            .expect("Workspace should list")
            .next()
            .is_none(),
        "publishing a Workflow must not write PaperMachine state into the Workspace"
    );

    let response = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!(
                "/api/projects/{}/workflow-programs/claim-challenge",
                project.id
            ),
        ))
        .await
        .expect("published workflow should load");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["source"], source);

    let changed = source.replace(
        "Run two independent evidence routes",
        "Run independent evidence routes",
    );
    let replaced = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs", project.id),
            json!({"source": changed}),
        ))
        .await
        .expect("replacement request should complete");
    assert_eq!(replaced.status(), StatusCode::CREATED);
    let loaded = app
        .oneshot(empty_request(
            "GET",
            &format!(
                "/api/projects/{}/workflow-programs/claim-challenge",
                project.id
            ),
        ))
        .await
        .expect("replaced workflow should load");
    assert_eq!(loaded.status(), StatusCode::OK);
    assert_eq!(response_json(loaded).await["source"], changed);

    let restarted = test_app(&directory).await;
    let reloaded = restarted
        .oneshot(empty_request(
            "GET",
            &format!(
                "/api/projects/{}/workflow-programs/claim-challenge",
                project.id
            ),
        ))
        .await
        .expect("filesystem catalog should reload after restart");
    assert_eq!(reloaded.status(), StatusCode::OK);
    assert_eq!(response_json(reloaded).await["source"], changed);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn workflow_can_pause_request_human_input_and_resume() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, origin) =
        create_project_and_session(&app, directory.path(), "Human-guided project").await;
    let source = r#"from papermachine import ask_human, workflow


@workflow(
    slug="human-decision",
    name="Human decision",
    description="Wait for a human decision before completing.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    answer = await ask_human(
        "Which direction should the project take?",
        response_schema={"type": "string"},
    )
    return {"decision": answer}
"#;
    let publish = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs", project.id),
            json!({"source": source}),
        ))
        .await
        .expect("workflow should publish");
    assert_eq!(publish.status(), StatusCode::CREATED);
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "human-decision",
                "request": "Choose a project direction.",
                "params": {},
                "started_from_session_id": origin.id,
                "model": "demo-model",
                "access": "research"
            }),
        ))
        .await
        .expect("run should start");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    let workflow_id = run["id"].as_str().expect("run id should exist");

    let mut request_id = None;
    for _ in 0..200 {
        let view = get_workflow_view(&app, workflow_id).await;
        request_id = view["human_requests"].as_array().and_then(|items| {
            items
                .iter()
                .find(|item| item["status"] == "open")
                .and_then(|item| item["id"].as_str())
                .map(str::to_string)
        });
        if request_id.is_some() {
            assert_eq!(view["workflow"]["attention_required"], true);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let request_id = request_id.expect("human request should open");

    let pause = app
        .clone()
        .oneshot(empty_request(
            "POST",
            &format!("/api/workflows/{workflow_id}/pause"),
        ))
        .await
        .expect("pause should complete");
    assert_eq!(pause.status(), StatusCode::ACCEPTED);
    wait_for_workflow_status(&app, workflow_id, "paused").await;
    let resume = app
        .clone()
        .oneshot(empty_request(
            "POST",
            &format!("/api/workflows/{workflow_id}/resume"),
        ))
        .await
        .expect("resume should complete");
    assert_eq!(resume.status(), StatusCode::ACCEPTED);

    let invalid = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/human-requests/{request_id}/answer"),
            json!({"answer": true}),
        ))
        .await
        .expect("invalid answer should be checked");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let answer = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/human-requests/{request_id}/answer"),
            json!({"answer": "Prioritize primary evidence."}),
        ))
        .await
        .expect("answer should complete");
    let answer_status = answer.status();
    let answer_body = response_json(answer).await;
    assert_eq!(answer_status, StatusCode::OK, "{answer_body}");

    let completed = wait_for_workflow_status(&app, workflow_id, "completed").await;
    assert_eq!(
        completed["workflow"]["output"]["decision"],
        "Prioritize primary evidence."
    );
    assert_eq!(completed["workflow"]["attention_required"], false);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn workflow_access_escalation_requires_a_human_grant() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, _origin) = create_project_and_session(&app, directory.path(), "Escalation").await;
    let source = r#"from papermachine import Agent, Team, action, workflow


class HostInspector(Agent):
    access = "research"
    role = "host inspection"

    @action
    async def inspect(self, question: str) -> str:
        """Answer the question after access has been granted."""


@workflow(
    slug="access-grant",
    name="Access grant",
    description="Require a human grant before creating a full-access Agent Session.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    inspector = HostInspector(name="Host inspector")
    team = Team("Host team", inspector)
    await team.activate()
    await inspector.set_access("full_access")
    answer = await inspector.inspect(ctx.request)
    return {"answer": answer}
"#;
    let publish = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs", project.id),
            json!({"source": source}),
        ))
        .await
        .expect("workflow should publish");
    assert_eq!(publish.status(), StatusCode::CREATED);
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "access-grant",
                "request": "Inspect the configured environment.",
                "params": {},
                "model": "demo-model",
                "access": "full_access"
            }),
        ))
        .await
        .expect("run should start");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    let workflow_id = run["id"].as_str().expect("run id should exist");

    let mut open_request = None;
    let mut participant_session_id = None;
    for _ in 0..200 {
        let view = get_workflow_view(&app, workflow_id).await;
        participant_session_id = view["sessions"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["id"].as_str())
            .map(str::to_string);
        open_request = view["human_requests"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["status"] == "open").cloned());
        if open_request.is_some() {
            assert_eq!(view["sessions"][0]["access"], "research");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let request = open_request.expect("access grant request should open");
    assert_eq!(request["response_schema"]["type"], "boolean");
    assert_eq!(
        request["response_schema"]["requested_access"],
        "full_access"
    );
    assert!(
        request["question"]
            .as_str()
            .is_some_and(|question| question.contains("research to full_access"))
    );
    let request_id = request["id"].as_str().expect("request id should exist");
    let participant_session_id = participant_session_id.expect("participant Session should exist");

    let answer = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/human-requests/{request_id}/answer"),
            json!({"answer": true}),
        ))
        .await
        .expect("grant should complete");
    assert_eq!(answer.status(), StatusCode::OK);
    let completed = wait_for_workflow_status(&app, workflow_id, "completed").await;
    assert_eq!(completed["sessions"][0]["access"], "full_access");

    let participant = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/sessions/{participant_session_id}"),
        ))
        .await
        .expect("participant Session should load");
    assert_eq!(participant.status(), StatusCode::OK);
    let participant = response_json(participant).await;
    assert_eq!(participant["session"]["access"], "full_access");
    assert_eq!(
        participant["turns"][0]["environment"]["authorization"]["preset"],
        "full_access"
    );
}
