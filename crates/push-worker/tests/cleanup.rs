//! The cleanup jobs on a real PostgreSQL (Docker must be running): auth-svc's through its
//! functions, as the worker's role, and the worker's own records.

use std::time::Duration;

use push_worker::cleanup::{
    Cleanup, REFRESH_TOKENS, Retention, UNVERIFIED_ACCOUNTS, WORKER_RECORDS, periodic_jobs,
};
use push_worker::queue::{Outcome, Queue};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use uuid::Uuid;

const RETENTION: Retention = Retention {
    unverified_accounts: Duration::from_secs(7 * 24 * 3600),
    worker_records: Duration::from_secs(30 * 24 * 3600),
    interval: Duration::from_secs(3600),
};

#[tokio::test]
async fn the_cleanups_delete_what_has_expired_and_keep_the_rest() {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let server = format!("127.0.0.1:{port}");
    let admin = PgPool::connect(&format!("postgres://postgres:postgres@{server}/postgres"))
        .await
        .unwrap();
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_auth",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let auth_url = format!("postgres://postgres:postgres@{server}/push_auth");
    push_auth::migrate_database(&auth_url).await.unwrap();
    push_auth::grant_worker_access(&auth_url, "push_worker")
        .await
        .unwrap();
    let worker_url = format!("postgres://push_worker:worker-secret@{server}/push_worker");
    push_worker::migrate_database(&worker_url).await.unwrap();
    let auth = PgPool::connect(&auth_url).await.unwrap();
    let worker = PgPool::connect(&worker_url).await.unwrap();

    // auth-svc: an account unverified for 8 days, one since now; a verified one with a
    // device, an expired refresh token and a live one.
    for (handle, age, verified) in [
        ("old", "8 days", false),
        ("new", "0 days", false),
        ("alice", "8 days", true),
    ] {
        sqlx::query(
            "INSERT INTO auth.user_account (user_id, email, handle, display_name, \
             password_hash, created_at, email_verified_at) VALUES ($1, $2, $3, 'X', 'h', \
             now() - $4::interval, CASE WHEN $5 THEN now() END)",
        )
        .bind(Uuid::now_v7())
        .bind(format!("{handle}@example.com"))
        .bind(handle)
        .bind(age)
        .bind(verified)
        .execute(&auth)
        .await
        .unwrap();
    }
    let device = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.device (device_id, user_id, name, platform, registration_id) \
         SELECT $1, user_id, 'Phone', 'ios', $1 FROM auth.user_account WHERE handle = 'alice'",
    )
    .bind(device)
    .execute(&auth)
    .await
    .unwrap();
    for (hash, expires) in [([1u8; 32], "-1 hour"), ([2u8; 32], "1 day")] {
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + $3::interval)",
        )
        .bind(hash.as_slice())
        .bind(device)
        .bind(expires)
        .execute(&auth)
        .await
        .unwrap();
    }
    // The worker: an old and a new email with their events, an old buried job, a live one.
    for (age, id) in [("40 days", Uuid::now_v7()), ("1 day", Uuid::now_v7())] {
        sqlx::query(
            "INSERT INTO worker.email_message (email_id, to_address, recipient_domain, \
             template, params, status, created_at) \
             VALUES ($1, 'a@example.com', 'example.com', 'verify_email', '{}', 2, \
             now() - $2::interval)",
        )
        .bind(id)
        .bind(age)
        .execute(&worker)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO worker.email_event (event_id, provider_message_id, kind, occurred_at, \
             raw, received_at) VALUES ($1, 're-1', 1, now(), '{}', now() - $2::interval)",
        )
        .bind(id.to_string())
        .bind(age)
        .execute(&worker)
        .await
        .unwrap();
    }
    for dead in ["now() - interval '40 days'", "NULL"] {
        sqlx::query(&format!(
            "INSERT INTO worker.job (job_id, kind, payload, dead_at) \
             VALUES ($1, 'email.send', '{{}}', {dead})"
        ))
        .bind(Uuid::now_v7())
        .execute(&worker)
        .await
        .unwrap();
    }

    let queue = Queue::new(worker.clone(), Duration::from_secs(60));
    assert_eq!(queue.enqueue(&periodic_jobs()).await.unwrap(), 3);
    assert_eq!(queue.enqueue(&periodic_jobs()).await.unwrap(), 0);
    let source = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_auth"
    ))
    .await
    .unwrap();
    let cleanup = Cleanup::new(source, worker.clone(), queue, RETENTION);
    for kind in [REFRESH_TOKENS, UNVERIFIED_ACCOUNTS, WORKER_RECORDS] {
        assert_eq!(
            cleanup.run(kind).await,
            Some(Outcome::RunAgain(RETENTION.interval)),
            "{kind}"
        );
    }
    assert_eq!(cleanup.run("email.send").await, None);

    let count = |pool: &PgPool, table: &str| {
        let query = format!("SELECT count(*) FROM {table}");
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(&query)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(count(&auth, "auth.refresh_token").await, 1);
    assert_eq!(count(&auth, "auth.user_account").await, 2);
    assert_eq!(count(&worker, "worker.email_message").await, 1);
    assert_eq!(count(&worker, "worker.email_event").await, 1);
    // The live job and the three cleanups.
    assert_eq!(count(&worker, "worker.job").await, 4);
}
