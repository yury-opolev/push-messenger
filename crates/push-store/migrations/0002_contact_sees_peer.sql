-- Whether the owner could see the peer's profile when they blocked them. A block counts
-- toward the owner's visibility of the peer only if true, so blocking a stranger does not
-- reveal whoever blocked them. Written only with state = 4 (blocked).
ALTER TABLE messaging.contact ADD COLUMN sees_peer boolean NOT NULL DEFAULT false;

-- The earlier rule counted every block. Existing blocks keep their sight, the
-- conservative choice: the history of what each blocker saw was never recorded.
UPDATE messaging.contact SET sees_peer = true WHERE state = 4;
