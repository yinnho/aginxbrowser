#!/usr/bin/env python3
"""Regenerate the deterministic font fixtures for diting_layout batch 3a.

Source: Noto Sans SC (OFL) variable font. We instance it at wght=400 and
wght=700 and subset to the character set below, producing two small static
TTFs committed under src/diting_layout/fixtures/. Tests on BOTH sides of the
blitz cross-check load these exact bytes (our side via swash, the blitz side
via DocumentConfig.font_ctx with system_fonts disabled), so text-derived
rects are a function of the fixture glyphs alone — no system fonts, no
network, no @font-face plumbing.

A test using a CJK char outside this set gets .notdef (zero-ish advance) and
the cross-check fails loudly; add the char here and rerun:

    python3 scripts/make_font_fixture.py /tmp/NotoSansSC-var.ttf

Requires fonttools (pip install fonttools). License: OFL-1.1 — see
fixtures/OFL.txt, must travel with the TTFs.
"""

import sys
from pathlib import Path

import fontTools.subset as subset

# Every CJK codepoint our fixtures may print. Keep deliberately small: subset
# size scales with glyph count (~1-2KB/glyph after subsetting).
CJK = (
    # in use by existing fixtures (harvested from the test sources)
    "。一七三与世中乙九二于五优你光八六内化十四好字容引形搜擎文断栅标段测用甲界第索行证试题验，"
    # headroom for batch-3 tests: layout/text vocabulary
    "上下左右大小高矮宽窄内外前后个只条张片亿万千百数字号码号名称本末首尾行列表排列版面页张段落"
    "真假实虚确定形符图样貌英文中文混搭合体独立单独整体部分全部各类别种样点线框边"
    "伸缩缩放旋转倾斜平移叠层阴影圆角矩形边框填充背景前景颜色深浅明暗红绿蓝白黑灰黄紫橙粉"
    "甲乙丙丁子丑寅卯天地人日月星山水火风雨雪云电气声音光明黑暗影儿无有是否在了不也"
    "这那哪里怎么为何因为所以但是而且或者如果虽然于是然后接着最终开始结束中间旁边附近"
    "天地玄黄宇宙洪荒盈昃辰宿寒暑闰余成岁律吕调阳云腾致雨露结为霜金生丽水玉出昆冈"
)

FIXTURES = Path(__file__).resolve().parent.parent / "src" / "diting_layout" / "fixtures"


def build(src: Path, wght: int, out: Path) -> None:
    # pyftsubset has no instancer option: instance the variable font first
    # (in-process), spill to a temp file, then subset that.
    from fontTools import ttLib
    from fontTools.varLib import instancer

    font = ttLib.TTFont(str(src))
    instancer.instantiateVariableFont(font, {"wght": wght}, inplace=True)
    tmp = out.with_suffix(".instanced.ttf")
    font.save(str(tmp))

    args = [
        str(tmp),
        f"--text={''.join(chr(c) for c in range(0x20, 0x7F))}{CJK}",
        f"--output-file={out}",
        "--no-hinting",
        # keep kern/GPOS and friends for the subset charset (default is '*')
        "--layout-features=*",
        # drop web-only and doc tables
        "--drop-tables+=FFTM,DSIG,hdmx,META",
        "--no-recalc-bounds",
    ]
    subset.main(args)
    tmp.unlink()
    print(f"{out}  {out.stat().st_size} bytes  wght={wght}")


def main() -> None:
    src = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/NotoSansSC-var.ttf")
    if not src.exists():
        sys.exit(f"source font not found: {src} (download NotoSansSC[wght].ttf from google/fonts)")
    FIXTURES.mkdir(parents=True, exist_ok=True)
    build(src, 400, FIXTURES / "diting-fixture-regular.ttf")
    build(src, 700, FIXTURES / "diting-fixture-bold.ttf")


if __name__ == "__main__":
    main()
