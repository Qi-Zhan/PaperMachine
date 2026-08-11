use crate::DEFAULT_MODEL_REQUEST_TIMEOUT;
use crate::DEFAULT_MODEL_STREAM_IDLE_TIMEOUT;
use crate::ModelClient;
use crate::ModelError;
use crate::ModelStream;
use crate::OpenAiChatClient;
use crate::OpenAiChatCompatibility;
use crate::OpenAiChatConfig;
use crate::OpenAiPromptCacheMode;
use crate::OpenAiReasoningEffort;
use crate::OpenAiResponsesClient;
use crate::OpenAiResponsesConfig;
use crate::hash_route_config;
use async_trait::async_trait;
use futures::StreamExt;
use papermachine_protocol::HostedTool;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::ModelRouteCapabilities;
use papermachine_protocol::ModelRouteSnapshot;
use papermachine_protocol::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelProfile {
    pub id: String,
    pub provider: String,
    pub api: ModelApi,
    pub model: String,
    pub context_window: usize,
    pub capabilities: Vec<ModelCapability>,
    pub default_reasoning_effort: Option<ReasoningEffort>,
    pub config_sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelApi {
    OpenAiResponses,
    OpenAiChatCompletions,
}

impl ModelApi {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "open_ai_responses",
            Self::OpenAiChatCompletions => "open_ai_chat_completions",
        }
    }
}

impl ModelProfile {
    pub fn supports(&self, capability: ModelCapability) -> bool {
        self.capabilities.contains(&capability)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    HostedWebSearch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelProviderInfo {
    pub id: String,
    pub base_url: String,
    pub apis: Vec<ModelApi>,
    pub chat_compatibility: String,
    pub max_request_retries: u32,
    pub request_timeout_seconds: u64,
    pub stream_idle_timeout_seconds: u64,
    pub responses_websockets: bool,
    pub prompt_cache_mode: String,
}

#[derive(Clone)]
pub struct ConfiguredModels {
    pub default_model: String,
    pub profiles: Vec<ModelProfile>,
    pub providers: Vec<ModelProviderInfo>,
    pub router: ModelRouter,
}

impl fmt::Debug for ConfiguredModels {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredModels")
            .field("default_model", &self.default_model)
            .field("profiles", &self.profiles)
            .field("providers", &self.providers)
            .finish_non_exhaustive()
    }
}

impl ConfiguredModels {
    pub fn from_file(path: &Path) -> Result<Self, ModelError> {
        let source = fs::read_to_string(path).map_err(|error| {
            ModelError::Configuration(format!(
                "failed to read PaperMachine model config {}: {error}",
                path.display()
            ))
        })?;
        Self::from_toml_with_key_lookup(&source, |name| std::env::var(name).ok()).map_err(|error| {
            ModelError::Configuration(format!(
                "failed to load PaperMachine model config {}: {error}",
                path.display()
            ))
        })
    }

    fn from_toml_with_key_lookup(
        source: &str,
        mut key_lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self, ModelError> {
        let file: ModelConfigFile = toml::from_str(source).map_err(|error| {
            ModelError::Configuration(format!("invalid PaperMachine model config: {error}"))
        })?;
        if file.default_model.trim().is_empty() {
            return Err(ModelError::Configuration(
                "default_model must name a model profile".to_string(),
            ));
        }
        if file.providers.is_empty() || file.models.is_empty() {
            return Err(ModelError::Configuration(
                "model config requires at least one provider and one model profile".to_string(),
            ));
        }
        if !file.models.contains_key(&file.default_model) {
            return Err(ModelError::Configuration(format!(
                "default_model {:?} is not defined in [models]",
                file.default_model
            )));
        }

        let ModelConfigFile {
            default_model,
            providers: provider_configs,
            models: model_configs,
        } = file;
        let declared_providers = provider_configs.keys().cloned().collect::<HashSet<_>>();
        let responses_hosted_search = model_configs
            .values()
            .filter(|profile| {
                profile.api == ModelApi::OpenAiResponses
                    && profile
                        .capabilities
                        .contains(&ModelCapability::HostedWebSearch)
            })
            .map(|profile| profile.provider.clone())
            .collect::<HashSet<_>>();

        let mut resolved_providers = HashMap::with_capacity(provider_configs.len());
        let mut unavailable_providers = HashSet::new();
        for (provider_id, provider) in provider_configs {
            validate_identifier("provider", &provider_id)?;
            if provider.api_key_env.trim().is_empty() {
                return Err(ModelError::Configuration(format!(
                    "provider {provider_id:?} must set api_key_env"
                )));
            }
            let api_key = match key_lookup(provider.api_key_env.trim())
                .filter(|value| !value.trim().is_empty())
            {
                Some(api_key) => api_key,
                None if provider.optional => {
                    tracing::warn!(
                        provider = provider_id,
                        api_key_env = provider.api_key_env,
                        "optional model provider is unavailable"
                    );
                    unavailable_providers.insert(provider_id);
                    continue;
                }
                None => {
                    return Err(ModelError::Configuration(format!(
                        "provider {provider_id:?} requires non-empty environment variable {}",
                        provider.api_key_env
                    )));
                }
            };
            let base_url = normalize_base_url(&provider.base_url)?;
            if base_url.scheme() != "https" {
                tracing::warn!(
                    provider = provider_id,
                    endpoint = %base_url,
                    "model endpoint is not protected by HTTPS"
                );
            }
            let request_timeout = positive_duration(
                "request_timeout_seconds",
                provider.request_timeout_seconds,
                DEFAULT_MODEL_REQUEST_TIMEOUT,
            )?;
            let stream_idle_timeout = positive_duration(
                "stream_idle_timeout_seconds",
                provider.stream_idle_timeout_seconds,
                DEFAULT_MODEL_STREAM_IDLE_TIMEOUT,
            )?;
            let prompt_cache_mode = provider.prompt_cache_mode.unwrap_or_default();
            let responses_websockets = provider.responses_websockets.unwrap_or(true);
            let max_request_retries = provider.max_request_retries.unwrap_or(2);
            let store_responses = provider.store_responses.unwrap_or(false);
            let default_reasoning_effort = provider.reasoning_effort.map(protocol_reasoning_effort);
            resolved_providers.insert(
                provider_id,
                ResolvedProvider {
                    base_url: base_url.to_string(),
                    api_key_env: provider.api_key_env.trim().to_string(),
                    optional: provider.optional,
                    api_key,
                    organization: provider.organization,
                    project: provider.project,
                    max_request_retries,
                    request_timeout,
                    stream_idle_timeout,
                    reasoning_effort: provider.reasoning_effort,
                    default_reasoning_effort,
                    store_responses,
                    responses_websockets,
                    prompt_cache_mode,
                    chat_compatibility: provider.chat_compatibility,
                },
            );
        }

        let mut api_clients: HashMap<(String, ModelApi), Arc<dyn ModelClient>> = HashMap::new();
        let mut api_config_sha256 = HashMap::new();
        let mut profile_clients = HashMap::new();
        let mut used_apis: HashMap<String, HashSet<ModelApi>> = HashMap::new();
        let mut profiles = Vec::with_capacity(model_configs.len());
        for (profile_id, profile) in model_configs {
            validate_identifier("model profile", &profile_id)?;
            if !declared_providers.contains(&profile.provider) {
                return Err(ModelError::Configuration(format!(
                    "model profile {profile_id:?} references unknown provider {:?}",
                    profile.provider
                )));
            }
            if profile.model.trim().is_empty() {
                return Err(ModelError::Configuration(format!(
                    "model profile {profile_id:?} must set model"
                )));
            }
            if profile.context_window < 4_096 {
                return Err(ModelError::Configuration(format!(
                    "model profile {profile_id:?} context_window must be at least 4096"
                )));
            }
            if profile.capabilities.iter().collect::<HashSet<_>>().len()
                != profile.capabilities.len()
            {
                return Err(ModelError::Configuration(format!(
                    "model profile {profile_id:?} contains duplicate capabilities"
                )));
            }
            if profile.api == ModelApi::OpenAiChatCompletions
                && profile
                    .capabilities
                    .contains(&ModelCapability::HostedWebSearch)
            {
                return Err(ModelError::Configuration(format!(
                    "model profile {profile_id:?} cannot declare hosted_web_search through open_ai_chat_completions"
                )));
            }
            if unavailable_providers.contains(&profile.provider) {
                tracing::warn!(
                    model_profile = profile_id,
                    provider = profile.provider,
                    "model profile is unavailable"
                );
                continue;
            }
            let provider = resolved_providers.get(&profile.provider).ok_or_else(|| {
                ModelError::Configuration(format!(
                    "model profile {profile_id:?} has no available provider {:?}",
                    profile.provider
                ))
            })?;
            let api_key = (profile.provider.clone(), profile.api);
            if !api_clients.contains_key(&api_key) {
                let hosted_web_search = profile.api == ModelApi::OpenAiResponses
                    && responses_hosted_search.contains(&profile.provider);
                let (client, config_sha256) =
                    build_api_client(&profile.provider, provider, profile.api, hosted_web_search)?;
                api_clients.insert(api_key.clone(), client);
                api_config_sha256.insert(api_key.clone(), config_sha256);
            }
            let client = api_clients
                .get(&api_key)
                .cloned()
                .expect("API client was inserted above");
            let config_sha256 = hash_route_config(&serde_json::json!({
                "profile": profile_id,
                "provider": profile.provider,
                "api": profile.api,
                "upstream_model": profile.model,
                "context_window": profile.context_window,
                "capabilities": profile.capabilities,
                "default_reasoning_effort": provider.default_reasoning_effort,
                "api_config_sha256": api_config_sha256.get(&api_key),
            }))?;
            profile_clients.insert(profile_id.clone(), client);
            used_apis
                .entry(profile.provider.clone())
                .or_default()
                .insert(profile.api);
            profiles.push(ModelProfile {
                id: profile_id,
                provider: profile.provider,
                api: profile.api,
                model: profile.model,
                context_window: profile.context_window,
                capabilities: profile.capabilities,
                default_reasoning_effort: provider.default_reasoning_effort,
                config_sha256,
            });
        }
        let mut providers = used_apis
            .into_iter()
            .map(|(provider_id, apis)| {
                let provider = resolved_providers
                    .get(&provider_id)
                    .expect("used provider must be available");
                let mut apis = apis.into_iter().collect::<Vec<_>>();
                apis.sort();
                ModelProviderInfo {
                    id: provider_id,
                    base_url: provider.base_url.clone(),
                    apis,
                    chat_compatibility: provider.chat_compatibility.as_str().to_string(),
                    max_request_retries: provider.max_request_retries,
                    request_timeout_seconds: provider.request_timeout.as_secs(),
                    stream_idle_timeout_seconds: provider.stream_idle_timeout.as_secs(),
                    responses_websockets: provider.responses_websockets,
                    prompt_cache_mode: prompt_cache_mode_name(provider.prompt_cache_mode)
                        .to_string(),
                }
            })
            .collect::<Vec<_>>();
        profiles.sort_by(|left, right| left.id.cmp(&right.id));
        providers.sort_by(|left, right| left.id.cmp(&right.id));
        if !profiles.iter().any(|profile| profile.id == default_model) {
            return Err(ModelError::Configuration(format!(
                "default_model {default_model:?} is unavailable; its provider credentials are required"
            )));
        }
        let router = ModelRouter::new(profiles.clone(), profile_clients)?;
        Ok(Self {
            default_model,
            profiles,
            providers,
            router,
        })
    }
}

#[derive(Clone)]
pub struct ModelRouter {
    routes: Arc<HashMap<String, ModelRoute>>,
    clients: Arc<Vec<Arc<dyn ModelClient>>>,
}

impl fmt::Debug for ModelRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRouter")
            .field("profiles", &self.routes.keys().collect::<Vec<_>>())
            .field("client_count", &self.clients.len())
            .finish()
    }
}

#[derive(Clone)]
struct ModelRoute {
    profile: ModelProfile,
    client: Arc<dyn ModelClient>,
}

impl ModelRouter {
    pub fn new(
        profiles: Vec<ModelProfile>,
        clients_by_profile: HashMap<String, Arc<dyn ModelClient>>,
    ) -> Result<Self, ModelError> {
        let mut routes = HashMap::with_capacity(profiles.len());
        for mut profile in profiles {
            if profile.config_sha256.is_empty() {
                profile.config_sha256 = hash_route_config(&serde_json::json!({
                    "profile": profile.id,
                    "provider": profile.provider,
                    "api": profile.api,
                    "upstream_model": profile.model,
                    "context_window": profile.context_window,
                    "capabilities": profile.capabilities,
                    "default_reasoning_effort": profile.default_reasoning_effort,
                }))?;
            }
            let client = clients_by_profile
                .get(&profile.id)
                .cloned()
                .ok_or_else(|| {
                    ModelError::Configuration(format!(
                        "model profile {:?} has no model client",
                        profile.id
                    ))
                })?;
            if routes
                .insert(profile.id.clone(), ModelRoute { profile, client })
                .is_some()
            {
                return Err(ModelError::Configuration(
                    "model profile identifiers must be unique".to_string(),
                ));
            }
        }
        if clients_by_profile.len() != routes.len() {
            return Err(ModelError::Configuration(
                "model clients contain an unknown profile identifier".to_string(),
            ));
        }
        let mut clients = Vec::new();
        for client in clients_by_profile.into_values() {
            if !clients
                .iter()
                .any(|existing| Arc::ptr_eq(existing, &client))
            {
                clients.push(client);
            }
        }
        Ok(Self {
            routes: Arc::new(routes),
            clients: Arc::new(clients),
        })
    }
}

#[async_trait]
impl ModelClient for ModelRouter {
    async fn stream(&self, mut request: ModelRequest) -> Result<ModelStream, ModelError> {
        let profile_id = request.model.clone();
        let route = self.routes.get(&profile_id).cloned().ok_or_else(|| {
            let mut available = self.routes.keys().cloned().collect::<Vec<_>>();
            available.sort();
            ModelError::Configuration(format!(
                "unknown model profile {profile_id:?}; configured profiles: {}",
                available.join(", ")
            ))
        })?;
        request.model = route.profile.model.clone();
        let provider_id = route.profile.provider.clone();
        let api = route.profile.api.as_str().to_string();
        let upstream_model = route.profile.model.clone();
        let stream = route.client.stream(request).await?;
        Ok(stream
            .map(move |event| {
                event.map(|event| match event {
                    ModelEvent::RequestMetadata { mut metadata } => {
                        metadata.provider = Some(provider_id.clone());
                        metadata.api = Some(api.clone());
                        metadata.model_profile = Some(profile_id.clone());
                        metadata.upstream_model = Some(upstream_model.clone());
                        ModelEvent::RequestMetadata { metadata }
                    }
                    other => other,
                })
            })
            .boxed())
    }

    async fn close_transport_session(&self, session_key: &str) {
        for client in self.clients.iter() {
            client.close_transport_session(session_key).await;
        }
    }

    fn model_context_window(&self, model: &str) -> Option<usize> {
        self.routes
            .get(model)
            .map(|route| route.profile.context_window)
    }

    fn supports_hosted_tool(&self, model: &str, tool: HostedTool) -> bool {
        self.routes.get(model).is_some_and(|route| {
            matches!(tool, HostedTool::WebSearch)
                && route.profile.supports(ModelCapability::HostedWebSearch)
                && route
                    .client
                    .supports_hosted_tool(&route.profile.model, tool)
        })
    }

    fn resolve_route_snapshot(
        &self,
        profile: &str,
        reasoning_effort: Option<ReasoningEffort>,
        _fallback_context_window: usize,
    ) -> Result<ModelRouteSnapshot, ModelError> {
        let route = self.routes.get(profile).ok_or_else(|| {
            let mut available = self.routes.keys().cloned().collect::<Vec<_>>();
            available.sort();
            ModelError::Configuration(format!(
                "unknown model profile {profile:?}; configured profiles: {}",
                available.join(", ")
            ))
        })?;
        let snapshot = ModelRouteSnapshot {
            profile: route.profile.id.clone(),
            provider: route.profile.provider.clone(),
            upstream_model: route.profile.model.clone(),
            context_window: route.profile.context_window,
            capabilities: ModelRouteCapabilities {
                hosted_web_search: route.profile.supports(ModelCapability::HostedWebSearch)
                    && route
                        .client
                        .supports_hosted_tool(&route.profile.model, HostedTool::WebSearch),
            },
            reasoning_effort: reasoning_effort.or(route.profile.default_reasoning_effort),
            config_sha256: route.profile.config_sha256.clone(),
        };
        snapshot.validate().map_err(ModelError::Configuration)?;
        Ok(snapshot)
    }

    fn validate_route_snapshot(
        &self,
        snapshot: &ModelRouteSnapshot,
        fallback_context_window: usize,
    ) -> Result<(), ModelError> {
        snapshot.validate().map_err(ModelError::Configuration)?;
        let current = self.resolve_route_snapshot(
            &snapshot.profile,
            snapshot.reasoning_effort,
            fallback_context_window,
        )?;
        if current != *snapshot {
            return Err(ModelError::Configuration(format!(
                "model route configuration changed for profile {:?}",
                snapshot.profile
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelConfigFile {
    default_model: String,
    providers: HashMap<String, ProviderConfigFile>,
    models: HashMap<String, ModelProfileFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfigFile {
    base_url: String,
    api_key_env: String,
    #[serde(default)]
    optional: bool,
    organization: Option<String>,
    project: Option<String>,
    max_request_retries: Option<u32>,
    request_timeout_seconds: Option<u64>,
    stream_idle_timeout_seconds: Option<u64>,
    reasoning_effort: Option<OpenAiReasoningEffort>,
    store_responses: Option<bool>,
    responses_websockets: Option<bool>,
    prompt_cache_mode: Option<OpenAiPromptCacheMode>,
    #[serde(default)]
    chat_compatibility: OpenAiChatCompatibility,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelProfileFile {
    provider: String,
    api: ModelApi,
    model: String,
    context_window: usize,
    capabilities: Vec<ModelCapability>,
}

struct ResolvedProvider {
    base_url: String,
    api_key_env: String,
    optional: bool,
    api_key: String,
    organization: Option<String>,
    project: Option<String>,
    max_request_retries: u32,
    request_timeout: Duration,
    stream_idle_timeout: Duration,
    reasoning_effort: Option<OpenAiReasoningEffort>,
    default_reasoning_effort: Option<ReasoningEffort>,
    store_responses: bool,
    responses_websockets: bool,
    prompt_cache_mode: OpenAiPromptCacheMode,
    chat_compatibility: OpenAiChatCompatibility,
}

fn build_api_client(
    provider_id: &str,
    provider: &ResolvedProvider,
    api: ModelApi,
    hosted_web_search: bool,
) -> Result<(Arc<dyn ModelClient>, String), ModelError> {
    match api {
        ModelApi::OpenAiResponses => {
            let endpoint = super::openai::responses_endpoint(&provider.base_url)?;
            let config_sha256 = hash_route_config(&serde_json::json!({
                "provider": provider_id,
                "api": api,
                "endpoint": endpoint.as_str(),
                "api_key_env": provider.api_key_env,
                "optional": provider.optional,
                "organization": provider.organization,
                "project": provider.project,
                "max_request_retries": provider.max_request_retries,
                "request_timeout_seconds": provider.request_timeout.as_secs(),
                "stream_idle_timeout_seconds": provider.stream_idle_timeout.as_secs(),
                "reasoning_effort": provider.default_reasoning_effort,
                "store_responses": provider.store_responses,
                "responses_websockets": provider.responses_websockets,
                "hosted_web_search": hosted_web_search,
                "prompt_cache_mode": prompt_cache_mode_name(provider.prompt_cache_mode),
            }))?;
            let client = OpenAiResponsesClient::new(OpenAiResponsesConfig {
                provider_id: provider_id.to_string(),
                endpoint,
                api_key: provider.api_key.clone(),
                organization: provider.organization.clone(),
                project: provider.project.clone(),
                max_request_retries: provider.max_request_retries,
                request_timeout: provider.request_timeout,
                stream_idle_timeout: provider.stream_idle_timeout,
                reasoning_effort: provider.reasoning_effort,
                store_responses: provider.store_responses,
                responses_websockets: provider.responses_websockets,
                hosted_web_search,
                prompt_cache_mode: provider.prompt_cache_mode,
            })?;
            Ok((Arc::new(client), config_sha256))
        }
        ModelApi::OpenAiChatCompletions => {
            let endpoint = super::openai_chat::chat_completions_endpoint(&provider.base_url)?;
            let config_sha256 = hash_route_config(&serde_json::json!({
                "provider": provider_id,
                "api": api,
                "endpoint": endpoint.as_str(),
                "api_key_env": provider.api_key_env,
                "optional": provider.optional,
                "organization": provider.organization,
                "project": provider.project,
                "max_request_retries": provider.max_request_retries,
                "request_timeout_seconds": provider.request_timeout.as_secs(),
                "stream_idle_timeout_seconds": provider.stream_idle_timeout.as_secs(),
                "reasoning_effort": provider.default_reasoning_effort,
                "chat_compatibility": provider.chat_compatibility,
            }))?;
            let client = OpenAiChatClient::new(OpenAiChatConfig {
                provider_id: provider_id.to_string(),
                endpoint,
                api_key: provider.api_key.clone(),
                organization: provider.organization.clone(),
                project: provider.project.clone(),
                max_request_retries: provider.max_request_retries,
                request_timeout: provider.request_timeout,
                stream_idle_timeout: provider.stream_idle_timeout,
                reasoning_effort: provider.reasoning_effort,
                compatibility: provider.chat_compatibility,
            })?;
            Ok((Arc::new(client), config_sha256))
        }
    }
}

fn normalize_base_url(value: &str) -> Result<Url, ModelError> {
    let parsed = value
        .parse::<Url>()
        .map_err(|error| ModelError::Configuration(format!("invalid model base URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(ModelError::Configuration(
            "model base URL must be an absolute HTTP(S) URL".to_string(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ModelError::Configuration(
            "model base URL must not contain credentials".to_string(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ModelError::Configuration(
            "model base URL must not contain a query or fragment".to_string(),
        ));
    }
    Ok(parsed)
}

fn validate_identifier(kind: &str, value: &str) -> Result<(), ModelError> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(ModelError::Configuration(format!(
            "{kind} identifier {value:?} may contain only letters, digits, dot, dash, and underscore"
        )))
    }
}

fn positive_duration(
    name: &str,
    seconds: Option<u64>,
    default: Duration,
) -> Result<Duration, ModelError> {
    match seconds {
        Some(0) => Err(ModelError::Configuration(format!(
            "{name} must be greater than zero"
        ))),
        Some(seconds) => Ok(Duration::from_secs(seconds)),
        None => Ok(default),
    }
}

const fn prompt_cache_mode_name(mode: OpenAiPromptCacheMode) -> &'static str {
    match mode {
        OpenAiPromptCacheMode::Auto => "auto",
        OpenAiPromptCacheMode::Implicit => "implicit",
        OpenAiPromptCacheMode::Explicit => "explicit",
    }
}

const fn protocol_reasoning_effort(value: OpenAiReasoningEffort) -> ReasoningEffort {
    match value {
        OpenAiReasoningEffort::None => ReasoningEffort::None,
        OpenAiReasoningEffort::Low => ReasoningEffort::Low,
        OpenAiReasoningEffort::Medium => ReasoningEffort::Medium,
        OpenAiReasoningEffort::High => ReasoningEffort::High,
        OpenAiReasoningEffort::Xhigh => ReasoningEffort::Xhigh,
        OpenAiReasoningEffort::Max => ReasoningEffort::Max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use papermachine_protocol::ModelRequestMetadata;
    use papermachine_protocol::ModelToolChoice;
    use papermachine_protocol::ModelTransport;
    use papermachine_protocol::PromptCacheMode;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapturingClient {
        models: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ModelClient for CapturingClient {
        async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
            self.models
                .lock()
                .expect("capture lock should not be poisoned")
                .push(request.model.clone());
            Ok(futures::stream::iter([Ok(ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    provider: None,
                    api: None,
                    model_profile: None,
                    upstream_model: None,
                    transport: ModelTransport::HttpSse,
                    prompt_cache_mode: PromptCacheMode::Implicit,
                    prompt_cache_key: None,
                    prompt_cache_breakpoint: false,
                    used_previous_response_id: false,
                    continuation_miss_reason: Some("test".to_string()),
                    websocket_fallback_reason: None,
                },
            })])
            .boxed())
        }
    }

    #[test]
    fn loads_multiple_providers_without_persisting_keys() {
        let source = r#"
default_model = "deepseek-flash"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
responses_websockets = false
prompt_cache_mode = "implicit"
reasoning_effort = "medium"
chat_compatibility = "deepseek"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[models.deepseek-flash]
provider = "deepseek"
api = "open_ai_responses"
model = "deepseek-v4-flash"
context_window = 1000000
capabilities = ["hosted_web_search"]

[models.openai-main]
provider = "openai"
api = "open_ai_responses"
model = "gpt-5.6-sol"
context_window = 1000000
capabilities = []

[models.deepseek-no-search]
provider = "deepseek"
api = "open_ai_chat_completions"
model = "deepseek-v4-pro"
context_window = 1000000
capabilities = []
"#;
        let configured = ConfiguredModels::from_toml_with_key_lookup(source, |name| {
            Some(format!("secret-for-{name}"))
        })
        .expect("provider config should load");

        assert_eq!(configured.default_model, "deepseek-flash");
        assert_eq!(configured.profiles.len(), 3);
        assert_eq!(configured.providers.len(), 2);
        assert_eq!(
            configured
                .profiles
                .iter()
                .find(|profile| profile.id == "deepseek-no-search")
                .map(|profile| profile.api),
            Some(ModelApi::OpenAiChatCompletions)
        );
        assert_eq!(
            configured
                .providers
                .iter()
                .find(|provider| provider.id == "deepseek")
                .map(|provider| provider.apis.as_slice()),
            Some([ModelApi::OpenAiResponses, ModelApi::OpenAiChatCompletions,].as_slice())
        );
        assert!(
            configured
                .profiles
                .iter()
                .find(|profile| profile.id == "deepseek-flash")
                .is_some_and(|profile| profile.supports(ModelCapability::HostedWebSearch))
        );
        assert!(
            configured
                .profiles
                .iter()
                .find(|profile| profile.id == "openai-main")
                .is_some_and(|profile| !profile.supports(ModelCapability::HostedWebSearch))
        );
        assert!(
            configured
                .router
                .supports_hosted_tool("deepseek-flash", HostedTool::WebSearch)
        );
        assert!(
            !configured
                .router
                .supports_hosted_tool("openai-main", HostedTool::WebSearch)
        );
        assert!(
            !configured
                .router
                .supports_hosted_tool("deepseek-no-search", HostedTool::WebSearch)
        );
        assert_eq!(
            configured.router.model_context_window("deepseek-flash"),
            Some(1_000_000)
        );
        let debug = format!("{configured:?}");
        assert!(!debug.contains("secret-for"));
        let route = configured
            .router
            .resolve_route_snapshot("deepseek-flash", None, 128_000)
            .expect("route should resolve");
        assert_eq!(route.provider, "deepseek");
        assert_eq!(route.upstream_model, "deepseek-v4-flash");
        assert_eq!(route.context_window, 1_000_000);
        assert_eq!(route.reasoning_effort, Some(ReasoningEffort::Medium));
        assert!(route.capabilities.hosted_web_search);
        configured
            .router
            .validate_route_snapshot(&route, 128_000)
            .expect("unchanged route should validate");
        let independently_loaded = ConfiguredModels::from_toml_with_key_lookup(source, |name| {
            Some(format!("different-secret-for-{name}"))
        })
        .expect("provider config should reload with a different key");
        let reloaded_route = independently_loaded
            .router
            .resolve_route_snapshot("deepseek-flash", None, 128_000)
            .expect("reloaded route should resolve");
        assert_eq!(route.config_sha256, reloaded_route.config_sha256);
        let mut drifted = route;
        drifted.context_window -= 1;
        assert!(
            configured
                .router
                .validate_route_snapshot(&drifted, 128_000)
                .is_err()
        );
    }

    #[test]
    fn checked_in_config_declares_openai_deepseek_and_glm_without_network_calls() {
        let source = include_str!("../../../papermachine.toml");
        let configured = ConfiguredModels::from_toml_with_key_lookup(source, |name| {
            Some(format!("test-only-{name}"))
        })
        .expect("checked-in model config should load with dummy credentials");

        assert_eq!(configured.default_model, "glm-5-2");
        for profile in [
            "glm-5-2",
            "glm-direct",
            "deepseek-flash",
            "deepseek-pro",
            "openai-main",
        ] {
            assert!(
                configured.profiles.iter().any(|item| item.id == profile),
                "missing model profile {profile}"
            );
        }
        assert_eq!(
            configured
                .profiles
                .iter()
                .find(|profile| profile.id == "deepseek-pro")
                .map(|profile| profile.api),
            Some(ModelApi::OpenAiChatCompletions)
        );
        assert_eq!(
            configured
                .profiles
                .iter()
                .find(|profile| profile.id == "openai-main")
                .map(|profile| profile.api),
            Some(ModelApi::OpenAiResponses)
        );
    }

    #[test]
    fn missing_provider_key_fails_with_the_variable_name() {
        let source = r#"
default_model = "main"
[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
[models.main]
provider = "deepseek"
api = "open_ai_responses"
model = "deepseek-v4-flash"
context_window = 1000000
capabilities = []
"#;
        let error = ConfiguredModels::from_toml_with_key_lookup(source, |_| None)
            .expect_err("missing key should fail");
        assert!(error.to_string().contains("DEEPSEEK_API_KEY"));
    }

    #[test]
    fn unavailable_optional_provider_and_its_profiles_are_skipped() {
        let source = r#"
default_model = "local-main"

[providers.local]
base_url = "https://models.example.test"
api_key_env = "LOCAL_API_KEY"

[providers.optional]
base_url = "https://optional.example.test"
api_key_env = "OPTIONAL_API_KEY"
optional = true

[models.local-main]
provider = "local"
api = "open_ai_responses"
model = "main"
context_window = 100000
capabilities = []

[models.optional-search]
provider = "optional"
api = "open_ai_responses"
model = "search"
context_window = 100000
capabilities = ["hosted_web_search"]
"#;
        let configured = ConfiguredModels::from_toml_with_key_lookup(source, |name| {
            (name == "LOCAL_API_KEY").then(|| "available".to_string())
        })
        .expect("optional provider should not block available profiles");

        assert_eq!(configured.default_model, "local-main");
        assert_eq!(configured.profiles.len(), 1);
        assert_eq!(configured.profiles[0].id, "local-main");
        assert_eq!(configured.providers.len(), 1);
        assert_eq!(configured.providers[0].id, "local");
    }

    #[test]
    fn model_api_is_required() {
        let source = r#"
default_model = "main"
[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
[models.main]
provider = "deepseek"
model = "deepseek-v4-flash"
context_window = 1000000
capabilities = []
"#;
        let error =
            ConfiguredModels::from_toml_with_key_lookup(source, |_| Some("test-key".to_string()))
                .expect_err("model API should be explicit");
        assert!(error.to_string().contains("missing field `api`"));
    }

    #[test]
    fn model_capabilities_are_required() {
        let source = r#"
default_model = "main"
[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
[models.main]
provider = "deepseek"
api = "open_ai_responses"
model = "deepseek-v4-flash"
context_window = 1000000
"#;
        let error =
            ConfiguredModels::from_toml_with_key_lookup(source, |_| Some("test-key".to_string()))
                .expect_err("hosted web search support should be explicit");
        assert!(error.to_string().contains("missing field `capabilities`"));
    }

    #[test]
    fn chat_completions_cannot_claim_hosted_web_search() {
        let source = r#"
default_model = "main"
[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
chat_compatibility = "deepseek"
[models.main]
provider = "deepseek"
api = "open_ai_chat_completions"
model = "deepseek-v4-pro"
context_window = 1000000
capabilities = ["hosted_web_search"]
"#;
        let error =
            ConfiguredModels::from_toml_with_key_lookup(source, |_| Some("test-key".to_string()))
                .expect_err("unsupported hosted search must fail closed");
        assert!(
            error
                .to_string()
                .contains("cannot declare hosted_web_search")
        );
    }

    #[tokio::test]
    async fn router_rewrites_profile_and_annotates_metadata() {
        let capture = Arc::new(CapturingClient::default());
        let mut clients: HashMap<String, Arc<dyn ModelClient>> = HashMap::new();
        clients.insert("fast-research".to_string(), capture.clone());
        let router = ModelRouter::new(
            vec![ModelProfile {
                id: "fast-research".to_string(),
                provider: "deepseek".to_string(),
                api: ModelApi::OpenAiResponses,
                model: "deepseek-v4-flash".to_string(),
                context_window: 1_000_000,
                capabilities: vec![ModelCapability::HostedWebSearch],
                default_reasoning_effort: None,
                config_sha256: String::new(),
            }],
            clients,
        )
        .expect("router should build");
        let request = ModelRequest {
            model: "fast-research".to_string(),
            reasoning_effort: None,
            instructions: "test".to_string(),
            input: Vec::new(),
            prompt_cache: None,
            transport_session_key: None,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: false,
            tool_choice: ModelToolChoice::None,
            response_format: None,
        };

        let events = router
            .stream(request)
            .await
            .expect("route should resolve")
            .try_collect::<Vec<_>>()
            .await
            .expect("stream should complete");

        assert_eq!(
            capture
                .models
                .lock()
                .expect("capture lock should not be poisoned")
                .as_slice(),
            ["deepseek-v4-flash"]
        );
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    provider: Some(provider),
                    api: Some(api),
                    model_profile: Some(profile),
                    upstream_model: Some(model),
                    ..
                }
            }] if provider == "deepseek"
                && api == "open_ai_responses"
                && profile == "fast-research"
                && model == "deepseek-v4-flash"
        ));
    }
}
