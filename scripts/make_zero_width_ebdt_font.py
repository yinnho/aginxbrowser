#!/usr/bin/env python3
"""Generate the zero-width EBDT strike fixture for diting_layout.

Font bytes are OURS (built here, deterministic, no third-party glyph data),
so no license file travels with it — unlike the Noto-derived fixtures next
door (see make_font_fixture.py / OFL.txt).

Shape: a minimal 2-glyph TTF (".notdef" + "a", cmap U+0061 and U+E000 both
-> gid 1) plus hand-rolled EBLC/EBDT tables declaring one horizontal strike
at ppem 12, bit depth 1, whose single glyph carries SMALL metrics with
**width = 0** (height 1). U+E000 rides the cmap because the diting bundle
doesn't cover private use (the test-fallback-face precedent), so the char
routes to a fallback segment — the only segment whose swash source list
visits Source::Bitmap. That is exactly the SimSun blank-glyph shape from
dfrg/swash#139: swash 0.2.10's `Bitmap::decode` Alpha(1/2/4) branches call
`src.chunks(((w * bits) + 7) / 8)` with w == 0, and `slice::chunks(0)`
panics — in release builds too (not an overflow-checks panic). The
`target.len() < decoded_size()` guard can't catch it because
`decoded_size()` for a 0x1 bitmap is 0.

The EBDT layout below is written to match how swash 0.2.10 actually reads
the tables (strike.rs `get_location`/`get_data`), byte for byte:

  EBLC header (8B): version 2.0, numSizes = 1
  bitmapSize[0] (48B): array@56, 1 subtable, glyph range [1,1],
                       ppem 12/12, bitDepth 1, flags 1 (horizontal)
  indexSubTableArray[0] (8B) @56: first=1 last=1, subtable at +8 (=64)
  indexSubTable format 1 @64: imageFormat 1 (byte-aligned + small metrics),
                       imageDataOffset 8 (from EBDT start), offsets [0, 5]
  EBDT header (4B): version 2.0, then 4B slack so glyph data sits at +8
  glyph data @8 (5B small metrics): height=1 width=0 bx=0 by=0 adv=1,
                       zero image bytes (stride is 0 when width is 0)

Run:

    python3 scripts/make_zero_width_ebdt_font.py

Writes src/diting_layout/fixtures/zero-width-ebdt.ttf. Requires fonttools
(for the base TTF only — the EBLC/EBDT bytes are emitted by this script).
"""

import struct
from pathlib import Path

from fontTools.fontBuilder import FontBuilder
from fontTools.ttLib import newTable
from fontTools.ttLib.tables.DefaultTable import DefaultTable

OUT = Path(__file__).parent.parent / "src/diting_layout/fixtures/zero-width-ebdt.ttf"


def build_base_ttf() -> FontBuilder:
    fb = FontBuilder(1000, isTTF=True)
    fb.setupGlyphOrder([".notdef", "a"])
    fb.setupCharacterMap({0x61: "a", 0xE000: "a"})
    from fontTools.ttLib.tables._g_l_y_f import Glyph

    def empty_glyph() -> Glyph:
        g = Glyph()
        g.numberOfContours = 0  # empty outline — the strike is the raster source
        return g

    fb.setupGlyf({".notdef": empty_glyph(), "a": empty_glyph()})
    fb.setupHorizontalMetrics({".notdef": (600, 0), "a": (600, 0)})
    fb.setupHorizontalHeader(ascent=800, descent=-200)
    fb.setupNameTable({"familyName": "ZW EBDT", "styleName": "Regular"})
    fb.setupOS2()
    fb.setupPost()
    return fb


def build_eblc() -> bytes:
    # header: majorVersion=2, minorVersion=0, numSizes=1 (8 bytes)
    eblc = struct.pack(">HHI", 2, 0, 1)
    # bitmapSize[0]: 48 bytes. indexSubTableArrayOffset@0=56, size@4,
    # count@8=1, colorRef@12=0, hori metrics@16 (12B), vert metrics@28
    # (12B), startGlyph@40=1, endGlyph@42=1, ppemX@44=12, ppemY@45=12,
    # bitDepth@46=1, flags@47=1.
    eblc += struct.pack(">III", 56, 24, 1)  # array@56, sizes 8+8+8, 1 subtable
    eblc += struct.pack(">I", 0)  # colorRef
    eblc += struct.pack(
        ">bbbbbbbbbbbb", 12, -3, 8, 1, 0, 0, 0, 0, 0, 0, 0, 0
    )  # hori sbitLineMetrics
    eblc += struct.pack(">bbbbbbbbbbbb", 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0)  # vert
    eblc += struct.pack(">HH", 1, 1)  # startGlyph, endGlyph
    eblc += struct.pack(">BBBb", 12, 12, 1, 1)  # ppemX, ppemY, bitDepth, flags
    assert len(eblc) == 56
    # indexSubTableArray[0] @56: first=1, last=1, additionalOffset=8 -> @64
    eblc += struct.pack(">HHI", 1, 1, 8)
    # indexSubTable format 1 @64: indexFormat=1, imageFormat=1,
    # imageDataOffset=8 (from EBDT start), offsets=[0, 5]
    eblc += struct.pack(">HHIII", 1, 1, 8, 0, 5)
    return eblc


def build_ebdt() -> bytes:
    ebdt = struct.pack(">HH", 2, 0)  # version 2.0
    ebdt += b"\0" * 4  # slack: glyph data starts at +8 (imageDataOffset)
    # smallGlyphMetrics @8: height=1, width=0, bearingX=0, bearingY=0,
    # advance=1. Zero image bytes follow — the (width*depth+7)/8 stride is 0.
    ebdt += struct.pack(">BBbbB", 1, 0, 0, 0, 1)
    return ebdt


def main() -> None:
    fb = build_base_ttf()
    eblc = DefaultTable("EBLC")
    eblc.data = build_eblc()
    ebdt = DefaultTable("EBDT")
    ebdt.data = build_ebdt()
    fb.font["EBLC"] = eblc
    fb.font["EBDT"] = ebdt
    fb.save(str(OUT))
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
