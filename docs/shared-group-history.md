# Receiving deliberately shared group history

Investigation for [client #245](https://github.com/oxidezap/client/issues/245),
2026-10-22. Static protocol analysis and synthetic SQLite tests only. No account
was connected, and the issue author's authenticated observation was not repeated.

## Evidence snapshot

The remote whatspec main was re-fetched and resolved to
`46f630441389e703e04ff3fc012192cf80fdd4ba`. Manifest and bundle lock were read
from that commit, rather than the older local checkout. Schema is `4.4.0`,
WhatsApp Web snapshot is `2.3000.1047483476`, with 579 bundles and set hash
`05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`.
This identifies preserved bytes, not today's deployed WhatsApp service.

The [exact release asset](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz)
was already cached. Its SHA-256 is
`22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`,
matching the release asset digest. All 579 individual sizes and SHA-256 hashes
were checked against the selected lock. Archive hash and set hash are distinct.

Acorn parsed the original JavaScript without executing it. Complete `__d`
definitions were extracted with byte offsets. The first history-focused extraction found 112 definitions with 82 unique
names. All 30 duplicate names have identical factory hashes. Additional parser
registry and validation modules were then followed through their imports.
Relevant modules and half-open byte ranges are below.

| Module | Bundle SHA-256 | Bytes |
| --- | --- | --- |
| `WAWebDecompressAndDecodeBundle` | `012a3b924a76e250362922ed59882da7bfe95b399d33d3da1426e90323d10e5a` | 936551..936924 |
| `WAWebMessageHistoryValidation` | `09d91fdc4089c50ab1d35dd6c796ee82ef4c3296710316d20383c85738b871b8` | 4442707..4446708 |
| `WAWebGroupHistoryBundleProcessor` | `012a3b924a76e250362922ed59882da7bfe95b399d33d3da1426e90323d10e5a` | 950427..967054 |
| `WAWebParseMessageHistoryBundleProto` | `09d91fdc4089c50ab1d35dd6c796ee82ef4c3296710316d20383c85738b871b8` | 4488598..4492768 |
| `WAWebParseMessageHistoryNoticeProto` | `09d91fdc4089c50ab1d35dd6c796ee82ef4c3296710316d20383c85738b871b8` | 4492770..4494956 |
| `WAWebGroupHistoryNoticeHandler` | `09d91fdc4089c50ab1d35dd6c796ee82ef4c3296710316d20383c85738b871b8` | 607474..610912 |
| `WAWebE2EProtoUtils` | `430cb8979511aada95178605faa35a8483446f677ce6d1fe0fd66333c7879eee` | 442807..459797 |
| `WAWebGroupHistoryUtils` | `f588ee2f033b87e018a3be34a37ec1347d8b6b8700de539e6be31481b29c4690` | 4362646..4364221 |
| `WAWebGenerateMessageHistoryBundleProto` | `f588ee2f033b87e018a3be34a37ec1347d8b6b8700de539e6be31481b29c4690` | 1299976..1300973 |
| `WAWebGenerateMessageHistoryNoticeProto` | `f588ee2f033b87e018a3be34a37ec1347d8b6b8700de539e6be31481b29c4690` | 1300975..1301605 |

The [manifest](https://github.com/oxidezap/whatspec/blob/46f630441389e703e04ff3fc012192cf80fdd4ba/generated/manifest.json)
and [lock](https://github.com/oxidezap/whatspec/blob/46f630441389e703e04ff3fc012192cf80fdd4ba/generated/bundles.lock.json)
are the reproducible index. Diagnostics include incoming drops for two nonliteral
attributes and seven shadowed callback reads. Notification diagnostics report five degraded shapes and one drop each for
unresolved action tables, nonliteral attributes, byte content and shadowed
callback reads. The incoming/notification schemas were inspected too. Absence from those catalogs alone is not evidence
that a handler does not exist.

## Observed behavior and implementation boundary

The message parser plugin registry registers the two history parsers as
separate message plugins. `WAWebMessageHistoryValidation` requires nonempty
receivers, an oldest-window timestamp, a positive count, protobuf MIME type
and 32-byte media key and hashes. `WAWebDecompressAndDecodeBundle` calls
`WAGzip.inflate` before the group-history protobuf decoder.

The Web parses a media `messageHistoryBundle` separately from the group
`messageHistoryNotice`. `WAWebGroupHistoryUtils.isValidHistoryBundleToProcess`
requires a received group bundle addressed to the local account in
`historyReceivers`, as well as process-state and time-window guards. The notice
parser hides the notice from the sharing account and history recipients and
shows it to other group members. The notice handler updates per-participant
sharing state; it does not contain the shared message bodies.

The bundle processor inflates and decodes `GroupHistory`, translates message
keys from the sharing account's perspective, validates chat ownership, skips
expired/unsupported messages and existing rows, and injects the accepted rows.
`translateRegularMessageKeyToLocalReference` calls `W` in `WAWebE2EProtoUtils`.
For an incoming share, a true `fromMe` refers to the sharing account. A false
bit with no participant refers to the recipient's own account. This differs
from copying `WebMessageInfo` directly into ordinary local history.

The processor also handles reporting tokens, out-of-window pins, add-ons and
receiver feature flags. This correction does not claim complete parity with
those Web features or infer a server contract from module names. In particular,
only the primary `messages` collection is materialized here. No request for
unshared history, Restricted-chat access, view-once recovery or account-policy
change is added.

The current remote engine was `ac9d0df3d6269c6b5b60881a7154414d7c348991`;
the bridge was `dc96100911d99a6076c521965e47763f1c6123a2`. Their current source
contains send/retry support and codecs, but no receiver that downloads a
`messageHistoryBundle` and emits its messages as history. The client still pins
all engine crates together at `633f01480fe3a95510172c6312d01484563dcf3c`.
That version already implements `Downloadable` with the `Group History` HKDF
context and `/mms/group-history`, and `group_history_decode`. No dependency
update or generated protocol edits are needed.

The group's notification metadata (`group_history_sent_state` on membership
updates) is independent of the pairwise message attachment. It already reaches
`group_notice.rs`; treating it as a download source would invent a contract.

## Client behavior

Inbound durability commits persist a pending attachment instead of an unknown
message. Own, unshared, nongroup and history-sync carrier copies create no work.
Completed/discarded keys remain as deduplication records; media envelopes
are cleared on consumption. A serial session
worker downloads bounded pages, keeps transient failures for retry, rotates
attempted work behind untouched work, and retires permanently missing media or
invalid bodies. Teardown cancels its in-flight request without losing work.

A bounded, checksum-valid zlib stream and protobuf decode must complete before
any message insertion. Partial collections skip invalid individual entries.
Consumption and insertion share the writer transaction; history fills gaps,
preserves live edits/revokes and keeps sender identity. Shared rows retain a
persistent read-backfill marker so later unread recounts do not badge them.
Accepted rows keep their original timestamps and media protobufs. Store
invalidations hydrate the public UI after commit. Visible notices have their own
kind and render as group system lines, without unread attention.

A versioned startup repair moves old unknown carriers to the pending table,
reclassifies visible notices, removes hidden notices and corrects carrier badge
contributions. When a sender-matched revoke placeholder precedes its content,
the later copy supplies only its original timestamp and kind. Deleted content
stays absent.

Local ceilings of 32 MiB compressed/inflated and 10,000 top-level records bound
work; these are application policy, not asserted server limits. Effective Web
feature flags, reporting-token behavior, authenticated downloads and cross-device
server timing remain outside the synthetic tests.

## Validation

Local final validation used the normal Cargo cache with two build jobs:

- `cargo fmt --all -- --check` passed.
- `cargo clippy -p oxidezap-chat-store -p oxidezap-session --all-targets --all-features -j 2 -- -D warnings` passed.
- `cargo nextest run -p oxidezap-chat-store -p oxidezap-session --all-features --build-jobs 2 --test-threads 2` passed all 672 tests, with two intentional skips.
- Both crates' doctests completed without failures, with three ignored examples.

The exact same `tests/regression_245.rs`, SHA-256
`dccce7af6dcf23e194bcb6ebd29e4395194ea601045fd1566d3ca8b95f36cc5c`,
ran on unchanged client base `0ba4118bcdb02c613fda94c5a0208bff44316e82`.
Both tests failed there: a valid received carrier became an unknown row, and a
revoke placeholder retained the revoke's timestamp. Both pass with the correction.
The other fixtures exercise durability-hook completion, complete database closure
and reopening, repeated and reordered copies, sender collisions, local identities,
malformed/truncated bodies, partial acceptance, expired and view-once content,
notices, unread pagination, queue pressure and legacy repair.

An earlier combined `cargo test` invocation passed 281 session tests but exhausted
`a_session_is_never_observed_half_open`'s existing 500,000-poll opening budget.
Its partial-state assertion did not fail. That test's opening path precedes the
new downloader task, and it passed in both complete Nextest runs, including the
final run after the pagination correction. No change to that test was made.

## Post-merge review corrections

Review of merged PR #246 found three reproducible gaps. Pending attachments
survived clear/delete-chat, missing-target shared edits and revokes created
unread placeholders, and aggregated reaction keys still described the sharing
account's perspective. Synthetic regressions reproduce each on merged base
`5ee0050c279987abf16c6418d9ff9c1251ce975a`.

Clear/delete now retires pending attachment envelopes covered by the action's
existing timestamp/key range in the same writer transaction. Retired keys remain
for deduplication. A downloaded body held by an older task cannot consume that
retired envelope after restart, and re-delivery cannot queue it again. Other
chats and envelopes outside the action's range remain untouched. This is local
store deletion behavior, not a claim about server-side history retention.

Shared edit/revoke insertions retain the read-backfill marker and contribute no
unread delta. Existing live targets keep their own unread eligibility. Aggregated
reaction keys are translated before ordinary history import, allowing the
sharing account's later live replacement/removal to address the same reaction.

The same verified Web snapshot supports the reaction interpretation.
`WAWebGroupHistorySerializationUtils.serializeSingleMessage` serializes reaction
keys from the original message keys and sets their `unread` field false. Its
identical factory appears in bundle `414c0febcdba83636838a75d6f3f50ef4ad2cb39dbfaa2a16e3a6ece37fc86cb`,
bytes 436848..441175, and bundle `9b681e073f14f9d50e3dc12b456e4f980b554eff4d58827bc91c90a697e9ca38`,
bytes 1266844..1271171. `WAWebAddonProcessMsgsUtils.parseHistorySyncMsg` forwards
aggregates to add-on conversion in bundle `430cb8979511aada95178605faa35a8483446f677ce6d1fe0fd66333c7879eee`,
bytes 343106..347691. The `WAWebE2EProtoUtils` key-perspective conversion is
recorded above. No authenticated reproduction or new server contract is claimed.

The final follow-up fixture (`tests/shared_history.rs`, SHA-256
`c8acac7866197ec32a669292e6ef07c06f3ddde57b6452bbf4b6f025c2971aa6`)
was run unchanged on the merged base: six `review_245` checks failed and the
existing-live-target eligibility guard passed. All seven pass with the patch.
Validation also passed formatting, chat-store Clippy with all targets/features
and warnings denied, and the complete chat-store Nextest suite (397 passed,
one skipped). Chat-store doctests completed with three ignored examples.
