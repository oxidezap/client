# Message display coverage

Business messages are projected once from decoded protobuf in `chat-store::business`.
Live reception and durable hydration share that projection. `core::BusinessContent`
is serialized with the GUI's daemon events; CLI DTOs receive the same card as a
readable summary. Existing rows with missing template text are rendered from their
stored proto without a migration. Their old FTS entries are not rebuilt.

| Content | Display |
| --- | --- |
| Hydrated template, direct field or format oneof | Header, formatted body/footer, button labels; image/video/document attachment and location label |
| Interactive template, standalone interactive | Header/subtitle, formatted body/footer, bounded native-flow button labels; header attachments and inline thumbnail |
| Legacy buttons, lists | Header/body/footer, choices and row descriptions |
| Template/button/list/interactive response | Display text, never response IDs or native-flow JSON |
| HSM with hydrated template | Same hydrated card projection |
| Untranslated template | Explanation that translation requires the phone |
| Carousel | Bounded readable card text; attachment/action limitation |

Business button actions are display-only. Each choice has a visible explanation;
URL buttons, call buttons, replies and native flows are not executed. Ordinary
links in displayed text retain the client's explicit link controls. Rendering
never opens a URL, starts a call, submits a response or executes flow JSON.
Business attachments are lazy and use the existing media cache/download guards,
including cached-only access while offline. Inline thumbnails need no fetch.

Projection limits are 32 wrapper layers, 16 KiB per text field, 20 choices,
512 bytes per choice label, 16 KiB per button JSON and 10 carousel cards. Text is
truncated at UTF-8 boundaries. Carousel text shares one 16 KiB aggregate budget.
No flow identifiers or unrecognized parameter objects become fallback text.

Existing dedicated display includes text, photos, video/video notes, audio/voice
notes, stickers, documents, contacts, supported poll creations and system notices.
Location messages outside business headers, products/catalogs, events, group invites,
albums and future message kinds still rely on available textual projections or
kind placeholders. Poll v5/v6 voting, untranslated template localization, carousel
media, shop/collection interactions, Bloks widgets and native-flow execution remain
unsupported. This inventory does not claim support for every protocol type.

## Protocol evidence

Checked 2026-10-26 against whatspec
[`c581c20`](https://github.com/oxidezap/whatspec/tree/c581c20a9d0605df49abf297f2e7cfc04ccf1ff7),
schema `4.4.0`, WhatsApp Web `2.3000.1047483476`. All 579 preserved bundles matched
the lock's sizes and SHA-256 values. Bundle-set hash
`05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b`;
[release asset](https://github.com/oxidezap/whatspec/releases/download/bundle-store/bundles-2.3000.1047483476-05609307e68b0f6ccfd2a121a573049999e38b752cfe7155ebbe5c661805751b.tar.xz).

AST-extracted modules in bundle
`09d91fdc4089c50ab1d35dd6c796ee82ef4c3296710316d20383c85738b871b8`:

| Module | Byte range | Observed behavior |
| --- | --- | --- |
| `WAWebParseTemplateMessageProto` | 4631735..4635211 | Interactive-template dispatch first, otherwise hydratedTemplate then hydratedFourRowTemplate; content/footer/title and header media |
| `WAWebParseInteractiveMessageProto` | 4475063..4482388 | Body/footer/header fields; media guarded by hasMediaAttachment; type/version/native-flow validation and phone-only fallback |
| `WAWebParseTemplateButtonReplyMessageProto` | 4535672..4536276 | Display text is separate from selected ID/index |

The official Web parser applies relay/business verification, feature gates and
native-flow validation before actions. This client only reads display fields and
never implements those actions from this projection. These are static observations
of one preserved Web version, not authenticated execution or a server contract.
Committed fixtures are synthetic.
