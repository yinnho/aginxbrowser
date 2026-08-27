#!/usr/bin/env python3
"""Summarize a bench/run.py TSV into the results table.

Reads the newest (or given) run-*.tsv and prints per-scenario aggregates:
wall-time p50/p95, mean output chars, estimated tokens (chars/4 — the usual
English approximation, labeled as an estimate everywhere it's used), tier
hit-rate for `auto`, and Chrome's per-page peak RSS.

Usage: python3 bench/summarize.py [path/to/run.tsv]
"""

import sys
from pathlib import Path

RESULTS = Path(__file__).resolve().parent / "results"


def pct(sorted_vals, p):
    if not sorted_vals:
        return 0
    idx = min(len(sorted_vals) - 1, round(p / 100 * (len(sorted_vals) - 1)))
    return sorted_vals[idx]


def main():
    if len(sys.argv) > 1:
        tsv = Path(sys.argv[1])
    else:
        runs = sorted(RESULTS.glob("run-*.tsv"))
        if not runs:
            sys.exit("no run-*.tsv in bench/results/")
        tsv = runs[-1]

    by_scen = {}
    server_peak = None
    for line in tsv.read_text().splitlines():
        if line.startswith("# server_peak_rss_kb"):
            server_peak = int(line.split("\t")[1])
            continue
        if line.startswith("#") or not line.strip():
            continue
        scen, url, rnd, wall_ms, out_chars, tier, rss_kb = line.split("\t")
        by_scen.setdefault(scen, []).append(
            (url, int(wall_ms), int(out_chars), tier, int(rss_kb) if rss_kb else None)
        )

    print(f"source: {tsv.name}")
    print()
    print(f"{'scenario':8s} {'p50 ms':>8s} {'p95 ms':>8s} {'mean chars':>11s} {'≈tokens':>8s}  notes")
    for scen in ("tier1", "tier2", "auto", "chrome"):
        rows = by_scen.get(scen)
        if not rows:
            continue
        walls = sorted(r[1] for r in rows)
        # Wall time includes failures (a page you never got is infinitely
        # late); char/token means cover only runs that produced output, with
        # the failure count reported — Chrome hard-fails outright on some
        # CN-routed sites and letting zero-byte runs drag the mean down
        # would flatter everyone else.
        ok_rows = [r for r in rows if r[2] > 0]
        chars = sorted(r[2] for r in ok_rows)
        mean_chars = sum(chars) // max(1, len(chars))
        notes = ""
        if scen == "auto":
            hits = sum(1 for r in rows if r[3] == "http")
            notes = f"tier1 hit {hits}/{len(rows)} ({100 * hits // max(1, len(rows))}%)"
        if scen == "chrome":
            fails = len(rows) - len(ok_rows)
            if fails:
                notes += f"{fails}/{len(rows)} loads produced no DOM; "
            peaks = [r[4] for r in rows if r[4]]
            if peaks:
                notes += f"peak RSS/page p50 {pct(sorted(peaks), 50) // 1024} MB"
        print(
            f"{scen:8s} {pct(walls, 50):>8d} {pct(walls, 95):>8d} "
            f"{mean_chars:>11d} {mean_chars // 4:>8d}  {notes}"
        )
    if server_peak:
        print(f"\naginxbrowser server peak RSS (whole run, all scenarios): {server_peak // 1024} MB")


if __name__ == "__main__":
    main()
