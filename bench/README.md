# Benchmark

Numbers behind the "browser efficiency × model efficiency" story: an agent's
total cost is what the browser burns (time, memory) times what the model burns
(tokens reading the output). AginxBrowser is the token half — this bench
measures both sides.

## Results (2026-08-28, Apple Silicon macOS, CN-routed network)

20 pages × 2 rounds × 4 scenarios, sequential, cold Chrome profile per page.
Full raw data: [`results/run-darwin-2026-08-28.tsv`](results/).

| scenario | p50 | p95 | mean output chars | ≈tokens | notes |
|---|---|---|---|---|---|
| `tier1` forced plain HTTP | 647 ms | 2.1 s | 15 903 | ~4.0k | |
| `tier2` forced V8 render | 1.95 s | 3.9 s | 6 464 | ~1.6k | |
| `auto` (shipped default) | **532 ms** | 1.8 s | 15 903 | ~4.0k | 90% served by Tier 1, no V8 |
| headless Chrome `--dump-dom` | 4 053 ms | 90 s | 7 035 | ~1.8k | 5/40 loads produced no DOM at all; peak RSS **2 105 MB per page** |

**aginxbrowser server peak RSS across the entire run (all scenarios): 227 MB** — one process.

Headlines, same network, same pages:

- **7.6× faster to agent-usable text** (auto p50 532 ms vs Chrome 4053 ms)
- **~10× less memory** (227 MB total for one process vs ~2.1 GB per Chrome page,
  process tree summed)
- **0 vs 12.5% hard failures** — Chrome produced no DOM for python.org,
  react.dev, bun.sh and vitejs.dev on these CN-routed loads; every
  AginxBrowser scenario returned content
- Even forced through the full browser tier (tier2), we land at half of
  Chrome's p50 wall time

The token column reads opposite to intuition at first: our markdown carries
more characters per page than Chrome's tag-stripped DOM text (markdown spells
out link URLs, image alts). Formats differ — treat ≈tokens as scale, not a
bill. The efficiency story is the time/memory/reliability columns; the model
half of total cost is what you do with `max_chars`, `selector` and
`js_extract` to read less in the first place.

## What is measured

Per page of the fixed 20-page set in [`pages.txt`](pages.txt) (public,
robots.txt-allowed — checked against the engine's own robots gate; HN, go.dev,
lib.rs and StackOverflow were dropped because their robots.txt disallows bots):

| Scenario | What runs |
|---|---|
| `tier1` | `POST /fetch {"render_tier":"http"}` — plain HTTP + built-in HTML→markdown. Falls back to the browser tier when HTTP content is insufficient (the `tier` column records what actually served). |
| `tier2` | `POST /fetch {"render_tier":"obscura"}` — full V8 browser render + markdown |
| `auto` | default `/fetch` — the shipped behavior; records the tier hit-rate |
| `chrome` | `headless Chrome --headless=new --dump-dom --virtual-time-budget=8000`, cold profile per page (what a launch-per-fetch Puppeteer pipeline pays), DOM tag-stripped with the Python stdlib — the minimal real-world conversion step |

Metrics:

- **wall ms** — time to agent-usable text (for Chrome: time to DOM; the
  tag-strip is negligible)
- **output chars** — characters of text delivered to the agent; ours is
  markdown, Chrome's is stripped DOM text. Tokens are estimated at chars/4
  and always labeled ≈.
- **peak RSS** — our single server process across the whole run (sampled at
  0.2 s), vs Chrome's summed process tree per page

## How to reproduce

```bash
cargo build --release --features stealth,screenshot
python3 bench/run.py                       # ~15 min, all scenarios
python3 bench/summarize.py                 # prints the table from the newest TSV
```

Raw data lands in `results/run-<host>-<date>.tsv` (one row per page × round ×
scenario). The server is started fresh per run with
`AGINXBROWSER_CACHE_TTL_SECS=0` so repeat fetches aren't served from cache;
one example.com warmup fetch primes the V8 snapshot and is excluded.

## Honest caveats

- Wall time includes real network round trips from the machine running the
  bench (the CN↔global routes in our published numbers average high RTT);
  all scenarios share the same network, so the comparison holds even where
  the absolute numbers wouldn't transfer.
- Sequential fetches, no concurrency — matches how an agent reads pages.
- Chrome's output is tag-stripped DOM text, not markdown: it keeps some
  chrome (nav/boilerplate) that our markdown conversion trims. If anything
  that flatters Chrome's char count, not ours.
- ≈tokens = chars/4 is an approximation across tokenizers, used only for
  scale, never quoted as an exact model bill.
- 2 rounds, 20 pages: this is a signal, not a statistical study. The TSV is
  committed so anyone can re-derive or extend it.
