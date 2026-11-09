-- Proactive agents (spec docs/superpowers/specs/2026-11-09-proactive-agents.md §10): an
-- agent's settings, the actions it started that wait for (or were decided by) its owner,
-- its action log for the rate limits, and the started_by_agent marks. New tables and new
-- columns with defaults, plus a backfill of the marks; older images ignore them. To
-- reverse:
--   DROP TABLE messaging.agent_action_log; DROP TABLE messaging.outgoing_action;
--   DROP TABLE messaging.agent_settings;
--   ALTER TABLE messaging.conversation DROP COLUMN started_by_agent;
--   ALTER TABLE messaging.call_history DROP COLUMN started_by_agent;

-- An agent's settings, set by its owner. inbound: 1 owner_only, 2 owner_agents,
-- 3 contacts. requests, chats, groups, calls: 1 allowed, 2 ask, 3 off. No row: the
-- defaults (owner_only; ask, ask, off, off).
CREATE TABLE messaging.agent_settings (
    agent_user_id uuid PRIMARY KEY REFERENCES messaging.agent_owner (agent_user_id),
    inbound       smallint NOT NULL CHECK (inbound BETWEEN 1 AND 3),
    requests      smallint NOT NULL CHECK (requests BETWEEN 1 AND 3),
    chats         smallint NOT NULL CHECK (chats BETWEEN 1 AND 3),
    groups        smallint NOT NULL CHECK (groups BETWEEN 1 AND 3),
    calls         smallint NOT NULL CHECK (calls BETWEEN 1 AND 3),
    changed_at    timestamptz NOT NULL
);

-- An action an agent started under the ask policy. kind: 1 request, 2 chat, 3 group,
-- 4 invite, 5 call. state: 1 waiting, 2 approved, 3 declined, 4 expired, 5 failed.
-- error_code (failed only): the codes of push_domain::OutgoingError. Waiting actions
-- expire after 24 hours; resolved ones are deleted 24 hours after their resolution.
CREATE TABLE messaging.outgoing_action (
    action_id       uuid PRIMARY KEY,
    agent_user_id   uuid NOT NULL REFERENCES messaging.known_user,
    kind            smallint NOT NULL CHECK (kind BETWEEN 1 AND 5),
    target_user_id  uuid,
    conversation_id uuid,
    title           text,
    -- Sorted, without duplicates.
    member_ids      uuid[] NOT NULL DEFAULT '{}',
    state           smallint NOT NULL CHECK (state BETWEEN 1 AND 5),
    error_code      smallint,
    created_at      timestamptz NOT NULL,
    resolved_at     timestamptz,
    used_at         timestamptz
);
CREATE INDEX outgoing_action_agent ON messaging.outgoing_action (agent_user_id, created_at);
CREATE INDEX outgoing_action_waiting ON messaging.outgoing_action (created_at) WHERE state = 1;
CREATE INDEX outgoing_action_resolved ON messaging.outgoing_action (resolved_at)
    WHERE state <> 1;

-- Agents' actions carried out or queued, for their rate limits (count: the members added
-- by an invite, else 1); pruned after 24 hours.
CREATE TABLE messaging.agent_action_log (
    agent_user_id uuid NOT NULL REFERENCES messaging.known_user,
    kind          smallint NOT NULL,
    count         integer NOT NULL,
    at            timestamptz NOT NULL
);
CREATE INDEX agent_action_log_agent ON messaging.agent_action_log (agent_user_id, kind, at);
CREATE INDEX agent_action_log_at ON messaging.agent_action_log (at);

-- A direct chat an agent opened with someone other than its owner, or a group an agent
-- created.
ALTER TABLE messaging.conversation ADD COLUMN started_by_agent boolean NOT NULL DEFAULT false;
-- A call whose caller was an agent (both participants' rows).
ALTER TABLE messaging.call_history ADD COLUMN started_by_agent boolean NOT NULL DEFAULT false;

UPDATE messaging.conversation c SET started_by_agent = true
FROM messaging.agent_owner a
WHERE a.agent_user_id = c.created_by
  AND (c.kind = 2
       OR NOT EXISTS (SELECT 1 FROM messaging.conversation_member m
                      WHERE m.conversation_id = c.conversation_id
                        AND m.user_id = a.owner_user_id));

UPDATE messaging.call_history h SET started_by_agent = true
WHERE (h.direction = 'out'
       AND EXISTS (SELECT 1 FROM messaging.agent_owner a WHERE a.agent_user_id = h.user_id))
   OR (h.direction = 'in'
       AND EXISTS (SELECT 1 FROM messaging.agent_owner a
                   WHERE a.agent_user_id = h.peer_user_id));
