-- Losing an intent or redirect could strand cleanup or select a removed id.
CREATE TABLE retirement_downgrade_guard (empty INTEGER NOT NULL CHECK(empty = 0));
INSERT INTO retirement_downgrade_guard SELECT COUNT(*) FROM account_retirement;
DROP TABLE retirement_downgrade_guard;
DROP TABLE account_retirement;
