# Channel image previews

Channel media can be plaintext. Hydration must preserve `fileSha256` and
`staticUrl` without requiring `mediaKey` or `fileEncSha256`. Returning an empty
key as `Some([])` incorrectly selects encrypted download in the library.

The virtual message list schedules a full-image fetch for channel rows near the
viewport when the declared image size is known and at most 4 MiB. Downloads use
the library's streaming byte budget (4 MiB total wire and plaintext, including
retries), with at most two image requests in flight. A failed automatic request
keeps the inline thumbnail, stays manually retryable and is attempted only once
in that account session. The attempt set stops growing at 256 entries. Unknown
or larger images remain manual downloads. This is application policy, not a
claim that the official client uses those limits.

Native and web preserve the same download metadata and cache identity. Automatic
fetch is currently native only: the browser HTTP adapter implements buffered
`fetch`, not the SDK's synchronous streaming reader. Bounded SDK downloads refuse
that adapter before HTTP, so browser manual downloads retain their existing
buffered path and automatic attempts are disabled there. A browser attached to a
native daemon can still manually fetch/cache the full channel image. Adding an
asynchronous bounded browser media API is a separate dependency-level change.
Full channel images use the account-scoped content cache. Old `f-<message>`
entries cannot be trusted: earlier clients treated a channel thumbnail with
discarded download metadata as a full image and could cache it there. No
resizing, recompression or upscaling is added. Session changes discard late
image completions; existing retained-media and decoded-image budgets still apply.

## Protocol evidence

Static analysis on 2026-10-29 of whatspec commit
`eb16f595d455d8b0b3f7966a59444f4f9894c127`, schema `4.4.0`, WhatsApp Web
`2.3000.1047483476`. The preserved snapshot contains 579 bundles, set hash
`05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`.
The [exact release asset](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz)
has SHA-256 `22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`.
Archive digest, complete bundle hash/size multiset and set fingerprint were
verified before AST extraction. Offsets below are bytes in original bundles.

- `WAWebParseImageMessageProto`, bundle `09d91f…b871b8`, 344242–347609:
  preserves inline JPEG, dimensions, original and encrypted hashes, direct path,
  allowlisted static URL and separate server-thumbnail metadata.
- `WAWebMediaMessageGetValidatedProperties`, same bundle, 337063–340702:
  key validation is conditional on the message not being a newsletter;
  plaintext SHA-256 remains validated.
- `WAWebMediaUrlAllowlist`, same bundle, 326506–327138: HTTPS and
  `whatsapp.net` or its subdomains. The client applies that boundary to retained
  static URLs.
- `WAWebMediaCryptoEligibilityUtils`, bundle `430cb8…9eee`, 979236–980120:
  media crypto is not expected for newsletter messages.
- `useWAWebMsgDownloadMedia`, bundle `36dc12…717`, 563446–565504:
  render triggers download subject to cached-file state, photo-download
  preference and chat suspension. The client uses the bounded policy above.
- `WAWebMediaDownloadMmsThumbnail`, bundle `f588ee…c4690`, 1902352–1908535:
  server thumbnails can replace the inline fallback separately from full media.
  This change fetches the original small channel image; separate HQ link-card
  thumbnails are not implemented here.

This is not an authenticated G1 replay or a visual parity result. Library
materialization of both supplied screenshots failed in the executor, and the
returned workspace paths were not readable there. Screenshot-specific pixel
quality and live native/browser rendering still need verification.
