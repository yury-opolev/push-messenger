//! The `auth` schema on a real PostgreSQL (Docker must be running): the migrations run as
//! the database's owner, a plain role like the chart's, the constraints hold, and the
//! worker's role may drain the outbox and run the cleanups, and nothing else.

use push_auth_core::{MIGRATOR, grant_worker_access, migrate_database};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

/// A running Postgres with a database `push_auth` owned by the plain role `push_auth`,
/// a plain role `push_worker`, and the URL to connect as `push_auth`.
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
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
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
        [
            "agent_enrollment",
            "device",
            "external_identity",
            "outbox",
            "refresh_token",
            "user_account"
        ]
    );
}

#[tokio::test]
async fn agents_have_an_immutable_human_owner_and_no_human_credentials() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    let owner = insert_user(&pool, "owner@example.com", "owner")
        .await
        .unwrap();
    let other = insert_user(&pool, "other@example.com", "other")
        .await
        .unwrap();
    let agent = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.user_account (user_id,handle,display_name,kind,owner_user_id) \
         VALUES ($1,'agent','Agent','agent',$2)",
    )
    .bind(agent)
    .bind(owner)
    .execute(&pool)
    .await
    .expect("pending agent has no email or password");
    for statement in [
        "UPDATE auth.user_account SET owner_user_id=$2 WHERE user_id=$1",
        "UPDATE auth.user_account SET kind='user',owner_user_id=NULL,email='agent@example.com',password_hash='hash' WHERE user_id=$1",
        "UPDATE auth.user_account SET password_hash='hash' WHERE user_id=$1",
        "UPDATE auth.user_account SET owner_user_id=user_id WHERE user_id=$1",
    ] {
        let query = sqlx::query(statement).bind(agent);
        let query = if statement.contains("$2") {
            query.bind(other)
        } else {
            query
        };
        let error = query.execute(&pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
    let error = sqlx::query(
        "INSERT INTO auth.user_account (user_id,handle,display_name,kind,owner_user_id) \
         VALUES ($1,'nested_agent','Nested','agent',$2)",
    )
    .bind(Uuid::now_v7())
    .bind(agent)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    assert!(
        sqlx::query("UPDATE auth.user_account SET disabled_at=now() WHERE user_id=$1")
            .bind(owner)
            .execute(&pool)
            .await
            .is_err(),
        "only agents have a disabled tombstone"
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
        "bob-",
        "a--b",
        "-bob",
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

/// The URL of the same database as the worker's role.
fn worker_url(url: &str) -> String {
    url.replacen("push_auth:auth-secret@", "push_worker:worker-secret@", 1)
}

/// The SQLSTATE of a failed statement.
fn sqlstate(error: sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .unwrap_or_default()
        .into_owned()
}

#[tokio::test]
async fn the_worker_may_drain_the_outbox_and_nothing_else() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant");
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant again");
    let event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, 'user.verified', '{}')",
    )
    .bind(event)
    .execute(&pool)
    .await
    .expect("an event");

    let worker = PgPool::connect(&worker_url(&url))
        .await
        .expect("connect as the worker");
    let drained: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM auth.outbox WHERE event_id IN (SELECT event_id FROM auth.outbox \
         ORDER BY created_at, event_id LIMIT 10 FOR UPDATE SKIP LOCKED) RETURNING event_id",
    )
    .fetch_all(&worker)
    .await
    .expect("drain the outbox");
    assert_eq!(drained, [event]);
    for statement in [
        "SELECT count(*) FROM auth.user_account",
        "SELECT count(*) FROM auth.refresh_token",
        "INSERT INTO auth.outbox (event_id, kind, payload) \
         VALUES (gen_random_uuid(), 'user.verified', '{}')",
        "UPDATE auth.outbox SET payload = '{}'",
    ] {
        let error = sqlx::query(statement)
            .execute(&worker)
            .await
            .expect_err(statement);
        assert_eq!(sqlstate(error), "42501", "{statement}");
    }
}

#[tokio::test]
async fn the_cleanups_delete_expired_tokens_and_stale_unverified_accounts() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant");
    let alice = insert_user(&pool, "alice@example.com", "alice")
        .await
        .expect("alice");
    sqlx::query("UPDATE auth.user_account SET email_verified_at = now() WHERE user_id = $1")
        .bind(alice)
        .execute(&pool)
        .await
        .expect("verify alice");
    let device = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.device (device_id, user_id, name, platform, registration_id) \
         VALUES ($1, $2, 'Phone', 'ios', $1)",
    )
    .bind(device)
    .bind(alice)
    .execute(&pool)
    .await
    .expect("a device");
    for (hash, expires) in [([1u8; 32], "-1 day"), ([2u8; 32], "1 day")] {
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + $3::interval)",
        )
        .bind(hash.as_slice())
        .bind(device)
        .bind(expires)
        .execute(&pool)
        .await
        .expect("a refresh token");
    }
    // Unverified for 8 days, unverified since now, and verified 8 days ago.
    for (email, handle, verified) in [
        ("bob@example.com", "bob", false),
        ("carol@example.com", "carol", false),
        ("dave@example.com", "dave", true),
    ] {
        let user = insert_user(&pool, email, handle).await.expect(handle);
        if handle != "carol" {
            sqlx::query(
                "UPDATE auth.user_account SET created_at = now() - interval '8 days', \
                 email_verified_at = CASE WHEN $2 THEN now() END WHERE user_id = $1",
            )
            .bind(user)
            .bind(verified)
            .execute(&pool)
            .await
            .expect("age the account");
        }
    }

    let worker = PgPool::connect(&worker_url(&url))
        .await
        .expect("connect as the worker");
    let expired = |batch: i32| {
        sqlx::query_scalar::<_, i32>("SELECT auth.delete_expired_refresh_tokens($1)")
            .bind(batch)
            .fetch_one(&worker)
    };
    assert_eq!(expired(100).await.expect("tokens"), 1);
    assert_eq!(expired(100).await.expect("tokens again"), 0);
    // A NULL batch or an age under a day cannot touch carol, unverified since now.
    let misused: Option<i32> =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '0 seconds', NULL)")
            .fetch_one(&worker)
            .await
            .expect("a NULL batch");
    assert_eq!(misused, None);
    let misused: i32 =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '-1 day', 100)")
            .fetch_one(&worker)
            .await
            .expect("a negative age");
    assert_eq!(misused, 1, "only the 8-day-old account, never carol");
    let unverified: i32 =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '7 days', 100)")
            .fetch_one(&worker)
            .await
            .expect("accounts");
    assert_eq!(unverified, 0);

    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.refresh_token")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, 1, "the live token stays");
    let handles: Vec<String> =
        sqlx::query_scalar("SELECT handle::text FROM auth.user_account ORDER BY handle")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(handles, ["alice", "carol", "dave"]);
}

#[tokio::test]
async fn an_external_account_has_no_email_or_password_and_the_cleanup_keeps_it() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant");
    // Every account was created 8 days ago.
    let account = |handle: &'static str,
                   email: Option<&'static str>,
                   password: Option<&'static str>,
                   active: bool| {
        let pool = pool.clone();
        async move {
            let user = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO auth.user_account \
                 (user_id, email, handle, display_name, password_hash, email_verified_at, created_at) \
                 VALUES ($1, $2, $3, 'Someone', $4, CASE WHEN $5 THEN now() END, \
                 now() - interval '8 days')",
            )
            .bind(user)
            .bind(email)
            .bind(handle)
            .bind(password)
            .bind(active)
            .execute(&pool)
            .await
            .map(|_| user)
        }
    };
    let erin = account("erin", None, None, true)
        .await
        .expect("an active external account");
    for (handle, email, password, active) in [
        ("fay", None, None, false),
        ("gus", None, Some("hash"), true),
        ("hal", Some("hal@example.com"), None, true),
    ] {
        let error = account(handle, email, password, active)
            .await
            .expect_err(handle);
        assert_eq!(constraint(error), "user_account_kind", "{handle}");
    }

    let tenant = Uuid::now_v7();
    let identity = |user: Uuid, subject: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO auth.external_identity \
                 (identity_id, provider, tenant_id, subject, user_id) \
                 VALUES ($1, 'entra', $2, $3, $4)",
            )
            .bind(Uuid::now_v7())
            .bind(tenant)
            .bind(subject)
            .bind(user)
            .execute(&pool)
            .await
        }
    };
    identity(erin, "oid-1").await.expect("an identity");
    let ivy = account("ivy", None, None, true).await.expect("ivy");
    assert_eq!(
        constraint(identity(ivy, "oid-1").await.expect_err("the same subject")),
        "external_identity_provider_tenant_id_subject_key"
    );
    assert_eq!(
        constraint(
            identity(erin, "oid-2")
                .await
                .expect_err("a second identity")
        ),
        "external_identity_user_id_key"
    );

    // An email/password account unverified for 8 days is deleted; external ones are kept.
    account("joe", Some("joe@example.com"), Some("hash"), false)
        .await
        .expect("joe");
    let worker = PgPool::connect(&worker_url(&url))
        .await
        .expect("connect as the worker");
    let deleted: i32 =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '7 days', 100)")
            .fetch_one(&worker)
            .await
            .expect("the cleanup");
    assert_eq!(deleted, 1);
    let left: Vec<String> =
        sqlx::query_scalar("SELECT handle::text FROM auth.user_account ORDER BY handle")
            .fetch_all(&pool)
            .await
            .expect("the accounts");
    assert_eq!(left, ["erin", "ivy"]);
}

/// Plan 13A's Entra identities survive migration 0005 unchanged (spec D8, AC6), the new
/// rules hold (any provider id of the pattern, an issuer of 1 to 256 characters, one
/// identity per (provider, issuer, subject)), and Plan 13A's SQL keeps working beside the
/// new code during a rolling update.
#[tokio::test]
async fn migration_0005_keeps_entra_identities_and_admits_other_providers() {
    const TENANT: &str = "33333333-0000-4000-8000-000000000003";
    const OID: &str = "55555555-0000-4000-8000-000000000005";
    let (_container, url) = auth_database().await;
    // Versions 1 to 4 only: the database as Plan 13A left it. (sqlx's `Migrator` fields
    // are public but semver-exempt; the version is pinned by Cargo.lock.)
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version <= 4)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    let pool = PgPool::connect(&url).await.expect("connect");
    before.run(&pool).await.expect("migrations 1 to 4");
    let account = |pool: PgPool| async move {
        let user = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO auth.user_account \
             (user_id, email, handle, display_name, password_hash, email_verified_at) \
             VALUES ($1, NULL, $2, 'Someone', NULL, now())",
        )
        .bind(user)
        .bind(format!("u{}", &user.simple().to_string()[20..]))
        .execute(&pool)
        .await
        .expect("an external account");
        user
    };
    let erin = account(pool.clone()).await;
    let identity = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.external_identity \
         (identity_id, provider, tenant_id, subject, user_id, email) \
         VALUES ($1, 'entra', $2, $3, $4, 'erin@contoso.test')",
    )
    .bind(identity)
    .bind(Uuid::parse_str(TENANT).unwrap())
    .bind(OID)
    .bind(erin)
    .execute(&pool)
    .await
    .expect("a Plan 13A identity");

    migrate_database(&url).await.expect("migration 0005");
    let row: (Uuid, String, String, String, Uuid, Option<String>) = sqlx::query_as(
        "SELECT identity_id, provider, issuer, subject, user_id, email \
         FROM auth.external_identity",
    )
    .fetch_one(&pool)
    .await
    .expect("the identity");
    assert_eq!(
        row,
        (
            identity,
            "entra".to_owned(),
            TENANT.to_owned(),
            OID.to_owned(),
            erin,
            Some("erin@contoso.test".to_owned())
        )
    );

    let link = |provider: String, issuer: String| {
        let pool = pool.clone();
        async move {
            let user = account(pool.clone()).await;
            sqlx::query(
                "INSERT INTO auth.external_identity \
                 (identity_id, provider, issuer, subject, user_id) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(Uuid::now_v7())
            .bind(provider)
            .bind(issuer)
            .bind(OID)
            .bind(user)
            .execute(&pool)
            .await
        }
    };
    let issuer = "https://id.example.com/".to_owned();
    // Another provider with a URL issuer, and the same subject: another identity.
    link("acme-id".to_owned(), issuer.clone())
        .await
        .expect("another provider");
    link("acme-id".to_owned(), TENANT.to_owned())
        .await
        .expect("another issuer");
    assert_eq!(
        constraint(
            link("acme-id".to_owned(), issuer.clone())
                .await
                .expect_err("the same identity")
        ),
        "external_identity_provider_issuer_subject_key"
    );
    // Erin's identity again: refused by either key (the trigger fills tenant_id too).
    let again = constraint(
        link("entra".to_owned(), TENANT.to_owned())
            .await
            .expect_err("erin's identity"),
    );
    assert!(
        again == "external_identity_provider_issuer_subject_key"
            || again == "external_identity_provider_tenant_id_subject_key",
        "{again}"
    );

    // During the rollout, Plan 13A's pods still run their own SQL on the new schema, and
    // both kinds of pods see each other's Entra ID identities.
    let gus = account(pool.clone()).await;
    let old_tenant = Uuid::now_v7();
    let inserted: Option<i32> = sqlx::query_scalar(
        "INSERT INTO auth.external_identity \
         (identity_id, provider, tenant_id, subject, user_id, email) \
         VALUES ($1, 'entra', $2, 'oid-gus', $3, NULL) \
         ON CONFLICT (provider, tenant_id, subject) DO NOTHING RETURNING 1",
    )
    .bind(Uuid::now_v7())
    .bind(old_tenant)
    .bind(gus)
    .fetch_optional(&pool)
    .await
    .expect("Plan 13A's insert");
    assert_eq!(inserted, Some(1));
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM auth.external_identity \
         WHERE provider = 'entra' AND issuer = $1 AND subject = 'oid-gus'",
    )
    .bind(old_tenant.to_string())
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(found, Some(gus), "a new pod finds an old pod's identity");
    let hal = account(pool.clone()).await;
    let new_tenant = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.external_identity \
         (identity_id, provider, issuer, subject, user_id) VALUES ($1, 'entra', $2, 'oid-hal', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(new_tenant.to_string())
    .bind(hal)
    .execute(&pool)
    .await
    .expect("a new pod's insert");
    let found: Option<Uuid> = sqlx::query_scalar(
        "UPDATE auth.external_identity SET last_sign_in_at = now() \
         WHERE provider = 'entra' AND tenant_id = $1 AND subject = 'oid-hal' RETURNING user_id",
    )
    .bind(new_tenant)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(found, Some(hal), "an old pod finds a new pod's identity");
    let other: Option<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM auth.external_identity WHERE provider = 'acme-id' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(other, None, "other providers have no tenant");
    for provider in ["Acme", "9acme", "acme_id", &"a".repeat(33)] {
        assert_eq!(
            constraint(
                link(provider.to_owned(), issuer.clone())
                    .await
                    .expect_err(provider)
            ),
            "external_identity_provider_check",
            "{provider}"
        );
    }
    for issuer in [String::new(), "x".repeat(257)] {
        assert_eq!(
            constraint(
                link("acme-id".to_owned(), issuer.clone())
                    .await
                    .expect_err("issuer length")
            ),
            "external_identity_issuer_check",
            "{} characters",
            issuer.len()
        );
    }
}

/// Migration 0007 widens the handle CHECK to the rule of `rules::handle`: hyphens, but not
/// at the end and never two in a row. Handles stored before it stay valid.
#[tokio::test]
async fn migration_0007_admits_single_hyphens_and_keeps_existing_handles() {
    let (_container, url) = auth_database().await;
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version <= 6)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    let pool = PgPool::connect(&url).await.expect("connect");
    before.run(&pool).await.expect("migrations 1 to 6");
    let longest = "a".repeat(30);
    for handle in ["abc", "alice_99", longest.as_str()] {
        insert_user(&pool, &format!("{handle}@example.com"), handle)
            .await
            .expect(handle);
    }

    migrate_database(&url).await.expect("migration 0007");
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.user_account")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(kept, 3);
    let hyphenated = format!("{}-b", "a".repeat(28));
    for handle in [
        "a-b",
        "has-dash",
        "yury-opolev",
        "a-1_b-c",
        hyphenated.as_str(),
    ] {
        insert_user(&pool, &format!("{}@example.com", Uuid::now_v7()), handle)
            .await
            .expect(handle);
    }
    let too_long = format!("{}-b", "a".repeat(29));
    for handle in ["bob-", "a--b", "-bob", "a-", "ab", "_ab", too_long.as_str()] {
        let error = insert_user(&pool, &format!("{}@example.com", Uuid::now_v7()), handle)
            .await
            .expect_err(handle);
        assert_eq!(constraint(error), "user_account_handle_check", "{handle}");
    }
}
