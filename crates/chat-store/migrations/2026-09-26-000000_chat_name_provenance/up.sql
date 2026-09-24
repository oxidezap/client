-- Keep address-book-derived history names distinguishable from independent
-- display-name/username values so ContactRemoved can clear only the former.
-- Existing rows stay unclassified rather than guessing at their provenance.
ALTER TABLE chats ADD COLUMN name_from_address_book BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE chats ADD COLUMN address_book_fallback TEXT;

-- A removal may race with delayed history sync; retain the account-scoped
-- withdrawal so stale history cannot rematerialize the removed address-book name.
CREATE TABLE contact_name_removals (
    device_id INTEGER NOT NULL,
    jid TEXT NOT NULL,
    PRIMARY KEY (device_id, jid),
    FOREIGN KEY (device_id) REFERENCES device(id) ON DELETE CASCADE
);
