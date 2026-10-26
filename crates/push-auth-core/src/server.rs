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
use crate::agent_key::AgentKeySignIn;
use crate::api::{Api, router};
use crate::config::{Config, MigrateConfig};
use crate::db::{MIGRATOR, grant_worker_access_on};
use crate::directory::{Directory, GrpcDirectory, NoDirectory};
use crate::entra::{EntraSignIn, Providers};
use crate::identity::{
    AGENT_PROVIDERS, Admission, Authenticator, ENTRA_PROVIDER, MAX_SESSION_LIMIT,
    RESERVED_PROVIDER_IDS, is_provider_id,
};
use crate::limits::{Limits, RedisCounter};
use crate::links::LinkSigner;
use crate::password::Passwords;
use crate::service::{Auth, Settings, SignIn};
use crate::tokens::AccessTokens;

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// Longest wait for a password-hash slot before a request is refused as `unavailable`.
const HASH_WAIT: Duration = Duration::from_secs(2);
/// How often an Entra ID-only service signs out password devices created since.
const PASSWORD_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// What a binary of one's own substitutes (spec 2026-10-19 pluggable identity, D6): an
/// authenticator, served at `/v1/auth/{its provider id}` when `PUSH_AUTH_PROVIDERS` names
/// `external`, and an admission asked on every sign-in by token (Entra ID's included).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub authenticator: Option<Arc<dyn Authenticator>>,
    pub admission: Option<Arc<dyn Admission>>,
}

/// Runs the service with the built-in sign-in until `shutdown` is cancelled:
/// [`serve_with`] and no overrides.
pub async fn serve(
    config: Config,
    http: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    serve_with(config, http, health, shutdown, Overrides::default()).await
}

/// Runs the service until `shutdown` is cancelled, then finishes the requests in
/// progress and closes the database pool. `overrides` must agree with
/// `PUSH_AUTH_PROVIDERS` (spec D7): anything else stops start-up at once, before a
/// datastore is contacted.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping, and never after shutdown began. Postgres and
/// Redis are retried until they answer, so the service may start before them.
pub async fn serve_with(
    config: Config,
    http: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
    overrides: Overrides,
) -> anyhow::Result<()> {
    check_overrides(&config.providers, &overrides).map_err(anyhow::Error::msg)?;
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
    let entra_app = match (&entra, &config.entra_app) {
        (Some(verifier), Some(settings)) => {
            Some(verifier.app(settings.clone()).map_err(anyhow::Error::msg)?)
        }
        _ => None,
    };
    let entra = entra.filter(|_| config.providers.entra);
    // The agent-key authenticator needs the pool, but its settings are checked now too.
    if let Some(settings) = &config.agent_key {
        crate::agent_key::check(settings).map_err(anyhow::Error::msg)?;
    }
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

    if !config.providers.password {
        if config.providers.external || config.providers.agent_key {
            tracing::info!("email and password sign-in is off: only sign-in by token is served");
        } else {
            tracing::info!(
                "email and password sign-in is off: only Microsoft Entra ID sign-in is served"
            );
        }
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
    .with_agents_per_user(config.agents_per_user)
    .with_sign_in(SignIn {
        password: config.providers.password,
        entra,
    });
    let auth = match entra_app {
        Some(app) => auth.with_entra_app(app),
        None => auth,
    };
    let auth = match &config.agent_key {
        Some(settings) => auth.with_agent_key(
            AgentKeySignIn::new(settings.clone(), pool.clone()).map_err(anyhow::Error::msg)?,
        ),
        None => auth,
    };
    let auth = match overrides.authenticator {
        Some(authenticator) => auth.with_external(authenticator),
        None => auth,
    };
    let auth = match overrides.admission {
        Some(admission) => auth.with_admission(admission),
        None => auth,
    };
    for authenticator in auth.authenticators() {
        // Keys are fetched now so the first sign-in is quick; a failure only warns:
        // readiness never depends on an identity provider (spec 2026-10-19 D4).
        tokio::spawn(async move { authenticator.warm().await });
    }
    let sweeper = if config.providers.password {
        None
    } else {
        // Entra ID only: every password session ends now, before this replica is ready
        // (spec D16). Idempotent; replicas share the work.
        let revoked = auth
            .revoke_password_devices(&shutdown)
            .await
            .context("sign out the password devices")?;
        tracing::info!(
            target: "security",
            event = "auth.password_sessions.revoked",
            devices = revoked,
            "email and password sign-in is off: password devices were signed out"
        );
        if shutdown.is_cancelled() {
            pool.close().await;
            return finish(health_server).await;
        }
        // Replicas of the previous version (password still on) may sign password devices
        // in during a rolling update: they are signed out again periodically.
        Some(password_sweeper(
            auth.clone(),
            PASSWORD_SWEEP_INTERVAL,
            shutdown.clone(),
        ))
    };
    let _ = ready.pool.set(pool.clone());
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
    if let Some(sweeper) = sweeper {
        let _ = sweeper.await;
    }
    pool.close().await;
    if let Some(admin_server) = admin_server {
        admin_server.await??;
    }
    tracing::info!("stopped");
    result?;
    finish(health_server).await
}

/// Whether `overrides` agree with `providers` (spec D7): `external` exactly with an
/// authenticator, whose id is a provider id, no route of its own, not `entra` beside the
/// built-in Entra ID check, with a maximum session of 1 s to 720 h; an admission only with
/// a sign-in by token to ask it. The error says what to change.
pub(crate) fn check_overrides(providers: &Providers, overrides: &Overrides) -> Result<(), String> {
    match (&overrides.authenticator, providers.external) {
        (None, true) => {
            return Err(
                "PUSH_AUTH_PROVIDERS names external, but no authenticator was \
                        supplied: external needs a push-auth of one's own that passes \
                        Overrides.authenticator to serve_with"
                    .to_owned(),
            );
        }
        (Some(_), false) => {
            return Err(
                "an authenticator was supplied in Overrides, but PUSH_AUTH_PROVIDERS \
                        does not name external: add external to it, or supply none"
                    .to_owned(),
            );
        }
        (Some(authenticator), true) => {
            let id = authenticator.provider();
            if !is_provider_id(id) {
                return Err(format!(
                    "the provider id {id:?} of the authenticator in Overrides is not a \
                     provider id: 1 to 32 lower-case letters, digits or -, starting with a \
                     letter (PUSH_AUTH_PROVIDERS names external)"
                ));
            }
            if RESERVED_PROVIDER_IDS.contains(&id) {
                return Err(format!(
                    "the provider id {id:?} of the authenticator in Overrides is a route of \
                     push-auth (/v1/auth/{id}): choose another (PUSH_AUTH_PROVIDERS names \
                     external)"
                ));
            }
            if AGENT_PROVIDERS.contains(&id) {
                return Err(format!(
                    "{id} is a built-in provider: explicitly enable it in \
                     PUSH_AUTH_PROVIDERS, not an Overrides authenticator"
                ));
            }
            if providers.entra && id == ENTRA_PROVIDER {
                return Err("PUSH_AUTH_PROVIDERS names entra and external, but the \
                            authenticator in Overrides has the id entra too: drop entra \
                            from PUSH_AUTH_PROVIDERS or give it another id"
                    .to_owned());
            }
            let session = authenticator.max_session();
            if session < Duration::from_secs(1) || session > MAX_SESSION_LIMIT {
                return Err(format!(
                    "the maximum session of the authenticator {id:?} in Overrides must be \
                     1 s to 720 h (PUSH_AUTH_PROVIDERS names external)"
                ));
            }
        }
        (None, false) => {}
    }
    if overrides.admission.is_some()
        && !providers.entra
        && !providers.entra_app
        && !providers.agent_key
        && !providers.external
    {
        return Err(
            "an admission was supplied in Overrides, but PUSH_AUTH_PROVIDERS names \
                    neither entra nor external: no sign-in would ask it"
                .to_owned(),
        );
    }
    Ok(())
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

/// While password sign-in is off, signs out every `every` the password devices that
/// appeared since the last sweep (an older replica's sign-in during a rolling update),
/// until `shutdown`. Failures are logged and tried again at the next round.
pub(crate) fn password_sweeper(
    auth: Auth,
    every: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(every) => {}
            }
            match auth.revoke_password_devices(&shutdown).await {
                Ok(0) => {}
                Ok(revoked) => tracing::info!(
                    target: "security",
                    event = "auth.password_sessions.revoked",
                    devices = revoked,
                    "password devices created since the last sweep were signed out"
                ),
                Err(error) => {
                    tracing::warn!(%error, "signing out password devices failed; trying again");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::tests::RequireGroup;

    /// An authenticator of `provider` whose sessions last `session`.
    #[derive(Debug)]
    struct Fixed(&'static str, Duration);

    #[async_trait::async_trait]
    impl Authenticator for Fixed {
        fn provider(&self) -> &str {
            self.0
        }

        fn max_session(&self) -> Duration {
            self.1
        }

        async fn authenticate(
            &self,
            _token: &str,
        ) -> Result<crate::Principal, crate::AuthenticationError> {
            Err(crate::AuthenticationError::Rejected("fixed"))
        }
    }

    fn providers(names: &str) -> Providers {
        Providers::parse(Some(names)).unwrap()
    }

    fn with(provider: &'static str, session: Duration) -> Overrides {
        Overrides {
            authenticator: Some(Arc::new(Fixed(provider, session))),
            admission: None,
        }
    }

    #[test]
    fn overrides_and_providers_must_agree() {
        let hour = Duration::from_secs(3600);
        for (names, overrides) in [
            ("password", Overrides::default()),
            ("password,entra", Overrides::default()),
            ("external", with("acme", hour)),
            ("entra,external", with("acme-id", hour)),
            ("entra-app,external", with("acme-id", hour)),
            ("external", with("entra", Duration::from_secs(720 * 3600))),
            ("password,external", with("a", Duration::from_secs(1))),
            (
                "entra",
                Overrides {
                    authenticator: None,
                    admission: Some(RequireGroup::new("staff")),
                },
            ),
            (
                "entra-app",
                Overrides {
                    authenticator: None,
                    admission: Some(RequireGroup::new("agents")),
                },
            ),
            (
                "agent-key",
                Overrides {
                    authenticator: None,
                    admission: Some(RequireGroup::new("agents")),
                },
            ),
        ] {
            assert_eq!(
                check_overrides(&providers(names), &overrides),
                Ok(()),
                "{names}"
            );
        }
        let refused = [
            ("external", Overrides::default(), "Overrides.authenticator"),
            (
                "password,entra",
                with("acme", hour),
                "does not name external",
            ),
            ("external", with("Acme", hour), "is not a provider id"),
            ("external", with("a b", hour), "is not a provider id"),
            (
                "external",
                with("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", hour),
                "is not a provider id",
            ),
            ("entra,external", with("entra", hour), "entra too"),
            ("external", with("entra-app", hour), "built-in provider"),
            ("external", with("agent-key", hour), "built-in provider"),
            (
                "agent-key,external",
                with("agent-key", hour),
                "built-in provider",
            ),
            (
                "entra-app,external",
                with("entra-app", hour),
                "built-in provider",
            ),
            ("external", with("acme", Duration::ZERO), "1 s to 720 h"),
            (
                "external",
                with("acme", Duration::from_secs(720 * 3600 + 1)),
                "1 s to 720 h",
            ),
            (
                "password",
                Overrides {
                    authenticator: None,
                    admission: Some(RequireGroup::new("staff")),
                },
                "neither entra nor external",
            ),
        ];
        for (names, overrides, expected) in refused {
            let message = check_overrides(&providers(names), &overrides).unwrap_err();
            assert!(message.contains(expected), "{names}: {message}");
            assert!(
                message.contains("PUSH_AUTH_PROVIDERS") || message.contains("Overrides"),
                "{message}"
            );
        }
        for route in ["signup", "verify", "login", "password", "refresh", "jwks"] {
            let message = check_overrides(&providers("external"), &with(route, hour)).unwrap_err();
            assert!(
                message.contains("is a route of push-auth"),
                "{route}: {message}"
            );
        }
    }
}
