-- Durable receipt of trusted auth enrollment. Separate from contacts so retries cannot
-- resurrect a relationship either party has since blocked or removed.
CREATE TABLE messaging.agent_owner (
    agent_user_id uuid PRIMARY KEY REFERENCES messaging.known_user,
    owner_user_id uuid NOT NULL REFERENCES messaging.known_user,
    CHECK (agent_user_id <> owner_user_id)
);
