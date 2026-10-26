CREATE TABLE audio_played_pending (
    device_id INTEGER NOT NULL REFERENCES device(id) ON DELETE CASCADE,
    chat_jid TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    author_jid TEXT NOT NULL,
    received_at_ms BIGINT NOT NULL,
    ambiguous BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (device_id, chat_jid, msg_id, author_jid)
);
