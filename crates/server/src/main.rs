use anyhow::Context;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::HOST;
use axum::http::uri::Authority;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use clap::Parser;
use papermachine_model::ConfiguredModels;
use papermachine_server::ServerConfig;
use papermachine_server::ServerModelConfig;
use papermachine_server::paths::default_data_dir;
use papermachine_server::paths::default_workspace_root;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;
use tracing::warn;
use tracing_subscriber::EnvFilter;

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(name = "papermachine-server")]
#[command(about = "Local-first auto-research server")]
struct Args {
    /// Read-only PaperMachine resources: web assets and built-in Workflows.
    #[arg(long, env = "PAPERMACHINE_RESOURCE_ROOT")]
    resource_root: PathBuf,
    /// Durable PaperMachine application data. Uses the platform user-data directory by default.
    #[arg(long, env = "PAPERMACHINE_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Use isolated development defaults under the platform PaperMachine data directory.
    #[arg(long)]
    dev: bool,
    #[arg(long, env = "PAPERMACHINE_PORT", default_value_t = 4310)]
    port: u16,
    /// PaperMachine-owned provider and model-profile configuration. Defaults
    /// to <resource-root>/papermachine.toml in development, otherwise
    /// <data-dir>/config.toml, outside explicit demo mode.
    #[arg(long, env = "PAPERMACHINE_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, env = "PAPERMACHINE_DEMO")]
    demo: bool,
    #[arg(long, default_value_t = 4)]
    max_concurrent_runs: usize,
    #[arg(long, default_value_t = 4)]
    max_parallel_actions: usize,
    /// Debug-build-only durability boundary used by process recovery tests.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true, requires = "process_fault_marker")]
    process_fault_boundary: Option<String>,
    /// Marker created when the armed debug durability boundary is reached.
    #[cfg(debug_assertions)]
    #[arg(long, hide = true, requires = "process_fault_boundary")]
    process_fault_marker: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    papermachine_execution::run_windows_sandbox_wrapper_if_requested();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create the PaperMachine runtime")?
        .block_on(run_server())
}

async fn run_server() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    #[cfg(debug_assertions)]
    if let (Some(boundary), Some(marker)) = (
        args.process_fault_boundary.as_ref(),
        args.process_fault_marker.as_ref(),
    ) {
        papermachine_store::process_fault::install_process_fault_injection(
            boundary.clone(),
            marker.clone(),
        )
        .map_err(anyhow::Error::msg)?;
    }
    let resource_root = args.resource_root.canonicalize().with_context(|| {
        format!(
            "PaperMachine resource root does not exist: {}",
            args.resource_root.display()
        )
    })?;
    let requested_data_dir = match args.data_dir {
        Some(path) => path,
        None if args.dev => default_data_dir()?.join("dev"),
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
        let config_path = args.config.unwrap_or_else(|| {
            if args.dev {
                resource_root.join("papermachine.toml")
            } else {
                data_dir.join("config.toml")
            }
        });
        let configured = ConfiguredModels::from_file(&config_path)
            .context("failed to load PaperMachine provider configuration")?;
        ServerModelConfig::Providers(configured)
    };
    let config = ServerConfig {
        resource_root: resource_root.clone(),
        data_dir: data_dir.clone(),
        default_workspace_root: default_workspace_root()?,
        models,
        max_concurrent_runs: args.max_concurrent_runs,
        max_parallel_actions: args.max_parallel_actions,
    };
    let state = papermachine_server::initialize(&config).await?;
    let mode = state.mode();
    let shutdown_started = state.shutdown_token();
    let app = papermachine_server::router(state, resource_root.join("apps/web/dist"))
        .layer(axum::middleware::from_fn(require_loopback_host));
    let address = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("failed to bind PaperMachine server")?;
    info!(%address, %mode, resource_root = %resource_root.display(), data_dir = %data_dir.display(), "PaperMachine server listening");
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

async fn require_loopback_host(request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_loopback_authority);
    if !allowed {
        return (
            StatusCode::FORBIDDEN,
            "PaperMachine accepts only loopback Host headers",
        )
            .into_response();
    }
    next.run(request).await
}

fn is_loopback_authority(value: &str) -> bool {
    value.parse::<Authority>().is_ok_and(|authority| {
        let host = authority.host();
        let address = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || address
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::is_loopback_authority;

    #[test]
    fn local_host_boundary_accepts_only_loopback_authorities() {
        assert!(is_loopback_authority("127.0.0.1:4310"));
        assert!(is_loopback_authority("localhost:4310"));
        assert!(is_loopback_authority("[::1]:4310"));
        assert!(!is_loopback_authority("example.com:4310"));
        assert!(!is_loopback_authority("0.0.0.0:4310"));
    }
}
