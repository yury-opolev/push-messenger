//! PostgreSQL storage for push-messenger: the migrations of the `messaging` schema
//! (spec §3.2) and the push-domain [`Store`](push_domain::Store) on sqlx.
//!
//! Every query is checked when it runs (`sqlx::query`, not the `query!` macros), so
//! building needs no database. Transactions run at READ COMMITTED; they are serialised
//! where the domain needs it by row locks (`next_seq`, `lock_invite`) and by
//! transaction-scoped advisory locks (contact pairs, conversation positions).
//!
//! Errors: serialization failures, deadlocks and lock timeouts become the retryable
//! `DomainError::Conflict`; every other database error, constraint violations included,
//! becomes `DomainError::Store`. Both are logged with `tracing` first.
//!
//! A service connects with [`PgStore::connect`], which sets the lock, statement and
//! idle-in-transaction timeouts of [`StoreOptions`] on every connection. Each service has
//! a database of its own (message-svc: `push_messaging`), so sqlx's migrations table
//! never mixes two services' migrations.

mod codes;
mod error;
mod locks;
mod options;
mod rows;
mod store;

pub use options::StoreOptions;
pub use store::{PgStore, PgTx};

/// The migrations of the `messaging` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Checks that `url` parses as a PostgreSQL connection URL, without connecting. The error
/// never contains the URL, which carries the password.
pub fn check_database_url(url: &str) -> Result<(), String> {
    // sqlx parses any scheme as PostgreSQL options, so the scheme is checked here.
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err("not a valid PostgreSQL connection URL".to_owned());
    }
    url.parse::<sqlx::postgres::PgConnectOptions>()
        .map(drop)
        .map_err(|_| "not a valid PostgreSQL connection URL".to_owned())
}

/// Applies the migrations the database at `url` does not have yet, on a connection of its
/// own: a migration must not inherit the pool's statement and lock timeouts, which are
/// sized for requests. Run it as the owner of the database.
///
/// This is the entry point services use: at start-up they call
/// `push_store::migrate_database(url)` on a dedicated connection, not `PgStore::migrate`
/// on the request pool.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    run_migrations(url, &MIGRATOR).await
}

/// Applies `migrator`'s migrations that the database at `url` does not have yet, the way
/// [`migrate_database`] does: on a one-connection pool of its own, without the session
/// timeouts of [`connect_pool`]. Every service migrates its own database with it.
pub async fn run_migrations(
    url: &str,
    migrator: &sqlx::migrate::Migrator,
) -> Result<(), sqlx::migrate::MigrateError> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await?;
    let result = migrator.run(&pool).await;
    pool.close().await;
    result
}

/// Connects a request pool to `url` as `options` say: every connection the pool opens
/// first sets the lock, statement and idle-in-transaction timeouts. Fails if the options
/// do not pass [`StoreOptions::validate`] or the first connection cannot be made.
pub async fn connect_pool(url: &str, options: &StoreOptions) -> Result<sqlx::PgPool, sqlx::Error> {
    use sqlx::Executor;

    options
        .validate()
        .map_err(|message| sqlx::Error::Configuration(message.into()))?;
    let settings = options.session_settings();
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(options.max_connections)
        .acquire_timeout(options.acquire_timeout)
        .after_connect(move |connection, _| {
            let settings = settings.clone();
            Box::pin(async move {
                connection.execute(settings.as_str()).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
}
