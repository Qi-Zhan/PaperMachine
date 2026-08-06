use crate::AgentAccessProfile;
use crate::Budget;
use crate::WorkflowId;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowManifest {
    pub id: WorkflowId,
    pub slug: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub entrypoint: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub default_budget: Budget,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSource {
    Builtin,
    User,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowRegistration {
    pub manifest: WorkflowManifest,
    pub source: WorkflowSource,
    pub definition_path: String,
    pub sha256: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowSnapshot {
    pub manifest: WorkflowManifest,
    pub source: WorkflowSource,
    pub definition_path: String,
    pub sha256: String,
    pub source_code: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowValidation {
    pub valid: bool,
    pub manifest: Option<WorkflowManifest>,
    #[serde(default)]
    pub agents: Vec<WorkflowAgentDeclaration>,
    #[serde(default)]
    pub features: WorkflowFeatureSummary,
    #[serde(default)]
    pub diagnostics: Vec<WorkflowDiagnostic>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowAgentDeclaration {
    pub class_name: String,
    pub actions: Vec<String>,
    #[serde(default)]
    pub access: AgentAccessProfile,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowFeatureSummary {
    pub parallel_blocks: u32,
    pub teams: Vec<String>,
    pub relations: u32,
    pub scopes: Vec<String>,
    pub channels: Vec<String>,
    pub timers: Vec<WorkflowTimerDeclaration>,
    pub human_checkpoints: u32,
    pub background_tasks: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowTimerDeclaration {
    pub callback: String,
    pub seconds: Option<f64>,
    pub policy: Option<String>,
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
