-- The messaging schema (spec §3.2), owned by message-svc.
--
-- Enumerations are smallints with CHECK constraints; push-store's codes module maps them:
--   contact.state              1 requested_out, 2 requested_in, 3 accepted, 4 blocked
--   conversation.kind          1 direct, 2 group
--   conversation_member.role   1 admin, 2 member
--   message.kind               1 message, 2 edit, 3 delete, 4 system
--   message.content_type       1 text, 2 system event

CREATE SCHEMA messaging;

-- Mirrored from auth through user.verified events.
CREATE TABLE messaging.known_user (
    user_id    uuid PRIMARY KEY,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Mirrored from auth through device events.
CREATE TABLE messaging.member_device (
    device_id  uuid PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES messaging.known_user,
    revoked_at timestamptz
);
CREATE INDEX member_device_user ON messaging.member_device (user_id);

-- One row per direction.
CREATE TABLE messaging.contact (
    owner_id   uuid NOT NULL REFERENCES messaging.known_user,
    peer_id    uuid NOT NULL REFERENCES messaging.known_user,
    state      smallint NOT NULL CHECK (state BETWEEN 1 AND 4),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_id, peer_id),
    CHECK (owner_id <> peer_id)
);

CREATE TABLE messaging.invite (
    code       text PRIMARY KEY CHECK (code ~ '^[0-9A-HJKMNP-TV-Z]{10}$'),
    user_id    uuid NOT NULL REFERENCES messaging.known_user,
    expires_at timestamptz NOT NULL,
    max_uses   integer NOT NULL DEFAULT 1 CHECK (max_uses BETWEEN 1 AND 100),
    uses       integer NOT NULL DEFAULT 0,
    revoked_at timestamptz,
    CHECK (uses BETWEEN 0 AND max_uses)
);

CREATE TABLE messaging.conversation (
    conversation_id uuid PRIMARY KEY,
    kind            smallint NOT NULL CHECK (kind IN (1, 2)),
    -- Direct only: the two user ids, lower first.
    direct_key      bytea UNIQUE CHECK (octet_length(direct_key) = 32),
    title           text,
    created_by      uuid NOT NULL,
    -- The single-writer counter; see next_seq.
    last_seq        bigint NOT NULL DEFAULT 0 CHECK (last_seq >= 0),
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- Creation or last append; never moves back.
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CHECK (direct_key IS NULL OR kind = 1)
);

CREATE TABLE messaging.conversation_member (
    conversation_id  uuid NOT NULL REFERENCES messaging.conversation,
    user_id          uuid NOT NULL REFERENCES messaging.known_user,
    role             smallint NOT NULL CHECK (role IN (1, 2)),
    visible_from_seq bigint NOT NULL CHECK (visible_from_seq >= 1),
    left_at_seq      bigint CHECK (left_at_seq >= 0),
    read_seq         bigint NOT NULL DEFAULT 0 CHECK (read_seq >= 0),
    PRIMARY KEY (conversation_id, user_id)
);
-- "My conversations".
CREATE INDEX conversation_member_user ON messaging.conversation_member (user_id);

CREATE TABLE messaging.message (
    conversation_id  uuid NOT NULL REFERENCES messaging.conversation,
    seq              bigint NOT NULL CHECK (seq >= 1),
    message_id       uuid NOT NULL UNIQUE,
    client_msg_id    uuid NOT NULL,
    kind             smallint NOT NULL CHECK (kind BETWEEN 1 AND 4),
    target_seq       bigint,
    sender_id        uuid NOT NULL,
    sender_device_id uuid,
    content_type     smallint NOT NULL CHECK (content_type IN (1, 2)),
    body             bytea,
    client_ts        timestamptz,
    server_ts        timestamptz NOT NULL DEFAULT now(),
    edited_at        timestamptz,
    edit_seq         bigint,
    deleted_at       timestamptz,
    PRIMARY KEY (conversation_id, seq),
    UNIQUE (conversation_id, client_msg_id)
);
-- Edits and deletes of one message (erase_edit_bodies).
CREATE INDEX message_target ON messaging.message (conversation_id, target_seq)
    WHERE target_seq IS NOT NULL;
-- History pages: messages and system entries only (messages_before).
CREATE INDEX message_history ON messaging.message (conversation_id, seq)
    WHERE kind IN (1, 4);
-- Unread counts and effective positions: live messages only.
CREATE INDEX message_live ON messaging.message (conversation_id, seq) INCLUDE (sender_id)
    WHERE kind = 1 AND deleted_at IS NULL;

CREATE TABLE messaging.device_cursor (
    device_id       uuid NOT NULL REFERENCES messaging.member_device,
    conversation_id uuid NOT NULL REFERENCES messaging.conversation,
    -- The device has everything up to here.
    delivered_seq   bigint NOT NULL CHECK (delivered_seq >= 0),
    PRIMARY KEY (device_id, conversation_id)
);

-- The furthest delivery position of any of a user's devices, revoked ones included; 0 if
-- there is none.
CREATE FUNCTION messaging.delivered_seq(in_conversation uuid, in_user uuid)
RETURNS bigint
LANGUAGE sql STABLE
AS $$
    SELECT coalesce(max(c.delivered_seq), 0)
    FROM messaging.member_device d
    JOIN messaging.device_cursor c
        ON c.device_id = d.device_id AND c.conversation_id = in_conversation
    WHERE d.user_id = in_user
$$;

-- How far a member at a raw position has effectively got: just before the first live
-- message from someone else after max(position, visible_from_seq - 1), or `upper` if
-- there is none. See Tx::group_positions in push-domain.
CREATE FUNCTION messaging.effective_position(
    in_conversation uuid,
    in_user uuid,
    in_visible_from_seq bigint,
    in_upper bigint,
    in_position bigint
)
RETURNS bigint
LANGUAGE sql STABLE
AS $$
    SELECT CASE
        WHEN greatest(in_position, in_visible_from_seq - 1) >= in_upper THEN in_upper
        ELSE coalesce(
            (
                SELECT m.seq - 1
                FROM messaging.message m
                WHERE m.conversation_id = in_conversation
                    AND m.seq > greatest(in_position, in_visible_from_seq - 1)
                    AND m.seq <= in_upper
                    AND m.kind = 1
                    AND m.deleted_at IS NULL
                    AND m.sender_id <> in_user
                ORDER BY m.seq
                LIMIT 1
            ),
            in_upper
        )
    END
$$;

-- A group's group-wide positions with its log ending at in_last_seq: the minimums over
-- its current members. No row if it has no current members.
CREATE FUNCTION messaging.group_positions(in_conversation uuid, in_last_seq bigint)
RETURNS TABLE (delivered bigint, read bigint)
LANGUAGE sql STABLE
AS $$
    SELECT
        min(messaging.effective_position(
            in_conversation, m.user_id, m.visible_from_seq, in_last_seq,
            greatest(messaging.delivered_seq(in_conversation, m.user_id), m.read_seq)
        )),
        min(messaging.effective_position(
            in_conversation, m.user_id, m.visible_from_seq, in_last_seq, m.read_seq
        ))
    FROM messaging.conversation_member m
    WHERE m.conversation_id = in_conversation AND m.left_at_seq IS NULL
    HAVING count(*) > 0
$$;
