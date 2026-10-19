//! The `auth` schema's migrations, and what the worker may do in it.

use sqlx::postgres::PgPoolOptions;

/// The migrations of the `auth` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies the migrations the `push_auth` database at `url` does not have yet, on a
/// connection of its own (see [`push_store::run_migrations`]). Run it as the owner of the
/// database: it creates the `citext` extension, which PostgreSQL trusts database owners
/// with.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    push_store::run_migrations(url, &MIGRATOR).await
}

/// Lets the worker's `role` drain the outbox and run the two cleanups, and nothing else:
/// `USAGE` on the schema; `SELECT` and `DELETE` on `auth.outbox`, plus `UPDATE` of its
/// `created_at` alone, without which PostgreSQL refuses the `FOR UPDATE SKIP LOCKED` that
/// lets several workers drain at once; and `EXECUTE` on
/// `auth.delete_expired_refresh_tokens` and `auth.delete_unverified_accounts`. Idempotent.
/// Run it as the owner, after [`migrate_database`], on a connection of its own. `role`
/// must be a role name the service accepts ([`is_role_name`], or an Entra principal name) of
/// an existing role.
pub async fn grant_worker_access(url: &str, role: &str) -> Result<(), sqlx::Error> {
    let pool = PgPoolOptions::new().max_connections(1).connect(url).await?;
    let result = grant_worker_access_on(&pool, role).await;
    pool.close().await;
    result
}

/// [`grant_worker_access`] on an open connection to the `auth` schema's owner, e.g. the
/// migration identity's. `role` may also be an Entra principal name
/// ([`push_store::is_principal_name`]); it is quoted into SQL.
pub async fn grant_worker_access_on(pool: &sqlx::PgPool, role: &str) -> Result<(), sqlx::Error> {
    if !push_store::is_principal_name(role) {
        return Err(sqlx::Error::Configuration(
            "the worker's role name is not valid".into(),
        ));
    }
    let grants = format!(
        "GRANT USAGE ON SCHEMA auth TO \"{role}\"; \
         GRANT SELECT, DELETE, UPDATE (created_at) ON auth.outbox TO \"{role}\"; \
         GRANT EXECUTE ON FUNCTION auth.delete_expired_refresh_tokens(integer), \
         auth.delete_unverified_accounts(interval, integer) TO \"{role}\""
    );
    sqlx::raw_sql(&grants).execute(pool).await.map(drop)
}

/// A plain PostgreSQL role name: a lower-case letter or `_`, then up to 62 lower-case
/// letters, digits or `_`. It is quoted into SQL, so nothing else is accepted.
pub fn is_role_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z' | '_'))
        && name.len() <= 63
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
        // PostgreSQL reads `public` as every role even when quoted; `none` and `pg_*` are
        // reserved.
        && !matches!(name, "public" | "none")
        && !name.starts_with("pg_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_role_names_are_accepted() {
        for name in ["push_worker", "_w", "w2"] {
            assert!(is_role_name(name), "{name}");
        }
        let long = "w".repeat(64);
        for name in [
            "",
            "public",
            "none",
            "pg_monitor",
            "2w",
            "Push",
            "push-worker",
            "w\"; DROP",
            "w w",
            long.as_str(),
        ] {
            assert!(!is_role_name(name), "{name}");
        }
    }
}
