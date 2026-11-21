ALTER TABLE messages ADD COLUMN rendition_parent_id TEXT;
CREATE INDEX idx_messages_rendition_parent ON messages(device_id, chat_jid, rendition_parent_id, from_me)
    WHERE rendition_parent_id IS NOT NULL;
