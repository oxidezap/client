Noto Sans Arabic Regular, distributed under the SIL Open Font License in NotoSansArabic-OFL.txt.

Source: https://github.com/notofonts/noto-fonts/blob/ffebf8c1ee449e544955a7e813c54f9b73848eac/hinted/ttf/NotoSansArabic/NotoSansArabic-Regular.ttf

SHA-256: ceea25b464a656dc3b26849bab9356740401af62aedf1bfa8b7f0d9b75925b1b

Served filename: `NotoSansArabic-Regular.ceea25b464a656dc.ttf`.

The web frontend requests this same-origin asset only after a missing
Arabic-script grapheme, verifies its exact size and SHA-256, then adds it to
cosmic-text and refreshes visible text. It is not included in production WASM.
The face is 240,456 bytes uncompressed. Desktop builds use system fonts.

Cache Storage is best effort. Once stored, the asset can load offline in the
same origin/profile. First use offline, cache eviction or denied storage can
leave missing glyphs. An online event permits one retry after failure, and a
reload resets the per-page limit. The URL identifies only this public font,
never message text; the host can still observe which font was requested.
