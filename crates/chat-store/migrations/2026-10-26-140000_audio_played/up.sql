CREATE TABLE audio_played_receipts (
 device_id INTEGER NOT NULL REFERENCES device(id) ON DELETE CASCADE,
 message_row_id BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
 PRIMARY KEY (device_id, message_row_id)
);
