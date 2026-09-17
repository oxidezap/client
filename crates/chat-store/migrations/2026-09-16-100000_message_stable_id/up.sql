-- Stable message identity, cheaper server-ack index, and a codec slot.
--
-- 1. `id INTEGER PRIMARY KEY`: an explicit, persisted alias of the rowid.
--    Every arrival-order read, cursor, and the FTS external-content index key
--    off the implicit rowid today, so a VACUUM or table rewrite renumbers the
--    very values cursors hold and the index points at. An INTEGER PRIMARY KEY
--    is the same rowid under a durable name: VACUUM preserves it, and the
--    migration below carries the old values over (`id = old rowid`), so live
--    cursors and the FTS mapping survive the rewrite.
--
-- 2. `idx_messages_by_id` becomes partial (`WHERE from_me = TRUE`). The only
--    device-wide `(device_id, msg_id)` lookup is the chatless server-ack path,
--    which always filters `from_me = true` (outbound rows only); every other
--    `msg_id` lookup is chat-scoped and served by the composite PK autoindex.
--    Outbound rows are a small fraction of a store, so the index shrinks to
--    roughly that fraction. EXPLAIN QUERY PLAN on the ack shape still reports
--    the index (pinned by test).
--
-- 3. `proto_codec INTEGER NOT NULL DEFAULT 0`: storage representation of the
--    `proto` blob. 0 = raw protobuf, 1 = zlib-compressed protobuf. Large
--    protos skew heavily (a fraction of a percent of rows holds megabytes),
--    so only those pay compression; the reader switches on the codec.
--
-- FTS objects are dropped here and recreated + rebuilt by `ensure_fts` at the
-- next open (same precedent as the sender-identity migration): a DROP TABLE
-- fires no trigger and renumbers rowids, so the surviving index would
-- describe rows that no longer exist under those numbers.
--
-- 4. `FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE`: this
--    migration is not the one that introduces the constraint (that is the
--    multi-account cascade migration, timestamped one second before
--    midnight the day before so it unambiguously runs first), but it is the
--    last migration to rebuild this table, so it is the one that has to
--    carry the constraint forward -- recreating `messages` here without it
--    would silently drop it.
DROP TRIGGER IF EXISTS messages_fts_ai;
DROP TRIGGER IF EXISTS messages_fts_ad;
DROP TRIGGER IF EXISTS messages_fts_au;
DROP TABLE IF EXISTS messages_fts;

CREATE TABLE messages_new (
    id INTEGER PRIMARY KEY,
    device_id INTEGER NOT NULL,
    chat_jid TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    sender_jid TEXT NOT NULL,
    from_me BOOLEAN NOT NULL DEFAULT FALSE,
    timestamp_ms BIGINT NOT NULL,
    kind TEXT NOT NULL,
    text_content TEXT,
    proto BLOB,
    proto_codec INTEGER NOT NULL DEFAULT 0,
    status INTEGER NOT NULL DEFAULT 1,
    starred BOOLEAN NOT NULL DEFAULT FALSE,
    edited_at_ms BIGINT,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE(device_id, chat_jid, msg_id, sender_jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);

-- `id` takes over the old rowid values, so arrival order, live cursors, and
-- (after the FTS rebuild below) the index mapping are unchanged by the move.
INSERT INTO messages_new
    (id, device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms, kind,
     text_content, proto, proto_codec, status, starred, edited_at_ms, revoked)
SELECT rowid, device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms,
       kind, text_content, proto, 0, status, starred, edited_at_ms, revoked
  FROM messages;

DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;

CREATE INDEX idx_messages_chat_time
    ON messages (device_id, chat_jid, timestamp_ms);
CREATE INDEX idx_messages_by_id
    ON messages (device_id, msg_id)
    WHERE from_me = TRUE;
