-- Reversible: the descriptors are derived state, and dropping them costs one
-- metadata refresh on the next start rather than any history. The cached bytes
-- are untouched and the sweep reclaims whatever the refresh supersedes.
DROP TABLE avatar_descriptors;
