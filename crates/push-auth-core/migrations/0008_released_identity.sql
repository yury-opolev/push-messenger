-- Deleting an agent releases its identities (REQUEST PUSH feature 7): each row of
-- auth.external_identity is moved here, so the same app (or key) can be enrolled again
-- while the binding stays auditable. Append-only; one identity may be released many times.
-- Enrollment facts (tenant, client app, consent device, time) stay in auth.agent_enrollment.
CREATE TABLE auth.released_identity (
    identity_id uuid PRIMARY KEY,
    provider    text NOT NULL,
    issuer      text NOT NULL,
    subject     text NOT NULL,
    user_id     uuid NOT NULL REFERENCES auth.user_account,
    created_at  timestamptz NOT NULL,
    released_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX released_identity_key ON auth.released_identity (provider, issuer, subject);
CREATE INDEX released_identity_user ON auth.released_identity (user_id);
