use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::http::Request;
use axum::http::StatusCode;
use papermachine_protocol::Research;
use papermachine_protocol::Session;
use papermachine_protocol::WorkflowRunEvent;
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

async fn create_research_and_session(app: &Router, name: &str) -> (Research, Session) {
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/researches",
            json!({"name": name, "description": "API test"}),
        ))
        .await
        .expect("research request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let research: Research =
        serde_json::from_value(response_json(response).await).expect("research should deserialize");

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/researches/{}/sessions", research.id),
            json!({"title": "Origin Session"}),
        ))
        .await
        .expect("Session request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let session: Session =
        serde_json::from_value(response_json(response).await).expect("Session should deserialize");
    (research, session)
}

#[tokio::test]
async fn api_creates_and_updates_session_access_profiles() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let (research, origin) = create_research_and_session(&app, "Access API").await;
    assert_eq!(origin.access.as_str(), "research");

    let created = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/researches/{}/sessions", research.id),
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

async fn get_run_view(app: &Router, run_id: &str) -> Value {
    let response = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/workflow-runs/{run_id}"),
        ))
        .await
        .expect("run view request should complete");
    assert_eq!(response.status(), StatusCode::OK);
    response_json(response).await
}

async fn wait_for_run_status(app: &Router, run_id: &str, expected: &str) -> Value {
    for _ in 0..400 {
        let view = get_run_view(app, run_id).await;
        if view["workflow_run"]["status"] == expected {
            return view;
        }
        if matches!(
            view["workflow_run"]["status"].as_str(),
            Some("failed" | "cancelled")
        ) {
            panic!(
                "WorkflowRun terminated unexpectedly: {}",
                view["workflow_run"]
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("WorkflowRun did not reach {expected}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn api_runs_python_workflow_as_research_owned_sessions() {
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
    assert_eq!(health["workflow_runtime"], "python_effect_dsl_v1");

    let (research, origin) = create_research_and_session(&app, "Parallel research").await;
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
            &format!("/api/sessions/{}/workflow-runs", origin.id),
            json!({
                "workflow_slug": "parallel-discovery",
                "workflow_version": "0.3.0",
                "objective": "Compare two implementation approaches.",
                "input": {"perspectives": ["primary evidence", "failure modes"]}
            }),
        ))
        .await
        .expect("run request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    let run_id = run["id"].as_str().expect("run id should be present");

    let view = wait_for_run_status(&app, run_id, "completed").await;
    assert_eq!(view["participants"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["sessions"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["actions"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["attempts"].as_array().map(Vec::len), Some(3));
    assert_eq!(view["teams"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["relations"].as_array().map(Vec::len), Some(2));
    assert_eq!(view["task_scopes"].as_array().map(Vec::len), Some(1));
    assert!(
        view["workflow_run"]["output"]["summary"]
            .as_str()
            .is_some_and(|value| value.contains("Demo result"))
    );

    let response = app
        .clone()
        .oneshot(empty_request(
            "GET",
            &format!("/api/workflow-runs/{run_id}/events"),
        ))
        .await
        .expect("events request should complete");
    let events: Vec<WorkflowRunEvent> =
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
            &format!("/api/researches/{}", research.id),
        ))
        .await
        .expect("Research overview should load");
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
                "/api/workflow-runs/{run_id}/events/stream?after={}",
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
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/workflows/generate",
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
            "/api/workflows/validate",
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
            "/api/workflows/validate",
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
            "/api/workflows",
            json!({"source": source}),
        ))
        .await
        .expect("publish request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(
        directory
            .path()
            .join("workflows/user/claim-challenge/0.1.0/workflow.py")
            .is_file()
    );

    let response = app
        .clone()
        .oneshot(empty_request("GET", "/api/workflows/claim-challenge/0.1.0"))
        .await
        .expect("published workflow should load");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["source"], source);

    let changed = source.replace(
        "Run two independent evidence routes",
        "Run independent evidence routes",
    );
    let conflict = app
        .oneshot(json_request(
            "POST",
            "/api/workflows",
            json!({"source": changed}),
        ))
        .await
        .expect("immutable version request should complete");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn workflow_can_pause_request_human_input_and_resume() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
    let source = r#"from papermachine import ask_human, workflow


@workflow(
    slug="human-decision",
    name="Human decision",
    version="0.1.0",
    description="Wait for a human decision before completing.",
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object", "properties": {"decision": {"type": "string"}}},
)
async def main(ctx):
    answer = await ask_human(
        "Which direction should the research take?",
        response_schema={"type": "string"},
    )
    return {"decision": answer}
"#;
    let publish = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/workflows",
            json!({"source": source}),
        ))
        .await
        .expect("workflow should publish");
    assert_eq!(publish.status(), StatusCode::CREATED);
    let (_research, origin) = create_research_and_session(&app, "Human-guided research").await;
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/sessions/{}/workflow-runs", origin.id),
            json!({
                "workflow_slug": "human-decision",
                "workflow_version": "0.1.0",
                "objective": "Choose a research direction.",
                "input": {}
            }),
        ))
        .await
        .expect("run should start");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    let run_id = run["id"].as_str().expect("run id should exist");

    let mut request_id = None;
    for _ in 0..200 {
        let view = get_run_view(&app, run_id).await;
        request_id = view["human_requests"].as_array().and_then(|items| {
            items
                .iter()
                .find(|item| item["status"] == "open")
                .and_then(|item| item["id"].as_str())
                .map(str::to_string)
        });
        if request_id.is_some() {
            assert_eq!(view["workflow_run"]["attention_required"], true);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let request_id = request_id.expect("human request should open");

    let pause = app
        .clone()
        .oneshot(empty_request(
            "POST",
            &format!("/api/workflow-runs/{run_id}/pause"),
        ))
        .await
        .expect("pause should complete");
    assert_eq!(pause.status(), StatusCode::ACCEPTED);
    wait_for_run_status(&app, run_id, "paused").await;
    let resume = app
        .clone()
        .oneshot(empty_request(
            "POST",
            &format!("/api/workflow-runs/{run_id}/resume"),
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

    let completed = wait_for_run_status(&app, run_id, "completed").await;
    assert_eq!(
        completed["workflow_run"]["output"]["decision"],
        "Prioritize primary evidence."
    );
    assert_eq!(completed["workflow_run"]["attention_required"], false);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn workflow_access_escalation_requires_a_human_grant() {
    let directory = tempdir().expect("temporary directory should be created");
    let app = test_app(&directory).await;
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
    version="0.1.0",
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
            "/api/workflows",
            json!({"source": source}),
        ))
        .await
        .expect("workflow should publish");
    assert_eq!(publish.status(), StatusCode::CREATED);
    let (_research, origin) = create_research_and_session(&app, "Escalation").await;
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/sessions/{}/workflow-runs", origin.id),
            json!({
                "workflow_slug": "access-grant",
                "workflow_version": "0.1.0",
                "objective": "Inspect the configured environment.",
                "input": {}
            }),
        ))
        .await
        .expect("run should start");
    assert_eq!(response.status(), StatusCode::CREATED);
    let run = response_json(response).await;
    let run_id = run["id"].as_str().expect("run id should exist");

    let mut open_request = None;
    let mut participant_session_id = None;
    for _ in 0..200 {
        let view = get_run_view(&app, run_id).await;
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
    let completed = wait_for_run_status(&app, run_id, "completed").await;
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
