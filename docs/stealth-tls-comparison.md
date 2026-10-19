# Stealth / TLS comparison (evidence)

[English](stealth-tls-comparison.md) | [中文](stealth-tls-comparison.zh-CN.md)

> Snapshot: **2026-10-19 Asia/Shanghai** · AginxBrowser **0.6.0** (probed with local debug binary, `/health` commit `0e6f37d`; later tip commits were mostly docs / bootstrap split — TLS shape unchanged. Default TLS preset name `chrome145`, fleet UA Chrome/154. Initial run showed stealth-path `Sec-Ch-Ua` still v=145 — fixed in a follow-up to track the UA)  
> Raw receipts: [`bench/stealth-tls-20261019/`](../bench/stealth-tls-20261019/) · index [`INDEX.json`](../bench/stealth-tls-20261019/INDEX.json)

This page measures **what the wire looks like**, not a marketing “undetectable” score. Numbers below come from the committed receipts; peer engines we did not run are labeled **not run** with citations.

---

## Methodology

### What we measure

| Signal | Source | Why it matters |
|--------|--------|----------------|
| **TLS JA4** | [tls.peet.ws/api/all](https://tls.peet.ws/api/all) | Stable ClientHello shape (GREASE-stripped). Prefer over JA3. |
| **JA3 hash** | same | Recorded for completeness; **varies** when GREASE values shuffle. |
| **Akamai HTTP/2 fingerprint hash** | same | SETTINGS / WINDOW_UPDATE / pseudo-header order. |
| **User-Agent string** | peet + [httpbin.org/headers](https://httpbin.org/headers) | Must not be confused with the TLS preset name. |
| **HTTP status / challenge interstitial** | community.cloudflare.com, blog.cloudflare.com | Whether a bot challenge page appears; **not** a claim of Turnstile solve rate. |
| **Session / cookies** | out of scope this run | Long-login survival is a separate dogfood; not asserted here. |

### Engines

| Engine | How run | Notes |
|--------|---------|-------|
| **aginxbrowser 0.6.0** | Local `target/debug` (`/health`=`0e6f37d`, not Homebrew cellar), `POST /fetch` `render_tier:"auto"`, optional `tls_fingerprint` | Must-run. |
| **curl** | `/usr/bin/curl` on macOS | No browser TLS impersonation. |
| **Google Chrome 155 headless** | `--headless=new --dump-dom` | Real Chromium baseline on the same machine. |
| **Kitesurf / Lightpanda / Obscura** | **Not run** | No local binary / API token in this bench — see citations. |

### Limits (read this)

- **Not “undetectable.”** JA4 ≈ Chrome family ≠ site will accept you. JS canvas/WebGL, IP ASN, behavior, and challenge solvers are separate layers ([ANTI_BOT.md](ANTI_BOT.md)).
- **UA / TLS preset name / Client Hints must be read together.** Fleet UA defaults to Chrome/154; TLS preset name stays `chrome145` (wreq-util shape). Switching `tls_fingerprint` to `firefox133` / `safari18` changes JA4/H2 **without** rewriting the UA. The initial receipt showed **UA 154 + Sec-Ch-Ua v=145** on httpbin (stealth left the emulation bake-in) — see `aginx-httpbin-headers-auto.raw.json`; code now overrides CH from the UA. Re-probe: [`aginx-httpbin-headers-auto-after-ch-fix.summary.json`](../bench/stealth-tls-20261019/aginx-httpbin-headers-auto-after-ch-fix.summary.json) (UA and CH both 154).
- **Challenge ≠ bypass.** Hitting a Cloudflare interstitial and waiting for `cf_clearance` is recorded honestly when it times out.
- **No merchant / authenticated production APIs** were probed (no XHS Qianfan create, etc.).
- **Rate limit:** ~1–1.5 s between probes; public fingerprint endpoints only.

### How to reproduce

```bash
AGINXBROWSER_CACHE_TTL_SECS=0 AGINXBROWSER_BIND=127.0.0.1:18089 aginxbrowser &
curl -sS http://127.0.0.1:18089/health   # expect "stealth": true, "tls": "chrome145"
curl -sS -X POST http://127.0.0.1:18089/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://tls.peet.ws/api/all","format":"text","max_chars":0,"render_tier":"auto"}'
# Force a non-default TLS shape:
#   "tls_fingerprint":"firefox133" | "safari18" | "edge145" | ...
```

`render_tier:"http"` alone rejects peet’s JSON API with a 500 (“not servable over plain HTTP”) — use `auto` / `browser` so the stealth transport actually navigates.

---

## Matrix A — TLS / HTTP2 fingerprint (`tls.peet.ws/api/all`)

| Engine / config | JA4 | Akamai H2 hash | UA (abbrev.) | Receipt |
|-----------------|-----|----------------|--------------|---------|
| **aginx** default (`chrome145`) | `t13d1516h2_8daaf6152771_d8a2da3f94cd` | `52d84b11737d980aef856699f885ca86` | Chrome/154 macOS | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-default.fp.json) |
| **aginx** `tls_fingerprint=chrome145` | same JA4 / same H2 | same | Chrome/154 | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-chrome145.fp.json) |
| **aginx** `tls_fingerprint=edge145` | same JA4 / same H2 as chrome145 | same | Chrome/154 | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-edge145.fp.json) |
| **aginx** `tls_fingerprint=firefox133` | `t13d1716h2_5b57614c22b0_eeeea6562960` | `6ea73faa8fc5aac76bded7bd238f6433` | Chrome/154 *(UA not switched)* | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-firefox133.fp.json) |
| **aginx** `tls_fingerprint=safari18` | `t13d2014h2_a09f3c656075_14788d8d241b` | `d4a2dcbfde511b5040ed5a5190a8d78b` | Chrome/154 *(UA not switched)* | [fp](../bench/stealth-tls-20261019/aginx-peet-auto-safari18.fp.json) |
| **Chrome 155 headless** | `t13d1517h2_8daaf6152771_cb7bf5808d99` | `52d84b11737d980aef856699f885ca86` | HeadlessChrome/155 | [fp](../bench/stealth-tls-20261019/chrome-peet.fp.json) |
| **curl** | `t13d497h2_0d8feac7bc37_7395dae3b2f3` | `64a832f547be33249bf4d33e8a46c5dc` | `curl/8.0-stealth-bench` | [fp](../bench/stealth-tls-20261019/curl-peet.fp.json) |
| Kitesurf | **not run** | — | — | Published: cannot negotiate bot challenges with **real TLS fingerprints** — [CF docs](https://developers.cloudflare.com/browser-run/kitesurf/), [launch post](https://blog.cloudflare.com/kitesurf/) |
| Lightpanda | **not run** | — | — | Maintainers: UA override won’t hide HTTP/TLS fingerprint — [issue #1436](https://github.com/lightpanda-io/browser/issues/1436), [best practices](https://lightpanda.io/blog/posts/best-practices-for-using-lightpanda) |
| Obscura (standalone) | **not run** | — | — | Not installed on the bench machine |

### Reading the numbers

1. **curl is obviously not a browser** — huge JA4 (`t13d497…`), different H2 hash. Any TLS-aware WAF can separate it from Chrome.
2. **aginx `chrome145` / `edge145` share Chrome-family JA4 prefix and the same Akamai H2 hash as real Chrome 155** (`52d84b11737d980aef856699f885ca86`). JA4 is not byte-identical to this Chrome 155 build (`1516` vs `1517` cipher-slot digit; different final hash) — expected version/preset drift, not “same binary.”
3. **`tls_fingerprint` actually switches the wire**: Firefox and Safari presets produce distinct JA4 + H2 hashes (Firefox H2 hash `6ea73faa…` matches public Firefox-family tables elsewhere).
4. **Honest gap:** Firefox/Safari TLS presets still advertised the Chrome/154 UA in this run. Prefer coherent pairs (Chrome TLS + Chrome UA) unless you know the target.

HTTP headers on [httpbin.org/headers](https://httpbin.org/headers) via aginx showed Chrome-shaped `Sec-Ch-Ua` / `Sec-Fetch-*` (Client Hints major **145** while UA string says **154**) — another coherence tell to watch; receipt [`aginx-httpbin-headers-auto.summary.json`](../bench/stealth-tls-20261019/aginx-httpbin-headers-auto.summary.json).

---

## Matrix B — Challenge / reachability (read-only)

| Target | curl | aginxbrowser | Notes |
|--------|------|--------------|-------|
| `https://example.com/` | 200 | 200 (`tier=http`) | Sanity. |
| `https://blog.cloudflare.com/kitesurf/` | 200 (~349 KB HTML) | 200 (~21 KB markdown, `tier=http`) | **No challenge** from this exit for either client — not a stealth win. |
| `https://community.cloudflare.com/` | **403** “Just a moment…” | API 200 but body is CF interstitial (“正在进行安全验证”); auto-bypass waited for `cf_clearance` and **timed out at 25s** | Both blocked on hard challenge; aginx **did not** clear Turnstile in this run. |
| `https://browserleaks.com/tls` | 200 HTML | 200 markdown shell of the TLS test page | Reachability only; full cipher UI is JS-heavy. |

Receipts: [`curl-cf-community.summary.json`](../bench/stealth-tls-20261019/curl-cf-community.summary.json), [`aginx-cf-community-auto.summary.json`](../bench/stealth-tls-20261019/aginx-cf-community-auto.summary.json), [`aginx-cf-blog.summary.json`](../bench/stealth-tls-20261019/aginx-cf-blog.summary.json).

---

## Competitor claims (not run — cite only)

### Cloudflare Kitesurf

Official Browser Run docs ([What Kitesurf cannot do yet](https://developers.cloudflare.com/browser-run/kitesurf/)):

> Negotiate a bot-challenge handshake with **real TLS fingerprints**.  
> Start a long-running, authenticated session that requires persistent state.

Same wording in the [launch blog](https://blog.cloudflare.com/kitesurf/). That is the product gap this page is aimed at: AginxBrowser’s stealth build **does** speak named browser TLS presets on the wire (Matrix A). We still do **not** claim universal challenge clearance (Matrix B).

### Lightpanda

Project guidance prefers identifying as Lightpanda (`Sec-Ch-Ua: "Lightpanda"`) and [rejects Mozilla UA impersonation](https://lightpanda.io/blog/posts/best-practices-for-using-lightpanda). Maintainers state that changing UA does not fix distinguishability via web APIs or **HTTP/TLS fingerprint** ([#1436](https://github.com/lightpanda-io/browser/issues/1436)). Column left **not run** until we install a binary and repeat peet.ws.

### Obscura

Not installed standalone here. AginxBrowser’s `diting` stack shares lineage with the obscura/blitz line but ships its own stealth HTTP/TLS path — do not assume upstream Obscura equals these JA4 values.

---

## What remains unmeasured

- Side-by-side peet.ws JA4 for **Kitesurf playground / Lightpanda / Obscura** on the same network.
- Turnstile / managed-challenge **pass rate** over a fixed corpus (needs labeled targets + time budget).
- **Long-lived login** cookie survival (accounts / `import_curl`) — claimed in product docs, not in this bench.
- JA4 drift across AginxBrowser releases as Chrome presets advance past `chrome145`.
- Aligning Client Hints (`Sec-Ch-Ua` v=145) with fleet UA Chrome/154.

---

## Related

- [ANTI_BOT.md](ANTI_BOT.md) — layered anti-bot model and site dogfood notes  
- [API.md — `tls_fingerprint`](API.md) — preset names (`chrome145`, `firefox133`, `safari18`, …)  
- [README — Real TLS fingerprints](../README.md)
