-- Microsoft Entra ID sign-in (spec 2026-10-19). An Entra account has no email address and
-- no password here: the user is found by the immutable (tenant, object id) of the token,
-- never by an address, and is active from the first sign-in (email_verified_at is the time
-- of that sign-in, so lookup, profiles and the cleanup treat it like a verified account).
-- Email/password accounts are unchanged; the two are never linked.

ALTER TABLE auth.user_account ALTER COLUMN email DROP NOT NULL;
ALTER TABLE auth.user_account ALTER COLUMN password_hash DROP NOT NULL;
-- An email/password account, or an external one, which is always active.
ALTER TABLE auth.user_account ADD CONSTRAINT user_account_kind CHECK (
    (email IS NOT NULL AND password_hash IS NOT NULL)
    OR (email IS NULL AND password_hash IS NULL AND email_verified_at IS NOT NULL));

CREATE TABLE auth.external_identity (
    identity_id     uuid PRIMARY KEY,
    provider        text NOT NULL CHECK (provider IN ('entra')),
    -- Entra ID: the tenant (`tid`) and the user's object id (`oid`), both immutable.
    tenant_id       uuid NOT NULL,
    subject         text NOT NULL CHECK (length(subject) BETWEEN 1 AND 128),
    -- One identity per account: no linking (spec D5).
    user_id         uuid NOT NULL UNIQUE REFERENCES auth.user_account,
    -- The latest `email` or `preferred_username`: shown to its owner only, never used to
    -- find an account.
    email           text CHECK (length(email) <= 254),
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_sign_in_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (provider, tenant_id, subject)
);

-- When an Entra-backed device's session ends, in seconds since the Unix epoch: refresh is
-- refused from then on until the user signs in with Entra ID again (spec D8). NULL: no
-- limit (email/password devices).
ALTER TABLE auth.device ADD COLUMN session_expires_at bigint;

-- As in 0002, and never an account without a password (defence in depth: external
-- accounts are active anyway). CREATE OR REPLACE keeps the worker's EXECUTE grant.
CREATE OR REPLACE FUNCTION auth.delete_unverified_accounts(older_than interval, batch integer)
RETURNS integer
LANGUAGE sql STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    WITH gone AS (
        DELETE FROM auth.user_account
        WHERE user_id IN (
            SELECT account.user_id FROM auth.user_account AS account
            WHERE account.email_verified_at IS NULL
              AND account.password_hash IS NOT NULL
              AND account.created_at < statement_timestamp() - greatest(older_than, interval '1 day')
              AND NOT EXISTS (
                  SELECT 1 FROM auth.device WHERE device.user_id = account.user_id)
            LIMIT least(batch, 10000)
            FOR UPDATE SKIP LOCKED)
        RETURNING 1)
    SELECT count(*)::integer FROM gone
$$;
