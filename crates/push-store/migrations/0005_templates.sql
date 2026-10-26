-- Rich message templates and the template registry (REQUEST PUSH feature 8; spec
-- docs/superpowers/specs/2026-10-26-rich-message-templates-design.md).
--
-- New enumeration values; push-store's codes module maps them:
--   message.content_type       3 rich, 4 answer (1 text, 2 system event as before)
--   template_version.status    1 pending, 2 approved, 3 rejected, 4 revoked

ALTER TABLE messaging.message DROP CONSTRAINT message_content_type_check;
ALTER TABLE messaging.message ADD CONSTRAINT message_content_type_check
    CHECK (content_type BETWEEN 1 AND 4);
-- Cards: the seq of the first valid answer.
ALTER TABLE messaging.message ADD COLUMN answer_seq bigint CHECK (answer_seq >= 1);

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

-- A version's identity and definition never change, it is never deleted, and its status
-- only moves one way: pending to approved or rejected, approved to revoked.
CREATE FUNCTION messaging.template_version_guard()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'template versions are never deleted' USING ERRCODE = '23514';
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
    RETURN NEW;
END
$$;
CREATE TRIGGER template_version_guard
    BEFORE UPDATE OR DELETE ON messaging.template_version
    FOR EACH ROW EXECUTE FUNCTION messaging.template_version_guard();

-- One row: the last change number handed out (approve, revoke, seed). Updating it locks
-- the row, so change numbers commit in order.
CREATE TABLE messaging.template_change (
    id       boolean PRIMARY KEY DEFAULT true CHECK (id),
    last_seq bigint NOT NULL DEFAULT 0 CHECK (last_seq >= 0)
);
INSERT INTO messaging.template_change DEFAULT VALUES;

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
