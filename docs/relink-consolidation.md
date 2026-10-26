# Relink consolidation

CLI and tray account selection must work independently of consolidation.
A relink still allocates a separate account and retains the rejected account.
Automatic removal is not enabled by the consumer-selection change.

The proposed identity gate requires both persisted PN and LID to be present,
valid in their own namespaces, and equal after `Jid::to_non_ad`. Names,
photos, partial matches, PN alone and LID alone provide no deletion authority.
Conflicting or incomplete evidence retains both accounts. The pure
`store::same_account_identity` predicate captures this conservative gate;
it is not a durability check and has no cleanup side effects.

The pinned whatsapp-rust commit is
`633f01480fe3a95510172c6312d01484563dcf3c`. Its
`wacore/binary/src/jid.rs` separates `user`, `server`, `agent`, `device`
and `integrator`. `to_non_ad` clears device and agent while retaining the
namespace and integrator. `SqliteStore::list_devices_impl` reads persisted
PN/LID, but its `linked` flag means only that PN parsed successfully.
It is not proof that the complete new session is durable. No claim about
LID stability across relinks follows from these contracts.

Before activating automatic retirement, the lifecycle needs these additions:

1. A successful new-session durability barrier. Web's relaxed IndexedDB VFS
   does not expose completion of ordinary writes. Its in-memory SQL state
   must not authorize deletion of retained history. See `session/src/store/web.rs`.
2. A durable retirement journal outside rows deleted with the old account.
   `session_recovery.successor` currently belongs to the old account and has
   `ON DELETE SET NULL` on the target. It cannot retain redirects or resume
   media/plugin cleanup after deletion. Freeze the successor under lifecycle
   serialization before recording retirement. Reject cycles and changes of
   identity, and process successor chains without crossing different accounts.
3. Use the existing lifecycle to stop and join old sessions, writers and
   plugins before cleanup. Do not copy keys or reassign device ids. Persist
   cleanup progress separately, since `finish_forget` currently logs media
   and plugin-settings cleanup failures and can still return completed.
4. Publish an explicit retirement redirect and close old scoped connections.
   Consumers must acknowledge a new attachment before switching identity.
   GUI active and retained selections need reconciliation on snapshots and
   restart, including windows still showing old offline history.

Cleanup failure must preserve the valid new session and a retryable journal.
Restart and concurrent-window tests must cover every journal boundary, old
cleanup failure, chains, conflicts, different accounts and stale events before
activation. No real session database is needed to exercise these cases.
