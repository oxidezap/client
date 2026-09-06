-- Message ids are only unique within a chat and sender. Rebuild the table so
-- a replay or an id reused by another group participant cannot replace the
-- first participant's row.
DROP TRIGGER IF EXISTS messages_fts_ai;
DROP TRIGGER IF EXISTS messages_fts_ad;
DROP TRIGGER IF EXISTS messages_fts_au;
DROP TABLE IF EXISTS messages_fts;

CREATE TABLE messages_new (
    device_id INTEGER NOT NULL,
    chat_jid TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    sender_jid TEXT NOT NULL,
    from_me BOOLEAN NOT NULL DEFAULT FALSE,
    timestamp_ms BIGINT NOT NULL,
    kind TEXT NOT NULL,
    text_content TEXT,
    proto BLOB,
    status INTEGER NOT NULL DEFAULT 1,
    starred BOOLEAN NOT NULL DEFAULT FALSE,
    edited_at_ms BIGINT,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (device_id, chat_jid, msg_id, sender_jid)
);

INSERT INTO messages_new
    (device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms, kind,
     text_content, proto, status, starred, edited_at_ms, revoked)
SELECT device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms, kind,
       text_content, proto, status, starred, edited_at_ms, revoked
  FROM messages;

DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;

CREATE INDEX idx_messages_chat_time
    ON messages (device_id, chat_jid, timestamp_ms);
CREATE INDEX idx_messages_by_id ON messages (device_id, msg_id);
