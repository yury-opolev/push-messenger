-- The worker schema (spec §3.3), owned by the worker in its own database, push_worker.
--
-- job holds the work: auth-svc's outbox events, copied here before they are deleted
-- there, and the periodic cleanups. Workers claim due jobs with SELECT … FOR UPDATE SKIP
-- LOCKED and lease them by moving run_after forward, so several replicas share the work
-- and a crashed worker's jobs come back when their lease ends.

CREATE EXTENSION IF NOT EXISTS citext;

CREATE SCHEMA worker;

CREATE TABLE worker.job (
    -- An outbox event's id, so a second copy of the event is ignored.
    job_id        uuid PRIMARY KEY,
    kind          text NOT NULL,
    payload       jsonb NOT NULL,
    -- Due from then on; a claim moves it to the end of the lease.
    run_after     timestamptz NOT NULL DEFAULT now(),
    attempts      integer NOT NULL DEFAULT 0,
    claimed_at    timestamptz,
    failed_reason text,
    -- Set when the job gave up (poison): kept for inspection, never claimed again.
    dead_at       timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX job_due ON worker.job (run_after, job_id) WHERE dead_at IS NULL;
CREATE INDEX job_dead ON worker.job (dead_at) WHERE dead_at IS NOT NULL;

CREATE TABLE worker.email_message (
    -- The email.send event's id, also Resend's idempotency key.
    email_id            uuid PRIMARY KEY,
    to_address          citext NOT NULL,
    -- For deliverability metrics (spec §9).
    recipient_domain    text NOT NULL,
    template            text NOT NULL,
    -- Without the link: it would sign its holder in.
    params              jsonb NOT NULL CHECK (NOT params ? 'link'),
    provider_message_id text UNIQUE,
    -- 1 queued, 2 sent, 3 suppressed, 4 failed.
    status              smallint NOT NULL CHECK (status BETWEEN 1 AND 4),
    created_at          timestamptz NOT NULL DEFAULT now(),
    sent_at             timestamptz
);
CREATE INDEX email_message_created ON worker.email_message (created_at);

CREATE TABLE worker.email_event (
    -- The webhook's id (svix-id), the same on every delivery of it: a replay is ignored.
    event_id            text PRIMARY KEY,
    provider_message_id text NOT NULL,
    -- 1 delivered, 2 bounced, 3 complained.
    kind                smallint NOT NULL CHECK (kind BETWEEN 1 AND 3),
    occurred_at         timestamptz NOT NULL,
    raw                 jsonb NOT NULL,
    received_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX email_event_message ON worker.email_event (provider_message_id);
CREATE INDEX email_event_received ON worker.email_event (received_at);

CREATE TABLE worker.email_suppression (
    address    citext PRIMARY KEY,
    -- 1 hard bounce, 2 complaint.
    reason     smallint NOT NULL CHECK (reason BETWEEN 1 AND 2),
    created_at timestamptz NOT NULL DEFAULT now()
);
