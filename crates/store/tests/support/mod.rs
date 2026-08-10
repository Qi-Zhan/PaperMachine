use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionInvocation;
use papermachine_protocol::ActionSource;
use papermachine_protocol::Agent;
use papermachine_protocol::ModelRouteCapabilities;
use papermachine_protocol::ModelRouteSnapshot;
use papermachine_protocol::ProjectId;
use papermachine_protocol::PromptSnapshot;
use papermachine_protocol::Session;
use papermachine_protocol::SessionTrigger;
use papermachine_protocol::SessionTriggerKind;
use papermachine_protocol::ToolSetSnapshot;
use papermachine_protocol::Turn;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_store::NewActionInvocation;
use papermachine_store::NewSession;
use papermachine_store::Store;
use papermachine_store::StoreError;
use serde_json::json;

#[derive(Clone)]
pub struct ActionHarness {
    pub session: Session,
    pub agent: Agent,
}

#[allow(dead_code)]
pub struct ActionTurn {
    pub invocation: ActionInvocation,
    pub turn: Turn,
}

impl ActionHarness {
    pub fn create(store: &Store, origin: &Session, access: AccessPreset) -> Self {
        let session = create_session_from(store, origin, "Store integration test", access);
        let agent = store
            .create_agent(
                session.id,
                "TestAgent",
                "Test Agent",
                "test",
                "",
                "test-model",
                Vec::new(),
                access,
            )
            .expect("test Agent should be created");
        Self { session, agent }
    }

    pub fn create_turn(
        &self,
        store: &Store,
        input: &str,
        expected_access: AccessPreset,
    ) -> Result<Turn, StoreError> {
        Ok(self.create_action_turn(store, input, expected_access)?.turn)
    }

    pub fn create_action_turn(
        &self,
        store: &Store,
        input: &str,
        expected_access: AccessPreset,
    ) -> Result<ActionTurn, StoreError> {
        let invocation = store.create_action_invocation(NewActionInvocation {
            session_id: self.session.id,
            agent_id: self.agent.id,
            action_name: "test_action".to_string(),
            contract: "Exercise the Turn store".to_string(),
            arguments: json!({"input": input}),
            input: input.to_string(),
            source: ActionSource::Workflow,
            requested_tools: Vec::new(),
            tools_enabled: true,
            web_search_context_size: None,
            reasoning_effort: None,
            response_format: None,
        })?;
        let attempt = store.start_action_attempt(invocation.id)?;
        let turn = store.create_turn_for_attempt(
            attempt.id,
            self.agent.id,
            input,
            model_route("test-model"),
            PromptSnapshot::default(),
            true,
            expected_access,
            ToolSetSnapshot::materialize(Vec::new()).map_err(StoreError::Invariant)?,
            None,
            None,
            Vec::new(),
        )?;
        Ok(ActionTurn { invocation, turn })
    }
}

pub fn create_root_session(
    store: &Store,
    project_id: ProjectId,
    title: &str,
    access: AccessPreset,
) -> Session {
    let session = store
        .create_session(NewSession {
            project_id,
            program: workflow_snapshot(),
            title: title.to_string(),
            request: String::new(),
            instructions: String::new(),
            trigger: SessionTrigger::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("test Session should be created");
    store
        .start_session(session.id)
        .expect("test Session should start")
}

fn create_session_from(
    store: &Store,
    origin: &Session,
    title: &str,
    access: AccessPreset,
) -> Session {
    let session = store
        .create_session(NewSession {
            project_id: origin.project_id,
            program: workflow_snapshot(),
            title: title.to_string(),
            request: title.to_string(),
            instructions: String::new(),
            trigger: SessionTrigger {
                kind: SessionTriggerKind::User,
                source_session_id: Some(origin.id),
            },
            params: json!({}),
            default_model: "test-model".to_string(),
            access,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("test Session should be created");
    store
        .start_session(session.id)
        .expect("test Session should start")
}

pub fn model_route(profile: &str) -> ModelRouteSnapshot {
    ModelRouteSnapshot {
        profile: profile.to_string(),
        provider: "test".to_string(),
        upstream_model: profile.to_string(),
        context_window: 128_000,
        capabilities: ModelRouteCapabilities::default(),
        reasoning_effort: None,
        config_sha256: "0".repeat(64),
    }
}

pub fn workflow_snapshot() -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: "store-test".to_string(),
            name: "Store test".to_string(),
            description: String::new(),
            entrypoint: "main".to_string(),
            request_mode: Default::default(),
            params_schema: json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/store-test/workflow.py".to_string(),
        sha256: "source-sha".to_string(),
        runtime_sha256: "runtime-sha".to_string(),
        source_code: "async def main(ctx): return {}\n".to_string(),
    }
}
