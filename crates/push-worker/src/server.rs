//! Running the worker: the relay and job loops, the HTTP API, the health endpoints and
//! graceful shutdown.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use push_credentials::AzureConfig;
use push_credentials::startup::retry;
use push_email::{EmailSender, LogSender, ResendSender, ResendWebhooks, Templates};
use push_store::{Connector, grant_runtime_access, is_permanent_startup, tokens_for};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::accounts::Accounts;
use crate::cleanup::{Cleanup, Retention, periodic_jobs};
use crate::config::{Config, MigrateConfig, Sender};
use crate::db::MIGRATOR;
use crate::deliveries::Deliveries;
use crate::email::{Mailbox, MailboxSender, Mailer};
use crate::http::{Http, router};
use crate::jobs::Jobs;
use crate::queue::{Job, Outcome, Queue};
use crate::relay::OutboxRelay;

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// A claimed job is someone else's for this long.
const LEASE: Duration = Duration::from_secs(120);
/// Jobs claimed at once. They run one after another, so the last one starts only after
/// the others ended, and the whole batch must fit in the lease, or another worker claims
/// the jobs not started yet and they run twice. An attempt takes at most about 15 s
/// (message-svc: a 5 s connect and a 10 s call timeout; Resend: a 10 s request timeout,
/// plus statements of a few milliseconds; a cleanup run starts no batch after 5 s, and a
/// batch's statement times out after 10 s), so 4 jobs take at most about 60 s of the
/// 120 s lease. Even then a late outcome is never recorded over a newer claim
/// (`Queue::finish`).
const JOB_BATCH: i64 = 4;
/// Outbox events moved at once.
const OUTBOX_BATCH: i64 = 50;
/// The cleanups run this often.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Runs the worker until `shutdown` is cancelled, then finishes the job in progress, gives
/// back the jobs it claimed but did not start (due again at once, for any replica), and
/// closes its pools.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` once both databases
/// answer (its own migrated, auth-svc's with the outbox readable), and never after
/// shutdown began. Both databases are retried until they answer; message-svc is not
/// waited for: its events wait in the queue.
pub async fn serve(
    config: Config,
    http: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let ready = Health {
        pool: Arc::new(OnceLock::new()),
        shutdown: shutdown.clone(),
    };
    let health_server = tokio::spawn(
        axum::serve(health, health_router(ready.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
    );
    if config.dev_mailbox {
        tracing::warn!(
            "DEV ONLY: PUSH_DEV_MAILBOX=1 serves GET /dev/mailbox, which shows anyone every \
             email the log sender delivered, sign-in links included. Never enable it outside \
             a local cluster."
        );
    }
    let accounts = Accounts::new(&config.message_svc_url, &config.accounts_token)
        .map_err(anyhow::Error::msg)?;
    let mailbox = config.dev_mailbox.then(Mailbox::default);
    let sender: Option<Arc<dyn EmailSender>> = match (&config.sender, &mailbox) {
        (Sender::Off, _) => None,
        (Sender::Log, Some(mailbox)) => {
            Some(Arc::new(MailboxSender::new(LogSender, mailbox.clone())))
        }
        (Sender::Log, None) => Some(Arc::new(LogSender)),
        (Sender::Resend { api_url, api_key }, _) => Some(Arc::new(
            ResendSender::new(api_url, api_key.clone(), config.from.clone())
                .map_err(anyhow::Error::msg)?,
        )),
    };
    let webhooks = match &config.webhook_secret {
        Some(secret) => Some(ResendWebhooks::new(secret).map_err(anyhow::Error::msg)?),
        None => None,
    };

    push_credentials::install_crypto_provider();
    let azure = config
        .azure
        .as_ref()
        .map(AzureConfig::source)
        .transpose()
        .context("the Azure identity")?;
    // One identity, one scope: both databases share the token.
    let pg_tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let own = Connector::for_target(&config.database, pg_tokens.as_ref())?;
    let auth_side = Connector::for_target(&config.auth_database, pg_tokens.as_ref())?;
    // A refused token or certificate is configuration in Entra mode only, so password mode
    // keeps retrying exactly as before.
    let entra = own.is_entra();
    let permanent = move |error: &anyhow::Error| entra && is_permanent_startup(error);
    let Some(pool) = retry("postgres", RETRY_DELAY, &shutdown, &permanent, || async {
        if config.run_migrations {
            // Migrations run on a dedicated connection, not on the pool, whose statement
            // and lock timeouts are sized for jobs.
            own.migrate(&MIGRATOR, &["citext"]).await?;
        }
        anyhow::Ok(own.pool(&config.store).await?)
    })
    .await?
    else {
        return finish(health_server).await;
    };
    let Some(auth) = retry(
        "auth-svc's database",
        RETRY_DELAY,
        &shutdown,
        &permanent,
        || async {
            let auth = auth_side.pool(&config.store).await?;
            // Until auth-svc (or its migration job) has migrated and granted the worker's
            // role its access, this fails; permission denied here is retried, not fatal.
            if let Err(error) = sqlx::query("SELECT 1 FROM auth.outbox LIMIT 0")
                .execute(&auth)
                .await
            {
                auth.close().await;
                return Err(anyhow::Error::from(error));
            }
            anyhow::Ok(auth)
        },
    )
    .await?
    else {
        pool.close().await;
        return finish(health_server).await;
    };

    let queue = Queue::new(pool.clone(), LEASE);
    queue.enqueue(&periodic_jobs()).await?;
    let cleanup = Cleanup::new(
        auth.clone(),
        pool.clone(),
        queue.clone(),
        Retention {
            unverified_accounts: config.unverified_after,
            worker_records: config.retention,
            interval: CLEANUP_INTERVAL,
        },
    );
    let jobs = match sender {
        Some(sender) => Jobs::new(
            accounts,
            Mailer::new(
                pool.clone(),
                sender,
                Templates::new(config.product_name.clone()),
            ),
        ),
        None => {
            tracing::info!("email is off (PUSH_EMAIL_SENDER=none)");
            Jobs::without_email(accounts)
        }
    };
    let jobs = jobs.with_cleanup(cleanup);
    let relay = OutboxRelay::new(auth.clone(), queue.clone(), OUTBOX_BATCH);
    let deliveries = webhooks.map(|webhooks| Deliveries::new(pool.clone(), Arc::new(webhooks)));
    let _ = ready.pool.set(pool.clone());

    let moved = Arc::new(Notify::new());
    let relay_loop = tokio::spawn(run_relay(
        relay,
        Arc::clone(&moved),
        config.poll,
        shutdown.clone(),
    ));
    let job_loop = tokio::spawn(run_jobs(queue, jobs, moved, config.poll, shutdown.clone()));
    tracing::info!(addr = %config.http_addr, "serving HTTP");
    let result = axum::serve(
        http,
        router(Http {
            deliveries,
            mailbox,
        }),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await;
    relay_loop.await?;
    job_loop.await?;
    auth.close().await;
    pool.close().await;
    tracing::info!("stopped");
    result?;
    finish(health_server).await
}

/// Moves auth-svc's outbox into the queue: batch after batch while there is something,
/// then every `poll`. Rings `moved` when it moved events, so the job loop starts at once.
async fn run_relay(
    relay: OutboxRelay,
    moved: Arc<Notify>,
    poll: Duration,
    shutdown: CancellationToken,
) {
    loop {
        let drained = tokio::select! {
            () = shutdown.cancelled() => return,
            drained = relay.drain_once() => drained,
        };
        match drained {
            Ok(0) => {}
            Ok(_) => {
                moved.notify_one();
                continue;
            }
            Err(error) => tracing::warn!(%error, "draining auth-svc's outbox failed; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(poll) => {}
        }
    }
}

/// Claims and runs due jobs, one at a time, until shutdown; when there are none, waits for
/// `poll` or for the relay to move events. At shutdown the jobs of the batch not started
/// yet are given back.
async fn run_jobs(
    queue: Queue,
    jobs: Jobs,
    moved: Arc<Notify>,
    poll: Duration,
    shutdown: CancellationToken,
) {
    loop {
        let claimed = tokio::select! {
            () = shutdown.cancelled() => return,
            claimed = queue.claim(JOB_BATCH) => claimed,
        };
        match claimed {
            Ok(batch) if !batch.is_empty() => {
                for (index, job) in batch.iter().enumerate() {
                    if shutdown.is_cancelled() {
                        release(&queue, &batch[index..]).await;
                        return;
                    }
                    let outcome = jobs.run(job).await;
                    log(job, &outcome);
                    match queue.finish(job, outcome).await {
                        Ok(true) => {}
                        Ok(false) => tracing::warn!(
                            job_id = %job.id,
                            "the job's lease ended before it finished; its outcome is dropped"
                        ),
                        Err(error) => {
                            tracing::warn!(job_id = %job.id, %error, "recording a job's outcome failed");
                        }
                    }
                }
                continue;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "claiming jobs failed; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = moved.notified() => {}
            () = tokio::time::sleep(poll) => {}
        }
    }
}

/// Gives back jobs claimed but not started, so another replica can take them at once.
/// If that fails, they come back when their lease ends.
async fn release(queue: &Queue, jobs: &[Job]) {
    match queue.release(jobs).await {
        Ok(released) => tracing::info!(released, "gave back the jobs not started"),
        Err(error) => tracing::warn!(%error, "giving back the jobs not started failed"),
    }
}

/// One line per attempt that did not simply succeed; never a payload.
fn log(job: &Job, outcome: &Outcome) {
    match outcome {
        Outcome::Done | Outcome::RunAgain(_) => {
            tracing::debug!(job_id = %job.id, kind = %job.kind, "job done");
        }
        Outcome::Failed { reason, .. } => tracing::warn!(
            job_id = %job.id,
            kind = %job.kind,
            attempts = job.attempts,
            reason,
            "job failed; retrying later"
        ),
        Outcome::Poison(reason) => tracing::error!(
            job_id = %job.id,
            kind = %job.kind,
            attempts = job.attempts,
            reason,
            "job failed for good; buried"
        ),
    }
}

/// `push-worker migrate`: applies the migrations of `push_worker` (checking that `citext`
/// is allow-listed) and, with `PUSH_DATABASE_RUNTIME_ROLE`, grants that role the use of
/// the `worker` schema, then returns. For a migration job (`PUSH_RUN_MIGRATIONS=false`).
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
    if let Some(role) = &config.runtime_role {
        let pool = connector.single().await?;
        let granted = grant_runtime_access(&pool, "worker", role).await;
        pool.close().await;
        granted?;
    }
    shutdown.cancel();
    tracing::info!("push_worker is migrated");
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

/// Ready once both databases answered at start-up and the worker's own answers now.
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
