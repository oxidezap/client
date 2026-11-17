#!/usr/bin/env bash
# Regenerate the Linux hicolor theme icons in packaging/linux/hicolor from the organization
# icon (packaging/linux/org.oxidezap.client.local.png, see packaging/ICONS.md), the one the
# window embeds and install-desktop.sh installs.
#
# Needs ImageMagick (magick or convert). The outputs are committed, so packaging never
# needs it.
#
#   packaging/linux/icons.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NAME=org.oxidezap.client.local
SRC="$HERE/$NAME.png"
OUT="$HERE/hicolor"

if command -v magick >/dev/null; then IM=(magick); elif command -v convert >/dev/null; then IM=(convert); else
  echo "error: ImageMagick not found (apt install imagemagick)" >&2
  exit 1
fi

rm -rf "$OUT"
# The source is 460 x 460, so 256 is the largest size that is not upscaled.
for s in 16 24 32 48 64 128 256; do
  mkdir -p "$OUT/${s}x${s}/apps"
  # -strip drops timestamps and profiles, so a rerun on the same source is byte-identical.
  "${IM[@]}" "$SRC" -filter Lanczos -resize "${s}x${s}" -strip "PNG32:$OUT/${s}x${s}/apps/$NAME.png"
done
echo "icons written to $OUT"
