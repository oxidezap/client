-- A full message-identity repair is needed only when this device's mapping
-- ledger advances; mapping-triggered repairs update the marker after scoping
-- their work to the newly learned aliases.
CREATE TABLE message_identity_repair_state (
    device_id INTEGER NOT NULL PRIMARY KEY,
    mapping_high_water BIGINT NOT NULL,
    mapping_count BIGINT NOT NULL,
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
