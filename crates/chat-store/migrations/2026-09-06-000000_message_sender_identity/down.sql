-- This migration changes the identity key and cannot safely collapse rows
-- that became distinct. Fail the harness before it records a downgrade.
SELECT chat_store_sender_identity_migration_is_irreversible;
