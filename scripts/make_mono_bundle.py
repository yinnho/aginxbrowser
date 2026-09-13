#!/usr/bin/env python3
"""Regenerate the bundled monospace face (diting-mono-regular.ttf).

Companion of make_font_bundle.py: the diting stack renders every
`font-family: monospace` run (UA: code/kbd/samp/tt/pre) with a bundled
Noto Sans Mono face instead of the proportional CJK pair — the
monospace-advance gap (0.5em vs Chrome's 0.6em Courier) measured in the
vertical-align batch. Noto Sans Mono is a fixed 600/1000 advance at any
weight, so ASCII width matches Chrome's default monospace exactly.

Source: the Noto Sans Mono variable font (OFL), instanced at wght=400
wdth=100, subset to ASCII + Latin-1 + general punctuation + arrows +
box drawing (~200 glyphs). CJK in a mono run falls through to the CJK
pair per-character (browser posture: code blocks with Chinese comments
render CJK in the CJK face).

    python3 scripts/make_mono_bundle.py /tmp/NotoSansMono-var.ttf

Requires fonttools. License: OFL-1.1 — see diting_fonts/OFL.txt.
"""

import sys
from pathlib import Path

from fontTools import subset
from fontTools.varLib.instancer import instantiateVariableFont

OUT = Path(__file__).resolve().parent.parent / "src" / "diting_fonts"

# ASCII is the point (code); the rest covers what real <pre>/<code> bodies
# actually mix in — Latin-1 punctuation, typographic dashes/quotes, arrows
# and box drawing (terminal dumps / ASCII art).
CODEPOINTS = (
    list(range(0x20, 0x7F))          # printable ASCII
    + list(range(0xA0, 0x100))       # Latin-1 supplement
    + list(range(0x2010, 0x2028))    # hyphens, dashes, quotes, ellipsis
    + list(range(0x2190, 0x2194))    # ← ↑ → ↓
    + list(range(0x2500, 0x2580))    # box drawing
)


def main(src: str) -> None:
    from fontTools.ttLib import TTFont

    font = TTFont(src)
    if "fvar" in font:
        instantiateVariableFont(font, {"wght": 400, "wdth": 100}, inplace=True)
    opts = subset.Options()
    opts.name_IDs = ["*"]
    opts.notdef_outline = True
    ss = subset.Subsetter(options=opts)
    ss.populate(unicodes=CODEPOINTS)
    ss.subset(font)
    font.save(OUT / "diting-mono-regular.ttf")

    check = TTFont(OUT / "diting-mono-regular.ttf")
    upm = check["head"].unitsPerEm
    cmap = check.getBestCmap()
    hmtx = check["hmtx"]
    for ch in " 0ixMW~":
        g = cmap[ord(ch)]
        adv, _ = hmtx[g]
        assert adv == 600, f"{ch!r}: advance {adv} != 600"
    assert upm == 1000, f"upm {upm} != 1000"
    size = (OUT / "diting-mono-regular.ttf").stat().st_size
    print(f"ok: {len(cmap)} glyphs, advance 600/{upm}, {size} bytes")


if __name__ == "__main__":
    main(sys.argv[1])
