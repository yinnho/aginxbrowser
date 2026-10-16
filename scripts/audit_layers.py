#!/usr/bin/env python3
"""Layering audit — ARCHITECTURE.md §2 rules R1–R3, grep level, zero deps.

Runs in CI (ci.yml `layers` job) and locally (`python3 scripts/audit_layers.py`).
Reads source as text — it is a tripwire against accidental boundary crossings,
not a proof of layering; the compile-time guarantee is the workspace dep graph.

Checks:
  R1a  downward only  — engine code never names a product module (`crate::server`,
                        `crate::session`, ...) at any depth.
  R1b  nothing imports faces — outside the face modules themselves (+ the bin
                        root), no product file references `crate::{routers,
                        firecrawl_compat,doctor_cli}`.
  R2a  engine purity  — crates/diting/Cargo.toml never grows a faces/product
                        dependency (axum, tower-http, ...).
  R2b  engine purity  — engine source never references blitz crates or the
                        product cross-check pipeline (comment-stripped, so
                        doc mentions of upstream bugs are fine).
  R3   faces are thin — no `unsafe` anywhere in a face (comment-stripped).
  G    god-file ratchet — production line count only. For .rs files, lines inside
                        #[cfg(test)]-gated items (and whole files named tests.rs)
                        do not count; .js sources count every line. Cap is
                        GOD_FILE_CAP; files over the cap at ratchet time are
                        grandfathered at their production count and may not grow.
                        Splitting or shedding tests that drop a file under the cap
                        retires its grandfather entry. Intent: catch real prod
                        bloat, not incentivize split-for-cap of colocated tests.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ENGINE = ROOT / "crates" / "diting"
PRODUCT = ROOT / "src"

# The faces: doors hold routing/parameter structs only (R3), and nothing may
# import them (R1b) — main.rs is the bin root doing assembly, so it may.
FACE_MODULES = ("routers", "firecrawl_compat", "doctor_cli")
FACE_PATHS = (
    PRODUCT / "routers",
    PRODUCT / "firecrawl_compat.rs",
    PRODUCT / "doctor_cli.rs",
    PRODUCT / "main.rs",
)

# R1b documented exceptions: (repo-relative file, imported face) -> reason.
# Every entry must carry an open issue; the fix batch deletes the entry.
# (The table's first and so far only entry — session/interact reaching up
# into the CDP Input face, issue #47 — was deleted when the mouse-event
# builders moved down into core.)
R1B_EXCEPTIONS: dict[tuple[str, str], str] = {}

# Faces/product crates the engine must never depend on (R2a). Anything the
# engine legitimately needs (wreq, tokio, html5ever, ...) is absent here.
ENGINE_DENY_DEPS = (
    "axum",
    "tower",
    "tower-http",
    "http-body-util",
    "pulldown-cmark",
    "scraper",
    "html2md",
    "rusqlite",
    "ureq",
    "tracing-subscriber",
    "anyhow",
    "stylo",
)

GOD_FILE_CAP = 1500
# Repo-relative path -> production line count at last rebase (2026-10-13).
# Counts exclude #[cfg(test)]-gated items (see count_production_lines). These are
# the acknowledged god files the split program is chewing through; growth past
# the recorded production count fails CI. A file that falls to <= GOD_FILE_CAP
# production lines must leave this table (retire). Removing a file without
# removing its entry also fails (stale ratchet).
GOD_FILE_GRANDFATHER = {
    "crates/diting/js/bootstrap.js": 17551,  # #236: XMLHttpRequest split to js/xhr.js; EventTarget base stays
    "crates/diting/src/diting_layout/mod.rs": 9253,
    "crates/diting/src/diting_css/mod.rs": 7241,
    "crates/diting/src/diting_js/ops/mod.rs": 5761,
    "crates/diting/src/diting_layout/paint.rs": 2720,
    "src/bridge_cross_check.rs": 3766,
    "crates/diting/src/diting_layout/fork_deltas.rs": 2257,
    "crates/diting/src/diting_js/runtime.rs": 1743,
    "src/docgen/workflow.rs": 1592,
}

_BLOCK_COMMENT = re.compile(r"/\*.*?\*/", re.S)


def strip_comments(text: str) -> str:
    """Drop /* */ blocks and // line tails so rule matches only live code."""
    text = _BLOCK_COMMENT.sub("", text)
    return "\n".join(re.sub(r"//.*$", "", line) for line in text.splitlines())


def rs_files(base: Path) -> list[Path]:
    return sorted(p for p in base.rglob("*.rs") if p.is_file())


def top_modules(base: Path) -> set[str]:
    """Top-level module names of a crate: src/*.rs stems + src/ subdirs."""
    names = {p.stem for p in base.glob("*.rs") if p.is_file()}
    names |= {p.name for p in base.iterdir() if p.is_dir()}
    return names


def in_face(path: Path) -> bool:
    return any(path == f or f in path.parents for f in FACE_PATHS if f.is_file() or f.is_dir())


def rel(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def check_r1a() -> list[str]:
    """Engine never names a product module."""
    product_only = top_modules(PRODUCT) - top_modules(ENGINE) - {"main"}
    pattern = re.compile(rf"\bcrate::({'|'.join(sorted(product_only))})\b")
    out = []
    for f in rs_files(ENGINE):
        for i, line in enumerate(strip_comments(f.read_text()).splitlines(), 1):
            m = pattern.search(line)
            if m:
                out.append(
                    f"{rel(f)}:{i}: engine names product module `{m.group(1)}` — "
                    "R1 downward-only (ARCHITECTURE.md §2)"
                )
    return out


def check_r1b() -> list[str]:
    """Nothing outside the faces imports a face."""
    pattern = re.compile(rf"\bcrate::({'|'.join(FACE_MODULES)})\b")
    out = []
    for f in rs_files(PRODUCT):
        if in_face(f):
            continue
        for i, line in enumerate(strip_comments(f.read_text()).splitlines(), 1):
            m = pattern.search(line)
            if m:
                key = (rel(f), m.group(1))
                if key in R1B_EXCEPTIONS:
                    continue
                out.append(
                    f"{rel(f)}:{i}: core imports face `{m.group(1)}` — "
                    "nothing imports faces (ARCHITECTURE.md §2 R1)"
                )
    return out


def check_r2a() -> list[str]:
    """Engine Cargo.toml carries no faces/product dependency."""
    pattern = re.compile(rf"^\s*({'|'.join(ENGINE_DENY_DEPS)})\w*\s*=", re.M)
    out = []
    manifest = ENGINE / "Cargo.toml"
    for i, line in enumerate(manifest.read_text().splitlines(), 1):
        if pattern.search(line):
            out.append(
                f"{rel(manifest)}:{i}: engine depends on `{line.split('=')[0].strip()}` — "
                "R2 engine purity: faces/product crates belong in the product Cargo.toml"
            )
    return out


def check_r2b() -> list[str]:
    """Engine source never references blitz crates / the product cross-check."""
    pattern = re.compile(r"use\s+blitz|blitz\w*::|screenshot_reference")
    out = []
    for f in rs_files(ENGINE):
        for i, line in enumerate(strip_comments(f.read_text()).splitlines(), 1):
            if pattern.search(line):
                out.append(
                    f"{rel(f)}:{i}: engine references the blitz reference pipeline — "
                    "R2 engine purity (cross-checks live in the product crate)"
                )
    return out


def check_r3() -> list[str]:
    """Faces carry no unsafe (and stay thin enough to review by eye)."""
    out = []
    for f in rs_files(PRODUCT):
        if not in_face(f):
            continue
        for i, line in enumerate(strip_comments(f.read_text()).splitlines(), 1):
            if re.search(r"\bunsafe\b", line):
                out.append(f"{rel(f)}:{i}: `unsafe` in a face — R3 faces are thin")
    return out


def is_attr_line(line: str) -> bool:
    return bool(re.match(r"^\s*#\[", line))


def is_test_only_cfg_attr(line: str) -> bool:
    """True when this attribute gates the following item to test builds only.

    Matches `#[cfg(test)]` and `#[cfg(all(..., test, ...))]`.
    Does not match `#[cfg(not(test))]`, bare features, or `any(test, ...)`
    (the latter can still compile into non-test binaries).
    Multi-line cfg attributes are left alone (conservative: still counted).
    """
    s = line.strip()
    m = re.match(r"#\[cfg\s*\((.*)\)\s*\]\s*$", s)
    if not m:
        return False
    inner = m.group(1).strip()
    if inner == "test":
        return True
    if re.search(r"\bnot\s*\(\s*test\s*\)", inner):
        return False
    if inner.startswith("all(") and re.search(r"(^|[\s,(])test([\s,)]|$)", inner):
        return True
    return False


def _skip_rust_item(lines: list[str], i: int) -> int:
    """Advance past an attribute stack (if any) and the following Rust item."""
    n = len(lines)
    while i < n and is_attr_line(lines[i]):
        i += 1
    while i < n and (not lines[i].strip() or lines[i].lstrip().startswith("//")):
        i += 1
    if i >= n:
        return i
    depth = 0
    seen_brace = False
    while i < n:
        in_str = False
        esc = False
        for ch in lines[i]:
            if in_str:
                if esc:
                    esc = False
                elif ch == "\\":
                    esc = True
                elif ch == '"':
                    in_str = False
                continue
            if ch == '"':
                in_str = True
                continue
            if ch == "{":
                depth += 1
                seen_brace = True
            elif ch == "}":
                depth = max(0, depth - 1)
            elif ch == ";" and not seen_brace and depth == 0:
                return i + 1
        if seen_brace and depth == 0:
            return i + 1
        i += 1
    return i


def count_production_lines(path: Path) -> int:
    """Line count that feeds the god-file ratchet.

    .rs: drop #[cfg(test)]-gated items (attribute stack + item body). Colocated
    `tests.rs` files are skipped entirely by check_god_files — same exemption
    spirit as inline cfg(test) modules.
    Non-.rs (engine js/): every line counts.
    """
    text = path.read_text()
    if path.suffix != ".rs":
        return len(text.splitlines())
    lines = text.splitlines()
    i = 0
    n = 0
    while i < len(lines):
        if is_attr_line(lines[i]):
            j = i
            test_gated = False
            while j < len(lines) and is_attr_line(lines[j]):
                if is_test_only_cfg_attr(lines[j]):
                    test_gated = True
                j += 1
            if test_gated:
                i = _skip_rust_item(lines, i)
                continue
        n += 1
        i += 1
    return n


def check_god_files() -> list[str]:
    """Ratchet: production-line cap for new files, no growth for grandfathered ones."""
    out = []
    seen = set()
    # The engine's baked-in realm JS (js/, executed into the V8 snapshot by
    # build.rs) is engine source that happens to be JS — same ratchet.
    files = [f for base in (PRODUCT, ENGINE) for f in rs_files(base)]
    files += sorted((ENGINE / "js").glob("*.js"))
    for f in files:
        if f.name == "tests.rs":  # colocated contract suites are a feature, not debt
            continue
        lines = count_production_lines(f)
        key = rel(f)
        if key in GOD_FILE_GRANDFATHER:
            seen.add(key)
            cap = GOD_FILE_GRANDFATHER[key]
            if lines > cap:
                out.append(
                    f"{key}: grew {cap} -> {lines} production lines — grandfathered god "
                    "files only shrink; split production code (ARCHITECTURE.md §6 P2). "
                    "Inline #[cfg(test)] does not count toward this total."
                )
            elif lines <= GOD_FILE_CAP:
                out.append(
                    f"{key}: {lines} production lines is under the {GOD_FILE_CAP}-line "
                    "cap — retire this grandfather entry"
                )
        elif lines > GOD_FILE_CAP:
            out.append(
                f"{key}: {lines} production lines exceeds the {GOD_FILE_CAP}-line god-file "
                "cap — split production code (ARCHITECTURE.md §6 P2). "
                "Inline #[cfg(test)] / tests.rs do not count; do not split for the cap alone."
            )
    for key in sorted(set(GOD_FILE_GRANDFATHER) - seen):
        out.append(f"{key}: grandfather entry is stale (file gone) — remove it from the ratchet")
    return out



def main() -> int:
    checks = [
        ("R1a engine never names product", check_r1a),
        ("R1b nothing imports faces", check_r1b),
        ("R2a engine deps stay engine-only", check_r2a),
        ("R2b engine carries no blitz refs", check_r2b),
        ("R3 faces carry no unsafe", check_r3),
        ("G god-file ratchet", check_god_files),
    ]
    failed = False
    for name, fn in checks:
        violations = fn()
        if violations:
            failed = True
            print(f"FAIL {name}")
            for v in violations:
                print(f"     {v}")
        else:
            print(f"ok   {name}")
    if R1B_EXCEPTIONS:
        print(f"note R1b carries {len(R1B_EXCEPTIONS)} documented exception(s):")
        for (f, face), why in R1B_EXCEPTIONS.items():
            print(f"     {f} -> crate::{face}: {why}")
    print("layering audit: " + ("FAILED" if failed else "clean"))
    return 1 if failed else 0


def _self_test() -> None:
    """Fixtures for count_production_lines; `python3 scripts/audit_layers.py --self-test`."""
    import tempfile

    sample = """fn prod() {}

#[cfg(test)]
mod tests {
    fn t() {}
}

#[cfg(not(test))]
fn only_prod() {}

#[cfg(all(test, feature = "x"))]
fn test_only() {}
"""
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "x.rs"
        p.write_text(sample)
        got = count_production_lines(p)
        # Keep: fn prod, blank, blank-after-skip, not(test) attr, only_prod, blank.
        # Drop: cfg(test) mod block and all(test) fn.
        assert got == 6, f"expected 6 production lines, got {got}"

    shot = ROOT / "src" / "screenshot.rs"
    if shot.is_file():
        total = len(shot.read_text().splitlines())
        prod = count_production_lines(shot)
        assert prod < GOD_FILE_CAP, (prod, GOD_FILE_CAP)
        assert prod < total, (prod, total)
        assert total - prod > 500, "screenshot.rs should still carry a large inline test suite"


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        _self_test()
        print("audit_layers self-test: ok")
        sys.exit(0)
    sys.exit(main())
