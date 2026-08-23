#!/usr/bin/env python3
"""Regenerate the bundled CJK font supply for the screenshot build.

Distinct from make_font_fixture.py (deterministic TEST fixtures): this is the
PRODUCT bundle that makes /screenshot render CJK without any system fonts
installed (render claim batch 3c) — the determinism gap upstream obscura-render
leaves by design (zero CJK).

Source: Noto Sans SC (OFL) variable font, instanced at wght=400/700, subset to
full GB2312 (6763 chars — effectively all modern simplified Chinese) + ASCII.
~2.4MB per weight; rare chars outside GB2312 fall through to system fonts
(the injected fontique collection keeps system_fonts on as a tail fallback).

    python3 scripts/make_font_bundle.py /tmp/NotoSansSC-var.ttf

Requires fonttools. License: OFL-1.1 — see diting_fonts/OFL.txt, must travel
with the TTFs.
"""

import sys
from pathlib import Path

import fontTools.subset as subset

OUT = Path(__file__).resolve().parent.parent / "src" / "diting_fonts"


def gb2312_charset() -> str:
    chars = set(chr(c) for c in range(0x20, 0x7F))  # printable ASCII
    for hi in range(0xA1, 0xF8):  # GB2312 plane: rows A1-F7, cols A1-FE
        for lo in range(0xA1, 0xFF):
            try:
                chars.add(bytes([hi, lo]).decode("gb2312"))
            except UnicodeDecodeError:
                pass
    return "".join(sorted(chars))


def build(src: Path, wght: int, charset: str, out: Path) -> None:
    from fontTools import ttLib
    from fontTools.varLib import instancer

    font = ttLib.TTFont(str(src))
    instancer.instantiateVariableFont(font, {"wght": wght}, inplace=True)
    tmp = out.with_suffix(".instanced.ttf")
    font.save(str(tmp))

    subset.main(
        [
            str(tmp),
            f"--text={charset}",
            f"--output-file={out}",
            "--no-hinting",
            "--layout-features=*",
            "--drop-tables+=FFTM,DSIG,hdmx,META",
            "--no-recalc-bounds",
        ]
    )
    tmp.unlink()
    print(f"{out}  {out.stat().st_size} bytes  wght={wght}")


def main() -> None:
    src = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/NotoSansSC-var.ttf")
    if not src.exists():
        sys.exit(
            f"source font not found: {src} "
            "(download NotoSansSC[wght].ttf from google/fonts)"
        )
    OUT.mkdir(parents=True, exist_ok=True)
    charset = gb2312_charset()
    print(f"charset: {len(charset)} chars (GB2312 + ASCII)")
    build(src, 400, charset, OUT / "diting-cjk-regular.ttf")
    build(src, 700, charset, OUT / "diting-cjk-bold.ttf")


if __name__ == "__main__":
    main()
