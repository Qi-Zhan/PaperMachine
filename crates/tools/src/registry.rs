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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolDomain {
    Workspace,
    Project,
}

#[derive(Clone)]
struct CatalogEntry {
    domain: ToolDomain,
    executor: Arc<dyn ToolExecutor>,
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
    /// Workspace tools are filtered by the Turn access ceiling; Project tools
    /// are admitted only through this Workflow-only path.
    pub fn materialize_action_tools(
        &self,
        requested_tools: &[String],
        access: AccessPreset,
        tools_enabled: bool,
    ) -> Result<ToolSetSnapshot, ToolError> {
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
            if tools_enabled
                && (entry.domain == ToolDomain::Project || access.tool_capabilities().allows(name))
            {
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
    pub fn register_workspace<T>(self, tool: T) -> Result<Self, ToolError>
    where
        T: ToolExecutor + 'static,
    {
        self.register(tool, ToolDomain::Workspace)
    }

    pub fn register_project<T>(self, tool: T) -> Result<Self, ToolError>
    where
        T: ToolExecutor + 'static,
    {
        self.register(tool, ToolDomain::Project)
    }

    fn register<T>(mut self, tool: T, domain: ToolDomain) -> Result<Self, ToolError>
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
                domain,
                executor: Arc::new(tool),
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
