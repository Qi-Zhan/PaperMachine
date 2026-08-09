use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionInvocation;
use papermachine_protocol::ModelRouteCapabilities;
use papermachine_protocol::ModelRouteSnapshot;
use papermachine_protocol::PromptSnapshot;
use papermachine_protocol::Session;
use papermachine_protocol::ToolSetSnapshot;
use papermachine_protocol::Turn;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::Workflow;
use papermachine_protocol::WorkflowParticipant;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_store::NewWorkflow;
use papermachine_store::Store;
use papermachine_store::StoreError;
use serde_json::json;

#[derive(Clone)]
pub struct ActionHarness {
    pub workflow: Workflow,
    pub participant: WorkflowParticipant,
}

pub struct ActionTurn {
    pub invocation: ActionInvocation,
    pub turn: Turn,
}

impl ActionHarness {
    pub fn create(store: &Store, origin: &Session, access: AccessPreset) -> Self {
        let workflow = store
            .create_workflow(NewWorkflow {
                project_id: origin.project_id,
                started_from_session_id: Some(origin.id),
                program: workflow_snapshot(),
                request: "Store integration test".to_string(),
                instructions: String::new(),
                trigger: Default::default(),
                params: json!({}),
                default_model: "test-model".to_string(),
                access,
                enabled_skills: Vec::new(),
                launch_context: Default::default(),
                agent_access_overrides: Default::default(),
            })
            .expect("test Workflow should be created");
        store
            .start_workflow(workflow.id)
            .expect("test Workflow should start");
        let participant = store
            .create_participant(
                workflow.id,
                "TestAgent",
                "Test Agent",
                "test",
                "",
                "test-model",
                Vec::new(),
                access,
            )
            .expect("test participant should be created");
        Self {
            workflow,
            participant,
        }
    }

    pub fn create_turn(
        &self,
        store: &Store,
        origin: TurnOrigin,
        input: &str,
        expected_access: AccessPreset,
    ) -> Result<Turn, StoreError> {
        let created = self.create_action_turn(store, origin, input, expected_access)?;
        let _ = created.invocation.id;
        Ok(created.turn)
    }

    pub fn create_action_turn(
        &self,
        store: &Store,
        origin: TurnOrigin,
        input: &str,
        expected_access: AccessPreset,
    ) -> Result<ActionTurn, StoreError> {
        let invocation = store.create_action_invocation(
            self.workflow.id,
            self.participant.id,
            "test_action",
            "Exercise the Turn store",
            json!({"input": input}),
            Vec::new(),
        )?;
        let attempt = store.start_action_attempt(invocation.id)?;
        let turn = store.create_turn_for_attempt(
            attempt.id,
            self.participant.session_id,
            origin,
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

fn workflow_snapshot() -> WorkflowProgramSnapshot {
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
        sha256: "store-test".to_string(),
        runtime_sha256: "store-test-runtime".to_string(),
        source_code: "async def main(ctx): return {}\n".to_string(),
    }
}
