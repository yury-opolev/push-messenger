//! `PgStore::connect` and the operations a service needs around the store: session
//! timeouts, lock timeouts as conflicts, idempotent migrations, ping and close.
//!
//! Each test starts a Postgres container of its own (Docker must be running); the tests
//! run in parallel, so the file takes about as long as one container start.

use std::time::Duration;

use push_domain::{DomainError, Store, Tx, UserId};
use push_store::{MIGRATOR, PgStore, StoreOptions, migrate_database};
use sqlx::{Connection, PgConnection};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

/// A running Postgres and the URL of its `postgres` database.
async fn postgres() -> (ContainerAsync<Postgres>, String) {
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
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    (container, url)
}

#[tokio::test]
async fn every_connection_gets_the_session_timeouts() {
    let (_container, url) = postgres().await;
    let options = StoreOptions {
        lock_timeout: Duration::from_millis(250),
        statement_timeout: Duration::from_secs(7),
        idle_in_transaction_timeout: Duration::from_secs(9),
        ..StoreOptions::default()
    };
    let store = PgStore::connect(&url, &options).await.expect("connect");
    let settings: (String, String, String) = sqlx::query_as(
        "SELECT current_setting('lock_timeout'), current_setting('statement_timeout'), \
         current_setting('idle_in_transaction_session_timeout')",
    )
    .fetch_one(store.pool())
    .await
    .expect("read the settings");
    assert_eq!(
        settings,
        ("250ms".to_owned(), "7s".to_owned(), "9s".to_owned())
    );
}

#[tokio::test]
async fn a_lock_timeout_is_a_retryable_conflict() {
    let (_container, url) = postgres().await;
    let options = StoreOptions {
        lock_timeout: Duration::from_millis(200),
        ..StoreOptions::default()
    };
    let store = PgStore::connect(&url, &options).await.expect("connect");
    store.migrate().await.expect("migrate");

    // Another session inserts the same user and keeps its transaction open, so the
    // store's insert has to wait for that row lock until `lock_timeout` ends it.
    let user = UserId::new();
    let mut holder = PgConnection::connect(&url).await.expect("connect");
    let mut held = holder.begin().await.expect("begin");
    sqlx::query("INSERT INTO messaging.known_user (user_id) VALUES ($1)")
        .bind(user.0)
        .execute(&mut *held)
        .await
        .expect("insert the user");

    let mut tx = store.begin().await.expect("begin");
    let error = tx
        .insert_user(user)
        .await
        .expect_err("the lock wait must time out");
    assert_eq!(error, DomainError::Conflict);
    assert!(error.is_retryable());
    held.rollback().await.expect("roll back");
}

#[tokio::test]
async fn running_the_migrations_again_changes_nothing() {
    let (_container, url) = postgres().await;
    migrate_database(&url).await.expect("first run");
    migrate_database(&url).await.expect("second run");
    let store = PgStore::connect(&url, &StoreOptions::default())
        .await
        .expect("connect");
    store.migrate().await.expect("third run, through the pool");
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(store.pool())
        .await
        .expect("count the applied migrations");
    assert_eq!(applied, MIGRATOR.iter().count() as i64);
}

#[tokio::test]
async fn ping_answers_until_the_store_is_closed() {
    let (_container, url) = postgres().await;
    let store = PgStore::connect(&url, &StoreOptions::default())
        .await
        .expect("connect");
    store.ping().await.expect("ping an open store");
    store.close().await;
    assert!(
        store.ping().await.is_err(),
        "a closed store must not answer"
    );
}
