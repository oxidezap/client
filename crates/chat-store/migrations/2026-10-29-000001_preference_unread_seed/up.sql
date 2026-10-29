ALTER TABLE chats ADD COLUMN unread_initialized BOOLEAN NOT NULL DEFAULT 1 CHECK (unread_initialized IN (0, 1));
