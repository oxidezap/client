-- A full message-identity repair is needed only when this device's mapping
-- ledger advances; mapping-triggered repairs record the current revision after
-- scoping their work to the newly learned aliases. The triggers preserve a
-- revision across mapping changes that share a timestamp or replace a row.
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
