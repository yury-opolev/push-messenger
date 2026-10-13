-- What the worker may do in the auth schema (Plan 6), besides draining auth.outbox: two
-- cleanups, as functions, so the rules stay here and the worker never reads accounts or
-- tokens. auth-svc grants EXECUTE on them, SELECT and DELETE on auth.outbox, and UPDATE of
-- auth.outbox.created_at (which FOR UPDATE needs), to the worker's role at start-up
-- (PUSH_AUTH_WORKER_ROLE).
--
-- STRICT: a NULL argument deletes nothing. The batch is capped at 10000 and the age at no
-- less than a day, so a misused caller cannot wipe fresh signups in one call.
--
-- SECURITY DEFINER: they run with the rights of their owner, auth-svc's role. The
-- search_path is pinned, so a caller cannot slip in objects of its own, and nobody may
-- call them until auth-svc grants it.

CREATE INDEX refresh_token_expires ON auth.refresh_token (expires_at);
CREATE INDEX user_account_unverified ON auth.user_account (created_at)
    WHERE email_verified_at IS NULL;

-- Deletes up to `batch` refresh tokens past their expiry, which nothing can use any more,
-- rotated and revoked ones included. Returns how many it deleted; call it again until 0.
CREATE FUNCTION auth.delete_expired_refresh_tokens(batch integer) RETURNS integer
LANGUAGE sql STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    WITH gone AS (
        DELETE FROM auth.refresh_token
        WHERE token_hash IN (
            SELECT token_hash FROM auth.refresh_token
            WHERE expires_at < statement_timestamp()
            LIMIT least(batch, 10000)
            FOR UPDATE SKIP LOCKED)
        RETURNING 1)
    SELECT count(*)::integer FROM gone
$$;

-- Deletes up to `batch` accounts whose email was never verified, created (or taken over
-- by a new signup) more than `older_than` ago. Such an account has no devices; one that
-- somehow has is kept. Returns how many it deleted; call it again until 0.
CREATE FUNCTION auth.delete_unverified_accounts(older_than interval, batch integer)
RETURNS integer
LANGUAGE sql STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    WITH gone AS (
        DELETE FROM auth.user_account
        WHERE user_id IN (
            SELECT account.user_id FROM auth.user_account AS account
            WHERE account.email_verified_at IS NULL
              AND account.created_at < statement_timestamp() - greatest(older_than, interval '1 day')
              AND NOT EXISTS (
                  SELECT 1 FROM auth.device WHERE device.user_id = account.user_id)
            LIMIT least(batch, 10000)
            FOR UPDATE SKIP LOCKED)
        RETURNING 1)
    SELECT count(*)::integer FROM gone
$$;

REVOKE ALL ON FUNCTION auth.delete_expired_refresh_tokens(integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION auth.delete_unverified_accounts(interval, integer) FROM PUBLIC;
