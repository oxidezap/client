# Organization icon

The temporary client identity is the [OxideZap organization avatar](https://github.com/oxidezap.png).
The maintainer supplied the source PNG in `icon.zip` for issue #214. Its original
SHA-256 is `b9b24312829978aa7922e3d2ecf7228f5e96beab863595bf191ff0be9e319699`.

`linux/org.oxidezap.client.local.png` is the canonical image. Lossless compression
preserves all RGBA pixels and its 460 × 460 canvas. The Linux window embeds it,
and the desktop installer copies it into the user's icon search directory.

The web uses a transparent ICO with separate 16, 32 and 48 pixel frames, a
32 pixel PNG, and an opaque 180 pixel Apple touch icon. These are generated from
the same mark, with its empty margins reduced for legibility. The Apple icon
uses the page's initial background color. There is no traced or recolored logo.

To regenerate, install Python 3 with Pillow and put `oxipng` on `PATH`, then run:

```sh
python3 packaging/generate-icons.py
# Replace the source when the organization chooses a new identity:
python3 packaging/generate-icons.py path/to/organization.png
```

The checked-in assets were generated with Pillow 12.3.0 and oxipng 9.1.5.
The generator checks decoded pixel equality after every PNG optimization.
Trunk hashes the favicon ICO and copies the PNGs beside `index.html`; their
relative links also work in Pages previews and relocatable web archives.
