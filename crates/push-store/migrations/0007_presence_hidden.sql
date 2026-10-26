-- Users who switched presence sharing off (REQUEST PUSH feature 12). A row hides the
-- user's online state and last-seen time from every contact, and hides everyone else's
-- from them. No row: presence is shared.
CREATE TABLE messaging.presence_hidden (
    user_id   uuid PRIMARY KEY REFERENCES messaging.known_user,
    hidden_at timestamptz NOT NULL DEFAULT now()
);
