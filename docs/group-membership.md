# Group membership labels

Membership notifications retain the actor and each target JID, plus the explicit
`participant_pn`, `phone_number` and `lid` aliases supplied for that person.
Account-local contacts resolve first, followed by the event display label and an
internationally formatted PN. A LID without a known PN reads "Unknown contact".
Device suffixes are removed before formatting or identity comparison. Display
names never establish identity, including for the "left" wording.

Identified notifications are stored as `group_membership` message rows with the
structured facts in a separate nullable column. Hydration resolves labels again,
so contact changes and newly learned mappings apply after reload. Labels are not
written back. The rows contribute no unread count or read-receipt candidates.
Anonymous notifications remain transient because a timestamp is not a safe
persistent deduplication key. Previously discarded notifications cannot be
recovered by this change. Non-membership notices remain transient.

Resolution uses the session's local contact store and PN/LID cache only. It does
not fetch profiles or group metadata, including when reading retained history.
The GUI, chat previews and CLI message DTO use the same membership sentence.

## Protocol evidence

Checked on 2026-10-27 against [whatspec eb16f595](https://github.com/oxidezap/whatspec/tree/eb16f595d455d8b0b3f7966a59444f4f9894c127),
schema 4.4.0, WhatsApp Web 2.3000.1047483476. All 579 bundle sizes and SHA-256 hashes
matched that commit's lock. The [release asset](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz)
has SHA-256 `22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`.

AST extraction found one `WAWebHandleGroupNotification` definition at byte range
1693402..1716842 in bundle SHA-256
`09d91fdc4089c50ab1d35dd6c796ee82ef4c3296710316d20383c85738b871b8`.
Its parser reads the actor from root `participant` and typed `participant_pn`.
Its participant mapper reads distinct `jid`, typed `phone_number`, typed `lid`,
and `display_name` fields for add/remove/promote/demote. The implementation uses
these fields separately. This is static evidence for that Web snapshot, not an
authenticated test or a claim about all server/client versions.
