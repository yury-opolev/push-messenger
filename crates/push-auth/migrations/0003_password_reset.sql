-- When the account's password was last reset (spec §5.4), in seconds since the Unix epoch,
-- from the clock that sets the access tokens' `iat`. auth-svc refuses at its own API every
-- access token issued no later than this second, as message-svc does after the relayed
-- `user.password_reset`: a token stolen before a reset cannot keep reading the account or
-- revoking the devices its owner signs back in with. NULL: never reset.
ALTER TABLE auth.user_account ADD COLUMN password_reset_at bigint;
