-- Restores the pre-labels schema. Label data is device-local metadata with
-- no source to re-read it from, so downgrading drops it.
DROP TABLE contact_labels;
