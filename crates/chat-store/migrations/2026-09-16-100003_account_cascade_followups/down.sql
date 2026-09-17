-- Return both tables to the shape they had before the constraint was added.
-- Their data is derived or device-local, so nothing durable is lost, and a
-- downgrade that had to keep the new constraint would not be a downgrade.
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

CREATE TABLE contact_labels_old (
    device_id INTEGER NOT NULL,
    jid TEXT NOT NULL,
    alias TEXT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (device_id, jid)
);
INSERT INTO contact_labels_old (device_id, jid, alias, tags)
SELECT device_id, jid, alias, tags
FROM contact_labels;
DROP TABLE contact_labels;
ALTER TABLE contact_labels_old RENAME TO contact_labels;
