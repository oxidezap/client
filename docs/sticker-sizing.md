# Sticker sizing

Chat stickers use one square canvas for previews, cached files, static and
animated WebP, and download placeholders. The canvas follows the UI rem and
shrinks when the conversation cannot fit it beside its actions and gutters.
`Metrics::sticker_size` retains the client's existing 300px nominal media size
at the reference rem. It is a client UI choice, not a verified WhatsApp CSS
measurement. Photos and videos keep their existing dimension-based sizing.

Images use `ObjectFit::Contain`: portrait and landscape canvases keep their
aspect ratio, alpha and complete content. Transparent padding belongs to the
file and is retained. Small intrinsic images can be enlarged into the canvas;
the policy does not equalize the bounds of the opaque drawing.

## Evidence

Static analysis on 2026-10-29 used WhatSpec commit
[`eb16f595d455d8b0b3f7966a59444f4f9894c127`](https://github.com/oxidezap/whatspec/tree/eb16f595d455d8b0b3f7966a59444f4f9894c127),
schema 4.4.0, WhatsApp Web snapshot `2.3000.1047483476`, 579 bundles,
set hash `05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`.
The [locked release archive](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz)
has SHA-256 `22243b9c2ff18a66cea8fd66551821fe0f9f09786d107e2be8284637db86c349`.
The archive digest, exact bundle hash/size multiset, count and set hash were
verified against the committed lock. Modules were extracted with an AST parser;
the spans below are byte offsets in the original bundles, with exclusive ends.

| Module | Bundle SHA-256 | Byte span | Observed layout behavior |
| --- | --- | --- | --- |
| `WAWebMessageStickerBubble.react` | `012a3b924a76e250362922ed59882da7bfe95b399d33d3da1426e90323d10e5a` | 247061..253140 | Passes the `conversation` theme to the sticker component. |
| `WAWebSticker.react` | `65b89bd62b933ca76911900c83c40c2fad84263a99d2b99eced9578c1e4bdf55` | 576173..584581 | Uses fixed width class `xjzf9lu` and height class `x8sy99` for this theme. The same classes reach previews, native WebP images and animated/canvas paths. Declared media dimensions do not select the conversation box. |
| `WAWebControlledAnimatedSticker.react` | `65b89bd62b933ca76911900c83c40c2fad84263a99d2b99eced9578c1e4bdf55` | 346392..348454 | Applies the received class to the image and paused canvas. Canvas bitmap resolution is separate from its CSS size. |
| `WAWebStickerLikeMsgPlaceholder.react` | `65b89bd62b933ca76911900c83c40c2fad84263a99d2b99eced9578c1e4bdf55` | 348455..349209 | Receives the same theme class for an unavailable preview. |

The snapshot establishes the shared-box policy. Numeric CSS declarations and
the official object-fit rule were unavailable in this environment. No live
authenticated WhatsApp session or user screenshot was visually verified.
This does not establish behavior of other WhatsApp clients or later snapshots.
