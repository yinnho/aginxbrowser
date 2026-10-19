# Stealth / TLS bench — 2026-10-19

Raw receipts for the public comparison page:

- English: [`docs/stealth-tls-comparison.md`](../../docs/stealth-tls-comparison.md)
- 中文: [`docs/stealth-tls-comparison.zh-CN.md`](../../docs/stealth-tls-comparison.zh-CN.md)

## How to re-run

```bash
# start a stealth-enabled binary built from this repo (or release/Homebrew 0.6.0+)
# Note: the 2026-10-19 receipts used local target/debug @ 0e6f37d, not the cellar formula
AGINXBROWSER_CACHE_TTL_SECS=0 AGINXBROWSER_BIND=127.0.0.1:18089 aginxbrowser &

python3 bench/stealth-tls-20261019/run_probes.py
# then the redo block in docs notes, or re-run the INDEX builder
```

## Files to trust

| File | Role |
|------|------|
| `INDEX.json` | Compact matrix + challenge outcomes |
| `meta.json` | `/health` snapshot at run start |
| `*-*.fp.json` | Slim JA3/JA4/Akamai hashes from peet.ws |
| `*-*.summary.json` | Per-probe wall time, heads, challenge signals |
| `run_probes.py` | First-pass harness (http tier; peet needs `auto`) |

Do **not** treat JA3 as stable across GREASE; prefer JA4 + Akamai H2 hash.

Chrome headless profile dir `_chrome_profile/` is local scratch — not committed.
