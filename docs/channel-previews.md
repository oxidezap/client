# Channel image previews

Channel media can be plaintext. Preserve `fileSha256` and `staticUrl` without
requiring E2EE keys. An absent key must remain absent in `Downloadable`;
`Some([])` incorrectly selects encrypted download.

Native virtual-list rows prefer the separately addressed server HQ JPEG when
`thumbnailDirectPath` and a 32-byte `thumbnailSha256` are present on a plaintext
image. Its actual wire/plaintext budget is 512 KiB across retries; no thumbnail
length is declared by the protobuf. Without that reference, known originals up
to 4 MiB remain the fallback. HQ bytes retain the preview flag: opening the photo
still downloads its original. Full and HQ files have separate content identities.

Two requests run at once, with one automatic attempt per image and 256 remembered
attempts per account session. Failure keeps the inline fallback and manual retry.
HQ previews share the existing retained-media budget; inline-only bytes remain
protected. The client has no photo auto-download preference or metered-network
policy; these limits do not emulate such controls.

Web preserves metadata and manual downloads. Automatic fetching is disabled:
its buffered HTTP adapter cannot enforce the SDK's synchronous streaming budget.
Inline bytes cross the wire under a separate preview cache reference; loading
them preserves their preview flag and MIME type. Full channel images use the
account-scoped content hash, bypassing old
`f-<message>` entries that may contain inline thumbnails. No recompression or
upscaling is added. Session/logout guards reject late completions.

## Preserved Web evidence

Whatspec commit `eb16f595d455d8b0b3f7966a59444f4f9894c127`, schema `4.4.0`,
WhatsApp Web `2.3000.1047483476`; analyzed 2026-10-29. The 579-bundle set hash is
`05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`.
The [release asset](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz)
has SHA-256 `22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`.
Archive digest, complete hash/size multiset and set fingerprint were verified
before AST extraction; duplicate module bodies were identical. Offsets are bytes.

| Module | Bundle / offsets | Observed behavior |
| --- | --- | --- |
| `WAWebParseImageMessageProto` | `09d91f…b871b8`, 344242–347609 | Preserves original media and separate server-thumbnail metadata. |
| `WAWebMediaMessageGetValidatedProperties` | Same, 337063–340702 | Newsletter media does not require E2EE keys; SHA-256 remains validated. |
| `WAWebMediaUrlAllowlist` | Same, 326506–327138 | HTTPS, `whatsapp.net` or its subdomains. |
| `useWAWebMsgDownloadMedia` | `36dc12…717`, 563446–565504 | Full-media fetch on render checks cache, photo preferences and suspension. |
| `WAWebMediaMmsV4Download` | `f588ee…c4690`, 1919619–1937869 | `downloadEvenIfExpensive` enables phone RMR when no download entry exists; it is not evidence of a metered-network check. |
| `WAWebGetThumbnailTypeForMediaImageMsg` | `f588ee…c4690`, 1890966–1891311 | Newsletter image HQ uses NEWSLETTER_IMAGE; encrypted image HQ uses THUMBNAIL_IMAGE. |
| `WAWebMediaDownloadMmsThumbnail` | `f588ee…c4690`, 1902352–1908535 | Separate server thumbnail can replace the inline fallback. |

The official image-thumbnail registry uses NEWSLETTER_IMAGE for newsletter HQ
JPEGs, which the current SDK can download as plaintext Image media. Encrypted
image thumbnails use THUMBNAIL_IMAGE, absent from this SDK, so this path is kept
to plaintext channels. Link-card HQ thumbnails remain outside this change.
Neither this static trace nor local tests prove authenticated visual parity.
