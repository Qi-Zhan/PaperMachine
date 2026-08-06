use anyhow::Context;
use clap::Parser;
use papermachine_model::ConfiguredModels;
use papermachine_model::DEFAULT_MODEL_CONTEXT_WINDOW;
use papermachine_model::OpenAiResponsesConfig;
use papermachine_server::ServerConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(name = "papermachine-server")]
#[command(about = "Local-first auto-research server")]
struct Args {
    #[arg(long, env = "PAPERMACHINE_ROOT", default_value = ".")]
    root: PathBuf,
    #[arg(long, env = "PAPERMACHINE_HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long, env = "PAPERMACHINE_PORT", default_value_t = 4310)]
    port: u16,
    #[arg(long, env = "PAPERMACHINE_MODEL")]
    model: Option<String>,
    #[arg(long, env = "PAPERMACHINE_CODEX_HOME")]
    codex_home: Option<PathBuf>,
    /// PaperMachine-owned provider and model-profile configuration. When
    /// omitted, <root>/papermachine.toml is loaded if it exists.
    #[arg(long, env = "PAPERMACHINE_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, env = "PAPERMACHINE_MODEL_CONTEXT_WINDOW")]
    model_context_window: Option<usize>,
    #[arg(long, env = "PAPERMACHINE_DEMO")]
    demo: bool,
    #[arg(long, default_value_t = 4)]
    max_concurrent_runs: usize,
    #[arg(long, default_value_t = 4)]
    max_parallel_actions: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("workspace root does not exist: {}", args.root.display()))?;
    let config_path = args.config.clone().or_else(|| {
        root.join("papermachine.toml")
            .is_file()
            .then(|| root.join("papermachine.toml"))
    });
    let configured_models = if args.demo {
        None
    } else {
        config_path
            .as_deref()
            .map(ConfiguredModels::from_file)
            .transpose()
            .context("failed to load PaperMachine provider configuration")?
    };
    let codex_settings = if args.demo || configured_models.is_some() {
        None
    } else {
        args.codex_home
            .as_deref()
            .map(OpenAiResponsesConfig::from_codex_home)
            .transpose()
            .context("failed to load Codex OpenAI settings")?
    };
    let default_model = args
        .model
        .or_else(|| {
            configured_models
                .as_ref()
                .map(|settings| settings.default_model.clone())
        })
        .or_else(|| {
            codex_settings
                .as_ref()
                .map(|settings| settings.model.clone())
        })
        .unwrap_or_else(|| "gpt-5.2".to_string());
    let model_context_window = args
        .model_context_window
        .or_else(|| {
            configured_models.as_ref().and_then(|settings| {
                settings
                    .profiles
                    .iter()
                    .find(|profile| profile.id == default_model)
                    .map(|profile| profile.context_window)
            })
        })
        .or_else(|| {
            codex_settings
                .as_ref()
                .map(|settings| settings.model_context_window)
        })
        .unwrap_or(DEFAULT_MODEL_CONTEXT_WINDOW);
    let demo = args.demo
        || (configured_models.is_none()
            && codex_settings.is_none()
            && std::env::var_os("OPENAI_API_KEY").is_none());
    let config = ServerConfig {
        root: root.clone(),
        default_model,
        demo,
        configured_models,
        openai_config: codex_settings.map(|settings| settings.client),
        model_context_window,
        max_concurrent_runs: args.max_concurrent_runs,
        max_parallel_actions: args.max_parallel_actions,
    };
    let state = papermachine_server::initialize(&config).await?;
    let mode = state.mode();
    let app = papermachine_server::router(state, root.join("apps/web/dist"));
    let address = format!("{}:{}", args.host, args.port)
        .parse::<SocketAddr>()
        .context("invalid server address")?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("failed to bind PaperMachine server")?;
    info!(%address, %mode, "PaperMachine server listening");
    let shutdown_started = CancellationToken::new();
    let shutdown_notice = shutdown_started.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown_notice.cancel();
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("PaperMachine server failed"),
        _ = shutdown_started.cancelled() => {
            match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, &mut server).await {
                Ok(result) => result.context("PaperMachine server failed"),
                Err(_) => {
                    warn!(
                        timeout_seconds = GRACEFUL_SHUTDOWN_TIMEOUT.as_secs(),
                        "forcing server shutdown with active connections"
                    );
                    Ok(())
                }
            }
        }
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
