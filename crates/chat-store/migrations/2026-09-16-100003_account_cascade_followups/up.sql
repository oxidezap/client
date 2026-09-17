-- Two tables came in after the account cascade migration and without its
-- foreign key: `avatar_descriptors` and `contact_labels`. Both are keyed by
-- `device_id` and both are account data, so a reset or a removal that only
-- deletes the upstream `device` row would leave them behind -- a descriptor
-- naming the previous account's picture, and a set of labels for JIDs the
-- re-paired account has never seen. The chat store's whole account-lifecycle
-- story is that the cascade *is* the purge, so a table outside it is a table
-- that leaks across a re-pair.
--
-- SQLite cannot add a constraint to an existing table, so both are rebuilt the
-- way the older cascade migration rebuilt its six. `device_id` is already the
-- account key, so no data changes; the constraint is what changes.
CREATE TABLE avatar_descriptors_new (
    device_id       INTEGER NOT NULL,
    jid             TEXT NOT NULL,
    picture_id      TEXT NOT NULL,
    cache_key       TEXT NOT NULL,
    updated_at_ms   BIGINT NOT NULL,
    seq             BIGINT NOT NULL,
    PRIMARY KEY (device_id, jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
INSERT INTO avatar_descriptors_new
    (device_id, jid, picture_id, cache_key, updated_at_ms, seq)
SELECT device_id, jid, picture_id, cache_key, updated_at_ms, seq
FROM avatar_descriptors;
DROP TABLE avatar_descriptors;
ALTER TABLE avatar_descriptors_new RENAME TO avatar_descriptors;

CREATE TABLE contact_labels_new (
    device_id INTEGER NOT NULL,
    jid TEXT NOT NULL,
    alias TEXT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (device_id, jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
INSERT INTO contact_labels_new (device_id, jid, alias, tags)
SELECT device_id, jid, alias, tags
FROM contact_labels;
DROP TABLE contact_labels;
ALTER TABLE contact_labels_new RENAME TO contact_labels;
