use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use crate::ToolReconciliation;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::ToolEffectDisposition;
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

    pub fn definitions_for(&self, authorization: &AuthorizationContext) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|(name, _)| authorization.tools.allows(name))
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

    pub fn effect_disposition(&self, name: &str) -> Option<ToolEffectDisposition> {
        self.tools.get(name).map(|tool| tool.effect_disposition())
    }

    pub async fn execute(
        &self,
        name: &str,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if !context.authorization.tools.allows(name) {
            return Err(ToolError::PermissionDenied {
                tool: name.to_string(),
                access: context.authorization.preset,
            });
        }
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        tool.execute(context, arguments).await
    }

    pub async fn reconcile(
        &self,
        name: &str,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolReconciliation, ToolError> {
        if !context.authorization.tools.allows(name) {
            return Err(ToolError::PermissionDenied {
                tool: name.to_string(),
                access: context.authorization.preset,
            });
        }
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        tool.reconcile(context, arguments).await
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
