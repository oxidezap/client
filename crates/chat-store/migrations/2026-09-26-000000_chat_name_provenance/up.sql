-- Keep address-book-derived history names distinguishable from independent
-- display-name/username values so ContactRemoved can clear only the former.
-- Existing rows stay unclassified rather than guessing at their provenance.
ALTER TABLE chats ADD COLUMN name_from_address_book BOOLEAN NOT NULL DEFAULT FALSE;
