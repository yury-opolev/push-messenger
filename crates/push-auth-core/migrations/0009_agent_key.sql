-- Agent-key provider (REQUEST PUSH feature 6, spec 2026-10-26 agent-key provider).
-- A reservation made while only agent-key can redeem it has no tenant (D5); entra-app
-- refuses such a reservation.
ALTER TABLE auth.agent_enrollment ALTER COLUMN tenant_id DROP NOT NULL;

-- Single-use agent-key assertions (D2): each (thumbprint, jti) once, kept until its
-- expiry plus the clock skew, then deleted by later sign-ins.
CREATE TABLE auth.agent_key_assertion (
    thumbprint text NOT NULL CHECK (length(thumbprint) = 43),
    jti        text NOT NULL CHECK (length(jti) BETWEEN 16 AND 128),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (thumbprint, jti)
);
CREATE INDEX agent_key_assertion_expiry ON auth.agent_key_assertion (expires_at);
