-- Checks the existing rows against the message constraints 0005 added NOT VALID. VALIDATE
-- CONSTRAINT scans the table under SHARE UPDATE EXCLUSIVE, which does not block reads or
-- writes, so messages keep flowing while it runs (new rows were checked from 0005 on).
-- A migration of its own: the brief ACCESS EXCLUSIVE lock of 0005 is released before the
-- long scan starts.

ALTER TABLE messaging.message VALIDATE CONSTRAINT message_content_type_check;
ALTER TABLE messaging.message VALIDATE CONSTRAINT message_card_answer;
