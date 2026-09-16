-- Local labels for contacts: a free alias and a tag set, both owned by
-- this device and never synced. Separate table rather than columns on
-- `contacts` because that table is written wholesale by history materialize
-- batches; a label update must not race a contact upsert, and a contact row
-- deleted and re-materialized must not take its labels with it.
CREATE TABLE contact_labels (
    device_id INTEGER NOT NULL,
    jid TEXT NOT NULL,
    alias TEXT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (device_id, jid)
);
