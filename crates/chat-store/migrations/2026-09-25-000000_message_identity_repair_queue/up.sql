ALTER TABLE message_identity_repair_state
ADD COLUMN full_repair_pending BOOLEAN NOT NULL DEFAULT FALSE;

-- A prior scoped pass could have advanced the old global watermark past an
-- unrelated mapping change. Recheck existing stores once under the queued rule.
UPDATE message_identity_repair_state SET full_repair_pending = TRUE;

-- New mappings can be repaired by their PN/LID component. Remaps and
-- deletions instead use full_repair_pending because the old component is gone.
CREATE TABLE message_identity_repair_pending (
    device_id INTEGER NOT NULL,
    lid TEXT NOT NULL,
    PRIMARY KEY (device_id, lid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);

DROP TRIGGER message_identity_mapping_insert;
DROP TRIGGER message_identity_mapping_update;
DROP TRIGGER message_identity_mapping_delete;

CREATE TRIGGER message_identity_mapping_insert
AFTER INSERT ON lid_pn_mapping
BEGIN
    UPDATE message_identity_repair_state
    SET mapping_revision = mapping_revision + 1
    WHERE device_id = NEW.device_id;
    INSERT INTO message_identity_repair_pending(device_id, lid)
    VALUES (NEW.device_id, NEW.lid)
    ON CONFLICT(device_id, lid) DO NOTHING;
END;

-- A remap loses the old pairing, so keep the device dirty for a full repair.
CREATE TRIGGER message_identity_mapping_update
AFTER UPDATE ON lid_pn_mapping
WHEN OLD.device_id IS NOT NEW.device_id
  OR OLD.lid IS NOT NEW.lid
  OR OLD.phone_number IS NOT NEW.phone_number
BEGIN
    UPDATE message_identity_repair_state
    SET mapping_revision = mapping_revision + 1,
        full_repair_pending = TRUE
    WHERE device_id IN (OLD.device_id, NEW.device_id);
END;

-- Deletions likewise need a full sweep; do not enqueue a child row while
-- the upstream device table may be cascading the mapping away.
CREATE TRIGGER message_identity_mapping_delete
AFTER DELETE ON lid_pn_mapping
BEGIN
    UPDATE message_identity_repair_state
    SET mapping_revision = mapping_revision + 1,
        full_repair_pending = TRUE
    WHERE device_id = OLD.device_id;
END;
