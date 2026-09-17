-- Return the table to the shape it had before the constraint was added:
-- descriptors are derived state, so nothing durable is lost, and a downgrade
-- that had to keep the new constraint would not be a downgrade.
CREATE TABLE avatar_descriptors_old (
    device_id       INTEGER NOT NULL,
    jid             TEXT NOT NULL,
    picture_id      TEXT NOT NULL,
    cache_key       TEXT NOT NULL,
    updated_at_ms   BIGINT NOT NULL,
    seq             BIGINT NOT NULL,
    PRIMARY KEY (device_id, jid)
);
INSERT INTO avatar_descriptors_old
    (device_id, jid, picture_id, cache_key, updated_at_ms, seq)
SELECT device_id, jid, picture_id, cache_key, updated_at_ms, seq
FROM avatar_descriptors;
DROP TABLE avatar_descriptors;
ALTER TABLE avatar_descriptors_old RENAME TO avatar_descriptors;
