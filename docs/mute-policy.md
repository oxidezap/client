# Conversation mute policy

A muted conversation keeps its actual unread count and manual unread marker.
It contributes no unread messages or dot to the tray/taskbar and produces no
system notification on desktop or web, including personal mentions. Unmuting
or reaching a finite deadline restores its contribution without marking any
message read. Conversation and destination badges still describe their unread
content.

The store persists mute deadlines per device/account. App-state timestamps use
unix milliseconds: zero clears the mute, negative values mean indefinite, and
positive values are deadlines. A mute without its required timestamp is ignored.
History uses seconds, including the unsigned maximum for indefinite mute, and
cannot overwrite an explicit mute/unmute received from app-state. PN/LID aliases
are resolved by the store; notification policy considers both persisted sides
until reconciliation finishes.

Channels also expose the viewer's `MUTE_ADMIN_ACTIVITY` setting. Subscribed-list
and selective channel metadata lookups persist explicit true/false results;
missing settings leave the stored value intact. `MUTE_FOLLOWER_ACTIVITY` describes
admin activity alerts and is not used as the conversation mute. Channel metadata
is refreshed by the existing resolver on connection/reconnection and explicit
channel queries. The protocol's MuteUpdate path remains applicable to channel
JIDs as well.

Mute changes invalidate the affected identity, including a conversation outside
the first list page. Summaries and live message events carry the deadline so
startup snapshots and the first arrival before hydration respect it. The daemon
recomputes attention on changes and finite deadlines with a platform timer.
The timer does not scan conversations for every video frame. Notifications use
the committed store policy, rechecked after asynchronous identity/name work,
and retain the existing offline, self-read, self-echo, focus and duplicate guards.

## Preserved Web evidence

Inspected on 2026-10-29. WhatSpec commit
[`eb16f595d455d8b0b3f7966a59444f4f9894c127`](https://github.com/oxidezap/whatspec/tree/eb16f595d455d8b0b3f7966a59444f4f9894c127)
has schema 4.4.0 and WhatsApp version `2.3000.1047483476`. This is a preserved
snapshot, not a claim about the version currently served to every account.

All 579 JS bundles were verified against that commit's manifest and lock,
including the exact multiset of SHA-256/size pairs and set hash
`05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`.
The [exact release asset](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz)
has SHA-256 `22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`,
matching GitHub's asset digest. Modules were extracted with Acorn without parser
recovery or executing the bundle. Each module below has one definition in the
verified set.

In bundle SHA-256
`f588ee2f033b87e018a3be34a37ec1347d8b6b8700de539e6be31481b29c4690`:

| Module | Byte range | Observed behavior |
| --- | --- | --- |
| `WAWebMuteChatSync` | 1664460..1668188 | Requires muted and a timestamp when muting; converts milliseconds to local seconds; sends -1 for indefinite mute. |
| `WAWebNewsletterModelUtils` | 2688842..2698552 | Reads `MUTE_ADMIN_ACTIVITY`; ON becomes indefinite, OFF becomes zero, absent stays unknown. |
| `WAWebNewsletterUpdateUserSettingJob` | 2828330..2830563 | Sends ADMIN/FOLLOWER activity settings independently. |

`WAWebMsgNotification` calls `WAWebNotificationHelpers.isMutedAndNotMentioned`.
The official snapshot has a mention exception. This client deliberately follows
the requested stricter policy: all messages in a muted conversation are silent.
These are static code findings, not authenticated server or device observations.
