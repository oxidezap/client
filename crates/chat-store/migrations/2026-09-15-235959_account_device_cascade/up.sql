-- Tie client-owned account data to the upstream device lifecycle.
--
-- whatsapp-rust enables foreign keys on every pooled connection. Rebuilding
-- these tables is required because SQLite cannot add a foreign-key constraint
-- to an existing table with ALTER TABLE. The FTS virtual table is recreated by
-- ChatStore::new after migrations, so remove its triggers before replacing
-- messages.

DROP TRIGGER IF EXISTS messages_fts_ai;
DROP TRIGGER IF EXISTS messages_fts_ad;
DROP TRIGGER IF EXISTS messages_fts_au;
DROP TABLE IF EXISTS messages_fts;

CREATE TABLE chats_new (
    device_id INTEGER NOT NULL,
    jid TEXT NOT NULL,
    name TEXT,
    last_message_ts BIGINT NOT NULL DEFAULT 0,
    last_message_preview TEXT,
    last_message_kind TEXT,
    unread_count INTEGER NOT NULL DEFAULT 0,
    pinned_at BIGINT,
    muted_until BIGINT,
    archived BOOLEAN NOT NULL DEFAULT FALSE,
    ephemeral_expiration INTEGER,
    read_boundary_ms BIGINT NOT NULL DEFAULT 0,
    read_boundary_ids TEXT,
    PRIMARY KEY (device_id, jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
INSERT INTO chats_new
    (device_id, jid, name, last_message_ts, last_message_preview,
     last_message_kind, unread_count, pinned_at, muted_until, archived,
     ephemeral_expiration, read_boundary_ms, read_boundary_ids)
SELECT device_id, jid, name, last_message_ts, last_message_preview,
       last_message_kind, unread_count, pinned_at, muted_until, archived,
       ephemeral_expiration, read_boundary_ms, read_boundary_ids
FROM chats;
DROP TABLE chats;
ALTER TABLE chats_new RENAME TO chats;
CREATE INDEX idx_chats_order ON chats (device_id, last_message_ts DESC, jid DESC);
CREATE INDEX idx_chats_pinned
    ON chats (device_id, pinned_at DESC, last_message_ts DESC, jid DESC);

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
    PRIMARY KEY (device_id, chat_jid, msg_id, sender_jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
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

CREATE TABLE reactions_new (
    device_id INTEGER NOT NULL,
    chat_jid TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    sender_jid TEXT NOT NULL,
    emoji TEXT NOT NULL,
    ts_ms BIGINT NOT NULL,
    PRIMARY KEY (device_id, chat_jid, msg_id, sender_jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
INSERT INTO reactions_new
    (device_id, chat_jid, msg_id, sender_jid, emoji, ts_ms)
SELECT device_id, chat_jid, msg_id, sender_jid, emoji, ts_ms
FROM reactions;
DROP TABLE reactions;
ALTER TABLE reactions_new RENAME TO reactions;

CREATE TABLE contacts_new (
    device_id INTEGER NOT NULL,
    jid TEXT NOT NULL,
    push_name TEXT,
    full_name TEXT,
    first_name TEXT,
    business_name TEXT,
    PRIMARY KEY (device_id, jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
INSERT INTO contacts_new
    (device_id, jid, push_name, full_name, first_name, business_name)
SELECT device_id, jid, push_name, full_name, first_name, business_name
FROM contacts;
DROP TABLE contacts;
ALTER TABLE contacts_new RENAME TO contacts;

CREATE TABLE message_receipts_new (
    device_id INTEGER NOT NULL,
    chat_jid TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    user_jid TEXT NOT NULL,
    receipt_type INTEGER NOT NULL,
    ts_ms BIGINT NOT NULL,
    PRIMARY KEY (device_id, chat_jid, msg_id, user_jid, receipt_type),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
INSERT INTO message_receipts_new
    (device_id, chat_jid, msg_id, user_jid, receipt_type, ts_ms)
SELECT device_id, chat_jid, msg_id, user_jid, receipt_type, ts_ms
FROM message_receipts;
DROP TABLE message_receipts;
ALTER TABLE message_receipts_new RENAME TO message_receipts;

CREATE TABLE media_refs_new (
    device_id INTEGER NOT NULL,
    file_sha256 BLOB NOT NULL,
    file_path TEXT NOT NULL,
    mime_type TEXT,
    size_bytes BIGINT,
    downloaded_at_ms BIGINT NOT NULL,
    PRIMARY KEY (device_id, file_sha256),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
INSERT INTO media_refs_new
    (device_id, file_sha256, file_path, mime_type, size_bytes, downloaded_at_ms)
SELECT device_id, file_sha256, file_path, mime_type, size_bytes, downloaded_at_ms
FROM media_refs;
DROP TABLE media_refs;
ALTER TABLE media_refs_new RENAME TO media_refs;
