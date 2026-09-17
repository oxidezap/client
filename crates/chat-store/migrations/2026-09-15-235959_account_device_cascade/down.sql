-- This migration changes the lifecycle invariant of every chat table. A
-- downgrade could silently leave rows orphaned while another process still
-- owns the database, so the migration is intentionally not reversible.
SELECT chat_store_account_device_cascade_migration_is_irreversible;
