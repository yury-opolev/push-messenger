//! Running the service: the gRPC API, the health endpoints and graceful shutdown.

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use push_domain::{Messaging, SystemClock};
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::directory_server::DirectoryServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_store::{PgStore, migrate_database};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::grpc::{AccountsApi, DirectoryApi, InternalToken, MessagingApi};
use crate::notify::RedisNotifier;

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Runs the service until `shutdown` is cancelled, then finishes the calls in progress
/// and closes the database pool. `grpc` and `health` must be bound already.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping and Redis has connected, and never after
/// shutdown began. Postgres and Redis are retried until they answer, so the service may
/// start before them.
pub async fn serve(
    config: Config,
    grpc: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
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
    let Some(store) = retry("postgres", &shutdown, || async {
        // Services migrate with `push_store::migrate_database(url)` on a dedicated
        // connection, not with `PgStore::migrate` on the pool: a migration must not
        // inherit the pool's request-sized statement and lock timeouts.
        migrate_database(&config.database_url).await?;
        let store = PgStore::connect(&config.database_url, &config.store).await?;
        anyhow::Ok(store)
    })
    .await
    else {
        return finish(health_server).await;
    };
    let Some(notifier) = retry("redis", &shutdown, || {
        RedisNotifier::connect(&config.redis_url)
    })
    .await
    else {
        store.close().await;
        return finish(health_server).await;
    };
    let _ = ready.store.set(store.clone());

    // Ready before tonic's serve_with_incoming_shutdown runs: `grpc` is already bound and
    // listening (the caller binds it, as `main` does), so a client that connects as soon as
    // /readyz answers waits in the listen backlog until the server accepts, a moment later.
    let messaging = Arc::new(Messaging::new(store.clone(), SystemClock, notifier));

    tracing::info!(addr = %config.grpc_addr, "serving gRPC");
    let result = tonic::transport::Server::builder()
        .add_service(MessagingServer::with_interceptor(
            MessagingApi::new(Arc::clone(&messaging)),
            InternalToken::messaging(config.messaging_token.clone()),
        ))
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(Arc::clone(&messaging)),
            InternalToken::accounts(config.accounts_token.clone()),
        ))
        .add_service(DirectoryServer::with_interceptor(
            DirectoryApi::new(messaging),
            InternalToken::directory(config.directory_token.clone()),
        ))
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(grpc),
            shutdown.clone().cancelled_owned(),
        )
        .await;
    store.close().await;
    tracing::info!("stopped");
    result?;
    finish(health_server).await
}

/// Waits for the health server to stop.
async fn finish(health_server: tokio::task::JoinHandle<std::io::Result<()>>) -> anyhow::Result<()> {
    health_server.await??;
    Ok(())
}

/// Calls `connect` until it succeeds, logging each failure. `None` if `shutdown` came
/// first.
async fn retry<T, E, F, Fut>(what: &str, shutdown: &CancellationToken, connect: F) -> Option<T>
where
    E: std::fmt::Display,
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    loop {
        let attempt = tokio::select! {
            () = shutdown.cancelled() => return None,
            attempt = connect() => attempt,
        };
        match attempt {
            Ok(value) => {
                tracing::info!(dependency = what, "connected");
                return Some(value);
            }
            Err(error) => {
                tracing::warn!(dependency = what, %error, "not reachable yet; retrying");
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = tokio::time::sleep(RETRY_DELAY) => {}
        }
    }
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
            Ok(()) => (StatusCode::OK, "ready"),
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
