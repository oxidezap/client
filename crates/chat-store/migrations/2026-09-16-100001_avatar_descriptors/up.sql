-- Durable avatar descriptors: which picture a chat is showing, and where its
-- bytes live.
--
-- The media cache already addresses a picture deterministically by
-- `(jid, picture_id)`, so cached avatar bytes survive a restart until the
-- budget sweep reclaims them. What did not survive was the pointer: nothing
-- durable said which picture id belonged to a JID, so a restarted process had
-- the bytes on disk and no way to name them until WhatsApp answered a fresh
-- metadata lookup. This table is that pointer.
--
-- Only stable metadata is stored. The signed CDN URL is deliberately absent:
-- it expires, and persisting it would be storing a credential in the clear.
-- The bytes never live here either; they stay in the media cache under
-- `cache_key`.
--
-- `seq` orders writes. Two pictures can be resolved in quick succession and
-- their two commits can reach this table out of order, because they come from
-- separate tasks; without an order carried in the row, the stale one can land
-- last and a restart would then draw the picture that lost. The writer only
-- accepts a `seq` greater than the row's, so the newest resolution is the one
-- that survives regardless of commit order.
CREATE TABLE avatar_descriptors (
    device_id       INTEGER NOT NULL,
    jid             TEXT NOT NULL,
    picture_id      TEXT NOT NULL,
    cache_key       TEXT NOT NULL,
    updated_at_ms   BIGINT NOT NULL,
    seq             BIGINT NOT NULL,
    PRIMARY KEY (device_id, jid)
);
