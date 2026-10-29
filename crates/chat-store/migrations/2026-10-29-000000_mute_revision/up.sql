ALTER TABLE chats ADD COLUMN mute_revision BIGINT NOT NULL DEFAULT 0 CHECK (mute_revision >= 0);
