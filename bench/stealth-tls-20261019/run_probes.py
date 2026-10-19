#!/usr/bin/env python3
"""Stealth/TLS comparison probes — 2026-10-19 Asia/Shanghai."""
from __future__ import annotations

import json
import re
import subprocess
import time
import urllib.request
from pathlib import Path

OUT = Path(__file__).resolve().parent
BASE = "http://127.0.0.1:18089"
META = {
    "date_local": "2026-10-19 Asia/Shanghai",
    "aginx_health": None,
    "machine": "darwin-arm64",
    "notes": "Public fingerprint endpoints only; polite rate limits; no merchant APIs.",
}


def http_json(method: str, url: str, body: dict | None = None, timeout: int = 90) -> dict:
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers={"Content-Type": "application/json", "Accept": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8", "replace"))


def extract_fp(content: str) -> dict | None:
    if not content:
        return None
    # Prefer full JSON body
    content = content.strip()
    if content.startswith("{"):
        try:
            return json.loads(content)
        except json.JSONDecodeError:
            pass
    m = re.search(r"\{[\s\S]*\}", content)
    if not m:
        return None
    try:
        return json.loads(m.group(0))
    except json.JSONDecodeError:
        return None


def fp_summary(fp: dict | None) -> dict | None:
    if not fp:
        return None
    tls = fp.get("tls") or {}
    h2 = fp.get("http2") or {}
    return {
        "user_agent": fp.get("user_agent") or fp.get("ua"),
        "http_version": fp.get("http_version"),
        "ja3_hash": tls.get("ja3_hash"),
        "ja4": tls.get("ja4"),
        "peetprint_hash": tls.get("peetprint_hash"),
        "akamai_h2_hash": h2.get("akamai_fingerprint_hash"),
        "akamai_h2": h2.get("akamai_fingerprint"),
        "cipher_count": len(tls.get("ciphers") or []),
        "extension_count": len(tls.get("extensions") or []),
    }


def save(name: str, obj: dict) -> None:
    (OUT / name).write_text(json.dumps(obj, indent=2, ensure_ascii=False) + "\n")


def probe_aginx(name: str, url: str, extra: dict | None = None, render_tier: str = "http") -> dict:
    body = {
        "url": url,
        "format": "text",
        "max_chars": 0,
        "render_tier": render_tier,
        "auto_bypass_challenge": True,
    }
    if extra:
        body.update(extra)
    t0 = time.time()
    try:
        raw = http_json("POST", f"{BASE}/fetch", body, timeout=120)
        err = None
    except Exception as e:
        raw = {"error": str(e)}
        err = str(e)
    elapsed_ms = int((time.time() - t0) * 1000)
    content = (raw.get("content") or "") if isinstance(raw, dict) else ""
    fp = extract_fp(content)
    summary = {
        "engine": "aginxbrowser",
        "probe": name,
        "url": url,
        "elapsed_ms": elapsed_ms,
        "tier": raw.get("tier") if isinstance(raw, dict) else None,
        "title": raw.get("title") if isinstance(raw, dict) else None,
        "content_len": len(content),
        "content_head": content[:500],
        "fetch_error": err or (raw.get("error") if isinstance(raw, dict) else None),
        "tls_fingerprint_requested": (extra or {}).get("tls_fingerprint"),
        "fp": fp_summary(fp),
        "challenge_signals": {
            "just_a_moment": "just a moment" in content.lower(),
            "cf_ray" : "cf-ray" in content.lower() or "cf_ray" in content.lower(),
            "turnstile": "turnstile" in content.lower() or "cf-turnstile" in content.lower(),
            "captcha": "captcha" in content.lower(),
        },
    }
    save(f"aginx-{name}.raw.json", raw if isinstance(raw, dict) else {"raw": str(raw)})
    if fp:
        # Trim bulky cipher lists for committed artifact but keep hashes
        slim = {
            "user_agent": fp.get("user_agent") or fp.get("ua"),
            "http_version": fp.get("http_version"),
            "tls": {
                "ja3_hash": (fp.get("tls") or {}).get("ja3_hash"),
                "ja4": (fp.get("tls") or {}).get("ja4"),
                "peetprint_hash": (fp.get("tls") or {}).get("peetprint_hash"),
                "cipher_count": len((fp.get("tls") or {}).get("ciphers") or []),
                "extension_count": len((fp.get("tls") or {}).get("extensions") or []),
            },
            "http2": {
                "akamai_fingerprint": (fp.get("http2") or {}).get("akamai_fingerprint"),
                "akamai_fingerprint_hash": (fp.get("http2") or {}).get("akamai_fingerprint_hash"),
            },
        }
        save(f"aginx-{name}.fp.json", slim)
    save(f"aginx-{name}.summary.json", summary)
    print(json.dumps({"probe": name, "fp": summary["fp"], "err": summary["fetch_error"], "ms": elapsed_ms}, ensure_ascii=False))
    time.sleep(1.2)  # polite gap
    return summary


def probe_curl(name: str, url: str) -> dict:
    t0 = time.time()
    try:
        r = subprocess.run(
            ["curl", "-sS", "-m", "60", "-A", "curl/8.0-stealth-bench", "-L", url],
            capture_output=True,
            text=True,
            timeout=70,
        )
        content = r.stdout or ""
        err = None if r.returncode == 0 else (r.stderr or f"exit {r.returncode}")
    except Exception as e:
        content, err = "", str(e)
    elapsed_ms = int((time.time() - t0) * 1000)
    fp = extract_fp(content)
    summary = {
        "engine": "curl",
        "probe": name,
        "url": url,
        "elapsed_ms": elapsed_ms,
        "content_len": len(content),
        "content_head": content[:500],
        "fetch_error": err,
        "fp": fp_summary(fp),
        "challenge_signals": {
            "just_a_moment": "just a moment" in content.lower(),
            "turnstile": "turnstile" in content.lower(),
            "captcha": "captcha" in content.lower(),
        },
    }
    save(f"curl-{name}.summary.json", summary)
    if fp:
        save(
            f"curl-{name}.fp.json",
            {
                "user_agent": fp.get("user_agent") or fp.get("ua"),
                "http_version": fp.get("http_version"),
                "tls": {
                    "ja3_hash": (fp.get("tls") or {}).get("ja3_hash"),
                    "ja4": (fp.get("tls") or {}).get("ja4"),
                    "peetprint_hash": (fp.get("tls") or {}).get("peetprint_hash"),
                },
                "http2": {
                    "akamai_fingerprint_hash": (fp.get("http2") or {}).get("akamai_fingerprint_hash"),
                },
            },
        )
    print(json.dumps({"probe": f"curl-{name}", "fp": summary["fp"], "err": err, "ms": elapsed_ms}, ensure_ascii=False))
    time.sleep(1.0)
    return summary


def probe_chrome(name: str, url: str) -> dict:
    chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    profile = OUT / "_chrome_profile"
    profile.mkdir(exist_ok=True)
    t0 = time.time()
    try:
        r = subprocess.run(
            [
                chrome,
                "--headless=new",
                "--disable-gpu",
                "--virtual-time-budget=12000",
                f"--user-data-dir={profile}",
                "--dump-dom",
                url,
            ],
            capture_output=True,
            text=True,
            timeout=90,
        )
        content = r.stdout or ""
        err = None if r.returncode == 0 else (r.stderr[-500:] if r.stderr else f"exit {r.returncode}")
    except Exception as e:
        content, err = "", str(e)
    elapsed_ms = int((time.time() - t0) * 1000)
    # dump-dom wraps JSON in HTML sometimes — try extract
    fp = extract_fp(content)
    # Also try <pre> or body text
    if not fp:
        m = re.search(r"<pre[^>]*>([\s\S]*?)</pre>", content, re.I)
        if m:
            fp = extract_fp(m.group(1))
    summary = {
        "engine": "chrome-headless",
        "probe": name,
        "url": url,
        "elapsed_ms": elapsed_ms,
        "content_len": len(content),
        "content_head": content[:500],
        "fetch_error": err,
        "fp": fp_summary(fp),
        "challenge_signals": {
            "just_a_moment": "just a moment" in content.lower(),
            "turnstile": "turnstile" in content.lower(),
            "captcha": "captcha" in content.lower(),
        },
    }
    save(f"chrome-{name}.summary.json", summary)
    if fp:
        save(
            f"chrome-{name}.fp.json",
            {
                "user_agent": fp.get("user_agent") or fp.get("ua"),
                "http_version": fp.get("http_version"),
                "tls": {
                    "ja3_hash": (fp.get("tls") or {}).get("ja3_hash"),
                    "ja4": (fp.get("tls") or {}).get("ja4"),
                    "peetprint_hash": (fp.get("tls") or {}).get("peetprint_hash"),
                },
                "http2": {
                    "akamai_fingerprint_hash": (fp.get("http2") or {}).get("akamai_fingerprint_hash"),
                },
            },
        )
    print(json.dumps({"probe": f"chrome-{name}", "fp": summary["fp"], "err": err, "ms": elapsed_ms}, ensure_ascii=False))
    time.sleep(1.0)
    return summary


def probe_status(name: str, url: str, engine: str) -> dict:
    """Lightweight status/challenge probe — store heads only."""
    if engine == "curl":
        t0 = time.time()
        r = subprocess.run(
            ["curl", "-sS", "-m", "45", "-L", "-o", "/tmp/stealth-bench-body.txt", "-w", "%{http_code}|%{url_effective}|%{size_download}", url],
            capture_output=True,
            text=True,
            timeout=50,
        )
        elapsed_ms = int((time.time() - t0) * 1000)
        code, eff, size = (r.stdout or "||").split("|", 2)
        body = Path("/tmp/stealth-bench-body.txt").read_text(errors="replace") if Path("/tmp/stealth-bench-body.txt").exists() else ""
        summary = {
            "engine": "curl",
            "probe": name,
            "url": url,
            "http_code": code,
            "effective_url": eff,
            "bytes": size,
            "elapsed_ms": elapsed_ms,
            "content_head": body[:600],
            "challenge_signals": {
                "just_a_moment": "just a moment" in body.lower(),
                "turnstile": "turnstile" in body.lower() or "cf-turnstile" in body.lower(),
                "checking_browser": "checking your browser" in body.lower(),
            },
        }
        save(f"curl-{name}.summary.json", summary)
        print(json.dumps({"probe": f"curl-{name}", "code": code, "signals": summary["challenge_signals"], "ms": elapsed_ms}))
        time.sleep(1.0)
        return summary

    # aginx
    return probe_aginx(name, url, render_tier="browser")


def main() -> None:
    health = http_json("GET", f"{BASE}/health")
    META["aginx_health"] = health
    save("meta.json", META)
    print("health", json.dumps(health))

    results = []
    # --- TLS fingerprint matrix ---
    # peet JSON API is not servable on render_tier=http — use auto/browser
    results.append(probe_aginx("peet-default", "https://tls.peet.ws/api/all", render_tier="auto"))
    results.append(probe_aginx("peet-chrome145", "https://tls.peet.ws/api/all", {"tls_fingerprint": "chrome145"}, render_tier="auto"))
    results.append(probe_aginx("peet-firefox133", "https://tls.peet.ws/api/all", {"tls_fingerprint": "firefox133"}, render_tier="auto"))
    results.append(probe_aginx("peet-safari18", "https://tls.peet.ws/api/all", {"tls_fingerprint": "safari18"}, render_tier="auto"))
    results.append(probe_aginx("peet-edge145", "https://tls.peet.ws/api/all", {"tls_fingerprint": "edge145"}, render_tier="auto"))
    results.append(probe_curl("peet", "https://tls.peet.ws/api/all"))
    results.append(probe_chrome("peet", "https://tls.peet.ws/api/all"))

    # tls3 mirror
    results.append(probe_aginx("tls3-default", "https://tls3.peet.ws/api/all"))
    results.append(probe_curl("tls3", "https://tls3.peet.ws/api/all"))

    # Headers echo
    results.append(probe_aginx("httpbin-headers", "https://httpbin.org/headers"))
    results.append(probe_curl("httpbin-headers", "https://httpbin.org/headers"))

    # Cloudflare community (often challenges plain curl) — read-only, once each
    cf_url = "https://community.cloudflare.com/"
    results.append(probe_status("cf-community", cf_url, "curl"))
    results.append(probe_aginx("cf-community", cf_url, render_tier="browser"))

    # Example.com baseline (should always work)
    results.append(probe_aginx("example", "https://example.com/", render_tier="http"))
    results.append(probe_curl("example", "https://example.com/"))

    # browserleaks TLS page (HTML, not JSON — record challenge/reachability)
    results.append(probe_aginx("browserleaks-tls", "https://browserleaks.com/tls", render_tier="browser"))
    results.append(probe_curl("browserleaks-tls", "https://browserleaks.com/tls"))

    save("matrix-summaries.json", {"meta": META, "results": results})
    print("DONE", len(results), "probes")


if __name__ == "__main__":
    main()
