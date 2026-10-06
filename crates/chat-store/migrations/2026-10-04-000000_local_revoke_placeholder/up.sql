-- Mutation origin must not change the target message authorship.
ALTER TABLE messages ADD COLUMN local_revoke_placeholder BOOLEAN NOT NULL DEFAULT FALSE;
