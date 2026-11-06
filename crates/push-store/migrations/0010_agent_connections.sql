-- Agent connection policy and refused requests (spec
-- docs/superpowers/specs/2026-11-06-agent-connections-and-owner-view.md). New tables only:
-- no existing data changes, and older images ignore them. To reverse:
--   DROP TABLE messaging.agent_request_log; DROP TABLE messaging.refused_request;
--   DROP TABLE messaging.agent_policy;

-- An agent's connection policy, set by its owner: 1 ask, 2 auto_own,
-- 3 auto_owner_contacts, 4 auto_any, 5 refuse_all. No row: ask.
CREATE TABLE messaging.agent_policy (
    agent_user_id uuid PRIMARY KEY REFERENCES messaging.agent_owner (agent_user_id),
    policy        smallint NOT NULL CHECK (policy BETWEEN 1 AND 5),
    changed_at    timestamptz NOT NULL
);

-- A request an agent's policy refused: the requester's requested_out row stays until
-- release_at, then disappears without an event, as a decline looks to the requester.
CREATE TABLE messaging.refused_request (
    requester_id  uuid NOT NULL REFERENCES messaging.known_user,
    agent_user_id uuid NOT NULL REFERENCES messaging.known_user,
    release_at    timestamptz NOT NULL,
    PRIMARY KEY (requester_id, agent_user_id)
);
CREATE INDEX refused_request_due ON messaging.refused_request (release_at);

-- Agents' outgoing contact requests, for their rate limits; pruned after 24 hours.
CREATE TABLE messaging.agent_request_log (
    agent_user_id uuid NOT NULL REFERENCES messaging.known_user,
    peer_id       uuid NOT NULL,
    sent_at       timestamptz NOT NULL
);
CREATE INDEX agent_request_log_agent ON messaging.agent_request_log (agent_user_id, sent_at);
CREATE INDEX agent_request_log_sent ON messaging.agent_request_log (sent_at);
