use push_auth_core::{Config, init_logging, serve, terminated};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// `serve` migrates and grants at start-up unless PUSH_RUN_MIGRATIONS=false; `push-auth migrate`
// only migrates and grants, for a separate migration job.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return push_auth_core::migrate(push_auth_core::MigrateConfig::from_env()?).await;
    }
    let config = Config::from_env()?;
    let http = TcpListener::bind(config.http_addr).await?;
    let health = TcpListener::bind(config.health_addr).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve(config, http, health, shutdown).await
}
