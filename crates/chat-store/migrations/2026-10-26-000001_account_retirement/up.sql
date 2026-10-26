-- Survives both device-row deletion and interrupted external cleanup.
CREATE TABLE account_retirement (
    old_id INTEGER PRIMARY KEY NOT NULL CHECK(old_id > 0),
    successor INTEGER NOT NULL CHECK(successor > old_id),
    pn TEXT NOT NULL,
    lid TEXT NOT NULL,
    complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN (0, 1))
);
