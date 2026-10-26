CREATE TABLE session_recovery (
    device_id INTEGER PRIMARY KEY NOT NULL REFERENCES device(id) ON DELETE CASCADE,
    logout_message TEXT NOT NULL,
    successor INTEGER REFERENCES device(id) ON DELETE SET NULL
);
