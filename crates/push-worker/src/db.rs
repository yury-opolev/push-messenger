//! The `worker` schema's migrations.

/// The migrations of the `worker` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies the migrations the `push_worker` database at `url` does not have yet, on a
/// connection of its own (see [`push_store::run_migrations`]). Run it as the owner of the
/// database: it creates the `citext` extension, which PostgreSQL trusts database owners
/// with.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    push_store::run_migrations(url, &MIGRATOR).await
}
