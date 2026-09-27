#!/usr/bin/env python3
"""Make test-han-ja-face.ttf — the #139 Han-unification fixture.

Subsets the bundled regular face down to `A` + 直 (U+76F4) and renames the
name table to a JP-token family ("Diting Test JP"). The 直 outline is the
PRIMARY face's own glyph, so the FaceSel-level test assertions are honest:
same bytes for the glyph, a different face carrying it. What the routing
tests exercise is purely the slot decision — `face_han_slot` must classify
the name tokens as Japanese, and `segments` must route the ideograph to
Fallback(0) only under `han: Some(Japanese)`.

Fixture is OURS (subset of our bundled OFL Noto build + our own name
records) — no license tail. Regenerate:
    python3 scripts/make_han_ja_font.py
"""

from pathlib import Path

from fontTools.subset import Options, Subsetter
from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "crates/diting/src/diting_fonts/diting-cjk-regular.ttf"
OUT = ROOT / "crates/diting/src/diting_fonts/test-han-ja-face.ttf"

# The name records that make the face classify as the Japanese slot: the
# family and PostScript names carry the "JP" token `face_han_slot` scans
# (name beats GSUB script — the bundled pair has kana coverage and a 'kana'
# GSUB feature, so a name-less classifier would call it Japanese too).
NAMES = {1: "Diting Test JP", 2: "Regular", 4: "Diting Test JP", 6: "DitingTestJP-Regular"}


def main() -> None:
    font = TTFont(str(SRC))
    opts = Options()
    opts.name_IDs = ["*"]  # keep the name table; records are rewritten below
    opts.layout_features = ["*"]  # keep GSUB/GPOS so slot fallback sees structure
    sub = Subsetter(options=opts)
    sub.populate(text="A直")
    sub.subset(font)
    for rec in font["name"].names:
        if rec.nameID in NAMES:
            rec.string = NAMES[rec.nameID].encode(rec.getEncoding()) if rec.platformID == 1 else NAMES[rec.nameID]
    font.save(str(OUT))
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
