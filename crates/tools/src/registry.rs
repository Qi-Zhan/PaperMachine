use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::ToolSetSnapshot;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone)]
struct CatalogEntry {
    executor: Arc<dyn ToolExecutor>,
    kind: ToolKind,
}

#[derive(Clone, Copy)]
enum ToolKind {
    Native,
    Collaboration,
}

/// Host-owned catalog of every local tool implementation.
///
/// The catalog is never sent to a model. It materializes one exact
/// [`ToolRegistry`] for each Turn.
#[derive(Clone, Default)]
pub struct ToolCatalog {
    tools: Arc<BTreeMap<String, CatalogEntry>>,
}

impl ToolCatalog {
    pub fn builder() -> ToolCatalogBuilder {
        ToolCatalogBuilder::default()
    }

    /// Materialize a Workflow Action's declared local tool surface.
    /// Requested tools are filtered by the Turn access ceiling.
    pub fn materialize_action_tools(
        &self,
        tool_policy: Option<&[String]>,
        access: AccessPreset,
        allow_spawn: bool,
    ) -> Result<ToolSetSnapshot, ToolError> {
        let requested_tools = tool_policy
            .map(|tools| tools.iter().collect::<Vec<_>>())
            .unwrap_or_else(|| self.tools.keys().collect());
        let mut requested = BTreeSet::new();
        let mut selected = Vec::new();
        for name in requested_tools {
            if !requested.insert(name.as_str()) {
                return Err(ToolError::Execution(format!(
                    "Action tool list contains duplicate name: {name}"
                )));
            }
            let entry = self
                .tools
                .get(name)
                .ok_or_else(|| ToolError::UnknownTool(name.clone()))?;
            let allowed = match entry.kind {
                ToolKind::Native => access.allows_local_tool(name),
                ToolKind::Collaboration => allow_spawn || name != "spawn_agent",
            };
            if allowed {
                selected.push((name, entry));
            }
        }
        self.snapshot_from_entries(selected)
    }

    /// Rebuild the executable subset for an immutable Turn snapshot.
    pub fn registry_for_snapshot(
        &self,
        snapshot: &ToolSetSnapshot,
    ) -> Result<ToolRegistry, ToolError> {
        snapshot.validate().map_err(ToolError::Execution)?;
        let mut selected = BTreeMap::new();
        for saved in &snapshot.definitions {
            let entry = self
                .tools
                .get(&saved.name)
                .ok_or_else(|| ToolError::UnknownTool(saved.name.clone()))?;
            let current = definition_for(entry.executor.as_ref());
            if &current != saved {
                return Err(ToolError::Execution(format!(
                    "tool definition changed since Turn creation: {}",
                    saved.name
                )));
            }
            selected.insert(saved.name.clone(), Arc::clone(&entry.executor));
        }
        Ok(ToolRegistry {
            tools: Arc::new(selected),
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    fn snapshot_from_entries<'a>(
        &self,
        entries: impl IntoIterator<Item = (&'a String, &'a CatalogEntry)>,
    ) -> Result<ToolSetSnapshot, ToolError> {
        let definitions = entries
            .into_iter()
            .map(|(_, entry)| definition_for(entry.executor.as_ref()))
            .collect();
        ToolSetSnapshot::materialize(definitions).map_err(ToolError::Execution)
    }
}

fn definition_for(tool: &dyn ToolExecutor) -> ToolDefinition {
    let mut definition = tool.definition();
    definition.supports_parallel = tool.supports_parallel();
    definition
}

/// Exact model-visible and executable local tool set for one Turn.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<String, Arc<dyn ToolExecutor>>>,
}

impl ToolRegistry {
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| definition_for(tool.as_ref()))
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
        let tool = self
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        tool.execute(context, arguments).await
    }
}

#[derive(Default)]
pub struct ToolCatalogBuilder {
    tools: BTreeMap<String, CatalogEntry>,
}

impl ToolCatalogBuilder {
    pub fn register_native<T>(self, tool: T) -> Result<Self, ToolError>
    where
        T: ToolExecutor + 'static,
    {
        self.register(tool, ToolKind::Native)
    }

    pub fn register_collaboration<T>(self, tool: T) -> Result<Self, ToolError>
    where
        T: ToolExecutor + 'static,
    {
        self.register(tool, ToolKind::Collaboration)
    }

    fn register<T>(mut self, tool: T, kind: ToolKind) -> Result<Self, ToolError>
    where
        T: ToolExecutor + 'static,
    {
        let name = tool.definition().name;
        if self.tools.contains_key(&name) {
            return Err(ToolError::Execution(format!(
                "tool {name} is already registered"
            )));
        }
        self.tools.insert(
            name,
            CatalogEntry {
                executor: Arc::new(tool),
                kind,
            },
        );
        Ok(self)
    }

    pub fn build(self) -> ToolCatalog {
        ToolCatalog {
            tools: Arc::new(self.tools),
        }
    }
}
