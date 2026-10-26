//! The schema's own guards for templates and card messages: what migration 0005 refuses
//! even to SQL that bypasses the store.
//!
//! Each test starts a Postgres container of its own (Docker must be running); the tests
//! run in parallel, so the file takes about as long as one container start.

use push_store::migrate_database;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

const USER: &str = "00000000-0000-0000-0000-000000000001";
const CONVERSATION: &str = "00000000-0000-0000-0000-000000000002";

/// A migrated database holding one user, the `acme` publisher, an approved
/// `acme.approved@1` (change 1, the counter's last), a pending `acme.pending@1`, a
/// rejected `acme.rejected@1`, one audit event and a conversation.
async fn seeded() -> (ContainerAsync<Postgres>, PgPool) {
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
    migrate_database(&url).await.expect("migrate");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    sqlx::raw_sql(&format!(
        "INSERT INTO messaging.known_user (user_id) VALUES ('{USER}');
         INSERT INTO messaging.template_publisher (publisher, owner_user_id)
             VALUES ('acme', '{USER}');
         UPDATE messaging.template_change SET last_seq = 1;
         INSERT INTO messaging.template_version
             (publisher, name, version, title, definition, status, submitted_by,
              submitted_at, reviewed_at, change_seq)
             VALUES ('acme', 'approved', 1, 'Approved', '{{}}', 2, '{USER}', now(), now(), 1),
                    ('acme', 'pending', 1, 'Pending', '{{}}', 1, '{USER}', now(), NULL, NULL),
                    ('acme', 'rejected', 1, 'Rejected', '{{}}', 3, '{USER}', now(), now(),
                     NULL);
         INSERT INTO messaging.template_event (publisher, name, version, action, actor)
             VALUES ('acme', 'approved', 1, 'submit', '{USER}');
         INSERT INTO messaging.conversation (conversation_id, kind, created_by, last_seq)
             VALUES ('{CONVERSATION}', 2, '{USER}', 0);"
    ))
    .execute(&pool)
    .await
    .expect("seed");
    (container, pool)
}

/// Runs `sql`, which must fail with a check violation (SQLSTATE 23514) whose message
/// contains `reason`.
async fn refused(pool: &PgPool, sql: &str, reason: &str) {
    let error = sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect_err(&format!("{sql} must be refused"));
    let database = error.as_database_error().expect("a database error");
    assert_eq!(database.code().as_deref(), Some("23514"), "{sql}: {error}");
    assert!(
        database.message().contains(reason),
        "{sql}: expected {reason:?}, got {error}"
    );
}

async fn status(pool: &PgPool, name: &str) -> i16 {
    sqlx::query_scalar("SELECT status FROM messaging.template_version WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("read the status")
}

#[tokio::test]
async fn a_stored_definition_never_changes_and_is_never_deleted() {
    let (_container, pool) = seeded().await;
    let immutable = "identity and definition never change";
    refused(
        &pool,
        "UPDATE messaging.template_version SET definition = '{\"x\": 1}' \
         WHERE name = 'approved'",
        immutable,
    )
    .await;
    refused(
        &pool,
        "UPDATE messaging.template_version SET title = 'Other' WHERE name = 'pending'",
        immutable,
    )
    .await;
    refused(
        &pool,
        "UPDATE messaging.template_version SET version = 2 WHERE name = 'pending'",
        immutable,
    )
    .await;
    refused(
        &pool,
        "UPDATE messaging.template_version SET submitted_by = NULL WHERE name = 'approved'",
        immutable,
    )
    .await;
    refused(
        &pool,
        "DELETE FROM messaging.template_version WHERE name = 'rejected'",
        "never deleted",
    )
    .await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messaging.template_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn the_status_only_moves_one_way() {
    let (_container, pool) = seeded().await;
    let refused_moves = [
        // Approved back to pending; the row would satisfy the CHECK constraints.
        "UPDATE messaging.template_version \
         SET status = 1, reviewed_at = NULL, change_seq = NULL WHERE name = 'approved'",
        "UPDATE messaging.template_version \
         SET status = 4, reviewed_at = now(), change_seq = 5 WHERE name = 'pending'",
        "UPDATE messaging.template_version \
         SET status = 2, change_seq = 5 WHERE name = 'rejected'",
        // Same status, but a new review or change number.
        "UPDATE messaging.template_version SET change_seq = 7 WHERE name = 'approved'",
        "UPDATE messaging.template_version \
         SET reviewed_at = reviewed_at + interval '1 hour' WHERE name = 'rejected'",
    ];
    for sql in refused_moves {
        refused(&pool, sql, "cannot become").await;
    }
    assert_eq!(status(&pool, "approved").await, 2);
    assert_eq!(status(&pool, "pending").await, 1);
    assert_eq!(status(&pool, "rejected").await, 3);

    // The allowed moves, and an update that changes nothing.
    sqlx::raw_sql(
        "UPDATE messaging.template_version SET status = status;
         UPDATE messaging.template_change SET last_seq = 2;
         UPDATE messaging.template_version \
         SET status = 2, reviewed_at = now(), change_seq = 2 WHERE name = 'pending';
         UPDATE messaging.template_change SET last_seq = 3;
         UPDATE messaging.template_version \
         SET status = 4, reviewed_at = now(), change_seq = 3 WHERE name = 'approved';",
    )
    .execute(&pool)
    .await
    .expect("the one-way moves");
    assert_eq!(status(&pool, "pending").await, 2);
    assert_eq!(status(&pool, "approved").await, 4);
}

#[tokio::test]
async fn the_audit_trail_is_append_only() {
    let (_container, pool) = seeded().await;
    refused(
        &pool,
        "UPDATE messaging.template_event SET note = 'rewritten'",
        "append-only",
    )
    .await;
    refused(&pool, "DELETE FROM messaging.template_event", "append-only").await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messaging.template_event")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_version_enters_the_catalog_only_with_the_last_change_number() {
    let (_container, pool) = seeded().await;
    let not_last = "is not the last one taken";
    let insert = |name: &str, change_seq: i64| {
        format!(
            "INSERT INTO messaging.template_version \
             (publisher, name, version, title, definition, status, submitted_at, \
              reviewed_at, change_seq) \
             VALUES ('acme', '{name}', 1, 'New', '{{}}', 2, now(), now(), {change_seq})"
        )
    };
    // The counter's last number is 1; 2 has not been taken.
    refused(&pool, &insert("new", 2), not_last).await;
    refused(
        &pool,
        "UPDATE messaging.template_version \
         SET status = 2, reviewed_at = now(), change_seq = 2 WHERE name = 'pending'",
        not_last,
    )
    .await;
    refused(
        &pool,
        "UPDATE messaging.template_version \
         SET status = 4, reviewed_at = now(), change_seq = 2 WHERE name = 'approved'",
        not_last,
    )
    .await;
    // Taken in the same transaction, as the store does, the number is accepted.
    sqlx::raw_sql(&format!(
        "UPDATE messaging.template_change SET last_seq = last_seq + 1;
         UPDATE messaging.template_version \
         SET status = 2, reviewed_at = now(), change_seq = 2 WHERE name = 'pending';
         UPDATE messaging.template_change SET last_seq = last_seq + 1;
         {};",
        insert("new", 3)
    ))
    .execute(&pool)
    .await
    .expect("changes with the last number");
    // A number taken earlier is stale.
    sqlx::raw_sql("UPDATE messaging.template_change SET last_seq = last_seq + 1")
        .execute(&pool)
        .await
        .unwrap();
    refused(
        &pool,
        "UPDATE messaging.template_version \
         SET status = 4, reviewed_at = now(), change_seq = 3 WHERE name = 'pending'",
        not_last,
    )
    .await;
    assert_eq!(status(&pool, "pending").await, 2);
    assert_eq!(status(&pool, "new").await, 2);
}

#[tokio::test]
async fn the_change_counter_only_counts_up_by_one() {
    let (_container, pool) = seeded().await;
    let one_by_one = "only counts up by one";
    for sql in [
        "UPDATE messaging.template_change SET last_seq = last_seq + 2",
        "UPDATE messaging.template_change SET last_seq = last_seq - 1",
        "UPDATE messaging.template_change SET last_seq = last_seq",
        "UPDATE messaging.template_change SET last_seq = 0",
    ] {
        refused(&pool, sql, one_by_one).await;
    }
    refused(
        &pool,
        "DELETE FROM messaging.template_change",
        "never deleted",
    )
    .await;
    sqlx::raw_sql("UPDATE messaging.template_change SET last_seq = last_seq + 1")
        .execute(&pool)
        .await
        .expect("counting up");
    let last: i64 = sqlx::query_scalar("SELECT last_seq FROM messaging.template_change")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(last, 2);
}

#[tokio::test]
async fn a_publisher_keeps_its_owner_for_good() {
    let (_container, pool) = seeded().await;
    let owner = "owner never changes";
    refused(
        &pool,
        "UPDATE messaging.template_publisher SET owner_user_id = NULL",
        owner,
    )
    .await;
    refused(
        &pool,
        "UPDATE messaging.template_publisher SET publisher = 'other'",
        owner,
    )
    .await;
    refused(
        &pool,
        "DELETE FROM messaging.template_publisher",
        "never deleted",
    )
    .await;
    let owners: Vec<(String, Option<Uuid>)> =
        sqlx::query_as("SELECT publisher, owner_user_id FROM messaging.template_publisher")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        owners,
        [("acme".to_owned(), Some(Uuid::parse_str(USER).unwrap()))]
    );
}

#[tokio::test]
async fn the_template_tables_are_never_truncated() {
    let (_container, pool) = seeded().await;
    // CASCADE, so that the foreign keys do not refuse it first.
    for table in [
        "template_publisher",
        "template_version",
        "template_change",
        "template_event",
    ] {
        refused(
            &pool,
            &format!("TRUNCATE messaging.{table} CASCADE"),
            "never truncated",
        )
        .await;
    }
    let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM messaging.template_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 3);
}

#[tokio::test]
async fn messages_take_rich_and_answer_content_but_no_other() {
    let (_container, pool) = seeded().await;
    let insert = |seq: i64, content_type: i16, answer_seq: Option<i64>| {
        sqlx::query(
            "INSERT INTO messaging.message \
             (conversation_id, seq, message_id, client_msg_id, kind, sender_id, content_type, \
              body, answer_seq) \
             VALUES ($1, $2, gen_random_uuid(), gen_random_uuid(), 1, $3, $4, \
              '\\x00', $5)",
        )
        .bind(Uuid::parse_str(CONVERSATION).unwrap())
        .bind(seq)
        .bind(Uuid::parse_str(USER).unwrap())
        .bind(content_type)
        .bind(answer_seq)
        .execute(&pool)
    };
    for (seq, content_type) in [(1, 1), (2, 2), (3, 3), (4, 4)] {
        insert(seq, content_type, None)
            .await
            .unwrap_or_else(|error| panic!("content_type {content_type}: {error}"));
    }
    insert(5, 3, Some(6)).await.expect("a card with its answer");
    for (seq, content_type, answer_seq) in [
        (6, 5, None),
        (7, 0, None),
        (8, 3, Some(0)),
        // Only a card is answered, and by a later entry.
        (9, 1, Some(10)),
        (10, 4, Some(11)),
        (11, 3, Some(11)),
        (12, 3, Some(5)),
    ] {
        let error = insert(seq, content_type, answer_seq)
            .await
            .expect_err("refused");
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("23514"),
            "content_type {content_type}, answer_seq {answer_seq:?}: {error}"
        );
    }
}

#[tokio::test]
async fn a_card_is_answered_once_by_a_later_entry() {
    let (_container, pool) = seeded().await;
    sqlx::raw_sql(&format!(
        "INSERT INTO messaging.message \
         (conversation_id, seq, message_id, client_msg_id, kind, sender_id, content_type, body) \
         VALUES ('{CONVERSATION}', 1, gen_random_uuid(), gen_random_uuid(), 1, '{USER}', 3, \
                 '\\x00'), \
                ('{CONVERSATION}', 2, gen_random_uuid(), gen_random_uuid(), 1, '{USER}', 1, \
                 '\\x00')"
    ))
    .execute(&pool)
    .await
    .expect("a card and a text");
    let card = "message_card_answer";
    refused(
        &pool,
        "UPDATE messaging.message SET answer_seq = 3 WHERE seq = 2",
        card,
    )
    .await;
    refused(
        &pool,
        "UPDATE messaging.message SET answer_seq = 1 WHERE seq = 1",
        card,
    )
    .await;
    sqlx::raw_sql("UPDATE messaging.message SET answer_seq = 3 WHERE seq = 1")
        .execute(&pool)
        .await
        .expect("the first answer");
    let answered = "a card's answer never changes";
    refused(
        &pool,
        "UPDATE messaging.message SET answer_seq = 4 WHERE seq = 1",
        answered,
    )
    .await;
    refused(
        &pool,
        "UPDATE messaging.message SET answer_seq = NULL WHERE seq = 1",
        answered,
    )
    .await;
    // Other changes to an answered card, and rewriting the same answer, are allowed.
    sqlx::raw_sql(
        "UPDATE messaging.message SET answer_seq = 3, deleted_at = now(), body = NULL \
         WHERE seq = 1",
    )
    .execute(&pool)
    .await
    .expect("an update that keeps the answer");
    let answer: Option<i64> =
        sqlx::query_scalar("SELECT answer_seq FROM messaging.message WHERE seq = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(answer, Some(3));
}
