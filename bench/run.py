#!/usr/bin/env python3
"""Benchmark harness: AginxBrowser tiers vs headless Chrome.

Measures, per page of the fixed set in pages.txt:
  - wall time to agent-usable text
  - output size in characters (tokens estimated at chars/4 in the summary)
  - peak RSS (our single server process vs Chrome's process tree)

Scenarios:
  tier1   POST /fetch {"render_tier": "http"}     — plain HTTP + convert
  tier2   POST /fetch {"render_tier": "obscura"}  — full V8 browser render
  auto    POST /fetch (default)                   — records tier hit-rate
  chrome  headless Chrome --dump-dom + tag-strip  — the heavyweight baseline

Chrome's output is tag-stripped from the rendered DOM with the stdlib
(html.parser) — the minimal real-world conversion step; ours is built-in
markdown. Chrome cold-starts a fresh profile per page (what a
launch-per-fetch Puppeteer pipeline pays).

Usage: python3 bench/run.py [--rounds N] [--scenarios tier1,tier2,auto,chrome]
                            [--port 18099]
Requires a built binary (target/release/aginxbrowser). Writes
bench/results/run-<host>-<date>.tsv.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import urllib.request
from datetime import date
from html.parser import HTMLParser
from pathlib import Path

BENCH = Path(__file__).resolve().parent
REPO = BENCH.parent
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"


def pages():
    out = []
    for line in (BENCH / "pages.txt").read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            out.append(line)
    return out


class TextExtract(HTMLParser):
    """Minimal tag-strip: visible text chars, script/style dropped."""
    SKIP = {"script", "style", "noscript", "template"}

    def __init__(self):
        super().__init__()
        self.parts = []
        self._skip = 0

    def handle_starttag(self, tag, _):
        if tag in self.SKIP:
            self._skip += 1

    def handle_endtag(self, tag):
        if tag in self.SKIP and self._skip:
            self._skip -= 1

    def handle_data(self, data):
        if not self._skip:
            self.parts.append(data)


def text_chars_from_dom(dom: str) -> int:
    p = TextExtract()
    p.feed(dom)
    return sum(len(s.strip()) for s in p.parts)


class RssSampler(threading.Thread):
    """Samples summed RSS every 0.2 s for processes whose comm matches any
    pattern. Peak wins."""

    def __init__(self, patterns):
        super().__init__(daemon=True)
        self.patterns = patterns
        self.peak_kb = 0
        self._stop = threading.Event()

    def run(self):
        cmd = ["ps", "-Ao", "rss=,comm="]
        while not self._stop.is_set():
            try:
                out = subprocess.run(cmd, capture_output=True, text=True).stdout
            except OSError:
                return
            total = 0
            for line in out.splitlines():
                m = re.match(r"\s*(\d+)\s+(.+)", line)
                if m and any(p in m.group(2) for p in self.patterns):
                    total += int(m.group(1))
            self.peak_kb = max(self.peak_kb, total)
            self._stop.wait(0.2)

    def stop(self):
        self._stop.set()
        self.join(timeout=2)


def fetch(base, url, tier=None, max_chars=200_000):
    body = {"url": url, "max_chars": max_chars}
    if tier:
        body["render_tier"] = tier
    req = urllib.request.Request(
        base + "/fetch",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    started = time.monotonic()
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.loads(resp.read())
    return time.monotonic() - started, data


def chrome_dump(url, profile_dir, hard_cap=90):
    """One cold-started headless Chrome page load; returns (wall secs, DOM).

    Chrome 15x on macOS prints the serialized DOM but then lingers (the
    GoogleUpdater subprocess keeps the tree alive), so we stream stdout and
    count the wall clock at DOM completion, then kill. If `</html>` never
    arrives within the cap, whatever partial output exists is used — an
    honest slow/failed load.
    """
    started = time.monotonic()
    proc = subprocess.Popen(
        [
            CHROME,
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--virtual-time-budget=8000",
            f"--user-data-dir={profile_dir}",
            "--window-size=1280,800",
            "--dump-dom",
            url,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    buf = []
    try:
        import select

        fd = proc.stdout.fileno()
        while True:
            ready, _, _ = select.select([fd], [], [], max(0.0, hard_cap - (time.monotonic() - started)))
            if not ready:
                break
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            buf.append(chunk)
            if b"</html>" in chunk or b"</body>" in chunk:
                # DOM is serialized in one shot at the end; one full chunk
                # past the close tag means we have it all.
                tail = os.read(fd, 65536) if b"</html>" not in chunk else b""
                if tail:
                    buf.append(tail)
                break
    finally:
        elapsed = time.monotonic() - started
        proc.kill()
        proc.wait()
    return elapsed, b"".join(buf).decode("utf-8", "replace")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=2)
    ap.add_argument("--scenarios", default="tier1,tier2,auto,chrome")
    ap.add_argument("--port", type=int, default=18099)
    ap.add_argument("--binary", default=str(REPO / "target/release/aginxbrowser"))
    args = ap.parse_args()
    scenarios = [s for s in args.scenarios.split(",") if s]
    urls = pages()
    if "chrome" in scenarios and not Path(CHROME).exists():
        print(f"chrome not found at {CHROME}, dropping chrome scenario", file=sys.stderr)
        scenarios = [s for s in scenarios if s != "chrome"]

    results_dir = BENCH / "results"
    results_dir.mkdir(exist_ok=True)
    tsv = results_dir / f"run-{os.uname().sysname.lower()}-{date.today().isoformat()}.tsv"

    # Fresh server: no fetch cache (poisons repeat timing), robots honored
    # (the page set is preflighted for it).
    env = dict(os.environ)
    env["AGINXBROWSER_BIND"] = f"127.0.0.1:{args.port}"
    env["AGINXBROWSER_CACHE_TTL_SECS"] = "0"
    server = subprocess.Popen(
        [args.binary], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    base = f"http://127.0.0.1:{args.port}"
    try:
        for _ in range(40):
            try:
                urllib.request.urlopen(base + "/health", timeout=2)
                break
            except OSError:
                time.sleep(0.25)
        else:
            sys.exit("server did not come up")

        # Warmup (V8 snapshot etc.) — excluded from results.
        fetch(base, "https://example.com", tier="obscura", max_chars=500)

        server_rss = RssSampler(["aginxbrowser"])
        server_rss.start()

        rows = []
        for rnd in range(1, args.rounds + 1):
            for url in urls:
                for scen in scenarios:
                    if scen in ("tier1", "tier2", "auto"):
                        tier = {"tier1": "http", "tier2": "obscura"}.get(scen)
                        elapsed, data = fetch(base, url, tier=tier)
                        wall_ms = int(elapsed * 1000)
                        out_chars = len(data.get("content", ""))
                        served = data.get("tier") or "?"
                        row = (scen, url, rnd, wall_ms, out_chars, served, "")
                    else:  # chrome
                        profile = f"/tmp/agx-bench-profile-{rnd}"
                        shutil.rmtree(profile, ignore_errors=True)
                        chrome_rss = RssSampler(["Google Chrome"])
                        chrome_rss.start()
                        elapsed, dom = chrome_dump(url, profile)
                        chrome_rss.stop()
                        wall_ms = int(elapsed * 1000)
                        row = (scen, url, rnd, wall_ms, text_chars_from_dom(dom), "-",
                               chrome_rss.peak_kb)
                    rows.append(row)
                    print(
                        f"[r{rnd}] {scen:6s} {row[3]:6d} ms  {row[4]:7d} ch  "
                        f"{row[5]}  {url}",
                        flush=True,
                    )
        server_rss.stop()
        with tsv.open("w") as f:
            f.write("# scenario\turl\tround\twall_ms\tout_chars\ttier\tpeak_rss_kb\n")
            for row in rows:
                f.write("\t".join(map(str, row)) + "\n")
            f.write(f"# server_peak_rss_kb\t{server_rss.peak_kb}\n")
        print(f"\nwrote {tsv}")
        print(f"server peak RSS: {server_rss.peak_kb / 1024:.0f} MB")
    finally:
        server.terminate()
        server.wait(timeout=10)
        subprocess.run(["pkill", "-f", "agx-bench-profile"], capture_output=True)


if __name__ == "__main__":
    main()
