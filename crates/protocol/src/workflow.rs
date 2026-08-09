use crate::AccessPreset;
use crate::ProjectId;
use crate::WorkflowProgramId;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRequestMode {
    #[default]
    Required,
    None,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowProgramManifest {
    pub id: WorkflowProgramId,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub entrypoint: String,
    /// Whether this program starts from one immutable user task or creates its
    /// own interaction points (for example, a persistent interactive Session).
    pub request_mode: WorkflowRequestMode,
    pub params_schema: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowProgramSource {
    Builtin,
    User,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowProgram {
    /// `None` denotes a built-in program; user programs belong to one Project.
    pub project_id: Option<ProjectId>,
    pub manifest: WorkflowProgramManifest,
    pub source: WorkflowProgramSource,
    pub definition_path: String,
    pub sha256: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowProgramSnapshot {
    pub project_id: Option<ProjectId>,
    pub manifest: WorkflowProgramManifest,
    pub source: WorkflowProgramSource,
    pub definition_path: String,
    pub sha256: String,
    /// Exact PaperMachine Python DSL ABI used to validate and execute the Run.
    pub runtime_sha256: String,
    pub source_code: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowValidation {
    pub valid: bool,
    pub manifest: Option<WorkflowProgramManifest>,
    #[serde(default)]
    pub agents: Vec<WorkflowAgentDeclaration>,
    #[serde(default)]
    pub diagnostics: Vec<WorkflowDiagnostic>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowAgentDeclaration {
    pub class_name: String,
    pub actions: Vec<WorkflowActionDeclaration>,
    pub access: AccessPreset,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowActionDeclaration {
    pub name: String,
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}
