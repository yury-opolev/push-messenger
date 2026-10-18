//! Running the service: the HTTP API, the health endpoints and graceful shutdown.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use push_credentials::startup::{redis_permanent, retry};
use push_credentials::{AuthMode, AzureConfig, RedisConnector};
use push_store::{Connector, grant_runtime_access, is_permanent_startup, tokens_for};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::admin::{AdminApi, admin_router};
use crate::api::{Api, router};
use crate::config::{Config, MigrateConfig};
use crate::db::{MIGRATOR, grant_worker_access_on};
use crate::directory::{Directory, GrpcDirectory, NoDirectory};
use crate::entra::EntraSignIn;
use crate::limits::{Limits, RedisCounter};
use crate::links::LinkSigner;
use crate::password::Passwords;
use crate::service::{Auth, Settings, SignIn};
use crate::tokens::AccessTokens;

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// Longest wait for a password-hash slot before a request is refused as `unavailable`.
const HASH_WAIT: Duration = Duration::from_secs(2);

/// Runs the service until `shutdown` is cancelled, then finishes the requests in
/// progress and closes the database pool.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping, and never after shutdown began. Postgres and
/// Redis are retried until they answer, so the service may start before them.
pub async fn serve(
    config: Config,
    http: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let tokens = AccessTokens::from_seed(&config.signing_key, config.token_audience.clone())
        .map_err(anyhow::Error::msg)?;
    let links = LinkSigner::new(config.link_key.clone()).map_err(anyhow::Error::msg)?;
    let directory: Arc<dyn Directory> = match (&config.message_svc_url, &config.directory_token) {
        (Some(url), Some(token)) => {
            Arc::new(GrpcDirectory::new(url, token).map_err(anyhow::Error::msg)?)
        }
        _ => {
            tracing::warn!(
                "PUSH_MESSAGE_SVC_URL is not set: GET /v1/users answers 503, since only \
                 message-svc knows whose profiles a user may see"
            );
            Arc::new(NoDirectory)
        }
    };
    let ready = Health {
        pool: Arc::new(OnceLock::new()),
        shutdown: shutdown.clone(),
    };
    let health_server = tokio::spawn(
        axum::serve(health, health_router(ready.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
    );

    push_credentials::install_crypto_provider();
    // Built before the datastores, so a bad Entra ID setting stops start-up at once.
    let entra = match &config.entra {
        Some(settings) => Some(EntraSignIn::new(settings.clone()).map_err(anyhow::Error::msg)?),
        None => None,
    };
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
    let Some(pool) = retry(
        "postgres",
        RETRY_DELAY,
        &shutdown,
        &postgres_fatal,
        || async {
            if config.run_migrations {
                // Migrations run on a dedicated connection, not on the request pool, whose
                // statement and lock timeouts are sized for requests.
                connector.migrate(&MIGRATOR, &["citext"]).await?;
                if let Some(role) = &config.worker_role {
                    let owner = connector.single().await?;
                    let granted = grant_worker_access_on(&owner, role).await;
                    owner.close().await;
                    granted?;
                }
            }
            anyhow::Ok(connector.pool(&config.store).await?)
        },
    )
    .await?
    else {
        return finish(health_server).await;
    };
    let Some(counter) = retry("redis", RETRY_DELAY, &shutdown, &redis_fatal, || async {
        anyhow::Ok(RedisCounter::with_connector(&redis, shutdown.clone()).await?)
    })
    .await?
    else {
        pool.close().await;
        return finish(health_server).await;
    };
    let _ = ready.pool.set(pool.clone());

    if let Some(entra) = &entra {
        // Fetched now so the first sign-in is quick; a failure only warns: readiness never
        // depends on Entra ID (spec D4).
        let warming = entra.clone();
        tokio::spawn(async move { warming.warm().await });
    }
    if !config.providers.password {
        tracing::info!(
            "email and password sign-in is off: only Microsoft Entra ID sign-in is served"
        );
    }
    let auth = Auth::new(
        pool.clone(),
        Settings {
            public_url: config.public_url.clone(),
            passwords: Passwords::owasp(),
            links,
            tokens,
            hash_concurrency: config.hash_concurrency,
            hash_queue: config.hash_queue,
            hash_wait: HASH_WAIT,
        },
    )
    .with_sign_in(SignIn {
        password: config.providers.password,
        entra,
    });
    let limits = Limits::new(Arc::new(counter));
    let api = router(Api {
        auth: auth.clone(),
        limits: limits.clone(),
        client_ip_header: config.client_ip_header.clone(),
        directory,
    });
    // The admin listener exists only with its token (spec D15).
    let admin_server = match &config.admin_token {
        Some(token) => {
            let listener = TcpListener::bind(config.admin_addr)
                .await
                .with_context(|| format!("bind the admin listener {}", config.admin_addr))?;
            tracing::info!(addr = %config.admin_addr, "serving the admin API");
            let admin = admin_router(AdminApi::new(auth.clone(), limits.clone(), token));
            Some(tokio::spawn(
                axum::serve(
                    listener,
                    admin.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(shutdown.clone().cancelled_owned())
                .into_future(),
            ))
        }
        None => None,
    };
    tracing::info!(addr = %config.http_addr, "serving HTTP");
    let result = axum::serve(
        http,
        api.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await;
    pool.close().await;
    if let Some(admin_server) = admin_server {
        admin_server.await??;
    }
    tracing::info!("stopped");
    result?;
    finish(health_server).await
}

/// `push-auth migrate`: applies the `auth` migrations (checking that `citext` is
/// allow-listed), grants `PUSH_DATABASE_RUNTIME_ROLE` the use of the `auth` schema and
/// `PUSH_AUTH_WORKER_ROLE` the worker's narrow access, then returns.
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
    connector.migrate(&MIGRATOR, &["citext"]).await?;
    let pool = connector.single().await?;
    let granted = async {
        if let Some(role) = &config.runtime_role {
            grant_runtime_access(&pool, "auth", role).await?;
        }
        if let Some(role) = &config.worker_role {
            grant_worker_access_on(&pool, role).await?;
        }
        anyhow::Ok(())
    }
    .await;
    pool.close().await;
    granted?;
    shutdown.cancel();
    tracing::info!("push_auth is migrated");
    Ok(())
}

/// Waits for the health server to stop.
async fn finish(health_server: tokio::task::JoinHandle<std::io::Result<()>>) -> anyhow::Result<()> {
    health_server.await??;
    Ok(())
}

#[derive(Clone)]
struct Health {
    pool: Arc<OnceLock<PgPool>>,
    shutdown: CancellationToken,
}

fn health_router(health: Health) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .with_state(health)
}

/// Ready once the database is migrated and answers. Start-up waits for the first Redis
/// connection, but Redis is not checked afterwards: without it only
/// the rate-limited endpoints refuse (with a retryable `unavailable`, see `limits`), while
/// refresh, devices and profiles keep working.
async fn readyz(State(health): State<Health>) -> (StatusCode, &'static str) {
    if health.shutdown.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    }
    match health.pool.get() {
        None => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
        Some(pool) => match sqlx::query("SELECT 1").execute(pool).await {
            Ok(_) => (StatusCode::OK, "ready"),
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
