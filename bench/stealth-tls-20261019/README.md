# Stealth / TLS bench — 2026-10-19

Raw receipts for the public comparison page:

- English: [`docs/stealth-tls-comparison.md`](../../docs/stealth-tls-comparison.md)
- 中文: [`docs/stealth-tls-comparison.zh-CN.md`](../../docs/stealth-tls-comparison.zh-CN.md)

## Canonical tip run

| Field | Value |
|-------|-------|
| `/health` commit | `9d43490` |
| Binary | local `target/debug` (`cargo build --features stealth`), **not** Homebrew |
| CH | UA + `Sec-Ch-Ua` both Chrome/154 (fix `9af709f`) |
| JA4 vs first run (`0e6f37d`) | **unchanged** |

Primary receipts use the `-tip` suffix (`aginx-peet-auto-*-tip.fp.json`). Older non-`-tip` peet files are the first-pass `0e6f37d` matrix kept for contrast.

## How to re-run

```bash
cd /path/to/aginxbrowser
AGINXBROWSER_BUILD_COMMIT=$(git rev-parse --short HEAD) cargo build --features stealth
AGINXBROWSER_CACHE_TTL_SECS=0 AGINXBROWSER_BIND=127.0.0.1:18089 ./target/debug/aginxbrowser &
curl -sS http://127.0.0.1:18089/health   # expect commit == tip, stealth true
# then tip probe harness (or adapt run_probes.py to write *-tip)
```

## Files to trust

| File | Role |
|------|------|
| `INDEX.json` | Compact matrix (tip) + challenge outcomes |
| `meta.json` / `meta-tip.json` | `/health` snapshot for tip run |
| `*-tip.fp.json` | Slim JA3/JA4/Akamai hashes from peet.ws (tip) |
| `*-tip.summary.json` | Per-probe wall time / heads |
| `aginx-httpbin-headers-auto-tip.*` | CH alignment proof (154/154) |
| `run_probes.py` | First-pass harness |

Do **not** treat JA3 as stable across GREASE; prefer JA4 + Akamai H2 hash.

Chrome headless profile dirs `_chrome_profile*` are local scratch — not committed.

> **2026-10-21 note.** These receipts were taken while the fleet UA still claimed Chrome/154 on a chrome145
> handshake. The fleet UA is now Chrome/145 (matches the TLS preset) and request headers follow Chrome's
> HTTP/2 order (`chrome_headers.rs`); the JA4 / Akamai hashes are unchanged, the UA/Sec-Ch-Ua columns and
> the header-order result are not. Raw per-request dumps (`*.raw.json`, `*.fp.json`) were dropped from git —
> the summaries and `INDEX.json` carry the numbers; re-run `run_probes.py` for fresh receipts.
