-- Handles may contain hyphens (REQUEST PUSH 42): 3 to 30 characters, a letter first, then
-- letters, digits, underscores or hyphens, never a hyphen last and never two in a row.
-- The same rule as push_auth_core::rules::handle. PostgreSQL's regular expressions have no
-- lookahead, so "no --" is a second condition. Every existing handle matches, so re-adding
-- the CHECK (which validates the rows) changes no data.
ALTER TABLE auth.user_account DROP CONSTRAINT user_account_handle_check;
ALTER TABLE auth.user_account ADD CONSTRAINT user_account_handle_check CHECK (
    handle::text ~ '^[a-z][a-z0-9_-]{1,28}[a-z0-9_]$' AND strpos(handle::text, '--') = 0);
