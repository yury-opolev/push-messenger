//! Migration 0011's backfill of the `started_by_agent` marks on rows written before it.
//!
//! Starts a Postgres container of its own (Docker must be running), migrates it up to
//! 0010, writes rows as the older schema had them, then applies the remaining migrations.

use std::borrow::Cow;

use push_store::{MIGRATOR, run_migrations};
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

const OWNER: &str = "00000000-0000-0000-0000-000000000001";
const AGENT: &str = "00000000-0000-0000-0000-000000000002";
const PERSON: &str = "00000000-0000-0000-0000-000000000003";

#[tokio::test]
async fn migration_0011_marks_what_agents_started_before_it() {
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
    let before = Migrator {
        migrations: Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version < 11)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    run_migrations(&url, &before)
        .await
        .expect("migrate to 0010");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect");
    sqlx::raw_sql(&format!(
        "INSERT INTO messaging.known_user (user_id) VALUES ('{OWNER}'), ('{AGENT}'), ('{PERSON}');
         INSERT INTO messaging.agent_owner (agent_user_id, owner_user_id)
             VALUES ('{AGENT}', '{OWNER}');
         INSERT INTO messaging.conversation (conversation_id, kind, created_by) VALUES
             ('00000000-0000-0000-0000-0000000000c1', 1, '{AGENT}'),
             ('00000000-0000-0000-0000-0000000000c2', 1, '{AGENT}'),
             ('00000000-0000-0000-0000-0000000000c3', 2, '{AGENT}'),
             ('00000000-0000-0000-0000-0000000000c4', 1, '{PERSON}'),
             ('00000000-0000-0000-0000-0000000000c5', 2, '{OWNER}');
         INSERT INTO messaging.conversation_member
             (conversation_id, user_id, role, visible_from_seq) VALUES
             ('00000000-0000-0000-0000-0000000000c1', '{AGENT}', 1, 1),
             ('00000000-0000-0000-0000-0000000000c1', '{PERSON}', 1, 1),
             ('00000000-0000-0000-0000-0000000000c2', '{AGENT}', 1, 1),
             ('00000000-0000-0000-0000-0000000000c2', '{OWNER}', 1, 1),
             ('00000000-0000-0000-0000-0000000000c3', '{AGENT}', 2, 1),
             ('00000000-0000-0000-0000-0000000000c3', '{OWNER}', 1, 1),
             ('00000000-0000-0000-0000-0000000000c4', '{PERSON}', 1, 1),
             ('00000000-0000-0000-0000-0000000000c4', '{AGENT}', 1, 1),
             ('00000000-0000-0000-0000-0000000000c5', '{OWNER}', 2, 1),
             ('00000000-0000-0000-0000-0000000000c5', '{AGENT}', 1, 1);
         INSERT INTO messaging.call_history
             (user_id, call_id, peer_user_id, direction, started_at, ended_at, duration_s,
              outcome) VALUES
             ('{AGENT}', '00000000-0000-0000-0000-0000000000a1', '{PERSON}', 'out', now(),
              now(), 0, 'missed'),
             ('{PERSON}', '00000000-0000-0000-0000-0000000000a1', '{AGENT}', 'in', now(),
              now(), 0, 'missed'),
             ('{PERSON}', '00000000-0000-0000-0000-0000000000a2', '{AGENT}', 'out', now(),
              now(), 0, 'missed'),
             ('{AGENT}', '00000000-0000-0000-0000-0000000000a2', '{PERSON}', 'in', now(),
              now(), 0, 'missed'),
             ('{OWNER}', '00000000-0000-0000-0000-0000000000a3', '{PERSON}', 'out', now(),
              now(), 0, 'missed');"
    ))
    .execute(&pool)
    .await
    .expect("write rows as 0010 had them");

    run_migrations(&url, &MIGRATOR)
        .await
        .expect("migrate the rest");

    let conversations: Vec<(String, bool)> = sqlx::query_as(
        "SELECT right(conversation_id::text, 2), started_by_agent \
         FROM messaging.conversation ORDER BY conversation_id",
    )
    .fetch_all(&pool)
    .await
    .expect("read the conversations");
    assert_eq!(
        conversations,
        [
            // A direct chat the agent opened with someone other than its owner.
            ("c1".to_owned(), true),
            // The agent's chat with its owner.
            ("c2".to_owned(), false),
            // A group the agent created, its owner in it or not.
            ("c3".to_owned(), true),
            // Started by others.
            ("c4".to_owned(), false),
            ("c5".to_owned(), false),
        ]
    );
    let calls: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT right(call_id::text, 2), direction, started_by_agent \
         FROM messaging.call_history ORDER BY call_id, direction",
    )
    .fetch_all(&pool)
    .await
    .expect("read the call history");
    assert_eq!(
        calls,
        [
            // The agent called: both rows.
            ("a1".to_owned(), "in".to_owned(), true),
            ("a1".to_owned(), "out".to_owned(), true),
            // The agent was called.
            ("a2".to_owned(), "in".to_owned(), false),
            ("a2".to_owned(), "out".to_owned(), false),
            ("a3".to_owned(), "out".to_owned(), false),
        ]
    );
    pool.close().await;
}
