CREATE TABLE offline_receipts (
 device_id INTEGER NOT NULL REFERENCES device(id) ON DELETE CASCADE,
 chat_jid TEXT NOT NULL, msg_id TEXT NOT NULL, user_jid TEXT NOT NULL,
 receipt_type INTEGER NOT NULL, ts_ms BIGINT NOT NULL, received_at_ms BIGINT NOT NULL,
 PRIMARY KEY (device_id, chat_jid, msg_id, user_jid, receipt_type)
);
CREATE INDEX offline_receipts_age ON offline_receipts(device_id, received_at_ms);
