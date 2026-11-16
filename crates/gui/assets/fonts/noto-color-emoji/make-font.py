"""Package Noto's PNG artwork as sbix for Swash, CoreText and DirectWrite.

Usage: python3 make-font.py NotoColorEmoji.ttf OxideZapEmoji.ttf
Requires fonttools==4.46.0, only when regenerating this checked-in asset.
"""

import hashlib
import sys

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont, newTable
from fontTools.ttLib.tables.sbixGlyph import Glyph
from fontTools.ttLib.tables.sbixStrike import Strike


def convert(source, destination):
    with open(source, "rb") as stream:
        assert hashlib.sha256(stream.read()).hexdigest() == (
            "72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b"
        ), "expected the unmodified Noto Emoji v2.051 font"
    font = TTFont(source, recalcTimestamp=False)
    sbix = newTable("sbix")
    for bitmap, images in zip(font["CBLC"].strikes, font["CBDT"].strikeData):
        strike = Strike(ppem=bitmap.bitmapSizeTable.ppemY, resolution=72)
        for name, image in images.items():
            metrics = image.metrics
            strike.glyphs[name] = Glyph(
                glyphName=name,
                originOffsetX=metrics.BearingX,
                originOffsetY=metrics.BearingY - metrics.height,
                graphicType="png ",
                imageData=image.imageData,
            )
        sbix.strikes[strike.ppem] = strike
    font["sbix"] = sbix
    del font["CBDT"], font["CBLC"]
    # GPUI requires a primary face to supply 'm' for its width measurement.
    # This empty metric glyph is never used to display ordinary message text.
    metric = "oxidezapMetricM"
    font.setGlyphOrder(font.getGlyphOrder() + [metric])
    font["hmtx"].metrics[metric] = (font["head"].unitsPerEm // 2, 0)
    if "vmtx" in font:
        font["vmtx"].metrics[metric] = (font["head"].unitsPerEm, 0)
    for table in font["cmap"].tables:
        if table.isUnicode() and table.format != 14:
            table.cmap[ord("m")] = metric
    # CoreText expects TrueType outline tables even for bitmap-only sbix.
    builder = FontBuilder(font=font)
    builder.isTTF = True
    builder.setupGlyf({name: TTGlyphPen(None).glyph() for name in font.getGlyphOrder()})
    builder.setupMaxp()
    for record in font["name"].names:
        if record.nameID in (1, 4, 16):
            record.string = "OxideZap Emoji".encode(record.getEncoding())
    # gpui-fast 0.1.13 selects Swash's RGBA rasterization by this PostScript
    # identifier. Keep it while giving the modified font its own family name.
    font.save(destination)


if __name__ == "__main__":
    convert(*sys.argv[1:])
