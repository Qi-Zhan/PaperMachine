use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptLayerKind {
    Runtime,
    Project,
    Workflow,
    Agent,
    Skills,
    Control,
}

impl PromptLayerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Project => "project",
            Self::Workflow => "workflow",
            Self::Agent => "agent",
            Self::Skills => "skills",
            Self::Control => "control",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PromptLayer {
    pub kind: PromptLayerKind,
    pub name: String,
    /// Stable, inspectable origin such as `builtin:runtime`, a Project-relative
    /// path, or a Workflow/Session identifier.
    pub source: String,
    pub content: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PromptSnapshot {
    pub layers: Vec<PromptLayer>,
    /// Exact provider instructions produced from `layers` for this Turn.
    pub rendered: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectSystemPrompt {
    pub relative_path: String,
    pub content: String,
    pub sha256: String,
}
