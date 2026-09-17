-- The avatar descriptor table came in without the account-lifecycle foreign
-- key the rest of the chat store carries, and it is exactly the table that
-- must not outlive its account: a stored descriptor names a picture id and a
-- cache key for one JID, so a row left behind by a reset or a removal points
-- the re-paired account at the previous account's picture.
--
-- SQLite cannot add a constraint to an existing table, so this rebuilds it, the
-- same way the cascade migration does for every older table. `device_id` is
-- already the account key, so no data changes; the constraint is what changes.
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
