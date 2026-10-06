-- A NULL hierarchy is unknown, not standalone. The typed value is stored as
-- JSON so parent identity and subgroup role move atomically and remain
-- forward-compatible with roles introduced by newer engine versions.
ALTER TABLE chats ADD COLUMN group_hierarchy TEXT;
