UPDATE message_identity_repair_state
SET repaired_revision = mapping_revision - 1
WHERE full_repair_pending = TRUE
   OR EXISTS (
       SELECT 1 FROM message_identity_repair_pending
       WHERE message_identity_repair_pending.device_id = message_identity_repair_state.device_id
   );

DROP TRIGGER message_identity_mapping_delete;
DROP TRIGGER message_identity_mapping_update;
DROP TRIGGER message_identity_mapping_insert;
DROP TABLE message_identity_repair_pending;
ALTER TABLE message_identity_repair_state DROP COLUMN full_repair_pending;

CREATE TRIGGER message_identity_mapping_insert
AFTER INSERT ON lid_pn_mapping
BEGIN
    UPDATE message_identity_repair_state
    SET mapping_revision = mapping_revision + 1
    WHERE device_id = NEW.device_id;
END;

CREATE TRIGGER message_identity_mapping_update
AFTER UPDATE ON lid_pn_mapping
BEGIN
    UPDATE message_identity_repair_state
    SET mapping_revision = mapping_revision + 1
    WHERE device_id = NEW.device_id;
END;

CREATE TRIGGER message_identity_mapping_delete
AFTER DELETE ON lid_pn_mapping
BEGIN
    UPDATE message_identity_repair_state
    SET mapping_revision = mapping_revision + 1
    WHERE device_id = OLD.device_id;
END;
