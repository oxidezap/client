-- Removal bookkeeping is bounded separately from retained picture descriptors.
CREATE INDEX avatar_removal_order ON avatar_descriptors(device_id, seq)
WHERE picture_id = '';
