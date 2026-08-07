use anyhow::Context;
use clap::Parser;
use papermachine_model::ConfiguredModels;
use papermachine_server::ServerConfig;
use papermachine_server::ServerModelConfig;
use papermachine_server::paths::default_data_dir;
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
    /// Read-only PaperMachine resources: web assets, Python runtime, and built-in Workflows.
    #[arg(long, env = "PAPERMACHINE_RESOURCE_ROOT")]
    resource_root: PathBuf,
    /// Durable PaperMachine application data. Uses the platform user-data directory by default.
    #[arg(long, env = "PAPERMACHINE_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "PAPERMACHINE_HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long, env = "PAPERMACHINE_PORT", default_value_t = 4310)]
    port: u16,
    /// PaperMachine-owned provider and model-profile configuration. Defaults
    /// to <data-dir>/config.toml outside explicit demo mode.
    #[arg(long, env = "PAPERMACHINE_CONFIG")]
    config: Option<PathBuf>,
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
    let resource_root = args.resource_root.canonicalize().with_context(|| {
        format!(
            "PaperMachine resource root does not exist: {}",
            args.resource_root.display()
        )
    })?;
    let requested_data_dir = match args.data_dir {
        Some(path) => path,
        None => default_data_dir()?,
    };
    std::fs::create_dir_all(&requested_data_dir).with_context(|| {
        format!(
            "failed to create PaperMachine data directory: {}",
            requested_data_dir.display()
        )
    })?;
    let data_dir = requested_data_dir.canonicalize().with_context(|| {
        format!(
            "failed to resolve PaperMachine data directory: {}",
            requested_data_dir.display()
        )
    })?;
    let models = if args.demo {
        ServerModelConfig::Demo
    } else {
        let config_path = args.config.unwrap_or_else(|| data_dir.join("config.toml"));
        let configured = ConfiguredModels::from_file(&config_path)
            .context("failed to load PaperMachine provider configuration")?;
        ServerModelConfig::Providers(configured)
    };
    let config = ServerConfig {
        resource_root: resource_root.clone(),
        data_dir: data_dir.clone(),
        models,
        max_concurrent_runs: args.max_concurrent_runs,
        max_parallel_actions: args.max_parallel_actions,
    };
    let state = papermachine_server::initialize(&config).await?;
    let mode = state.mode();
    let app = papermachine_server::router(state, resource_root.join("apps/web/dist"));
    let address = format!("{}:{}", args.host, args.port)
        .parse::<SocketAddr>()
        .context("invalid server address")?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("failed to bind PaperMachine server")?;
    info!(%address, %mode, resource_root = %resource_root.display(), data_dir = %data_dir.display(), "PaperMachine server listening");
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
