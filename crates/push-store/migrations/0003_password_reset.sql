-- When message-svc last heard (from auth-svc, through the worker) that the user's password
-- was reset. Identify refuses access tokens issued no later than that second, so a token
-- that outlives the reset, or a kick lost on Redis, cannot keep a device signed in.
ALTER TABLE messaging.known_user ADD COLUMN password_reset_at timestamptz;
