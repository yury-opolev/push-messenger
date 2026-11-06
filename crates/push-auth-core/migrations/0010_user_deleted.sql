-- Deleted accounts as tombstones (spec docs/superpowers/specs/2026-11-06-tombstones-hide-design.md).
-- Deleting a verified account (an owner deleting an agent) writes `user.deleted`; the
-- worker relays it to message-svc, which flags the deleted user's direct conversations
-- for their peers. Accounts disabled before this migration get one event each now.
-- To reverse: DELETE FROM auth.outbox WHERE kind = 'user.deleted'; then restore the
-- previous outbox_kind_check (0006_owned_agents.sql).
ALTER TABLE auth.outbox DROP CONSTRAINT outbox_kind_check;
ALTER TABLE auth.outbox ADD CONSTRAINT outbox_kind_check CHECK (kind IN (
    'email.send', 'user.verified', 'user.password_reset',
    'device.registered', 'device.revoked', 'agent.enrolled', 'user.deleted'));

INSERT INTO auth.outbox (event_id, kind, payload)
SELECT gen_random_uuid(), 'user.deleted', jsonb_build_object('user_id', user_id)
FROM auth.user_account
WHERE disabled_at IS NOT NULL AND email_verified_at IS NOT NULL;
