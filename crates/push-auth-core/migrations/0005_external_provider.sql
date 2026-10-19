-- Pluggable identity (spec 2026-10-19 pluggable identity, D8): an external identity is
-- (provider, issuer, subject) for any provider id, not only Entra ID's (tenant, object id).
-- Existing Entra ID rows keep their identity: the tenant id becomes its text form (a
-- lower-case GUID), the issuer. Nothing is deleted; the migration is one transaction.

ALTER TABLE auth.external_identity DROP CONSTRAINT external_identity_provider_check;
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_provider_check
    CHECK (provider ~ '^[a-z][a-z0-9-]{0,31}$');
ALTER TABLE auth.external_identity
    DROP CONSTRAINT external_identity_provider_tenant_id_subject_key;
ALTER TABLE auth.external_identity ALTER COLUMN tenant_id TYPE text USING tenant_id::text;
-- Entra ID: the tenant id; another provider: the stable key of its issuer.
ALTER TABLE auth.external_identity RENAME COLUMN tenant_id TO issuer;
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_issuer_check
    CHECK (length(issuer) BETWEEN 1 AND 256);
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_provider_issuer_subject_key
    UNIQUE (provider, issuer, subject);
