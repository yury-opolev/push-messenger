-- Operator-run upgrade for an existing bundled PostgreSQL installation, not bootstrap.
-- Stop all messaging clients first and keep them stopped until this commits. The
-- advisory lock serializes this script, not new connections or external provisioning.
-- Run on push_messaging as a superuser. Provision the login's password separately.
-- On any error, issue ROLLBACK on the same connection before inspecting/retrying.
BEGIN;
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '60s';
SET LOCAL search_path = pg_catalog;
SELECT pg_advisory_xact_lock(1123581321);

DO $upgrade$
DECLARE
    legacy oid;
    messaging_role oid;
    object record;
BEGIN
    IF current_database() <> 'push_messaging' THEN
        RAISE EXCEPTION 'must run on push_messaging';
    END IF;
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = current_user AND rolsuper) THEN
        RAISE EXCEPTION 'must run as an administrator/superuser';
    END IF;
    IF EXISTS (
        SELECT FROM pg_stat_activity
        WHERE datname = current_database()
          AND pid <> pg_backend_pid() AND backend_type = 'client backend'
    ) THEN
        RAISE EXCEPTION 'other client backends are connected to push_messaging';
    END IF;

    SELECT oid INTO legacy FROM pg_roles WHERE rolname = 'push';
    IF legacy IS NULL
       OR to_regnamespace('messaging') IS NULL
       OR to_regclass('public._sqlx_migrations') IS NULL
       OR NOT EXISTS (SELECT FROM pg_database WHERE datname = 'push_auth')
       OR NOT EXISTS (SELECT FROM pg_database WHERE datname = 'push_worker') THEN
        RAISE EXCEPTION 'requires legacy push role, messaging schema, SQLx history and auth/worker databases';
    END IF;

    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'push_messaging') THEN
        CREATE ROLE push_messaging LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
            NOREPLICATION NOBYPASSRLS;
    END IF;
    SELECT oid INTO messaging_role FROM pg_roles WHERE rolname = 'push_messaging';
    IF EXISTS (
        SELECT FROM pg_roles
        WHERE oid = messaging_role
          AND (NOT rolcanlogin OR rolsuper OR rolcreatedb OR rolcreaterole
               OR rolreplication OR rolbypassrls OR rolconfig IS NOT NULL)
    ) OR EXISTS (
        SELECT FROM pg_db_role_setting WHERE setrole = messaging_role
    ) THEN
        RAISE EXCEPTION 'incompatible push_messaging role';
    END IF;
    IF EXISTS (
        SELECT FROM pg_auth_members
        WHERE member = messaging_role OR roleid = messaging_role
    ) THEN
        RAISE EXCEPTION 'push_messaging must not have role memberships or granted members';
    END IF;
    IF has_database_privilege(messaging_role, 'push_auth', 'CONNECT')
       OR has_database_privilege(messaging_role, 'push_worker', 'CONNECT') THEN
        RAISE EXCEPTION 'push_messaging has access to auth/worker databases';
    END IF;
    IF EXISTS (
        SELECT FROM pg_database
        WHERE (datname = 'push_messaging' AND datdba NOT IN (legacy, messaging_role))
           OR (datname <> 'push_messaging' AND datdba = messaging_role)
    ) THEN
        RAISE EXCEPTION 'unexpected database owner';
    END IF;
    IF EXISTS (
        SELECT FROM pg_namespace
        WHERE (nspname = 'messaging' AND nspowner NOT IN (legacy, messaging_role))
           OR (nspname = 'public'
               AND nspowner NOT IN (legacy, messaging_role, 'pg_database_owner'::regrole))
    ) THEN
        RAISE EXCEPTION 'unexpected application schema owner';
    END IF;
    IF EXISTS (
        SELECT FROM pg_class c
        WHERE (c.relnamespace = 'messaging'::regnamespace
               OR c.oid = 'public._sqlx_migrations'::regclass)
          AND c.relowner NOT IN (legacy, messaging_role)
    ) OR EXISTS (
        SELECT FROM pg_proc p
        WHERE p.pronamespace = 'messaging'::regnamespace
          AND p.proowner NOT IN (legacy, messaging_role)
    ) OR EXISTS (
        SELECT FROM pg_type t
        WHERE t.typnamespace = 'messaging'::regnamespace
          AND t.typowner NOT IN (legacy, messaging_role)
    ) OR EXISTS (
        -- Public extension objects are deliberately outside the ownership transfer.
        SELECT FROM pg_class c
        WHERE c.relnamespace = 'public'::regnamespace
          AND c.relowner NOT IN (legacy, messaging_role)
          AND NOT EXISTS (
              SELECT FROM pg_depend d WHERE d.classid = 'pg_class'::regclass
                AND d.objid = c.oid AND d.deptype = 'e'
          )
    ) OR EXISTS (
        SELECT FROM pg_proc p
        WHERE p.pronamespace = 'public'::regnamespace
          AND p.proowner NOT IN (legacy, messaging_role)
          AND NOT EXISTS (
              SELECT FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass
                AND d.objid = p.oid AND d.deptype = 'e'
          )
    ) OR EXISTS (
        -- Check standalone public types; generated arrays/composites follow parents.
        SELECT FROM pg_type t
        WHERE t.typnamespace = 'public'::regnamespace
          AND t.typrelid = 0 AND t.typelem = 0
          AND t.typowner NOT IN (legacy, messaging_role)
          AND NOT EXISTS (
              SELECT FROM pg_depend d WHERE d.classid = 'pg_type'::regclass
                AND d.objid = t.oid AND d.deptype = 'e'
          )
    ) THEN
        RAISE EXCEPTION 'unexpected application object owner';
    END IF;
    -- Do not silently leave unsupported objects behind or alter extension ownership.
    IF EXISTS (
        SELECT FROM pg_class c
        WHERE c.relnamespace = 'messaging'::regnamespace
          AND c.relkind NOT IN ('r', 'p', 'v', 'm', 'S', 'i', 'I')
    ) OR EXISTS (
        SELECT FROM pg_proc p
        WHERE p.pronamespace = 'messaging'::regnamespace AND p.prokind NOT IN ('f', 'w')
    ) OR EXISTS (
        SELECT FROM pg_type t
        WHERE t.typnamespace = 'messaging'::regnamespace
          AND t.typrelid = 0 AND t.typelem = 0
    ) OR EXISTS (
        SELECT FROM pg_depend d
        WHERE d.deptype = 'e'
          AND ((d.classid = 'pg_namespace'::regclass
                AND d.objid = 'messaging'::regnamespace)
            OR (d.classid = 'pg_class'::regclass AND d.objid IN (
                SELECT oid FROM pg_class WHERE relnamespace = 'messaging'::regnamespace
                   OR oid = 'public._sqlx_migrations'::regclass
            ))
            OR (d.classid = 'pg_proc'::regclass AND d.objid IN (
                SELECT oid FROM pg_proc WHERE pronamespace = 'messaging'::regnamespace
            )))
    ) OR NOT EXISTS (
        SELECT FROM pg_class
        WHERE oid = 'public._sqlx_migrations'::regclass AND relkind = 'r'
    ) THEN
        RAISE EXCEPTION 'unsupported application objects require manual review';
    END IF;

    -- Never REASSIGN OWNED: it can transfer shared databases owned by legacy push.
    ALTER DATABASE push_messaging OWNER TO push_messaging;
    ALTER SCHEMA messaging OWNER TO push_messaging;
    ALTER SCHEMA public OWNER TO pg_database_owner;
    FOR object IN
        SELECT n.nspname, c.relname, c.relkind
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE (n.nspname = 'messaging' AND c.relkind IN ('r', 'p', 'v', 'm', 'S'))
           OR c.oid = 'public._sqlx_migrations'::regclass
        -- Owned sequences cannot change owners until their owning tables do.
        ORDER BY CASE WHEN c.relkind = 'S' THEN 1 ELSE 0 END, c.oid
    LOOP
        EXECUTE format('ALTER %s %I.%I OWNER TO push_messaging',
            CASE object.relkind
                WHEN 'S' THEN 'SEQUENCE'
                WHEN 'v' THEN 'VIEW'
                WHEN 'm' THEN 'MATERIALIZED VIEW'
                ELSE 'TABLE'
            END, object.nspname, object.relname);
    END LOOP;
    FOR object IN
        SELECT p.oid::regprocedure AS signature
        FROM pg_proc p WHERE p.pronamespace = 'messaging'::regnamespace
    LOOP
        EXECUTE format('ALTER FUNCTION %s OWNER TO push_messaging', object.signature);
    END LOOP;
    REVOKE ALL ON DATABASE push_messaging FROM PUBLIC;
END
$upgrade$;
COMMIT;
