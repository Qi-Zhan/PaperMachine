use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ProjectSkill {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub relative_path: String,
    pub sha256: String,
    pub instructions: String,
}
