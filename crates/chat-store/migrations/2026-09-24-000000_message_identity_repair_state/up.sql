-- Keep a per-device generation for durable mapping-ledger changes so startup
-- can tell whether message identities need another repair. A later migration
-- adds the pending-alias queue that lets scoped repairs acknowledge this
-- generation without covering unrelated mapping changes.
CREATE TABLE message_identity_repair_state (
    device_id INTEGER NOT NULL PRIMARY KEY,
    mapping_revision BIGINT NOT NULL DEFAULT 0,
    repaired_revision BIGINT NOT NULL DEFAULT -1,
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);

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
