use push_message_svc::{Config, Listeners, init_logging, serve, terminated};
use tokio_util::sync::CancellationToken;

// `serve` migrates at start-up unless PUSH_RUN_MIGRATIONS=false; `push-message-svc migrate`
// only migrates (and grants a runtime role), for a separate migration job.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return push_message_svc::migrate(push_message_svc::MigrateConfig::from_env()?).await;
    }
    let config = Config::from_env()?;
    let listeners = Listeners::bind(&config).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve(config, listeners, shutdown).await
}
