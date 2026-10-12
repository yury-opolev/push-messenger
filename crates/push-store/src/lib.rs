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

/// Applies the migrations the database at `url` does not have yet, on a connection of its
/// own: a migration must not inherit the pool's statement and lock timeouts, which are
/// sized for requests. Run it as the owner of the database.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    // A one-connection pool without the session settings of `PgStore::connect`.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await?;
    let result = MIGRATOR.run(&pool).await;
    pool.close().await;
    result
}
