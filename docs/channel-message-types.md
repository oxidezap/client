# Channel message display

Individual stickers use the existing static/animated sticker renderer. Their
plaintext media descriptors and cache identity are fixed separately in PR #267.
This change does not copy its downloader, preview scheduler or image-quality work.
Without that dependency, a channel sticker without inline thumbnail bytes can
still be unavailable. Animated WebP decoding is tracked separately in PR #263.

Sticker packs are archive messages, not individual WebP stickers. They now have
the durable `sticker_pack` kind and a bounded text projection of name, publisher,
description and caption. Opening/adding packs and pack thumbnails remain unsupported.
New reception, stored text, old rows with a retained proto and quoted previews use
the same projection. Pack IDs, archive filenames and transport paths are not shown.
Revocation removes the projection and quote. Existing unknown rows need no migration.

Unsupported content remains stored with its protobuf. Live and hydrated fallbacks
explain the limitation rather than showing internal `[unknown]`/`[Media]` labels.
This is not a claim that an arbitrary unknown payload is a sticker pack.

## Evidence

Static analysis on 2026-10-29 of whatspec commit
[`eb16f595d455d8b0b3f7966a59444f4f9894c127`](https://github.com/oxidezap/whatspec/tree/eb16f595d455d8b0b3f7966a59444f4f9894c127),
schema 4.4.0, WhatsApp Web 2.3000.1047483476. All 579 bundles were checked
against the lock's hash/size multiset and set hash
`05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`.
The exact [release asset](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz)
has SHA-256 `22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`.
Acorn parsed the complete relevant bundles without recovery, then extracted whole
`__d` definitions. Offsets below are bytes in bundle
`09d91fdc4089c50ab1d35dd6c796ee82ef4c3296710316d20383c85738b871b8`.

| Module | Offsets | Observed behavior |
| --- | --- | --- |
| WAWebParseStickerPackMessageProto | 4533877..4535670 | Separate sticker-pack type; name, publisher, description, caption and archive metadata |
| WAWebStickersParseStickerMessageProto | 4564180..4566538 | Individual sticker/lottie envelope, dimensions, animation flag, PNG thumbnail and media properties |
| WAWebMediaMessageGetValidatedProperties | 337063..340702 | Newsletter branch does not require E2EE keys; plaintext SHA-256 validation remains |

The two supplied screenshot references resolved in Library, but its transfer
helper returned `download failed` for both. Their pixels were not inspected, so
no screenshot parity or identification of their unknown payload is claimed.
Tests use synthetic protobufs; no authenticated WhatsApp session was accessed.
