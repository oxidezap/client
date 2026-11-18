CREATE TABLE scheduled_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    device_id INTEGER NOT NULL REFERENCES device(id) ON DELETE CASCADE,
    jid TEXT NOT NULL,
    text TEXT NOT NULL,
    send_at_ms BIGINT NOT NULL
);

CREATE INDEX scheduled_messages_due ON scheduled_messages(device_id, send_at_ms, id);
