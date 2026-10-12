use push_message_svc::{Config, init_logging, serve, terminated};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// `serve` runs the migrations itself, with `push_store::migrate_database(url)` on a
// dedicated connection at start-up (not `PgStore::migrate` on the request pool).
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let config = Config::from_env()?;
    let grpc = TcpListener::bind(config.grpc_addr).await?;
    let health = TcpListener::bind(config.health_addr).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve(config, grpc, health, shutdown).await
}
