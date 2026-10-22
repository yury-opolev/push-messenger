ALTER TABLE auth.user_account
    ADD COLUMN kind text NOT NULL DEFAULT 'user',
    ADD COLUMN owner_user_id uuid REFERENCES auth.user_account,
    ADD COLUMN disabled_at timestamptz,
    ADD CONSTRAINT user_account_kind_check CHECK (
        (kind = 'user' AND owner_user_id IS NULL AND disabled_at IS NULL)
        OR (kind = 'agent' AND owner_user_id IS NOT NULL AND owner_user_id <> user_id
            AND email IS NULL AND password_hash IS NULL));
CREATE INDEX user_account_owner ON auth.user_account (owner_user_id)
    WHERE kind = 'agent' AND disabled_at IS NULL;
ALTER TABLE auth.user_account DROP CONSTRAINT user_account_kind;
ALTER TABLE auth.user_account ADD CONSTRAINT user_account_kind CHECK (
    (kind = 'user' AND (
        (email IS NOT NULL AND password_hash IS NOT NULL)
        OR (email IS NULL AND password_hash IS NULL AND email_verified_at IS NOT NULL)))
    OR (kind = 'agent' AND email IS NULL AND password_hash IS NULL));

-- A pending account reserves the handle but is neither verified nor projected to messaging.
-- On redemption the hash is erased; the tenant and consent time remain for auditing.
CREATE TABLE auth.agent_enrollment (
    user_id uuid PRIMARY KEY REFERENCES auth.user_account ON DELETE CASCADE,
    tenant_id uuid NOT NULL CHECK (tenant_id <> '00000000-0000-0000-0000-000000000000'),
    consent_device_id uuid NOT NULL REFERENCES auth.device,
    code_hash bytea UNIQUE CHECK (octet_length(code_hash) = 32),
    expires_at timestamptz NOT NULL,
    enrolled_at timestamptz,
    client_app_id uuid,
    CHECK ((enrolled_at IS NULL) = (code_hash IS NOT NULL))
);
CREATE INDEX agent_enrollment_expiry ON auth.agent_enrollment (expires_at)
    WHERE enrolled_at IS NULL;

CREATE FUNCTION auth.agent_owner_check() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF NEW.kind = 'agent' AND NOT EXISTS (
        SELECT 1 FROM auth.user_account WHERE user_id = NEW.owner_user_id AND kind = 'user'
    ) THEN
        RAISE EXCEPTION 'an agent owner must be a human account' USING ERRCODE = '23514';
    END IF;
    IF TG_OP = 'UPDATE' AND (OLD.kind, OLD.owner_user_id) IS DISTINCT FROM (NEW.kind, NEW.owner_user_id) THEN
        RAISE EXCEPTION 'account kind and owner are immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER agent_owner_check BEFORE INSERT OR UPDATE ON auth.user_account
    FOR EACH ROW EXECUTE FUNCTION auth.agent_owner_check();

ALTER TABLE auth.outbox DROP CONSTRAINT outbox_kind_check;
ALTER TABLE auth.outbox ADD CONSTRAINT outbox_kind_check CHECK (kind IN (
    'email.send', 'user.verified', 'user.password_reset',
    'device.registered', 'device.revoked', 'agent.enrolled'));
