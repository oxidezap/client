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
--
-- Only rows whose `device_id` still names a `device` are copied. The new
-- constraint is enforced inside this migration's own transaction, so an
-- orphaned row would abort the upgrade; and an orphan is unreadable anyway,
-- since every query joins the account it belongs to.
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
SELECT a.device_id, a.jid, a.picture_id, a.cache_key, a.updated_at_ms, a.seq
FROM avatar_descriptors a
WHERE EXISTS (SELECT 1 FROM device d WHERE d.id = a.device_id);
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
SELECT c.device_id, c.jid, c.alias, c.tags
FROM contact_labels c
WHERE EXISTS (SELECT 1 FROM device d WHERE d.id = c.device_id);
DROP TABLE contact_labels;
ALTER TABLE contact_labels_new RENAME TO contact_labels;
