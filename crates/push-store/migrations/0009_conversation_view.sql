-- Deleted accounts and hidden direct conversations (spec
-- docs/superpowers/specs/2026-11-06-tombstones-hide-design.md). New tables only: no
-- existing data changes, and older images ignore them. To reverse:
--   DROP TABLE messaging.conversation_view; DROP TABLE messaging.deleted_user;

-- Users whose account auth-svc deleted (user.deleted, relayed by the worker). The first
-- time message-svc recorded it is kept. Their direct conversations show peer_deleted to
-- the other member.
CREATE TABLE messaging.deleted_user (
    user_id    uuid PRIMARY KEY REFERENCES messaging.known_user,
    deleted_at timestamptz NOT NULL
);

-- A member's own view of a direct conversation: hidden or not, and when that last
-- changed (listings with changed_since return the conversation after a change). No row:
-- shown, never changed. The other member's view is a row of its own.
CREATE TABLE messaging.conversation_view (
    conversation_id uuid NOT NULL,
    user_id         uuid NOT NULL,
    hidden          boolean NOT NULL,
    changed_at      timestamptz NOT NULL,
    PRIMARY KEY (conversation_id, user_id),
    FOREIGN KEY (conversation_id, user_id)
        REFERENCES messaging.conversation_member (conversation_id, user_id)
);
-- A user's flags (conversation_flags).
CREATE INDEX conversation_view_user ON messaging.conversation_view (user_id);
