# Bundled color emoji

Artwork: Google Noto Emoji v2.051, Unicode 17.0, under the accompanying
[SIL Open Font License](NotoColorEmoji-OFL.txt). Copyright and license metadata
from the source font remain embedded in the derived font.

Source: <https://github.com/googlefonts/noto-emoji/releases/tag/v2.051>

The unmodified font is downloaded from
<https://raw.githubusercontent.com/googlefonts/noto-emoji/v2.051/fonts/NotoColorEmoji.ttf>.
Its SHA-256 is
`72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b`.

The checked-in `OxideZapEmoji.ttf` stores the original PNG artwork in sbix
instead of CBDT/CBLC. CoreText does not support the source bitmap format;
Swash and DirectWrite also support sbix. Empty TrueType outline tables allow
CoreText to load the bitmap face. The custom family name `OxideZap Emoji`
avoids collisions with installed Noto versions and names the modified font.
An empty Latin `m` glyph supplies the width metric GPUI requires when resolving
a primary font. Ordinary message text continues to use the UI family.
Picker and composer controls extract the original PNGs from the same sbix data.
The internal PostScript identifier is preserved because gpui-fast 0.1.13
recognizes that identifier when choosing RGBA glyph rasterization.

To regenerate, install `fonttools==4.46.0` in a temporary Python environment,
download the source above, and run:

```sh
python3 make-font.py /path/to/NotoColorEmoji.ttf OxideZapEmoji.ttf
```

Output SHA-256:
`c396be69c558ea5de27671f808fdbfbcdd0ab20c21368a7cc1ef80cb52f2bd63`.

The production web build copies this file alongside WASM and its license.
It is never embedded in WASM. The desktop binary embeds the same bytes. Native release archives and the web
bundle also carry the accompanying copyright and OFL notice as a readable file.
The GUI font tests reproduce a fontless environment, check multi-codepoint
sequences, and verify that rasterization produces colored pixels. Optional
`OXIDEZAP_EMOJI_PREVIEW=/tmp/emoji.png` exports their sample strip.
