use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::http::Request;
use axum::http::StatusCode;
use papermachine_model::ConfiguredModels;
use papermachine_model::ModelApi;
use papermachine_model::ModelClient;
use papermachine_model::ModelProfile;
use papermachine_model::ModelRouter;
use papermachine_model::ScriptedModelClient;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::Project;
use papermachine_protocol::ProjectId;
use papermachine_protocol::Session;
use papermachine_protocol::SessionEvent;
use papermachine_protocol::SessionId;
use papermachine_protocol::TokenUsage;
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
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("response body should load");
    serde_json::from_slice(&bytes).expect("response should contain JSON")
}

fn prepare_root(root: &Path) {
    for slug in [
        "goal",
        "interactive-agent",
        "parallel-universe",
        "project-summary",
    ] {
        let builtin = root.join("workflows/builtin").join(slug);
        std::fs::create_dir_all(&builtin).expect("builtin directory should be created");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../workflows/builtin")
            .join(slug)
            .join("workflow.pm");
        std::fs::copy(source, builtin.join("workflow.pm")).expect("builtin Workflow should copy");
    }
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
            api: ModelApi::OpenAiResponses,
            model: "research-upstream".to_string(),
            context_window: 128_000,
            capabilities: Vec::new(),
            default_reasoning_effort: None,
            config_sha256: String::new(),
        },
        ModelProfile {
            id: "review-model".to_string(),
            provider: "scripted".to_string(),
            api: ModelApi::OpenAiResponses,
            model: "review-upstream".to_string(),
            context_window: 128_000,
            capabilities: Vec::new(),
            default_reasoning_effort: None,
            config_sha256: String::new(),
        },
    ];
    let configured = ConfiguredModels {
        default_model: "research-model".to_string(),
        profiles: profiles.clone(),
        providers: Vec::new(),
        router: ModelRouter::new(profiles, {
            let client = Arc::new(scripted) as Arc<dyn ModelClient>;
            HashMap::from([
                ("research-model".to_string(), Arc::clone(&client)),
                ("review-model".to_string(), client),
            ])
        })
        .expect("model router should be valid"),
    };
    let state = initialize(&ServerConfig {
        resource_root: directory.path().to_path_buf(),
        data_dir: directory.path().join("app-data"),
        default_workspace_root: directory.path().join("default-workspaces"),
        models: ServerModelConfig::Providers(configured),
        max_concurrent_runs: 2,
        max_parallel_actions: 4,
    })
    .await
    .expect("server should initialize");
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
                ..TokenUsage::default()
            },
        },
    ]
}

async fn create_project(app: &Router, base: &Path, name: &str) -> Project {
    let workspace = base.join("workspaces").join(
        name.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>(),
    );
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({"name": name, "workspace": {"path": workspace}}),
        ))
        .await
        .expect("Project request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    serde_json::from_value(response_json(response).await).expect("Project should deserialize")
}

async fn create_session(app: &Router, project_id: ProjectId, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{project_id}/sessions"),
            body,
        ))
        .await
        .expect("Session request should complete");
    let status = response.status();
    (status, response_json(response).await)
}

async fn start_interactive_session(
    app: &Router,
    project_id: ProjectId,
    title: &str,
    access: &str,
) -> Session {
    let (status, value) = create_session(
        app,
        project_id,
        json!({
            "program_slug": "interactive-agent",
            "title": title,
            "instructions": "",
            "params": {
                "session_title": title,
                "agent_access": access,
            },
            "model": "demo-model",
            "access": access,
            "enabled_skills": [],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    serde_json::from_value(value).expect("Session should deserialize")
}

async fn get_session_view(app: &Router, project_id: ProjectId, session_id: SessionId) -> Value {
    let response = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{project_id}/sessions/{session_id}"),
        ))
        .await
        .expect("Session view should load");
    assert_eq!(response.status(), StatusCode::OK);
    response_json(response).await
}

async fn wait_for_session_status(
    app: &Router,
    project_id: ProjectId,
    session_id: SessionId,
    statuses: &[&str],
) -> Value {
    for _ in 0..600 {
        let view = get_session_view(app, project_id, session_id).await;
        if view["session"]["status"] == "failed" {
            panic!("Session failed: {}", view["session"]);
        }
        if view["session"]["status"]
            .as_str()
            .is_some_and(|status| statuses.contains(&status))
        {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Session did not reach {statuses:?}")
}

async fn send_interactive_message(
    app: &Router,
    project_id: ProjectId,
    session_id: SessionId,
    message: &str,
) -> Value {
    let before = get_session_view(app, project_id, session_id).await;
    let prior_turns = before["turns"].as_array().map_or(0, Vec::len);
    let request_id = loop {
        let view = get_session_view(app, project_id, session_id).await;
        if let Some(id) = view["human_requests"]
            .as_array()
            .and_then(|requests| requests.iter().find(|request| request["status"] == "open"))
            .and_then(|request| request["id"].as_str())
        {
            break id.to_string();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let answered = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{project_id}/human-requests/{request_id}/answer"),
            json!({"answer": message}),
        ))
        .await
        .expect("Human answer should complete");
    assert_eq!(answered.status(), StatusCode::OK);

    for _ in 0..600 {
        let view = get_session_view(app, project_id, session_id).await;
        if let Some(turn) = view["turns"]
            .as_array()
            .filter(|turns| turns.len() > prior_turns)
            .and_then(|turns| turns.last())
            && turn["status"] == "completed"
        {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("interactive message did not complete")
}

#[tokio::test]
async fn initialization_validates_resources_before_writing_application_data() {
    let directory = tempdir().expect("temporary directory should be created");
    let data_dir = directory.path().join("app-data");
    let error = initialize(&ServerConfig {
        resource_root: directory.path().join("missing-resources"),
        data_dir: data_dir.clone(),
        default_workspace_root: directory.path().join("workspaces"),
        models: ServerModelConfig::Demo,
        max_concurrent_runs: 1,
        max_parallel_actions: 1,
    })
    .await
    .err()
    .expect("incomplete resources must fail");
    assert!(!error.to_string().is_empty());
    assert!(!data_dir.exists());
}

#[tokio::test]
async fn project_lifecycle_keeps_managed_state_out_of_the_workspace() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let workspace = directory.path().join("research/original");
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({"name": "Portable", "workspace": {"path": workspace}}),
        ))
        .await
        .expect("Project should create");
    assert_eq!(response.status(), StatusCode::CREATED);
    let project = response_json(response).await;
    let id = project["id"].as_str().expect("Project id should exist");
    let managed = directory.path().join("app-data/projects").join(id);
    assert!(managed.join("state/project.db").is_file());
    assert!(managed.join("prompts/system.md").is_file());
    assert!(
        std::fs::read_dir(&workspace)
            .expect("Workspace should list")
            .next()
            .is_none()
    );

    let relocated = directory.path().join("research/relocated");
    std::fs::rename(&workspace, &relocated).expect("Workspace should move externally");
    let restarted = test_app(&directory).await;
    let catalog = restarted
        .clone()
        .oneshot(empty_request("GET", "/api/projects"))
        .await
        .expect("catalog should load");
    assert_eq!(
        response_json(catalog).await[0]["workspace_available"],
        false
    );
    let response = restarted
        .clone()
        .oneshot(json_request(
            "PUT",
            &format!("/api/projects/{id}"),
            json!({"workspace": {"path": relocated}}),
        ))
        .await
        .expect("Project should relocate");
    assert_eq!(response.status(), StatusCode::OK);
    let response = restarted
        .oneshot(empty_request("DELETE", &format!("/api/projects/{id}")))
        .await
        .expect("Project should remove");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        relocated.is_dir(),
        "removing a Project must preserve its Workspace"
    );
}

#[tokio::test]
async fn inactive_project_runtime_failure_is_lazy_and_isolated() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Lazy runtime").await;
    let broken = directory
        .path()
        .join("app-data/projects")
        .join(project.id.to_string())
        .join("workflows/broken/workflow.pm");
    std::fs::create_dir_all(
        broken
            .parent()
            .expect("broken Workflow fixture should have a parent"),
    )
    .expect("fixture directory should create");
    std::fs::write(&broken, "not valid Workflow Language (").expect("fixture should write");
    drop(app);

    let restarted = test_app(&directory).await;
    assert_eq!(
        restarted
            .clone()
            .oneshot(empty_request("GET", "/api/projects"))
            .await
            .expect("Project catalog should remain available")
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        restarted
            .oneshot(empty_request(
                "GET",
                &format!("/api/projects/{}/workflow-programs", project.id),
            ))
            .await
            .expect("broken Project request should return a response")
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn session_creation_validates_program_contract_and_project_scope() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let owner = create_project(&app, directory.path(), "Owner").await;
    let other = create_project(&app, directory.path(), "Other").await;
    let (status, _) = create_session(
        &app,
        owner.id,
        json!({
            "program_slug": "interactive-agent",
            "request": "not allowed",
            "instructions": "",
            "params": {},
            "model": "demo-model",
            "access": "workspace"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = create_session(
        &app,
        owner.id,
        json!({
            "program_slug": "parallel-universe",
            "instructions": "",
            "params": {},
            "model": "demo-model",
            "access": "workspace"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let session = start_interactive_session(&app, owner.id, "Scoped", "workspace").await;
    let wrong_project = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}/sessions/{}", other.id, session.id),
        ))
        .await
        .expect("scoped request should complete");
    assert_eq!(wrong_project.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workflow_params_enforce_required_optional_and_default_semantics() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Params").await;
    let source = r#"
version 1;
workflow required_param {
    slug = "required-param";
    name = "Required param";
    description = "Exercise required and defaulted launch params.";
    request = none;
    params {
        topic: string(min_len = 1, title = "Topic");
        note?: string(default = "default note", title = "Note");
    }
    run(ctx) {
        return {topic: ctx.params.topic, note: ctx.params.note};
    }
}
"#;
    let saved = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs", project.id),
            json!({"source": source}),
        ))
        .await
        .expect("Workflow save should complete");
    assert_eq!(saved.status(), StatusCode::CREATED);

    let (status, value) = create_session(
        &app,
        project.id,
        json!({
            "program_slug": "required-param",
            "params": {},
            "model": "demo-model",
            "access": "model_only"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");

    let (status, value) = create_session(
        &app,
        project.id,
        json!({
            "program_slug": "required-param",
            "params": {"topic": "language boundary"},
            "model": "demo-model",
            "access": "model_only"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    let session: Session = serde_json::from_value(value).expect("Session should deserialize");
    assert_eq!(session.params["note"], "default note");
    let view = wait_for_session_status(&app, project.id, session.id, &["completed"]).await;
    assert_eq!(
        view["session"]["output"],
        json!({"topic":"language boundary","note":"default note"})
    );
}

#[tokio::test]
async fn interactive_session_has_one_agent_rollout_and_exact_human_provenance() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Interactive").await;
    let session = start_interactive_session(&app, project.id, "Conversation", "workspace").await;
    let waiting =
        wait_for_session_status(&app, project.id, session.id, &["waiting_for_input"]).await;
    assert_eq!(waiting["agents"].as_array().map(Vec::len), Some(1));
    let agent_id = waiting["agents"][0]["id"].clone();

    let first = send_interactive_message(&app, project.id, session.id, "First message").await;
    let second = send_interactive_message(&app, project.id, session.id, "Follow-up").await;
    assert_eq!(second["agents"].as_array().map(Vec::len), Some(1));
    assert_eq!(second["rollouts"].as_array().map(Vec::len), Some(1));
    assert_eq!(second["turns"].as_array().map(Vec::len), Some(2));
    assert!(
        second["turns"]
            .as_array()
            .expect("Turns should be an array")
            .iter()
            .all(|turn| turn["agent_id"] == agent_id)
    );
    assert_eq!(
        second["turns"][0]["tool_set"]["definitions"]
            .as_array()
            .expect("interactive ToolSet should be an array")
            .iter()
            .map(|definition| definition["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![
            "apply_patch",
            "exec_command",
            "interrupt_agent",
            "list_agents",
            "send_message",
            "spawn_agent",
            "wait_agent",
            "write_stdin",
        ]
    );
    let first_turn = &first["turns"][0];
    let action = first["actions"]
        .as_array()
        .expect("Actions should be an array")
        .iter()
        .find(|action| action["agent_id"] == first_turn["agent_id"])
        .expect("Action should belong to the Turn Agent");
    let request_id = action["source"]["request_id"]
        .as_str()
        .expect("interactive Action should preserve HumanRequest provenance");
    assert!(
        first["human_requests"]
            .as_array()
            .expect("HumanRequests should be an array")
            .iter()
            .any(|request| {
                request["id"] == request_id
                    && request["answer"] == "First message"
                    && request["agent_id"] == agent_id
            })
    );

    let events = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!(
                "/api/projects/{}/sessions/{}/events",
                project.id, session.id
            ),
        ))
        .await
        .expect("events should load");
    let status = events.status();
    let event_value = response_json(events).await;
    assert_eq!(status, StatusCode::OK, "{event_value}");
    let events: Vec<SessionEvent> =
        serde_json::from_value(event_value).expect("Session events should deserialize");
    assert!(events.iter().all(|event| event.session_id == session.id));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[1].sequence == pair[0].sequence + 1)
    );
}

#[tokio::test]
async fn archiving_a_session_cancels_its_runtime_and_hides_it_from_the_index() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Archive").await;
    let session = start_interactive_session(&app, project.id, "Close me", "workspace").await;
    wait_for_session_status(&app, project.id, session.id, &["waiting_for_input"]).await;
    let response = app
        .clone()
        .oneshot(empty_request(
            "DELETE",
            &format!("/api/projects/{}/sessions/{}", project.id, session.id),
        ))
        .await
        .expect("archive should complete");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let view = get_session_view(&app, project.id, session.id).await;
    assert_eq!(view["session"]["status"], "cancelled");
    assert!(view["session"]["archived_at"].is_string());
    let sessions = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}/sessions", project.id),
        ))
        .await
        .expect("Session index should load");
    assert!(
        response_json(sessions)
            .await
            .as_array()
            .expect("Session index should be an array")
            .is_empty()
    );
}

#[tokio::test]
async fn one_workflow_session_owns_all_of_its_agents_actions_and_turns() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Multi Agent").await;
    let (status, value) = create_session(
        &app,
        project.id,
        json!({
            "program_slug": "parallel-universe",
            "title": "Research routes",
            "request": "Compare the evidence.",
            "instructions": "",
            "params": {"perspectives": ["primary", "counterevidence"]},
            "model": "demo-model",
            "access": "workspace"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session: Session = serde_json::from_value(value).expect("Session should deserialize");
    let view = wait_for_session_status(&app, project.id, session.id, &["completed"]).await;
    assert_eq!(view["agents"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["actions"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["turns"].as_array().map(Vec::len), Some(3));
    assert!(
        view["agents"]
            .as_array()
            .expect("Agents should be an array")
            .iter()
            .all(|agent| agent["session_id"] == session.id.to_string())
    );
    let agent_ids = view["agents"]
        .as_array()
        .expect("Agents should be an array")
        .iter()
        .map(|agent| agent["id"].clone())
        .collect::<Vec<_>>();
    assert!(
        view["turns"]
            .as_array()
            .expect("Turns should be an array")
            .iter()
            .all(|turn| agent_ids.contains(&turn["agent_id"]))
    );
    assert!(
        view["actions"]
            .as_array()
            .expect("Actions should be an array")
            .iter()
            .all(|action| action["session_id"] == session.id.to_string())
    );
}

#[tokio::test]
async fn per_agent_model_profiles_are_bound_inside_one_session() {
    let directory = tempdir().expect("temporary directory should be created");
    let model = ScriptedModelClient::new([
        scripted_text("route one"),
        scripted_text("route two"),
        scripted_text("synthesis"),
    ]);
    let app = test_app_with_model_profiles(&directory, model).await;
    let project = create_project(&app, directory.path(), "Profiles").await;
    let (status, value) = create_session(
        &app,
        project.id,
        json!({
            "program_slug": "parallel-universe",
            "request": "Compare models.",
            "instructions": "",
            "params": {
                "perspectives": ["one", "two"],
                "research_model": "research-model",
                "synthesis_model": "review-model"
            },
            "model": "research-model",
            "access": "workspace"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session: Session = serde_json::from_value(value).expect("Session should deserialize");
    let view = wait_for_session_status(&app, project.id, session.id, &["completed"]).await;
    for agent in view["agents"].as_array().expect("Agents should exist") {
        match agent["class_name"].as_str().unwrap_or_default() {
            "Researcher" => assert_eq!(agent["model"], "research-model"),
            "Synthesizer" | "ContextAnalyst" => assert_eq!(agent["model"], "review-model"),
            class => panic!("unexpected Agent {class}"),
        }
    }
    assert!(
        view["turns"]
            .as_array()
            .expect("Turns should be an array")
            .iter()
            .all(|turn| {
                let profile = turn["model_route"]["profile"].as_str().unwrap_or_default();
                profile == "research-model" || profile == "review-model"
            })
    );
}

#[tokio::test]
async fn child_session_keeps_provenance_and_cannot_exceed_source_access() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Child Session").await;
    let origin = start_interactive_session(&app, project.id, "Origin", "workspace").await;
    let (status, _) = create_session(
        &app,
        project.id,
        json!({
            "program_slug": "parallel-universe",
            "request": "Escalate.",
            "instructions": "",
            "params": {},
            "source_session_id": origin.id,
            "model": "demo-model",
            "access": "full_access"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, value) = create_session(
        &app,
        project.id,
        json!({
            "program_slug": "parallel-universe",
            "request": "Continue from the source Session.",
            "instructions": "",
            "params": {"perspectives": ["one", "two"]},
            "source_session_id": origin.id,
            "model": "demo-model",
            "access": "workspace",
            "agent_access_overrides": {"Researcher": "read_only"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session: Session = serde_json::from_value(value).expect("Session should deserialize");
    assert_eq!(session.trigger.source_session_id, Some(origin.id));
    let view = wait_for_session_status(&app, project.id, session.id, &["completed"]).await;
    assert!(
        view["agents"]
            .as_array()
            .expect("Agents should be an array")
            .iter()
            .all(|agent| {
                match agent["class_name"].as_str().unwrap_or_default() {
                    "Researcher" => agent["access"] == "read_only",
                    "Synthesizer" | "ContextAnalyst" => agent["access"] == "model_only",
                    _ => false,
                }
            })
    );
}

#[tokio::test]
async fn project_summary_publishes_and_refreshes_the_managed_home() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Summary").await;
    let source = start_interactive_session(&app, project.id, "Evidence", "workspace").await;
    send_interactive_message(&app, project.id, source.id, "Record the current result.").await;

    let run_summary = || {
        create_session(
            &app,
            project.id,
            json!({
                "program_slug": "project-summary",
                "instructions": "Keep verified results and next actions visible.",
                "params": {"interval_minutes": 0},
                "model": "demo-model",
                "access": "model_only"
            }),
        )
    };
    let (status, value) = run_summary().await;
    assert_eq!(status, StatusCode::CREATED);
    let first: Session = serde_json::from_value(value).expect("Session should deserialize");
    let first_view = wait_for_session_status(&app, project.id, first.id, &["completed"]).await;
    assert_eq!(first_view["session"]["output"]["updated"], true);
    assert!(first_view["session"]["output"]["artifact_id"].is_string());
    assert_eq!(first_view["agents"].as_array().map(Vec::len), Some(1));
    assert!(
        first_view["turns"]
            .as_array()
            .expect("Turns should be an array")
            .iter()
            .all(|turn| {
                turn["tool_set"]["definitions"]
                    .as_array()
                    .expect("ToolSet definitions should be an array")
                    .iter()
                    .next()
                    .is_none()
            })
    );
    let overview = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project overview should load");
    let overview = response_json(overview).await;
    assert_eq!(overview["summary_session"]["id"], first.id.to_string());
    let revision = overview["project_home"]["revision"].clone();
    let artifact_id = overview["project_home_artifact"]["id"]
        .as_str()
        .expect("home Artifact should exist");
    let content = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!(
                "/api/projects/{}/artifacts/{artifact_id}/content",
                project.id
            ),
        ))
        .await
        .expect("home content should load");
    let html = String::from_utf8(
        to_bytes(content.into_body(), 2 * 1024 * 1024)
            .await
            .expect("Artifact body should be readable")
            .to_vec(),
    )
    .expect("Artifact body should be UTF-8");
    assert!(html.contains("Project overview"));

    let tool_names = first_view["steps"]
        .as_array()
        .expect("summary Agent Steps should exist")
        .iter()
        .filter(|step| step["kind"] == "tool")
        .filter_map(|step| step["name"].as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.is_empty());

    let (status, value) = run_summary().await;
    assert_eq!(status, StatusCode::CREATED);
    let second: Session = serde_json::from_value(value).expect("Session should deserialize");
    let second_view = wait_for_session_status(&app, project.id, second.id, &["completed"]).await;
    assert_eq!(second_view["session"]["output"]["updated"], false);
    assert!(second_view["session"]["output"]["artifact_id"].is_string());
    let overview = response_json(
        app.clone()
            .oneshot(empty_request(
                "GET",
                &format!("/api/projects/{}", project.id),
            ))
            .await
            .expect("Project overview should load"),
    )
    .await;
    assert_eq!(overview["summary_session"]["id"], second.id.to_string());
    assert_eq!(overview["project_home"]["revision"], revision);
}

#[tokio::test]
async fn generated_workflow_can_be_validated_saved_and_run_as_a_session() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let project = create_project(&app, directory.path(), "Generated").await;
    let generated = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs/generate", project.id),
            json!({
                "description": "Compare two evidence routes and review disagreements.",
                "name": "Claim challenge",
                "slug": "claim-challenge",
                "model": "demo-model"
            }),
        ))
        .await
        .expect("generation should complete");
    assert_eq!(generated.status(), StatusCode::OK);
    let generated = response_json(generated).await;
    assert_eq!(generated["validation"]["valid"], true);
    assert_eq!(generated["validation"]["manifest"]["language_version"], 1);
    let source = generated["source"].as_str().expect("source should exist");
    assert!(source.trim_start().starts_with("version 1;"));
    let saved = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflow-programs", project.id),
            json!({"source": source}),
        ))
        .await
        .expect("save should complete");
    assert_eq!(saved.status(), StatusCode::CREATED);
    let saved = response_json(saved).await;
    assert!(
        saved["definition_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("workflow.pm"))
    );

    let (status, value) = create_session(
        &app,
        project.id,
        json!({
            "program_slug": "claim-challenge",
            "request": "Challenge this claim.",
            "instructions": "",
            "params": {},
            "model": "demo-model",
            "access": "workspace"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session: Session = serde_json::from_value(value).expect("Session should deserialize");
    let view = wait_for_session_status(&app, project.id, session.id, &["completed"]).await;
    assert_eq!(view["agents"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["turns"].as_array().map(Vec::len), Some(3));
}
