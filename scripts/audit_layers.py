#!/usr/bin/env python3
"""Layering audit — ARCHITECTURE.md §2 rules R1–R3, grep level, zero deps.

Runs in CI (ci.yml `layers` job) and locally (`python3 scripts/audit_layers.py`).
Reads source as text — it is a tripwire against accidental boundary crossings,
not a proof of layering; the compile-time guarantee is the workspace dep graph.

Checks:
  R1a  downward only  — engine code never names a product module (`crate::server`,
                        `crate::session`, ...) at any depth.
  R1b  nothing imports faces — outside the face modules themselves (+ the bin
                        root), no product file references `crate::{routers,cdp,
                        mcp,firecrawl_compat,doctor_cli}`.
  R2a  engine purity  — crates/diting/Cargo.toml never grows a faces/product
                        dependency (axum, rmcp, schemars, ...).
  R2b  engine purity  — engine source never references blitz crates or the
                        product cross-check pipeline (comment-stripped, so
                        doc mentions of upstream bugs are fine).
  R3   faces are thin — no `unsafe` anywhere in a face (comment-stripped).
  G    god-file ratchet — non-test .rs files stay <= GOD_FILE_CAP lines;
                        files over the cap at ratchet time are grandfathered at
                        their exact line count and may not grow. Splitting a
                        grandfathered file shrinks or retires its entry.
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
FACE_MODULES = ("routers", "cdp", "mcp", "firecrawl_compat", "doctor_cli")
FACE_PATHS = (
    PRODUCT / "routers",
    PRODUCT / "cdp",
    PRODUCT / "mcp.rs",
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
    "rmcp",
    "schemars",
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
# Repo-relative path -> line count at ratchet time (2026-09-19, post P2 batch 3).
# These are the acknowledged god files the split program is chewing through;
# growth past the recorded count fails CI. Removing a file without removing its
# entry also fails (stale ratchet).
GOD_FILE_GRANDFATHER = {
    "crates/diting/src/diting_layout/mod.rs": 9642,
    "crates/diting/src/diting_css/mod.rs": 8511,
    "crates/diting/src/diting_js/ops/mod.rs": 6645,
    "src/cdp/dispatch.rs": 5408,
    "crates/diting/src/diting_layout/paint.rs": 4030,
    "src/bridge_cross_check.rs": 3766,
    "crates/diting/src/diting_js/runtime.rs": 2075,
    "crates/diting/src/diting_layout/fork_deltas.rs": 2445,
    "crates/diting/src/diting_net/client/mod.rs": 1394,
    "src/server/mod.rs": 2128,
    "crates/diting/src/diting_dom/tree.rs": 1664,
    "crates/diting/src/diting_layout/text.rs": 1826,
    "crates/diting/src/diting_layout/svg.rs": 1736,
    "src/store.rs": 1734,
    "src/video.rs": 1672,
    "src/docgen/workflow.rs": 1615,
    "src/docgen/graph.rs": 1584,
    "src/cdp/domains/page.rs": 1527,
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


def check_god_files() -> list[str]:
    """Ratchet: cap for new files, no growth for grandfathered ones."""
    out = []
    seen = set()
    for base in (PRODUCT, ENGINE):
        for f in rs_files(base):
            if f.name == "tests.rs":  # colocated contract suites are a feature, not debt
                continue
            lines = len(f.read_text().splitlines())
            key = rel(f)
            if key in GOD_FILE_GRANDFATHER:
                seen.add(key)
                cap = GOD_FILE_GRANDFATHER[key]
                if lines > cap:
                    out.append(
                        f"{key}: grew {cap} -> {lines} lines — grandfathered god "
                        "files only shrink; split the file (ARCHITECTURE.md §6 P2)"
                    )
            elif lines > GOD_FILE_CAP:
                out.append(
                    f"{key}: {lines} lines exceeds the {GOD_FILE_CAP}-line god-file "
                    "cap — split it, then grandfather entries shrink (ARCHITECTURE.md §6 P2)"
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


if __name__ == "__main__":
    sys.exit(main())
