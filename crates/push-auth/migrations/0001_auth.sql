-- The auth schema (spec §3.1), owned by auth-svc in its own database, push_auth.
--
-- Emails and handles are citext, so uniqueness ignores case. Handles are stored in lower
-- case; the CHECK compares the text value, since a citext regex match ignores case.
-- Verification links, password-reset links and registration tokens are signed and never
-- stored (spec §5.1); refresh tokens are stored as SHA-256 hashes only.

CREATE EXTENSION IF NOT EXISTS citext;

CREATE SCHEMA auth;

CREATE TABLE auth.user_account (
    user_id           uuid PRIMARY KEY,
    email             citext NOT NULL UNIQUE CHECK (length(email) BETWEEN 3 AND 254),
    -- Null until the verification link is opened: no API access before that.
    email_verified_at timestamptz,
    handle            citext NOT NULL UNIQUE CHECK (handle::text ~ '^[a-z][a-z0-9_]{2,29}$'),
    display_name      text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 64),
    -- Argon2id PHC string.
    password_hash     text NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE auth.device (
    device_id       uuid PRIMARY KEY,
    user_id         uuid NOT NULL REFERENCES auth.user_account,
    name            text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    platform        text NOT NULL CHECK (platform IN ('ios', 'android', 'web', 'desktop', 'other')),
    -- Chosen by the client for one registration and sent again with its retries: a retry
    -- finds this device instead of creating a second one.
    registration_id uuid NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_seen_at    timestamptz,
    revoked_at      timestamptz,
    UNIQUE (user_id, registration_id)
);
-- The device limit counts a user's active devices.
CREATE INDEX device_active_user ON auth.device (user_id) WHERE revoked_at IS NULL;

CREATE TABLE auth.refresh_token (
    -- SHA-256 of the raw token, which is never stored.
    token_hash bytea PRIMARY KEY CHECK (length(token_hash) = 32),
    device_id  uuid NOT NULL REFERENCES auth.device,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    -- The token that replaced this one; presenting a replaced token again is reuse,
    -- except within the grace window below.
    rotated_to bytea,
    -- When the holder exchanged this token; null when a retry of the token before it cut
    -- this one off. The token rotated last may be presented again for 30 s after this.
    rotated_at timestamptz
);
CREATE INDEX refresh_token_device ON auth.refresh_token (device_id);

-- Events for other services, written in the same transaction as the change. The worker
-- (Plan 6) reads and deletes them; nothing else reads this table.
CREATE TABLE auth.outbox (
    event_id   uuid PRIMARY KEY,
    kind       text NOT NULL CHECK (kind IN (
                   'email.send', 'user.verified', 'user.password_reset',
                   'device.registered', 'device.revoked')),
    payload    jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX outbox_created ON auth.outbox (created_at);
