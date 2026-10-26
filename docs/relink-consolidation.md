# Relink consolidation

Pairing uses an isolated local account. Retirement requires a connected
successor, a complete paired device loaded from durable storage, and both
PN and LID matching after the dependency's `Jid::to_non_ad` normalization.
Missing, malformed, different or conflicting identifiers preserve both
accounts. Hosted and unsupported namespaces also retain history. Names,
photos and partial strings provide no identity evidence.

The retirement journal survives device-row deletion. It records identity
evidence and a strictly increasing successor before cleanup, preserves
successor chains and resumes incomplete cleanup after restart. A successor
cannot be reset while pending retirement depends on it. The lifecycle joins
old session, chat, plugin and publication writers, drains web media operations,
then reuses account-scoped cleanup. Failure preserves the successor and
leaves the journal retryable. Credentials and device ids are never moved. A nonempty journal blocks
schema downgrade so cleanup intents and redirects cannot be discarded.

A retired connection closes. Native consumers receive an explicit redirect
and must attach again; the CLI never changes its bound identity. GUI active
and retained selections reconcile the redirect, including after restart.
Redirects are revalidated against the successor's identity before use.

Native SQLite and durable OPFS support consolidation. The relaxed IndexedDB
backend cannot confirm ordinary-write durability, so it preserves old
history until a durable backend is available.
