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
    let builtins = ["interactive-agent", "parallel-discovery", "project-summary"];
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
    std::fs::create_dir_all(root.join("workflows/user"))
        .expect("user workflow directory should be created");
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
                "objective": format!("Persistent interactive Session: {title}"),
                "input": {
                    "session_title": title,
                    "agent_system_prompt": "",
                    "agent_access": access,
                },
                "access": access,
            }),
        ))
        .await
        .expect("interactive Workflow request should complete");
    assert_eq!(response.status(), StatusCode::CREATED);
    let workflow = response_json(response).await;
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
    assert_eq!(
        project_prompt["relative_path"],
        ".papermachine/prompts/system.md"
    );

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

    let turn = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/sessions/{}/turns", session.id),
            json!({"input": "Compare the sources."}),
        ))
        .await
        .expect("Turn request should complete");
    assert_eq!(turn.status(), StatusCode::CREATED);
    let turn = response_json(turn).await;
    assert_eq!(turn["origin"], "user");
    let kinds = turn["prompt"]["layers"]
        .as_array()
        .expect("prompt layers should be an array")
        .iter()
        .map(|layer| layer["kind"].as_str().expect("kind should be a string"))
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["runtime", "project", "agent"]);

    let overview = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project overview should complete");
    assert_eq!(overview.status(), StatusCode::OK);
    let overview = response_json(overview).await;
    assert_eq!(
        overview["system_prompt"]["content"],
        "Use only primary evidence."
    );
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
    let (project, session) =
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

    let removed_endpoint = app
        .oneshot(json_request(
            "POST",
            &format!("/api/projects/{}/sessions", project.id),
            json!({"title": "Legacy standalone Session"}),
        ))
        .await
        .expect("removed endpoint should return a response");
    assert_eq!(removed_endpoint.status(), StatusCode::METHOD_NOT_ALLOWED);
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
                "system_prompt": "Prefer directly comparable implementation evidence.",
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
    assert_eq!(
        view["workflow"]["system_prompt"],
        "Prefer directly comparable implementation evidence."
    );
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
        Some(4)
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
    assert_eq!(participant_view["turns"][0]["origin"], "workflow");
    let prompt_kinds = participant_view["turns"][0]["prompt"]["layers"]
        .as_array()
        .expect("Workflow Turn prompt layers should exist")
        .iter()
        .filter_map(|layer| layer["kind"].as_str())
        .collect::<Vec<_>>();
    assert!(prompt_kinds.contains(&"runtime"));
    assert!(prompt_kinds.contains(&"workflow"));
    assert!(prompt_kinds.contains(&"agent"));
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
async fn project_summary_publishes_a_sandboxed_html_progress_page() {
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
                "objective": "Refresh the Project progress page now.",
                "system_prompt": "Lead with verified progress and unresolved blockers.",
                "input": {
                    "interval_minutes": 0,
                    "max_sessions": 50,
                    "turns_per_session": 12,
                    "max_artifacts": 50
                },
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
    assert_eq!(view["artifacts"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["artifacts"][0]["metadata"]["role"], "project_summary");
    assert_eq!(
        view["artifacts"][0]["media_type"],
        "text/html; charset=utf-8"
    );

    let artifact_id = view["artifacts"][0]["id"]
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
    assert!(html.to_ascii_lowercase().contains("<!doctype html>"));

    let overview = app
        .oneshot(empty_request(
            "GET",
            &format!("/api/projects/{}", project.id),
        ))
        .await
        .expect("Project overview should load");
    let overview = response_json(overview).await;
    assert!(overview["artifacts"].as_array().is_some_and(|artifacts| {
        artifacts
            .iter()
            .any(|artifact| artifact["id"] == artifact_id)
    }));
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
