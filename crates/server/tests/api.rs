use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::http::Request;
use axum::http::StatusCode;
use papermachine_protocol::Project;
use papermachine_protocol::Session;
use papermachine_protocol::WorkflowEvent;
use papermachine_server::ServerConfig;
use papermachine_server::initialize;
use papermachine_server::router;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
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

fn prepare_root(root: &Path) {
    let builtin = root.join("workflows/builtin/parallel-discovery");
    std::fs::create_dir_all(&builtin).expect("builtin directory should be created");
    std::fs::create_dir_all(root.join("workflows/user"))
        .expect("user workflow directory should be created");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../workflows/builtin/parallel-discovery/workflow.py");
    std::fs::copy(source, builtin.join("workflow.py")).expect("builtin workflow should be copied");
}

async fn test_app(directory: &TempDir) -> Router {
    prepare_root(directory.path());
    let state = initialize(&ServerConfig {
        root: directory.path().to_path_buf(),
        default_model: "demo-model".to_string(),
        demo: true,
        configured_models: None,
        openai_config: None,
        model_context_window: 128_000,
        max_concurrent_runs: 2,
        max_parallel_actions: 4,
    })
    .await
    .expect("server should initialize");
    router(state, directory.path().join("dist"))
}

async fn create_project_and_session(app: &Router, base: &Path, name: &str) -> (Project, Session) {
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
    let root_path = base.join("projects").join(directory_name);
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/projects",
            json!({
                "name": name,
                "description": "API test",
                "root_path": root_path.to_string_lossy()
            }),
        ))
        .await
        .expect("project request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let project: Project =
        serde_json::from_value(response_json(response).await).expect("project should deserialize");

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/sessions", project.id),
            json!({"title": "Origin Session"}),
        ))
        .await
        .expect("Session request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let session: Session =
        serde_json::from_value(response_json(response).await).expect("Session should deserialize");
    (project, session)
}

#[tokio::test]
async fn api_creates_and_updates_session_access_profiles() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (project, origin) = create_project_and_session(&app, directory.path(), "Access API").await;
    assert_eq!(origin.access.as_str(), "research");

    let created = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/sessions", project.id),
            json!({"title": "No tools", "access": "model_only"}),
        ))
        .await
        .expect("profiled Session request should complete");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created: Session = serde_json::from_value(response_json(created).await)
        .expect("profiled Session should deserialize");
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Workflow did not reach {expected}");
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
    let turn = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/sessions/{}/turns", origin.id),
            json!({"input": "Frame the comparison."}),
        ))
        .await
        .expect("Turn request should complete");
    assert_eq!(turn.status(), StatusCode::CREATED);

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/workflows", project.id),
            json!({
                "program_slug": "parallel-discovery",
                "objective": "Compare two implementation approaches.",
                "input": {"perspectives": ["primary evidence", "failure modes"]},
                "started_from_session_id": origin.id
            }),
        ))
        .await
        .expect("run request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    let workflow_id = run["id"].as_str().expect("run id should be present");

    let view = wait_for_workflow_status(&app, workflow_id, "completed").await;
    assert_eq!(view["participants"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["sessions"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["actions"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["attempts"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["teams"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["relations"].as_array().map(Vec::len), Some(2));
    assert_eq!(view["task_scopes"].as_array().map(Vec::len), Some(1));
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

    let overview = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project overview should load");
    let overview = response_json(overview).await;
    assert_eq!(overview["sessions"].as_array().map(Vec::len), Some(4));
    assert_eq!(
        overview["workflow_participants"].as_array().map(Vec::len),
        Some(3)
    );

    let participant_session_id = view["participants"][0]["session_id"]
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
    assert_eq!(participant_view["session"]["origin"], "workflow_agent");
    assert_eq!(participant_view["turns"].as_array().map(Vec::len), Some(1));
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
            "/api/workflow-programs/generate",
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

    let invalid_access = source.replace("access = \"research\"", "access = \"root\"");
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/workflow-programs/validate",
            json!({"source": invalid_access}),
        ))
        .await
        .expect("invalid Agent access should be validated");
    assert_eq!(response.status(), StatusCode::OK);
    let invalid_access = response_json(response).await;
    assert_eq!(invalid_access["valid"], false);
    assert!(
        invalid_access["diagnostics"]
            .as_array()
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("Agent access must be one of"))
                })
            })
    );

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/workflow-programs/validate",
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
        Path::new(&project.root_path)
            .join(".papermachine/workflows/claim-challenge/workflow.py")
            .is_file()
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
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object", "properties": {"decision": {"type": "string"}}},
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
                "objective": "Choose a project direction.",
                "input": {},
                "started_from_session_id": origin.id
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
    let (project, origin) = create_project_and_session(&app, directory.path(), "Escalation").await;
    let source = r#"from papermachine import Agent, action, workflow


class HostInspector(Agent):
    access = "full_access"
    role = "host inspection"

    @action(max_steps=1)
    async def inspect(self, question: str) -> str:
        """Answer the question after access has been granted."""


@workflow(
    slug="access-grant",
    name="Access grant",
    description="Require a human grant before creating a full-access Agent Session.",
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object", "properties": {"answer": {"type": "string"}}},
)
async def main(ctx):
    inspector = HostInspector(name="Host inspector")
    answer = await inspector.inspect(ctx.objective)
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
                "objective": "Inspect the configured environment.",
                "input": {},
                "started_from_session_id": origin.id
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
    assert_eq!(participant["turns"][0]["access"], "full_access");
}
