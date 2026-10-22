CREATE TABLE shared_group_history (
 device_id INTEGER NOT NULL REFERENCES device(id) ON DELETE CASCADE,
 chat_jid TEXT NOT NULL, msg_id TEXT NOT NULL, sender_jid TEXT NOT NULL,
 timestamp_ms BIGINT NOT NULL, proto BLOB NOT NULL,
 attempted_ms BIGINT NOT NULL DEFAULT 0,
 completed BOOLEAN NOT NULL DEFAULT FALSE,
 PRIMARY KEY(device_id, chat_jid, msg_id, sender_jid)
);
ALTER TABLE messages ADD COLUMN received_as_shared_history BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX shared_group_history_pending ON shared_group_history(device_id, completed, attempted_ms, timestamp_ms, msg_id);
