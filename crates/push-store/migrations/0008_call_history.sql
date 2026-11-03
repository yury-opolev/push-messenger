-- Call history (voice calls, spec docs/superpowers/specs/2026-11-03-voice-calls-design.md
-- §9). One row per participant of an ended call, written by message-svc when the call
-- ends; kept apart from conversation history (no "missed call" messages). A user reads and
-- hides only their own rows: hiding sets hidden_at on that row alone, so the other
-- participant's history is unchanged. Rows are deleted once they are older than the
-- configured retention (calls.historyDays, 90 days by default), hidden or not.
-- A new table only: no existing data changes.
CREATE TABLE messaging.call_history (
    user_id      uuid NOT NULL REFERENCES messaging.known_user,
    call_id      uuid NOT NULL,
    peer_user_id uuid NOT NULL REFERENCES messaging.known_user,
    direction    text NOT NULL CHECK (direction IN ('in', 'out')),
    started_at   timestamptz NOT NULL,
    answered_at  timestamptz,
    ended_at     timestamptz NOT NULL,
    duration_s   integer NOT NULL CHECK (duration_s >= 0),
    outcome      text NOT NULL CHECK (outcome IN
        ('completed', 'missed', 'declined', 'cancelled', 'busy', 'failed', 'unreachable')),
    hidden_at    timestamptz,
    PRIMARY KEY (user_id, call_id)
);
-- A user's history, newest first (GET /v1/calls).
CREATE INDEX call_history_by_user
    ON messaging.call_history (user_id, ended_at DESC, call_id DESC);
-- The retention purge.
CREATE INDEX call_history_by_end ON messaging.call_history (ended_at);
