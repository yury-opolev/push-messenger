-- Pluggable identity (spec 2026-10-19 pluggable identity, D8): an external identity is
-- (provider, issuer, subject) for any provider id, not only Entra ID's (tenant, object id).
-- Existing Entra ID rows keep their identity: the issuer is the tenant id's text form (a
-- lower-case GUID). Nothing is deleted; the migration is one transaction.
--
-- Expand only: the chart migrates before the pods roll, and Plan 13A's pods still find,
-- create and revoke Entra ID identities by tenant_id (and ON CONFLICT on its key) until
-- they are replaced. So tenant_id stays, nullable, with its unique key, and a trigger
-- keeps the two in step for Entra ID rows: an old pod's row gets its issuer, a new pod's
-- its tenant_id. A later migration drops tenant_id once no Plan 13A binary runs.

ALTER TABLE auth.external_identity DROP CONSTRAINT external_identity_provider_check;
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_provider_check
    CHECK (provider ~ '^[a-z][a-z0-9-]{0,31}$');

ALTER TABLE auth.external_identity ADD COLUMN issuer text;
UPDATE auth.external_identity SET issuer = tenant_id::text;
-- Entra ID: the tenant id; another provider: the stable key of its issuer.
ALTER TABLE auth.external_identity ALTER COLUMN issuer SET NOT NULL;
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_issuer_check
    CHECK (length(issuer) BETWEEN 1 AND 256);
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_provider_issuer_subject_key
    UNIQUE (provider, issuer, subject);
-- Plan 13A's column: the tenant of an Entra ID row, NULL for other providers.
ALTER TABLE auth.external_identity ALTER COLUMN tenant_id DROP NOT NULL;

CREATE FUNCTION auth.external_identity_issuer() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF NEW.issuer IS NULL AND NEW.tenant_id IS NOT NULL THEN
        NEW.issuer := NEW.tenant_id::text;
    END IF;
    IF NEW.provider = 'entra' AND NEW.tenant_id IS NULL
       AND NEW.issuer ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' THEN
        NEW.tenant_id := NEW.issuer::uuid;
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER external_identity_issuer BEFORE INSERT ON auth.external_identity
    FOR EACH ROW EXECUTE FUNCTION auth.external_identity_issuer();
