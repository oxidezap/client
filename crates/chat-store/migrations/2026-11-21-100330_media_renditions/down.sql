DROP INDEX idx_messages_rendition_parent;
ALTER TABLE messages DROP COLUMN rendition_parent_id;
DELETE FROM chat_store_meta WHERE key = 'media_renditions';
