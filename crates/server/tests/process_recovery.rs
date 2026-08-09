#![cfg(any(target_os = "macos", target_os = "linux"))]

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode as AxumStatusCode;
use axum::response::Response;
use axum::routing::post;
use papermachine_store::process_fault::ROLLOUT_APPENDED_BEFORE_PROJECTION;
use papermachine_store::process_fault::TOOL_EXECUTION_STARTED;
use papermachine_store::process_fault::TOOL_PREPARED_BEFORE_EXECUTION;
use papermachine_store::process_fault::TURN_TERMINAL_CHECKPOINTED_BEFORE_COMMIT;
use reqwest::Client;
use reqwest::Method;
use reqwest::StatusCode;
use serde_json::Value;
use serde_json::json;
use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::net::TcpListener as StdTcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const SERVER_BINARY: &str = env!("CARGO_BIN_EXE_papermachine-server");
const MODEL_PROFILE: &str = "process-test-model";
const PROVIDER_KEY_ENV: &str = "PAPERMACHINE_PROCESS_TEST_KEY";
const WAIT_LIMIT: Duration = Duration::from_secs(20);

const WORKFLOW_SOURCE: &str = r#"from papermachine import Agent, action, workflow


class ProcessWorker(Agent):
    access = "workspace"
    role = "process recovery test worker"

    @action(tools=["exec_command"])
    async def work(self, task: str):
        """Execute the supplied deterministic process recovery test task."""


@workflow(
    slug="process-recovery",
    name="Process recovery",
    description="Exercise one deterministic Agent Action across a process restart.",
    params_schema={"type": "object", "properties": {}, "additionalProperties": False},
    output_schema={
        "type": "object",
        "properties": {"result": {"type": "string"}},
        "required": ["result"],
        "additionalProperties": False,
    },
)
async def main(ctx):
    worker = ProcessWorker(name="Process worker")
    result = await worker.work(ctx.request)
    return {"result": str(result)}
"#;

#[derive(Clone)]
struct ResponseGate {
    open: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ResponseGate {
    fn new() -> Self {
        Self {
            open: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        while !self.open.load(Ordering::SeqCst) {
            let notified = self.notify.notified();
            if self.open.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
    }
}

enum MockResponse {
    Text(String),
    BlockedText { gate: ResponseGate, text: String },
    ToolCall { call_id: String, command: String },
}

struct MockProviderState {
    responses: Mutex<VecDeque<MockResponse>>,
    requests: Mutex<Vec<Value>>,
    queued: Notify,
}

struct MockProvider {
    state: Arc<MockProviderState>,
    base_url: String,
    server: JoinHandle<()>,
}

impl MockProvider {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock provider should bind");
        let address = listener
            .local_addr()
            .expect("mock provider should have an address");
        let state = Arc::new(MockProviderState {
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            queued: Notify::new(),
        });
        let app = Router::new()
            .route("/v1/responses", post(mock_responses))
            .with_state(Arc::clone(&state));
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock provider should serve");
        });
        Self {
            state,
            base_url: format!("http://{address}/v1"),
            server,
        }
    }

    async fn enqueue(&self, response: MockResponse) {
        self.state.responses.lock().await.push_back(response);
        self.state.queued.notify_one();
    }

    async fn enqueue_text(&self, text: &str) {
        self.enqueue(MockResponse::Text(text.to_string())).await;
    }

    async fn enqueue_tool_call(&self, call_id: &str, command: &str) {
        self.enqueue(MockResponse::ToolCall {
            call_id: call_id.to_string(),
            command: command.to_string(),
        })
        .await;
    }

    async fn enqueue_blocked_text(&self, text: &str) -> ResponseGate {
        let gate = ResponseGate::new();
        self.enqueue(MockResponse::BlockedText {
            gate: gate.clone(),
            text: text.to_string(),
        })
        .await;
        gate
    }

    async fn call_count(&self) -> usize {
        self.state.requests.lock().await.len()
    }

    async fn wait_for_calls(&self, expected: usize) {
        wait_until("mock provider call count", || async {
            (self.call_count().await >= expected).then_some(())
        })
        .await;
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn mock_responses(
    State(state): State<Arc<MockProviderState>>,
    Json(request): Json<Value>,
) -> Response {
    state.requests.lock().await.push(request);
    let response = loop {
        let notified = state.queued.notified();
        if let Some(response) = state.responses.lock().await.pop_front() {
            break response;
        }
        notified.await;
    };
    match response {
        MockResponse::Text(text) => sse_text(&text),
        MockResponse::BlockedText { gate, text } => {
            gate.wait().await;
            sse_text(&text)
        }
        MockResponse::ToolCall { call_id, command } => sse_tool_call(&call_id, &command),
    }
}

fn sse_text(text: &str) -> Response {
    let delta = json!({"type": "response.output_text.delta", "delta": text});
    sse_response(vec![delta, completed_event()])
}

fn sse_tool_call(call_id: &str, command: &str) -> Response {
    let arguments = serde_json::to_string(&json!({
        "command": command,
        "timeout_seconds": 30,
    }))
    .expect("tool arguments should serialize");
    let call = json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": "exec_command",
            "arguments": arguments,
        }
    });
    sse_response(vec![call, completed_event()])
}

fn completed_event() -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "usage": {
                "input_tokens": 20,
                "output_tokens": 5,
                "input_tokens_details": {"cached_tokens": 0}
            }
        }
    })
}

fn sse_response(events: Vec<Value>) -> Response {
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    Response::builder()
        .status(AxumStatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .expect("mock SSE response should build")
}

struct ServerProcess {
    child: Child,
    log_path: PathBuf,
}

impl ServerProcess {
    async fn wait_ready(&mut self, client: &Client, base_url: &str) {
        let started = tokio::time::Instant::now();
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .expect("server process status should be readable")
            {
                let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
                panic!("server exited before health check ({status}):\n{log}");
            }
            if client
                .get(format!("{base_url}/api/health"))
                .send()
                .await
                .is_ok_and(|response| response.status() == StatusCode::OK)
            {
                return;
            }
            assert!(
                started.elapsed() < WAIT_LIMIT,
                "server did not become ready; log: {}",
                std::fs::read_to_string(&self.log_path).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn signal_and_wait(&mut self, signal: &str) {
        let process_id = self.child.id().expect("server should have a process id");
        let status = Command::new("/bin/kill")
            .args([signal, &process_id.to_string()])
            .status()
            .await
            .expect("kill command should run");
        assert!(status.success(), "kill command should succeed");
        tokio::time::timeout(WAIT_LIMIT, self.child.wait())
            .await
            .expect("server should exit after signal")
            .expect("server wait should succeed");
    }

    async fn sigkill(&mut self) {
        self.signal_and_wait("-KILL").await;
    }

    async fn stop(&mut self) {
        self.signal_and_wait("-INT").await;
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct Scenario {
    _directory: TempDir,
    resource_root: PathBuf,
    data_dir: PathBuf,
    config_path: PathBuf,
    workspace: PathBuf,
    server_port: u16,
    starts: usize,
    client: Client,
    mock: MockProvider,
}

impl Scenario {
    async fn new() -> Self {
        let directory = tempdir().expect("scenario directory should be created");
        let resource_root = directory.path().join("resources");
        let data_dir = directory.path().join("data");
        let config_path = directory.path().join("config.toml");
        let workspace = directory.path().join("workspace");
        prepare_resource_root(&resource_root);
        std::fs::create_dir_all(&workspace).expect("Workspace should be created");
        let mock = MockProvider::start().await;
        std::fs::write(
            &config_path,
            format!(
                r#"default_model = "{MODEL_PROFILE}"

[providers.process-test]
kind = "open_ai_responses"
base_url = "{}"
api_key_env = "{PROVIDER_KEY_ENV}"
max_request_retries = 0
request_timeout_seconds = 30
stream_idle_timeout_seconds = 30
responses_websockets = false
hosted_web_search = false
prompt_cache_mode = "implicit"

[models.{MODEL_PROFILE}]
provider = "process-test"
model = "process-test-upstream"
context_window = 128000
"#,
                mock.base_url
            ),
        )
        .expect("provider config should be written");
        Self {
            _directory: directory,
            resource_root,
            data_dir,
            config_path,
            workspace,
            server_port: reserve_port(),
            starts: 0,
            client: Client::new(),
            mock,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.server_port)
    }

    async fn start(&mut self, boundary: Option<&str>) -> (ServerProcess, Option<PathBuf>) {
        self.starts += 1;
        let log_path = self
            .data_dir
            .parent()
            .expect("data directory should have a parent")
            .join(format!("server-{}.log", self.starts));
        let log = File::create(&log_path).expect("server log should be created");
        let mut command = Command::new(SERVER_BINARY);
        command
            .arg("--resource-root")
            .arg(&self.resource_root)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--config")
            .arg(&self.config_path)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(self.server_port.to_string())
            .arg("--max-concurrent-runs")
            .arg("2")
            .arg("--max-parallel-actions")
            .arg("2")
            .env(PROVIDER_KEY_ENV, "process-test-key")
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                log.try_clone().expect("server log should clone"),
            ))
            .stderr(Stdio::from(log))
            .kill_on_drop(true);
        let marker = boundary.map(|boundary| {
            let marker = self
                .data_dir
                .parent()
                .expect("data directory should have a parent")
                .join(format!("fault-{}-{}.marker", self.starts, boundary));
            command
                .arg("--process-fault-boundary")
                .arg(boundary)
                .arg("--process-fault-marker")
                .arg(&marker);
            marker
        });
        let child = command.spawn().expect("server process should start");
        let mut process = ServerProcess { child, log_path };
        process.wait_ready(&self.client, &self.base_url()).await;
        (process, marker)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base_url()));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("API request should complete");
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .expect("API response should be readable");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    async fn create_project(&self, name: &str) -> String {
        let (status, project) = self
            .request(
                Method::POST,
                "/api/projects",
                Some(json!({
                    "name": name,
                    "workspace": {
                        "roots": [self.workspace.to_string_lossy()],
                        "primary_root": 0,
                    }
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "Project creation: {project}");
        project["id"]
            .as_str()
            .expect("Project id should exist")
            .to_string()
    }

    async fn publish_workflow(&self, project_id: &str) {
        let (status, response) = self
            .request(
                Method::POST,
                &format!("/api/projects/{project_id}/workflow-programs"),
                Some(json!({"source": WORKFLOW_SOURCE})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "Workflow publish: {response}");
    }

    async fn launch_workflow(&self, project_id: &str, request: &str) -> String {
        let (status, workflow) = self
            .request(
                Method::POST,
                &format!("/api/projects/{project_id}/workflows"),
                Some(json!({
                    "program_slug": "process-recovery",
                    "request": request,
                    "params": {},
                    "model": MODEL_PROFILE,
                    "access": "workspace",
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "Workflow launch: {workflow}");
        workflow["id"]
            .as_str()
            .expect("Workflow id should exist")
            .to_string()
    }

    async fn workflow_view(&self, workflow_id: &str) -> Value {
        let (status, view) = self
            .request(Method::GET, &format!("/api/workflows/{workflow_id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "Workflow view: {view}");
        view
    }

    async fn wait_workflow_terminal(&self, workflow_id: &str) -> Value {
        wait_until("terminal Workflow", || async {
            let view = self.workflow_view(workflow_id).await;
            match view["workflow"]["status"].as_str() {
                Some("completed") => Some(view),
                Some("failed" | "cancelled") => {
                    panic!("Workflow terminated unsuccessfully: {}", view["workflow"])
                }
                _ => None,
            }
        })
        .await
    }

    async fn session_view(&self, session_id: &str) -> Value {
        let (status, view) = self
            .request(Method::GET, &format!("/api/sessions/{session_id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "Session view: {view}");
        view
    }
}

fn reserve_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("test port should be reserved")
        .local_addr()
        .expect("test port should have an address")
        .port()
}

fn copy_directory(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("destination directory should be created");
    for entry in std::fs::read_dir(source).expect("source directory should be readable") {
        let entry = entry.expect("source entry should be readable");
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory(&source_path, &destination_path);
        } else {
            std::fs::copy(&source_path, &destination_path).expect("resource file should be copied");
        }
    }
}

fn prepare_resource_root(root: &Path) {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    copy_directory(
        &repository.join("workflows/builtin"),
        &root.join("workflows/builtin"),
    );
    copy_directory(&repository.join("python"), &root.join("python"));
    let web_dist = root.join("apps/web/dist");
    std::fs::create_dir_all(&web_dist).expect("web dist should be created");
    std::fs::write(
        web_dist.join("index.html"),
        "<!doctype html><title>test</title>",
    )
    .expect("test web index should be written");
}

async fn wait_for_marker(path: &Path) {
    wait_until("process fault marker", || async {
        path.is_file().then_some(())
    })
    .await;
}

async fn wait_until<T, F, Fut>(description: &str, mut poll: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let started = tokio::time::Instant::now();
    loop {
        if let Some(value) = poll().await {
            return value;
        }
        assert!(
            started.elapsed() < WAIT_LIMIT,
            "timed out waiting for {description}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn assert_rollout_projected(view: &Value) {
    let last = view["rollout"]["last_sequence"]
        .as_u64()
        .expect("rollout sequence should exist");
    let projected = view["rollout"]["projected_sequence"]
        .as_u64()
        .expect("projection sequence should exist");
    assert!(last > 0, "Session should have a durable rollout");
    assert_eq!(last, projected, "rollout and projection must converge");
}

async fn rollout_ahead_of_projection_is_replayed_after_sigkill() {
    let mut scenario = Scenario::new().await;
    scenario.mock.enqueue_text("recovered rollout").await;
    let (mut server, marker) = scenario
        .start(Some(ROLLOUT_APPENDED_BEFORE_PROJECTION))
        .await;
    let project_id = scenario.create_project("Rollout projection crash").await;
    scenario.publish_workflow(&project_id).await;
    let workflow_id = scenario
        .launch_workflow(&project_id, "Recover the durable rollout record.")
        .await;
    wait_for_marker(marker.as_deref().expect("fault marker should exist")).await;
    server.sigkill().await;

    let (mut restarted, _) = scenario.start(None).await;
    let workflow = scenario.wait_workflow_terminal(&workflow_id).await;
    assert_eq!(workflow["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(workflow["actions"].as_array().map(Vec::len), Some(1));
    assert_eq!(workflow["attempts"].as_array().map(Vec::len), Some(1));
    let session_id = workflow["sessions"][0]["id"]
        .as_str()
        .expect("Session id should exist");
    assert_rollout_projected(&scenario.session_view(session_id).await);
    assert_eq!(scenario.mock.call_count().await, 1);
    restarted.stop().await;
}

async fn terminal_checkpoint_commits_without_resampling_after_sigkill() {
    let mut scenario = Scenario::new().await;
    let (mut server, _) = scenario.start(None).await;
    let project_id = scenario.create_project("Terminal checkpoint crash").await;
    scenario.publish_workflow(&project_id).await;
    server.stop().await;

    let (mut faulted, marker) = scenario
        .start(Some(TURN_TERMINAL_CHECKPOINTED_BEFORE_COMMIT))
        .await;
    scenario.mock.enqueue_text("durable terminal answer").await;
    let workflow_id = scenario
        .launch_workflow(&project_id, "Persist this final answer.")
        .await;
    wait_for_marker(marker.as_deref().expect("fault marker should exist")).await;
    faulted.sigkill().await;

    let calls_before_restart = scenario.mock.call_count().await;
    let (mut restarted, _) = scenario.start(None).await;
    let workflow = scenario.wait_workflow_terminal(&workflow_id).await;
    let session_id = workflow["sessions"][0]["id"]
        .as_str()
        .expect("Session id should exist");
    let view = scenario.session_view(session_id).await;
    let recovered = view["turns"]
        .as_array()
        .expect("Turns should exist")
        .first()
        .expect("checkpointed Turn should remain");
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["output"], "durable terminal answer");
    assert_eq!(scenario.mock.call_count().await, calls_before_restart);
    assert_rollout_projected(&view);
    restarted.stop().await;
}

async fn workflow_inflight_sample_resumes_automatically_after_sigkill() {
    let mut scenario = Scenario::new().await;
    let (mut server, _) = scenario.start(None).await;
    let project_id = scenario.create_project("Workflow sample crash").await;
    scenario.publish_workflow(&project_id).await;
    let gate = scenario
        .mock
        .enqueue_blocked_text("response from the dead server")
        .await;
    let workflow_id = scenario
        .launch_workflow(&project_id, "Interrupt this model sample.")
        .await;
    scenario.mock.wait_for_calls(1).await;
    server.sigkill().await;
    gate.release();

    scenario.mock.enqueue_text("recovered model answer").await;
    let (mut restarted, _) = scenario.start(None).await;
    let workflow = scenario.wait_workflow_terminal(&workflow_id).await;
    let session_id = workflow["sessions"][0]["id"]
        .as_str()
        .expect("Session id should exist");
    let view = scenario.session_view(session_id).await;
    let recovered = view["turns"]
        .as_array()
        .expect("Turns should exist")
        .first()
        .expect("recovered Turn should remain");
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["output"], "recovered model answer");
    assert_eq!(scenario.mock.call_count().await, 2);
    assert_rollout_projected(&view);
    restarted.stop().await;
}

async fn prepared_unknown_tool_executes_once_after_sigkill() {
    let mut scenario = Scenario::new().await;
    scenario
        .mock
        .enqueue_tool_call(
            "prepared-call",
            "printf 'prepared-once\\n' >> prepared-once.log",
        )
        .await;
    scenario.mock.enqueue_text("prepared tool recovered").await;
    let (mut server, marker) = scenario.start(Some(TOOL_PREPARED_BEFORE_EXECUTION)).await;
    let project_id = scenario.create_project("Prepared tool crash").await;
    scenario.publish_workflow(&project_id).await;
    let workflow_id = scenario
        .launch_workflow(&project_id, "Execute the prepared command once.")
        .await;
    wait_for_marker(marker.as_deref().expect("fault marker should exist")).await;
    server.sigkill().await;

    let (mut restarted, _) = scenario.start(None).await;
    let workflow = scenario.wait_workflow_terminal(&workflow_id).await;
    let session_id = workflow["sessions"][0]["id"]
        .as_str()
        .expect("Session id should exist");
    let view = scenario.session_view(session_id).await;
    let tools = view["steps"]
        .as_array()
        .expect("Steps should exist")
        .iter()
        .filter(|step| step["kind"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["tool_call_id"], "prepared-call");
    assert_eq!(tools[0]["status"], "completed");
    assert_eq!(tools[0]["execution_state"], "completed");
    assert_eq!(
        std::fs::read_to_string(scenario.workspace.join("prepared-once.log"))
            .expect("prepared tool effect should exist"),
        "prepared-once\n"
    );
    assert_eq!(scenario.mock.call_count().await, 2);
    assert_rollout_projected(&view);
    restarted.stop().await;
}

async fn executing_unknown_tool_is_not_replayed_after_sigkill() {
    let mut scenario = Scenario::new().await;
    scenario
        .mock
        .enqueue_tool_call(
            "executing-call",
            "printf 'must-not-run\\n' >> execution-unknown.log",
        )
        .await;
    scenario
        .mock
        .enqueue_text("unknown tool outcome reconciled by the Agent")
        .await;
    let (mut server, marker) = scenario.start(Some(TOOL_EXECUTION_STARTED)).await;
    let project_id = scenario.create_project("Executing tool crash").await;
    scenario.publish_workflow(&project_id).await;
    let workflow_id = scenario
        .launch_workflow(&project_id, "Do not replay an unknown tool effect.")
        .await;
    wait_for_marker(marker.as_deref().expect("fault marker should exist")).await;
    server.sigkill().await;

    let (mut restarted, _) = scenario.start(None).await;
    let workflow = scenario.wait_workflow_terminal(&workflow_id).await;
    let session_id = workflow["sessions"][0]["id"]
        .as_str()
        .expect("Session id should exist");
    let view = scenario.session_view(session_id).await;
    let tools = view["steps"]
        .as_array()
        .expect("Steps should exist")
        .iter()
        .filter(|step| step["kind"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["tool_call_id"], "executing-call");
    assert_eq!(tools[0]["status"], "execution_unknown");
    assert_eq!(tools[0]["execution_state"], "execution_unknown");
    assert!(
        !scenario.workspace.join("execution-unknown.log").exists(),
        "an executing unknown effect must never be automatically replayed"
    );
    assert_eq!(scenario.mock.call_count().await, 2);
    assert_rollout_projected(&view);
    restarted.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_sigkill_recovery_matrix_preserves_durable_boundaries() {
    rollout_ahead_of_projection_is_replayed_after_sigkill().await;
    terminal_checkpoint_commits_without_resampling_after_sigkill().await;
    workflow_inflight_sample_resumes_automatically_after_sigkill().await;
    prepared_unknown_tool_executes_once_after_sigkill().await;
    executing_unknown_tool_is_not_replayed_after_sigkill().await;
}
