use crate::DEFAULT_MODEL_REQUEST_TIMEOUT;
use crate::DEFAULT_MODEL_STREAM_IDLE_TIMEOUT;
use crate::ModelClient;
use crate::ModelError;
use crate::ModelStream;
use crate::OpenAiPromptCacheMode;
use crate::OpenAiReasoningEffort;
use crate::OpenAiResponsesClient;
use crate::OpenAiResponsesConfig;
use async_trait::async_trait;
use futures::StreamExt;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelRequest;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelProfile {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub context_window: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelProviderInfo {
    pub id: String,
    pub kind: String,
    pub endpoint: String,
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

        let mut clients: HashMap<String, Arc<dyn ModelClient>> = HashMap::new();
        let mut providers = Vec::with_capacity(file.providers.len());
        for (provider_id, provider) in file.providers {
            validate_identifier("provider", &provider_id)?;
            if provider.kind != ProviderKind::OpenAiResponses {
                return Err(ModelError::Configuration(format!(
                    "provider {provider_id:?} has an unsupported kind"
                )));
            }
            if provider.api_key_env.trim().is_empty() {
                return Err(ModelError::Configuration(format!(
                    "provider {provider_id:?} must set api_key_env"
                )));
            }
            let api_key = key_lookup(provider.api_key_env.trim())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    ModelError::Configuration(format!(
                        "provider {provider_id:?} requires non-empty environment variable {}",
                        provider.api_key_env
                    ))
                })?;
            let endpoint = super::openai::responses_endpoint(&provider.base_url)?;
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
            let client = OpenAiResponsesClient::new(OpenAiResponsesConfig {
                provider_id: provider_id.clone(),
                endpoint: endpoint.clone(),
                api_key,
                organization: provider.organization,
                project: provider.project,
                max_request_retries: provider.max_request_retries.unwrap_or(2),
                request_timeout,
                stream_idle_timeout,
                reasoning_effort: provider.reasoning_effort,
                store_responses: provider.store_responses.unwrap_or(false),
                responses_websockets,
                prompt_cache_mode,
            })?;
            providers.push(ModelProviderInfo {
                id: provider_id.clone(),
                kind: "openai_responses".to_string(),
                endpoint: endpoint.to_string(),
                max_request_retries: provider.max_request_retries.unwrap_or(2),
                request_timeout_seconds: request_timeout.as_secs(),
                stream_idle_timeout_seconds: stream_idle_timeout.as_secs(),
                responses_websockets,
                prompt_cache_mode: prompt_cache_mode_name(prompt_cache_mode).to_string(),
            });
            clients.insert(provider_id, Arc::new(client));
        }

        let mut profiles = Vec::with_capacity(file.models.len());
        for (profile_id, profile) in file.models {
            validate_identifier("model profile", &profile_id)?;
            if !clients.contains_key(&profile.provider) {
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
            profiles.push(ModelProfile {
                id: profile_id,
                provider: profile.provider,
                model: profile.model,
                context_window: profile.context_window,
            });
        }
        profiles.sort_by(|left, right| left.id.cmp(&right.id));
        providers.sort_by(|left, right| left.id.cmp(&right.id));
        if !profiles
            .iter()
            .any(|profile| profile.id == file.default_model)
        {
            return Err(ModelError::Configuration(format!(
                "default_model {:?} is not defined in [models]",
                file.default_model
            )));
        }
        let router = ModelRouter::new(profiles.clone(), clients)?;
        Ok(Self {
            default_model: file.default_model,
            profiles,
            providers,
            router,
        })
    }
}

#[derive(Clone)]
pub struct ModelRouter {
    routes: Arc<HashMap<String, ModelRoute>>,
    providers: Arc<HashMap<String, Arc<dyn ModelClient>>>,
}

impl fmt::Debug for ModelRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRouter")
            .field("profiles", &self.routes.keys().collect::<Vec<_>>())
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
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
        providers: HashMap<String, Arc<dyn ModelClient>>,
    ) -> Result<Self, ModelError> {
        let mut routes = HashMap::with_capacity(profiles.len());
        for profile in profiles {
            let client = providers.get(&profile.provider).cloned().ok_or_else(|| {
                ModelError::Configuration(format!(
                    "model profile {:?} references unknown provider {:?}",
                    profile.id, profile.provider
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
        Ok(Self {
            routes: Arc::new(routes),
            providers: Arc::new(providers),
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
        let upstream_model = route.profile.model.clone();
        let stream = route.client.stream(request).await?;
        Ok(stream
            .map(move |event| {
                event.map(|event| match event {
                    ModelEvent::RequestMetadata { mut metadata } => {
                        metadata.provider = Some(provider_id.clone());
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
        for client in self.providers.values() {
            client.close_transport_session(session_key).await;
        }
    }

    fn model_context_window(&self, model: &str) -> Option<usize> {
        self.routes
            .get(model)
            .map(|route| route.profile.context_window)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelConfigFile {
    default_model: String,
    providers: HashMap<String, ProviderConfigFile>,
    models: HashMap<String, ModelProfileFile>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ProviderKind {
    OpenAiResponses,
}

fn default_provider_kind() -> ProviderKind {
    ProviderKind::OpenAiResponses
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfigFile {
    #[serde(default = "default_provider_kind")]
    kind: ProviderKind,
    base_url: String,
    api_key_env: String,
    organization: Option<String>,
    project: Option<String>,
    max_request_retries: Option<u32>,
    request_timeout_seconds: Option<u64>,
    stream_idle_timeout_seconds: Option<u64>,
    reasoning_effort: Option<OpenAiReasoningEffort>,
    store_responses: Option<bool>,
    responses_websockets: Option<bool>,
    prompt_cache_mode: Option<OpenAiPromptCacheMode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelProfileFile {
    provider: String,
    model: String,
    context_window: usize,
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use papermachine_protocol::MaxToolCallsMode;
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
                    model_profile: None,
                    upstream_model: None,
                    transport: ModelTransport::HttpSse,
                    prompt_cache_mode: PromptCacheMode::Implicit,
                    prompt_cache_key: None,
                    prompt_cache_breakpoint: false,
                    max_tool_calls_mode: MaxToolCallsMode::NotRequested,
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
kind = "open_ai_responses"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"
responses_websockets = false
prompt_cache_mode = "implicit"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[models.deepseek-flash]
provider = "deepseek"
model = "deepseek-v4-flash"
context_window = 1000000

[models.openai-main]
provider = "openai"
model = "gpt-5.6-sol"
context_window = 1000000
"#;
        let configured = ConfiguredModels::from_toml_with_key_lookup(source, |name| {
            Some(format!("secret-for-{name}"))
        })
        .expect("provider config should load");

        assert_eq!(configured.default_model, "deepseek-flash");
        assert_eq!(configured.profiles.len(), 2);
        assert_eq!(configured.providers.len(), 2);
        assert_eq!(
            configured.router.model_context_window("deepseek-flash"),
            Some(1_000_000)
        );
        let debug = format!("{configured:?}");
        assert!(!debug.contains("secret-for"));
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
model = "deepseek-v4-flash"
context_window = 1000000
"#;
        let error = ConfiguredModels::from_toml_with_key_lookup(source, |_| None)
            .expect_err("missing key should fail");
        assert!(error.to_string().contains("DEEPSEEK_API_KEY"));
    }

    #[tokio::test]
    async fn router_rewrites_profile_and_annotates_metadata() {
        let capture = Arc::new(CapturingClient::default());
        let mut providers: HashMap<String, Arc<dyn ModelClient>> = HashMap::new();
        providers.insert("deepseek".to_string(), capture.clone());
        let router = ModelRouter::new(
            vec![ModelProfile {
                id: "fast-research".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-v4-flash".to_string(),
                context_window: 1_000_000,
            }],
            providers,
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
            max_tool_calls: None,
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
                    model_profile: Some(profile),
                    upstream_model: Some(model),
                    ..
                }
            }] if provider == "deepseek" && profile == "fast-research" && model == "deepseek-v4-flash"
        ));
    }
}
