//! Fresh database ownership and the data-preserving operator upgrade support unprivileged migrations.

use std::path::Path;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

const PASSWORD: &str = "fixture-only-password";
const TABLES: &[&str] = &[
    "messaging.known_user",
    "messaging.member_device",
    "messaging.contact",
    "messaging.conversation",
    "messaging.conversation_member",
    "messaging.message",
    "messaging.device_cursor",
    "messaging.upgrade_sequence",
    "messaging.upgrade_partition",
    "public._sqlx_migrations",
];

fn upgrade_sql() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("deploy")
        .join("helm")
        .join("push-messenger")
        .join("files")
        .join("postgres")
        .join("messaging-role.sql");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("ownership upgrade SQL must exist at {path:?}: {error}"))
}

async fn pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .expect("connect fixture pool")
}

struct Fixture {
    _container: ContainerAsync<Postgres>,
    endpoint: String,
    admin: PgPool,
}

impl Fixture {
    async fn new() -> Self {
        let container = Postgres::default()
            .with_tag("17-alpine")
            .start()
            .await
            .expect("start PostgreSQL 17 (Docker must be running)");
        let host = container.get_host().await.unwrap().to_string();
        let host = if host == "localhost" {
            "127.0.0.1"
        } else {
            &host
        };
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let endpoint = format!("{host}:{port}");
        let bootstrap = pool(&format!("postgres://postgres:postgres@{endpoint}/postgres")).await;
        bootstrap
            .execute(
                format!(
                    "CREATE ROLE push LOGIN SUPERUSER PASSWORD '{PASSWORD}';
                     CREATE ROLE push_auth LOGIN;
                     CREATE ROLE push_worker LOGIN;"
                )
                .as_str(),
            )
            .await
            .unwrap();
        // CREATE DATABASE cannot run inside a multi-statement implicit transaction.
        for (database, owner) in [
            ("push_messaging", "push"),
            ("push_auth", "push_auth"),
            ("push_worker", "push_worker"),
        ] {
            bootstrap
                .execute(format!("CREATE DATABASE {database} OWNER {owner}").as_str())
                .await
                .unwrap();
            bootstrap
                .execute(format!("REVOKE ALL ON DATABASE {database} FROM PUBLIC").as_str())
                .await
                .unwrap();
        }
        bootstrap.close().await;

        let legacy_url = format!("postgres://push:{PASSWORD}@{endpoint}/push_messaging");
        push_store::migrate_database(&legacy_url).await.unwrap();
        let legacy = pool(&legacy_url).await;
        legacy
            .execute(
                "INSERT INTO messaging.known_user (user_id) VALUES
                     ('00000000-0000-0000-0000-000000000001'),
                     ('00000000-0000-0000-0000-000000000002');
                 INSERT INTO messaging.member_device (device_id, user_id)
                     VALUES ('00000000-0000-0000-0000-000000000003',
                             '00000000-0000-0000-0000-000000000001');
                 INSERT INTO messaging.contact (owner_id, peer_id, state)
                     VALUES ('00000000-0000-0000-0000-000000000001',
                             '00000000-0000-0000-0000-000000000002', 3);
                 INSERT INTO messaging.conversation
                     (conversation_id, kind, title, created_by, last_seq)
                     VALUES ('00000000-0000-0000-0000-000000000004', 2, 'preserve me',
                             '00000000-0000-0000-0000-000000000001', 7);
                 INSERT INTO messaging.conversation_member
                     (conversation_id, user_id, role, visible_from_seq, read_seq)
                     VALUES ('00000000-0000-0000-0000-000000000004',
                             '00000000-0000-0000-0000-000000000001', 1, 1, 5);
                 INSERT INTO messaging.message
                     (conversation_id, seq, message_id, client_msg_id, kind,
                      sender_id, sender_device_id, content_type, body)
                     VALUES ('00000000-0000-0000-0000-000000000004', 7,
                             '00000000-0000-0000-0000-000000000005',
                             '00000000-0000-0000-0000-000000000006', 1,
                             '00000000-0000-0000-0000-000000000001',
                             '00000000-0000-0000-0000-000000000003', 1,
                             decode('00ff010203', 'hex'));
                 INSERT INTO messaging.device_cursor
                     (device_id, conversation_id, delivered_seq)
                     VALUES ('00000000-0000-0000-0000-000000000003',
                             '00000000-0000-0000-0000-000000000004', 6);
                 CREATE TABLE messaging.upgrade_sequence (id bigserial PRIMARY KEY);
                 INSERT INTO messaging.upgrade_sequence DEFAULT VALUES;
                 CREATE TABLE messaging.upgrade_partition (id integer)
                     PARTITION BY RANGE (id);
                 CREATE TABLE messaging.upgrade_partition_child
                     PARTITION OF messaging.upgrade_partition FOR VALUES FROM (0) TO (10);
                 INSERT INTO messaging.upgrade_partition VALUES (3);
                 CREATE VIEW messaging.upgrade_view AS SELECT * FROM messaging.message;
                 CREATE MATERIALIZED VIEW messaging.upgrade_materialized
                     AS SELECT * FROM messaging.device_cursor;
                 CREATE FUNCTION messaging.upgrade_overload(integer) RETURNS integer
                     LANGUAGE sql AS 'SELECT $1';
                 CREATE FUNCTION messaging.upgrade_overload(text) RETURNS text
                     LANGUAGE sql AS 'SELECT $1';",
            )
            .await
            .unwrap();
        legacy.close().await;

        let auth = pool(&format!(
            "postgres://postgres:postgres@{endpoint}/push_auth"
        ))
        .await;
        auth.execute(
            "SET ROLE push_auth;
             CREATE SCHEMA auth;
             CREATE TABLE auth.outbox (id bigint, body bytea);
             INSERT INTO auth.outbox VALUES (1, decode('ff00', 'hex'));
             CREATE FUNCTION auth.cleanup() RETURNS integer LANGUAGE sql AS 'SELECT 1';
             GRANT USAGE ON SCHEMA auth TO push_worker;
             GRANT SELECT, UPDATE ON auth.outbox TO push_worker;
             GRANT EXECUTE ON FUNCTION auth.cleanup() TO push_worker;
             RESET ROLE;
             GRANT CONNECT ON DATABASE push_auth TO push_worker;",
        )
        .await
        .unwrap();
        auth.close().await;
        let admin = pool(&format!(
            "postgres://postgres:postgres@{endpoint}/push_messaging"
        ))
        .await;
        Self {
            _container: container,
            endpoint,
            admin,
        }
    }

    async fn data(&self) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        for table in TABLES {
            rows.push(
                sqlx::query_scalar(&format!(
                    "SELECT row_to_json(t)::text FROM {table} t ORDER BY 1"
                ))
                .fetch_all(&self.admin)
                .await
                .unwrap(),
            );
        }
        rows
    }

    async fn other_databases(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT row_to_json(t)::text FROM (
                 SELECT datname, pg_get_userbyid(datdba) AS owner, datacl::text
                 FROM pg_database WHERE datname <> 'push_messaging'
             ) t ORDER BY 1",
        )
        .fetch_all(&self.admin)
        .await
        .unwrap()
    }

    async fn auth_state(&self) -> Vec<String> {
        let auth = pool(&format!(
            "postgres://postgres:postgres@{}/push_auth",
            self.endpoint
        ))
        .await;
        let rows = sqlx::query_scalar(
            "SELECT row_to_json(t)::text FROM (
                 SELECT 'schema' AS kind, nspowner AS owner, nspacl::text AS acl
                 FROM pg_namespace WHERE nspname = 'auth'
                 UNION ALL
                 SELECT 'table', relowner, relacl::text
                 FROM pg_class WHERE oid = 'auth.outbox'::regclass
                 UNION ALL
                 SELECT 'function', proowner, proacl::text
                 FROM pg_proc WHERE oid = 'auth.cleanup()'::regprocedure
             ) t ORDER BY 1",
        )
        .fetch_all(&auth)
        .await
        .unwrap();
        let body: String = sqlx::query_scalar("SELECT row_to_json(t)::text FROM auth.outbox t")
            .fetch_one(&auth)
            .await
            .unwrap();
        auth.close().await;
        [rows, vec![body]].concat()
    }

    async fn reject(&self, reason: &str) {
        let before = self.data().await;
        let error = self
            .admin
            .execute(upgrade_sql().as_str())
            .await
            .unwrap_err();
        // The script owns an explicit transaction; errors leave this connection aborted.
        self.admin.execute("ROLLBACK").await.unwrap();
        assert!(
            error.to_string().contains(reason),
            "expected {reason:?}, got {error}"
        );
        let owner: String = sqlx::query_scalar(
            "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'push_messaging'",
        )
        .fetch_one(&self.admin)
        .await
        .unwrap();
        assert_eq!(
            owner, "push",
            "rejected upgrade must not transfer ownership"
        );
        assert_eq!(self.data().await, before);
    }
}

#[tokio::test]
async fn fresh_database_owner_can_run_application_migrations_twice() {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start PostgreSQL 17 (Docker must be running)");
    let host = container.get_host().await.unwrap().to_string();
    let host = if host == "localhost" {
        "127.0.0.1"
    } else {
        &host
    };
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let endpoint = format!("{host}:{port}");
    let admin = pool(&format!("postgres://postgres:postgres@{endpoint}/postgres")).await;
    admin
        .execute(
            "CREATE ROLE push_messaging LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
                 NOREPLICATION NOBYPASSRLS PASSWORD 'FreshMessagingTest42'",
        )
        .await
        .unwrap();
    admin
        .execute("CREATE DATABASE push_messaging")
        .await
        .unwrap();
    admin
        .execute(
            "ALTER DATABASE push_messaging OWNER TO push_messaging;
             REVOKE ALL ON DATABASE push_messaging FROM PUBLIC;",
        )
        .await
        .unwrap();
    let flags: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT rolcanlogin, rolsuper, rolcreatedb, rolcreaterole, rolreplication, rolbypassrls
         FROM pg_roles WHERE rolname = 'push_messaging'",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(flags, (true, false, false, false, false, false));
    let owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'push_messaging'",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(owner, "push_messaging");
    let public_access: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT FROM pg_database d, aclexplode(d.datacl) a
             WHERE d.datname = 'push_messaging' AND a.grantee = 0
         )",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert!(!public_access);
    admin.close().await;

    let url = format!("postgres://push_messaging:FreshMessagingTest42@{endpoint}/push_messaging");
    let messaging = pool(&url).await;
    let schema: Option<String> = sqlx::query_scalar("SELECT to_regnamespace('messaging')::text")
        .fetch_one(&messaging)
        .await
        .unwrap();
    assert!(
        schema.is_none(),
        "bootstrap must leave app migrations to the service"
    );
    for _ in 0..2 {
        push_store::migrate_database(&url).await.unwrap();
    }
    let table_owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE oid = 'messaging.message'::regclass",
    )
    .fetch_one(&messaging)
    .await
    .unwrap();
    assert_eq!(table_owner, "push_messaging");
    messaging.close().await;
}

#[tokio::test]
async fn upgrade_preserves_data_and_supports_unprivileged_migrations() {
    let sql = upgrade_sql();
    let fixture = Fixture::new().await;
    let before = fixture.data().await;
    let databases = fixture.other_databases().await;
    let auth = fixture.auth_state().await;
    for _ in 0..2 {
        fixture.admin.execute(sql.as_str()).await.unwrap();
        assert_eq!(fixture.data().await, before);
        assert_eq!(fixture.other_databases().await, databases);
        assert_eq!(fixture.auth_state().await, auth);
    }
    let password: Option<String> =
        sqlx::query_scalar("SELECT rolpassword FROM pg_authid WHERE rolname = 'push_messaging'")
            .fetch_one(&fixture.admin)
            .await
            .unwrap();
    assert!(password.is_none(), "upgrade must not provision a password");
    let flags: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT rolcanlogin, rolsuper, rolcreatedb, rolcreaterole, rolreplication, rolbypassrls
         FROM pg_roles WHERE rolname = 'push_messaging'",
    )
    .fetch_one(&fixture.admin)
    .await
    .unwrap();
    assert_eq!(flags, (true, false, false, false, false, false));
    let wrong_owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT relowner AS owner FROM pg_class
             WHERE relnamespace = 'messaging'::regnamespace
                OR oid = 'public._sqlx_migrations'::regclass
             UNION ALL
             SELECT proowner FROM pg_proc WHERE pronamespace = 'messaging'::regnamespace
             UNION ALL
             SELECT nspowner FROM pg_namespace WHERE nspname = 'messaging'
             UNION ALL
             SELECT datdba FROM pg_database WHERE datname = 'push_messaging'
         ) t WHERE owner <> 'push_messaging'::regrole",
    )
    .fetch_one(&fixture.admin)
    .await
    .unwrap();
    assert_eq!(wrong_owners, 0);
    let public_owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname = 'public'",
    )
    .fetch_one(&fixture.admin)
    .await
    .unwrap();
    assert_eq!(public_owner, "pg_database_owner");
    let public_connect: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT FROM pg_database d, aclexplode(d.datacl) a
             WHERE d.datname = 'push_messaging' AND a.grantee = 0
         )",
    )
    .fetch_one(&fixture.admin)
    .await
    .unwrap();
    assert!(!public_connect);
    fixture
        .admin
        .execute(format!("ALTER ROLE push_messaging PASSWORD '{PASSWORD}'").as_str())
        .await
        .unwrap();
    let password: String =
        sqlx::query_scalar("SELECT rolpassword FROM pg_authid WHERE rolname = 'push_messaging'")
            .fetch_one(&fixture.admin)
            .await
            .unwrap();
    fixture.admin.execute(sql.as_str()).await.unwrap();
    let retained: String =
        sqlx::query_scalar("SELECT rolpassword FROM pg_authid WHERE rolname = 'push_messaging'")
            .fetch_one(&fixture.admin)
            .await
            .unwrap();
    assert_eq!(retained, password, "upgrade must not rotate a password");
    fixture.admin.close().await;

    let url = format!(
        "postgres://push_messaging:{PASSWORD}@{}/push_messaging",
        fixture.endpoint
    );
    push_store::migrate_database(&url).await.unwrap();
    let messaging = pool(&url).await;
    let history: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT body FROM messaging.message WHERE seq = 7")
            .fetch_one(&messaging)
            .await
            .unwrap();
    assert_eq!(history, Some(vec![0, 255, 1, 2, 3]));
    messaging
        .execute(
            "CREATE TABLE messaging.future_migration (id bigserial PRIMARY KEY);
             DROP TABLE messaging.future_migration;",
        )
        .await
        .unwrap();
    for database in ["push_auth", "push_worker"] {
        let allowed: bool =
            sqlx::query_scalar("SELECT has_database_privilege(current_user, $1, 'CONNECT')")
                .bind(database)
                .fetch_one(&messaging)
                .await
                .unwrap();
        assert!(!allowed);
        let denied = PgPoolOptions::new()
            .max_connections(1)
            .connect(&format!(
                "postgres://push_messaging:{PASSWORD}@{}/{database}",
                fixture.endpoint
            ))
            .await
            .expect_err("messaging role cannot connect to another service's database");
        assert_eq!(
            denied.as_database_error().unwrap().code().as_deref(),
            Some("42501")
        );
    }
    messaging.close().await;
}

#[tokio::test]
async fn rejects_preexisting_superuser() {
    let fixture = Fixture::new().await;
    fixture
        .admin
        .execute("CREATE ROLE push_messaging LOGIN SUPERUSER")
        .await
        .unwrap();
    fixture.reject("incompatible push_messaging role").await;
}

#[tokio::test]
async fn rejects_active_clients_before_creating_role() {
    let fixture = Fixture::new().await;
    let extra = pool(&format!(
        "postgres://postgres:postgres@{}/push_messaging",
        fixture.endpoint
    ))
    .await;
    fixture.reject("other client backends").await;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT FROM pg_roles WHERE rolname = 'push_messaging')")
            .fetch_one(&fixture.admin)
            .await
            .unwrap();
    assert!(!exists);
    extra.close().await;
}

#[tokio::test]
async fn rejects_cross_service_connect_without_partial_transfer() {
    let fixture = Fixture::new().await;
    fixture
        .admin
        .execute(
            "CREATE ROLE push_messaging LOGIN;
             GRANT CONNECT ON DATABASE push_auth TO push_messaging;",
        )
        .await
        .unwrap();
    fixture.reject("access to auth/worker databases").await;
}

#[tokio::test]
async fn rejects_role_settings_and_memberships() {
    let fixture = Fixture::new().await;
    fixture
        .admin
        .execute(
            "CREATE ROLE push_messaging LOGIN;
             ALTER ROLE push_messaging SET search_path = messaging;",
        )
        .await
        .unwrap();
    fixture.reject("incompatible push_messaging role").await;
    fixture
        .admin
        .execute(
            "ALTER ROLE push_messaging RESET ALL;
             ALTER ROLE push_messaging IN DATABASE push_messaging SET search_path = messaging;",
        )
        .await
        .unwrap();
    fixture.reject("incompatible push_messaging role").await;
    fixture
        .admin
        .execute(
            "ALTER ROLE push_messaging IN DATABASE push_messaging RESET ALL;
             GRANT push_auth TO push_messaging;",
        )
        .await
        .unwrap();
    fixture.reject("role memberships").await;
    fixture
        .admin
        .execute("REVOKE push_auth FROM push_messaging; GRANT push_messaging TO push_worker")
        .await
        .unwrap();
    fixture.reject("role memberships").await;
}

#[tokio::test]
async fn rejects_unexpected_application_owners() {
    let fixture = Fixture::new().await;
    fixture
        .admin
        .execute("ALTER TABLE messaging.message OWNER TO push_auth")
        .await
        .unwrap();
    fixture.reject("unexpected application object owner").await;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT FROM pg_roles WHERE rolname = 'push_messaging')")
            .fetch_one(&fixture.admin)
            .await
            .unwrap();
    assert!(!exists, "role creation must also roll back");
    fixture
        .admin
        .execute(
            "ALTER TABLE messaging.message OWNER TO push;
             CREATE DOMAIN public.unexpected_domain AS integer;
             ALTER DOMAIN public.unexpected_domain OWNER TO push_auth;",
        )
        .await
        .unwrap();
    fixture.reject("unexpected application object owner").await;
}

#[tokio::test]
async fn rejects_unexpected_database_and_schema_owners() {
    let fixture = Fixture::new().await;
    fixture
        .admin
        .execute("ALTER DATABASE push_messaging OWNER TO push_auth")
        .await
        .unwrap();
    let error = fixture
        .admin
        .execute(upgrade_sql().as_str())
        .await
        .unwrap_err();
    fixture.admin.execute("ROLLBACK").await.unwrap();
    assert!(error.to_string().contains("unexpected database owner"));
    let owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'push_messaging'",
    )
    .fetch_one(&fixture.admin)
    .await
    .unwrap();
    assert_eq!(owner, "push_auth");
    fixture
        .admin
        .execute(
            "ALTER DATABASE push_messaging OWNER TO push;
             ALTER SCHEMA public OWNER TO push_auth;",
        )
        .await
        .unwrap();
    fixture.reject("unexpected application schema owner").await;
    fixture
        .admin
        .execute(
            "ALTER SCHEMA public OWNER TO pg_database_owner;
             ALTER SCHEMA messaging OWNER TO push_auth;",
        )
        .await
        .unwrap();
    fixture.reject("unexpected application schema owner").await;
    fixture
        .admin
        .execute(
            "ALTER SCHEMA messaging OWNER TO push;
             CREATE ROLE push_messaging LOGIN;
             ALTER DATABASE push_worker OWNER TO push_messaging;",
        )
        .await
        .unwrap();
    // Ownership itself grants CONNECT; the cross-service check must catch it.
    fixture.reject("access to auth/worker databases").await;
}
