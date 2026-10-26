//! Running the service: the gRPC API, the health endpoints and graceful shutdown.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use push_credentials::startup::{redis_permanent, retry};
use push_credentials::{AuthMode, AzureConfig, RedisConnector};
use push_domain::{DomainError, Messaging, SystemClock};
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::directory_server::DirectoryServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_proto::internal::v1::templates_server::TemplatesServer;
use push_store::{Connector, PgStore, grant_runtime_access, is_permanent_startup, tokens_for};
use tokio::task::JoinSet;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;

use crate::Listeners;
use crate::config::{Config, MigrateConfig};
use crate::grpc::{AccountsApi, DirectoryApi, InternalToken, MessagingApi};
use crate::notify::RedisNotifier;
use crate::templates::{TemplatesApi, seed_builtins, seeding_is_permanent};

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Runs the service until `shutdown` is cancelled, then finishes the calls in progress
/// and closes the database pool. All `listeners` must be bound already.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping, Redis has connected and the built-in templates
/// are seeded, and never after shutdown began. Postgres, Redis and seeding are retried
/// until they succeed, so the service may start before its datastores.
pub async fn serve(
    config: Config,
    listeners: Listeners,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let Listeners {
        messaging: grpc,
        accounts,
        directory,
        health,
    } = listeners;
    let ready = Health {
        store: Arc::new(OnceLock::new()),
        shutdown: shutdown.clone(),
    };
    let health_server = tokio::spawn(
        axum::serve(health, health_router(ready.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
    );

    if config.accounts_token.is_none() {
        tracing::warn!(
            "PUSH_ACCOUNTS_TOKEN is not set: the Accounts API refuses every call, so the \
             worker cannot relay users and devices"
        );
    }
    if config.messaging_token.is_none() {
        tracing::warn!(
            "PUSH_MESSAGING_TOKEN is not set: the Messaging API refuses every call, so the \
             gateway cannot serve clients"
        );
    }
    if config.directory_token.is_none() {
        tracing::warn!(
            "PUSH_DIRECTORY_TOKEN is not set: the Directory API refuses every call, so \
             auth-svc cannot show profiles"
        );
    }
    push_credentials::install_crypto_provider();
    let azure = config
        .azure
        .as_ref()
        .map(AzureConfig::source)
        .transpose()
        .context("the Azure identity")?;
    let pg_tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let connector = Connector::for_target(&config.database, pg_tokens.as_ref())?;
    let redis = RedisConnector::open(&config.redis, azure.as_ref(), shutdown.clone())
        .await
        .context("Redis credentials")?;
    // Per datastore: a refused token or certificate is configuration in Entra mode only,
    // so a password-mode datastore keeps retrying exactly as before.
    let pg_entra = config.database.mode() == AuthMode::AzureEntra;
    let postgres_fatal = move |error: &anyhow::Error| pg_entra && is_permanent_startup(error);
    let redis_entra = config.redis.mode() == AuthMode::AzureEntra;
    let redis_fatal = move |error: &anyhow::Error| redis_entra && redis_permanent(error);
    let Some(store) = retry(
        "postgres",
        RETRY_DELAY,
        &shutdown,
        &postgres_fatal,
        || async {
            // Services migrate on a connection of their own, not on the pool: a migration must
            // not inherit the pool's request-sized statement and lock timeouts.
            if config.run_migrations {
                connector.migrate(&push_store::MIGRATOR, &[]).await?;
            }
            anyhow::Ok(PgStore::new(connector.pool(&config.store).await?))
        },
    )
    .await?
    else {
        return finish(health_server).await;
    };
    let Some(notifier) = retry("redis", RETRY_DELAY, &shutdown, &redis_fatal, || async {
        anyhow::Ok(RedisNotifier::with_connector(&redis).await?)
    })
    .await?
    else {
        store.close().await;
        return finish(health_server).await;
    };
    let messaging = Arc::new(Messaging::new(store.clone(), SystemClock, notifier));
    // Before readiness and serving, so clients find the built-ins in the first catalog.
    let seeding_fatal = |error: &anyhow::Error| {
        error
            .downcast_ref::<DomainError>()
            .is_some_and(seeding_is_permanent)
    };
    let seeded = retry("templates", RETRY_DELAY, &shutdown, &seeding_fatal, || {
        let messaging = Arc::clone(&messaging);
        async move { anyhow::Ok(seed_builtins(&messaging).await?) }
    })
    .await;
    match seeded {
        Ok(Some(_)) => {}
        Ok(None) => {
            store.close().await;
            return finish(health_server).await;
        }
        Err(error) => {
            store.close().await;
            return Err(error);
        }
    }
    let _ = ready.store.set(store.clone());

    // Every socket is already listening, so connections wait in the backlog until
    // the corresponding tonic server starts accepting.
    let mut servers = JoinSet::new();
    tracing::info!(addr = %config.grpc_addr, "serving Messaging gRPC");
    servers.spawn(
        tonic::transport::Server::builder()
            .add_service(MessagingServer::with_interceptor(
                MessagingApi::new(Arc::clone(&messaging)),
                InternalToken::messaging(config.messaging_token),
            ))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(grpc),
                shutdown.clone().cancelled_owned(),
            ),
    );
    tracing::info!(addr = %config.accounts_grpc_addr, "serving Accounts gRPC");
    servers.spawn(
        tonic::transport::Server::builder()
            .add_service(AccountsServer::with_interceptor(
                AccountsApi::new(Arc::clone(&messaging)),
                InternalToken::accounts(config.accounts_token),
            ))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(accounts),
                shutdown.clone().cancelled_owned(),
            ),
    );
    tracing::info!(addr = %config.directory_grpc_addr, "serving Directory and Templates gRPC");
    let directory_token = InternalToken::directory(config.directory_token);
    servers.spawn(
        tonic::transport::Server::builder()
            .add_service(DirectoryServer::with_interceptor(
                DirectoryApi::new(Arc::clone(&messaging)),
                directory_token.clone(),
            ))
            .add_service(TemplatesServer::with_interceptor(
                TemplatesApi::new(messaging),
                directory_token,
            ))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(directory),
                shutdown.clone().cancelled_owned(),
            ),
    );
    let result = supervise(servers, &shutdown).await;
    store.close().await;
    let health_result = finish(health_server).await;
    tracing::info!("stopped");
    if let Err(error) = &health_result {
        tracing::error!(error = %format!("{error:#}"), "health server failed");
    }
    result?;
    health_result
}

/// Cancels and drains every API before the caller releases shared resources.
async fn supervise<E>(
    mut servers: JoinSet<Result<(), E>>,
    shutdown: &CancellationToken,
) -> anyhow::Result<()>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let first = servers.join_next().await;
    let coordinated = shutdown.is_cancelled();
    shutdown.cancel();
    let mut result = match first {
        Some(outcome) => outcome
            .context("gRPC server task failed")
            .and_then(|outcome| outcome.context("gRPC server failed")),
        None => anyhow::bail!("no gRPC servers to supervise"),
    };
    if result.is_ok() && !coordinated {
        result = Err(anyhow::anyhow!("gRPC server stopped before shutdown"));
    }
    while let Some(outcome) = servers.join_next().await {
        let sibling = outcome
            .context("gRPC server task failed")
            .and_then(|outcome| outcome.context("gRPC server failed"));
        if let Err(error) = sibling {
            tracing::error!(error = %format!("{error:#}"), "gRPC sibling failed during shutdown");
            if result.is_ok() {
                result = Err(error);
            }
        }
    }
    result
}

/// `push-message-svc migrate`: applies the migrations of `push_messaging` and, with
/// `PUSH_DATABASE_RUNTIME_ROLE`, grants that role the use of the `messaging` schema, then
/// returns. For a migration job whose identity owns the database while the service runs
/// as another (`PUSH_RUN_MIGRATIONS=false`).
pub async fn migrate(config: MigrateConfig) -> anyhow::Result<()> {
    push_credentials::install_crypto_provider();
    let shutdown = CancellationToken::new();
    let azure = config
        .azure
        .as_ref()
        .map(AzureConfig::source)
        .transpose()
        .context("the Azure identity")?;
    let tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let connector = Connector::for_target(&config.database, tokens.as_ref())?;
    connector.migrate(&push_store::MIGRATOR, &[]).await?;
    if let Some(role) = &config.runtime_role {
        let pool = connector.single().await?;
        let granted = grant_runtime_access(&pool, "messaging", role).await;
        pool.close().await;
        granted?;
    }
    shutdown.cancel();
    tracing::info!("push_messaging is migrated");
    Ok(())
}

/// Waits for the health server to stop.
async fn finish(health_server: tokio::task::JoinHandle<std::io::Result<()>>) -> anyhow::Result<()> {
    health_server.await??;
    Ok(())
}

#[derive(Clone)]
struct Health {
    store: Arc<OnceLock<PgStore>>,
    shutdown: CancellationToken,
}

fn health_router(health: Health) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .with_state(health)
}

async fn readyz(State(health): State<Health>) -> (StatusCode, &'static str) {
    if health.shutdown.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    }
    match health.store.get() {
        None => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
        Some(store) => match store.ping().await {
            Ok(()) if !health.shutdown.is_cancelled() => (StatusCode::OK, "ready"),
            Ok(()) => (StatusCode::SERVICE_UNAVAILABLE, "shutting down"),
            Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unreachable"),
        },
    }
}

/// Structured JSON logs on stdout. The level comes from `RUST_LOG` (default `info`).
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .init();
}

/// Resolves on Ctrl-C, or on SIGTERM where there is one (Kubernetes sends it).
pub async fn terminated() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn an_unexpected_api_exit_cancels_and_drains_all_siblings() {
        for failure in ["success", "error", "panic"] {
            let shutdown = CancellationToken::new();
            let drained = Arc::new(AtomicUsize::new(0));
            let mut servers = JoinSet::new();
            servers.spawn(async move {
                match failure {
                    "success" => Ok(()),
                    "error" => Err(std::io::Error::other("API transport failure")),
                    _ => panic!("API task panic"),
                }
            });
            for _ in 0..2 {
                let shutdown = shutdown.clone();
                let drained = Arc::clone(&drained);
                servers.spawn(async move {
                    shutdown.cancelled().await;
                    tokio::task::yield_now().await;
                    drained.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
            }

            let error = tokio::time::timeout(Duration::from_secs(1), supervise(servers, &shutdown))
                .await
                .expect("supervision cancels siblings promptly")
                .unwrap_err();
            let expected = match failure {
                "success" => "stopped before shutdown",
                "error" => "API transport failure",
                _ => "API task panic",
            };
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert!(shutdown.is_cancelled());
            assert_eq!(drained.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn coordinated_shutdown_preserves_sibling_errors() {
        for fails in [false, true] {
            let shutdown = CancellationToken::new();
            let mut servers = JoinSet::new();
            servers.spawn(async { Ok::<(), std::io::Error>(()) });
            servers.spawn(async move {
                tokio::task::yield_now().await;
                if fails {
                    Err(std::io::Error::other("failure while draining"))
                } else {
                    Ok(())
                }
            });
            shutdown.cancel();
            let result = supervise(servers, &shutdown).await;
            if fails {
                let error = result.unwrap_err();
                assert!(format!("{error:#}").contains("failure while draining"));
            } else {
                result.unwrap();
            }
        }
    }
}
