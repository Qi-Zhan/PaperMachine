use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use papermachine_protocol::AgentAccessProfile;
use papermachine_protocol::ToolDefinition;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<String, Arc<dyn ToolExecutor>>>,
}

impl ToolRegistry {
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| {
                let mut definition = tool.definition();
                definition.supports_parallel = tool.supports_parallel();
                definition
            })
            .collect()
    }

    pub fn definitions_for(&self, access: AgentAccessProfile) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|(name, _)| tool_is_allowed(access, name))
            .map(|(_, tool)| {
                let mut definition = tool.definition();
                definition.supports_parallel = tool.supports_parallel();
                definition
            })
            .collect()
    }

    pub fn supports_parallel(&self, name: &str) -> Option<bool> {
        self.tools.get(name).map(|tool| tool.supports_parallel())
    }

    pub async fn execute(
        &self,
        name: &str,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if !tool_is_allowed(context.access, name) {
            return Err(ToolError::PermissionDenied {
                tool: name.to_string(),
                access: context.access,
            });
        }
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        tool.execute(context, arguments).await
    }
}

fn tool_is_allowed(access: AgentAccessProfile, name: &str) -> bool {
    if access.is_unrestricted() {
        return true;
    }
    match name {
        "read_file" => access.allows_workspace_read(),
        "write_file" => access.allows_workspace_write(),
        "exec_command" => access.allows_sandboxed_command(),
        "fetch_url" => access.allows_research_network(),
        _ => false,
    }
}

#[derive(Default)]
pub struct ToolRegistryBuilder {
    tools: BTreeMap<String, Arc<dyn ToolExecutor>>,
}

impl ToolRegistryBuilder {
    pub fn register<T>(mut self, tool: T) -> Result<Self, ToolError>
    where
        T: ToolExecutor + 'static,
    {
        let name = tool.definition().name;
        if self.tools.contains_key(&name) {
            return Err(ToolError::Execution(format!(
                "tool {name} is already registered"
            )));
        }
        self.tools.insert(name, Arc::new(tool));
        Ok(self)
    }

    pub fn build(self) -> ToolRegistry {
        ToolRegistry {
            tools: Arc::new(self.tools),
        }
    }
}
