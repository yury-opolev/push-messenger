//! The `auth` schema on a real PostgreSQL (Docker must be running): the migrations run as
//! the database's owner, a plain role like the chart's, and the constraints hold.

use push_auth::{MIGRATOR, migrate_database};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

/// A running Postgres with a database `push_auth` owned by the plain role `push_auth`,
/// and the URL to connect as that role.
async fn auth_database() -> (ContainerAsync<Postgres>, String) {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("the Postgres port");
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let admin = PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .expect("connect as the superuser");
    for statement in [
        "CREATE ROLE push_auth LOGIN PASSWORD 'auth-secret'",
        "CREATE DATABASE push_auth OWNER push_auth",
    ] {
        sqlx::query(statement)
            .execute(&admin)
            .await
            .expect(statement);
    }
    admin.close().await;
    let url = format!("postgres://push_auth:auth-secret@127.0.0.1:{port}/push_auth");
    (container, url)
}

async fn migrated(url: &str) -> PgPool {
    migrate_database(url).await.expect("migrate");
    PgPool::connect(url).await.expect("connect")
}

async fn insert_user(pool: &PgPool, email: &str, handle: &str) -> Result<Uuid, sqlx::Error> {
    let user = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.user_account (user_id, email, handle, display_name, password_hash) \
         VALUES ($1, $2, $3, 'Someone', 'hash')",
    )
    .bind(user)
    .bind(email)
    .bind(handle)
    .execute(pool)
    .await?;
    Ok(user)
}

fn constraint(error: sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(|database| database.constraint())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn the_owner_migrates_and_a_second_run_changes_nothing() {
    let (_container, url) = auth_database().await;
    migrate_database(&url).await.expect("first run");
    let pool = migrated(&url).await;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .expect("count the applied migrations");
    assert_eq!(applied, MIGRATOR.iter().count() as i64);
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables \
         WHERE table_schema = 'auth' ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .expect("list the tables");
    assert_eq!(
        tables,
        ["device", "outbox", "refresh_token", "user_account"]
    );
}

#[tokio::test]
async fn emails_are_unique_ignoring_case_and_handles_unique() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    insert_user(&pool, "Alice@Example.com", "alice")
        .await
        .expect("the first account");
    let error = insert_user(&pool, "alice@example.COM", "alice2")
        .await
        .expect_err("the same email in other case");
    assert_eq!(constraint(error), "user_account_email_key");
    let error = insert_user(&pool, "bob@example.com", "alice")
        .await
        .expect_err("a taken handle");
    assert_eq!(constraint(error), "user_account_handle_key");
}

#[tokio::test]
async fn malformed_handles_and_platforms_are_refused() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    for handle in [
        "al",
        "9lives",
        "has-dash",
        "Alice",
        "a_very_long_handle_over_thirty_c",
    ] {
        let error = insert_user(&pool, &format!("{}@example.com", Uuid::now_v7()), handle)
            .await
            .expect_err(handle);
        assert_eq!(constraint(error), "user_account_handle_check", "{handle}");
    }
    let user = insert_user(&pool, "carol@example.com", "carol")
        .await
        .expect("a valid account");
    let error = sqlx::query(
        "INSERT INTO auth.device (device_id, user_id, name, platform, registration_id) \
         VALUES ($1, $2, 'Phone', 'toaster', $1)",
    )
    .bind(Uuid::now_v7())
    .bind(user)
    .execute(&pool)
    .await
    .expect_err("an unknown platform");
    assert_eq!(constraint(error), "device_platform_check");
}

#[tokio::test]
async fn a_registration_id_names_one_device_of_a_user() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    let alice = insert_user(&pool, "alice@example.com", "alice")
        .await
        .expect("alice");
    let bob = insert_user(&pool, "bob@example.com", "bob")
        .await
        .expect("bob");
    let registration = Uuid::now_v7();
    let insert = |user: Uuid| {
        sqlx::query(
            "INSERT INTO auth.device (device_id, user_id, name, platform, registration_id) \
             VALUES ($1, $2, 'Phone', 'ios', $3)",
        )
        .bind(Uuid::now_v7())
        .bind(user)
        .bind(registration)
        .execute(&pool)
    };
    insert(alice).await.expect("the first device");
    insert(bob).await.expect("another user may use the same id");
    let error = insert(alice).await.expect_err("the same id again");
    assert_eq!(constraint(error), "device_user_id_registration_id_key");
}
