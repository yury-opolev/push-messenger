-- Rich message templates and the template registry (REQUEST PUSH feature 8; spec
-- docs/superpowers/specs/2026-10-26-rich-message-templates-design.md).
--
-- New enumeration values; push-store's codes module maps them:
--   message.content_type       3 rich, 4 answer (1 text, 2 system event as before)
--   template_version.status    1 pending, 2 approved, 3 rejected, 4 revoked

-- Metadata only: one brief ACCESS EXCLUSIVE lock, no table scan or rewrite. The new
-- constraints are NOT VALID (enforced for new and updated rows at once; existing rows
-- are checked by 0006 without blocking reads and writes) and the new column is nullable
-- without a default. Give up rather than queue behind long transactions and stall every
-- statement waiting behind this one.
SET LOCAL lock_timeout = '5s';
-- Cards: answer_seq is the seq of the first valid answer. Only a card is answered, by a
-- later entry.
ALTER TABLE messaging.message
    DROP CONSTRAINT message_content_type_check,
    ADD CONSTRAINT message_content_type_check CHECK (content_type BETWEEN 1 AND 4) NOT VALID,
    ADD COLUMN answer_seq bigint,
    ADD CONSTRAINT message_card_answer
        CHECK (answer_seq IS NULL OR (content_type = 3 AND answer_seq > seq)) NOT VALID;

-- A card is answered once: its answer is never replaced or cleared.
CREATE FUNCTION messaging.message_answer_guard()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION 'a card''s answer never changes' USING ERRCODE = '23514';
END
$$;
CREATE TRIGGER message_answer_guard
    BEFORE UPDATE OF answer_seq ON messaging.message
    FOR EACH ROW
    WHEN (OLD.answer_seq IS NOT NULL AND NEW.answer_seq IS DISTINCT FROM OLD.answer_seq)
    EXECUTE FUNCTION messaging.message_answer_guard();

-- Who may publish under a name: the first account to submit claims it.
CREATE TABLE messaging.template_publisher (
    publisher     text PRIMARY KEY CHECK (publisher ~ '^[a-z][a-z0-9-]{0,31}$'),
    -- NULL: the server's own (built-ins).
    owner_user_id uuid REFERENCES messaging.known_user,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE messaging.template_version (
    publisher    text NOT NULL REFERENCES messaging.template_publisher,
    name         text NOT NULL CHECK (name ~ '^[a-z][a-z0-9-]{0,31}$'),
    version      integer NOT NULL CHECK (version BETWEEN 1 AND 65535),
    title        text NOT NULL CHECK (length(title) BETWEEN 1 AND 80),
    -- The canonical JSON of the tpl/1 definition.
    definition   text NOT NULL CHECK (octet_length(definition) <= 16384),
    status       smallint NOT NULL CHECK (status BETWEEN 1 AND 4),
    -- NULL: a built-in, seeded by the server.
    submitted_by uuid REFERENCES messaging.known_user,
    submitted_at timestamptz NOT NULL,
    reviewed_at  timestamptz,
    -- The catalog position of the last approval or revocation; see template_change.
    change_seq   bigint UNIQUE,
    PRIMARY KEY (publisher, name, version),
    -- A seeded built-in is reviewed (approved) when it is inserted.
    CHECK ((status = 1) = (reviewed_at IS NULL)),
    CHECK ((status IN (2, 4)) = (change_seq IS NOT NULL))
);
-- The review queue (templates_with_status).
CREATE INDEX template_version_status ON messaging.template_version (status, submitted_at);
-- Pending submissions per account (pending_templates_of).
CREATE INDEX template_version_pending_by ON messaging.template_version (submitted_by)
    WHERE status = 1;

-- One row: the last change number handed out (approve, revoke, seed). Updating it locks
-- the row, so change numbers commit in order. It only counts up, one at a time, and the
-- row is never deleted.
CREATE TABLE messaging.template_change (
    id       boolean PRIMARY KEY DEFAULT true CHECK (id),
    last_seq bigint NOT NULL DEFAULT 0 CHECK (last_seq >= 0)
);
INSERT INTO messaging.template_change DEFAULT VALUES;

CREATE FUNCTION messaging.template_change_guard()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'the template change counter is never deleted'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.id IS DISTINCT FROM OLD.id OR NEW.last_seq IS DISTINCT FROM OLD.last_seq + 1 THEN
        RAISE EXCEPTION 'the template change counter only counts up by one'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER template_change_guard
    BEFORE UPDATE OR DELETE ON messaging.template_change
    FOR EACH ROW EXECUTE FUNCTION messaging.template_change_guard();

-- A version's identity and definition never change, it is never deleted, and its status
-- only moves one way: pending to approved or rejected, approved to revoked. A version
-- enters the catalog (is inserted or moves with a change number) only with the counter's
-- last number, which the writer took in the same transaction.
CREATE FUNCTION messaging.template_version_guard()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'template versions are never deleted' USING ERRCODE = '23514';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.change_seq IS NOT NULL
            AND NEW.change_seq IS DISTINCT FROM
                (SELECT last_seq FROM messaging.template_change) THEN
            RAISE EXCEPTION 'change number % is not the last one taken', NEW.change_seq
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF (OLD.publisher, OLD.name, OLD.version, OLD.title, OLD.definition, OLD.submitted_by,
            OLD.submitted_at)
        IS DISTINCT FROM (NEW.publisher, NEW.name, NEW.version, NEW.title, NEW.definition,
            NEW.submitted_by, NEW.submitted_at) THEN
        RAISE EXCEPTION 'a template version''s identity and definition never change'
            USING ERRCODE = '23514';
    END IF;
    IF NOT (
        (OLD.status = 1 AND NEW.status IN (2, 3))
        OR (OLD.status = 2 AND NEW.status = 4)
        OR (
            OLD.status = NEW.status
            AND OLD.change_seq IS NOT DISTINCT FROM NEW.change_seq
            AND OLD.reviewed_at IS NOT DISTINCT FROM NEW.reviewed_at
        )
    ) THEN
        RAISE EXCEPTION 'template status % cannot become %', OLD.status, NEW.status
            USING ERRCODE = '23514';
    END IF;
    IF OLD.status <> NEW.status
        AND NEW.change_seq IS NOT NULL
        AND NEW.change_seq IS DISTINCT FROM
            (SELECT last_seq FROM messaging.template_change) THEN
        RAISE EXCEPTION 'change number % is not the last one taken', NEW.change_seq
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER template_version_guard
    BEFORE INSERT OR UPDATE OR DELETE ON messaging.template_version
    FOR EACH ROW EXECUTE FUNCTION messaging.template_version_guard();

-- Who may publish under a name never changes, and a claim is never given up.
CREATE FUNCTION messaging.template_publisher_guard()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'template publishers are never deleted' USING ERRCODE = '23514';
    END IF;
    IF (OLD.publisher, OLD.owner_user_id) IS DISTINCT FROM (NEW.publisher, NEW.owner_user_id)
    THEN
        RAISE EXCEPTION 'a template publisher''s owner never changes' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER template_publisher_guard
    BEFORE UPDATE OR DELETE ON messaging.template_publisher
    FOR EACH ROW EXECUTE FUNCTION messaging.template_publisher_guard();

-- The audit trail of every version, append-only.
CREATE TABLE messaging.template_event (
    event_id  bigserial PRIMARY KEY,
    publisher text NOT NULL,
    name      text NOT NULL,
    version   integer NOT NULL,
    action    text NOT NULL CHECK (action IN ('submit', 'approve', 'reject', 'revoke', 'seed')),
    -- The account that acted (a submitter); NULL when a reviewer or the server did.
    actor     uuid,
    -- The operator who reviewed, as given to the admin API.
    reviewer  text CHECK (length(reviewer) BETWEEN 1 AND 64),
    note      text CHECK (length(note) <= 500),
    at        timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (publisher, name, version) REFERENCES messaging.template_version
);
-- A version's trail in recording order (template_events).
CREATE INDEX template_event_version
    ON messaging.template_event (publisher, name, version, event_id);

CREATE FUNCTION messaging.template_event_append_only()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION 'the template audit trail is append-only' USING ERRCODE = '23514';
END
$$;
CREATE TRIGGER template_event_append_only
    BEFORE UPDATE OR DELETE ON messaging.template_event
    FOR EACH ROW EXECUTE FUNCTION messaging.template_event_append_only();

-- The guards above are row triggers, which TRUNCATE skips: refuse it outright.
CREATE FUNCTION messaging.template_no_truncate()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% is never truncated', TG_TABLE_NAME USING ERRCODE = '23514';
END
$$;
CREATE TRIGGER template_publisher_no_truncate
    BEFORE TRUNCATE ON messaging.template_publisher
    FOR EACH STATEMENT EXECUTE FUNCTION messaging.template_no_truncate();
CREATE TRIGGER template_version_no_truncate
    BEFORE TRUNCATE ON messaging.template_version
    FOR EACH STATEMENT EXECUTE FUNCTION messaging.template_no_truncate();
CREATE TRIGGER template_change_no_truncate
    BEFORE TRUNCATE ON messaging.template_change
    FOR EACH STATEMENT EXECUTE FUNCTION messaging.template_no_truncate();
CREATE TRIGGER template_event_no_truncate
    BEFORE TRUNCATE ON messaging.template_event
    FOR EACH STATEMENT EXECUTE FUNCTION messaging.template_no_truncate();
