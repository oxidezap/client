-- Which status updates have been watched on this device.
--
-- A status update is a message row like any other, and nothing in the message
-- table can say it has been looked at: the read state of incoming history is
-- the chat's unread cursor, and the status broadcast is one chat carrying
-- everybody's updates — clearing it would mark every contact's run watched at
-- once. WhatsApp's own answer is a read receipt, which is a privacy setting
-- the library does not expose, so the honest half of it is local: the ring
-- stops claiming there is something new *here*, and nobody is told.
--
-- Local, and therefore stored rather than derived. Kept in the same file as
-- the rows it describes so a wipe takes both, and keyed by device_id like
-- every other table here, because a re-pairing files its history under a new
-- device and must not inherit the last one's views.
CREATE TABLE status_views (
    device_id INTEGER NOT NULL,
    msg_id TEXT NOT NULL,
    -- When it was watched, which is what makes the row prunable: an update
    -- lapses 24 hours after it was posted, and it cannot be watched before it
    -- is posted, so a view older than that describes something nobody can see
    -- any more.
    watched_at_ms BIGINT NOT NULL,
    PRIMARY KEY (device_id, msg_id)
);
