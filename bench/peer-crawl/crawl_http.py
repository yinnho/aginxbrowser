#!/usr/bin/env python3
"""Amiibo crawl against one AginxBrowser process.

Same rules as lightpanda-io/demo chromedp/crawler: start at the seed, wait
until the page has at least one <a href>, keep same-host links, dedupe by
absolute URL. The official client speaks CDP. This one speaks the HTTP
session API (navigate + eval), because this binary has no CDP listener.
Page work matches; the wire to the browser does not.

One process, N sessions. That is the Obscura/Chrome shape. Lightpanda's
published multi-tab run is N processes (--fork), because that engine has
no tabs.
"""

from __future__ import annotations

import argparse
import json
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import deque


READY, QUEUED, DONE = 0, 1, 2


def request(method: str, url: str, body: dict | None = None, timeout: float = 60) -> dict:
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"content-type": "application/json"} if data else {},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read()
    if not raw:
        return {}
    return json.loads(raw)


def links_of(base: str, hrefs: list[str]) -> list[str]:
    out = []
    for href in hrefs:
        if not href:
            continue
        abs_url = urllib.parse.urljoin(base, href)
        parsed = urllib.parse.urlsplit(abs_url)
        if parsed.scheme not in ("http", "https"):
            continue
        # Drop fragments. The Go crawler keeps them if present; these pages
        # have none. Query and path stay so two files stay two URLs.
        cleaned = urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, parsed.path, parsed.query, ""))
        out.append(cleaned)
    return out


class Crawl:
    def __init__(self, seed: str, limit: int):
        self.seed = urllib.parse.urlsplit(seed)
        self.limit = limit
        self.known: dict[str, int] = {}
        self.queue: deque[str] = deque()
        self.lock = threading.Lock()
        self.cv = threading.Condition(self.lock)
        self.error: str | None = None
        self.pages = 0
        self._add(seed, READY)

    def _add(self, url: str, state: int) -> None:
        self.known[url] = state
        if state == READY:
            self.queue.append(url)

    def host_ok(self, url: str) -> bool:
        return urllib.parse.urlsplit(url).netloc == self.seed.netloc

    def take(self) -> str | None:
        with self.cv:
            while not self.queue and self.error is None and not self._done():
                self.cv.wait(timeout=0.2)
            if self.error is not None or not self.queue:
                return None
            url = self.queue.popleft()
            self.known[url] = QUEUED
            return url

    def _done(self) -> bool:
        return bool(self.known) and all(s == DONE for s in self.known.values())

    def finish(self, url: str, found: list[str]) -> None:
        with self.cv:
            self.known[url] = DONE
            self.pages += 1
            for link in found:
                if self.limit and len(self.known) >= self.limit:
                    break
                if not self.host_ok(link) or link in self.known:
                    continue
                self._add(link, READY)
            self.cv.notify_all()

    def fail(self, message: str) -> None:
        with self.cv:
            if self.error is None:
                self.error = message
            self.cv.notify_all()


LINK_JS = (
    "JSON.stringify(Array.from(document.querySelectorAll('a[href]'))"
    ".map(function(a){return a.getAttribute('href')||'';}))"
)


def fetch_page(base: str, session: str, url: str, page_timeout: float) -> list[str]:
    try:
        request(
            "POST",
            f"{base}/session/{session}/navigate",
            {"url": url},
            timeout=page_timeout + 5,
        )
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")[:400]
        raise RuntimeError(f"navigate HTTP {exc.code}: {detail}") from exc
    deadline = time.monotonic() + page_timeout
    last = "no anchors"
    while time.monotonic() < deadline:
        try:
            payload = request(
                "POST",
                f"{base}/session/{session}/eval",
                {"script": LINK_JS, "timeout_ms": 2000},
                timeout=8,
            )
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", "replace")[:400]
            # A 5xx is the server failing the command, not a page that is
            # still booting. Surface it immediately.
            if exc.code >= 500:
                raise RuntimeError(f"HTTP {exc.code}: {detail}") from exc
            last = f"HTTP {exc.code}: {detail}"
            time.sleep(0.1)
            continue
        raw = payload.get("result", payload.get("value"))
        if isinstance(raw, str):
            try:
                hrefs = json.loads(raw)
            except json.JSONDecodeError:
                hrefs = []
        elif isinstance(raw, list):
            hrefs = raw
        else:
            hrefs = []
        found = links_of(url, hrefs if isinstance(hrefs, list) else [])
        if found:
            return found
        last = "zero anchors"
        time.sleep(0.1)
    raise TimeoutError(f"{url}: {last} within {page_timeout:.0f}s")


def worker(base: str, session: str, crawl: Crawl, page_timeout: float) -> None:
    while True:
        url = crawl.take()
        if url is None:
            return
        try:
            found = fetch_page(base, session, url, page_timeout)
        except Exception as exc:  # noqa: BLE001 — one page failure fails the run, matching chromedp
            crawl.fail(f"{url}: {exc}")
            return
        crawl.finish(url, found)


def open_session(base: str) -> str:
    created = request(
        "POST",
        f"{base}/session/create",
        {
            "url": "about:blank",
            "ttl_secs": 3600,
            "keepalive": True,
            "width": 1280,
            "height": 720,
        },
        timeout=30,
    )
    sid = created.get("session_id") or created.get("id")
    if not sid:
        raise RuntimeError(f"session create returned {created}")
    return sid


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://127.0.0.1:18092")
    parser.add_argument("--seed", default="https://demo-browser.lightpanda.io/amiibo/")
    parser.add_argument("--pool", type=int, default=1)
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--page-timeout", type=float, default=30)
    parser.add_argument("--out", default="")
    args = parser.parse_args()

    health = request("GET", f"{args.base}/health", timeout=5)
    sessions = [open_session(args.base) for _ in range(args.pool)]
    crawl = Crawl(args.seed, args.limit)
    started = time.monotonic()
    threads = [
        threading.Thread(target=worker, args=(args.base, sid, crawl, args.page_timeout), daemon=True)
        for sid in sessions
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    elapsed = time.monotonic() - started

    summary = {
        "engine": "aginxbrowser",
        "client": "http-session",
        "base": args.base,
        "seed": args.seed,
        "pool": args.pool,
        "limit": args.limit,
        "pages": crawl.pages,
        "known": len(crawl.known),
        "elapsed_s": round(elapsed, 3),
        "error": crawl.error,
        "health": {
            "version": health.get("version"),
            "commit": health.get("commit"),
            "tls": health.get("tls"),
        },
    }
    text = json.dumps(summary, ensure_ascii=False, indent=2)
    print(text)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(text + "\n")
    return 0 if crawl.error is None else 1


if __name__ == "__main__":
    sys.exit(main())
