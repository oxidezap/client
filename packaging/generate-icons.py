#!/usr/bin/env python3
"""Regenerate web icons from the organization PNG. Requires Pillow and oxipng."""

import argparse
import io
from pathlib import Path
import struct
import subprocess

from PIL import Image


def optimize(pixels):
    encoded = io.BytesIO()
    pixels.save(encoded, format="PNG", optimize=True, compress_level=9)
    result = subprocess.run(
        ["oxipng", "-o", "6", "-Z", "--strip", "safe", "--stdout", "-"],
        input=encoded.getvalue(), capture_output=True, check=True,
    ).stdout
    decoded = Image.open(io.BytesIO(result)).convert("RGBA")
    if decoded.size != pixels.size or decoded.tobytes() != pixels.convert("RGBA").tobytes():
        raise ValueError("PNG optimization changed pixels")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", nargs="?", type=Path, help="replacement organization PNG")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    master = root / "packaging/linux/org.oxidezap.client.local.png"
    original = Image.open(args.source or master).convert("RGBA")
    if original.width != original.height:
        raise ValueError("organization icon must have a square canvas")

    bounds = original.getchannel("A").getbbox()
    if bounds is None:
        raise ValueError("organization icon is empty")
    # Keep every desktop pixel, including its original transparent padding.
    master.write_bytes(optimize(original))
    mark = original.crop(bounds)
    # Small favicons need less empty space. Center the intact mark with an
    # eight-percent inset on each side, without stretching its proportions.
    side = round(max(mark.size) / 0.84)
    canvas = Image.new("RGBA", (side, side))
    canvas.alpha_composite(mark, ((side - mark.width) // 2, (side - mark.height) // 2))
    icons = root / "web/icons"
    icons.mkdir(parents=True, exist_ok=True)
    sizes = (16, 32, 48)
    frames = [optimize(canvas.resize((size, size), Image.Resampling.LANCZOS)) for size in sizes]
    offset = 6 + 16 * len(sizes)
    directory = bytearray(struct.pack("<HHH", 0, 1, len(sizes)))
    for size, frame in zip(sizes, frames):
        directory.extend(struct.pack("<BBBBHHII", size, size, 0, 0, 1, 32, len(frame), offset))
        offset += len(frame)
    (icons / "favicon.ico").write_bytes(directory + b"".join(frames))
    (icons / "favicon-32.png").write_bytes(frames[1])
    # Apple home-screen icons need an opaque background before the OS masks them.
    apple = Image.new("RGBA", (180, 180), "#1a1b26")
    apple.alpha_composite(canvas.resize((180, 180), Image.Resampling.LANCZOS))
    (icons / "apple-touch-icon.png").write_bytes(optimize(apple.convert("RGB")))


if __name__ == "__main__":
    main()
