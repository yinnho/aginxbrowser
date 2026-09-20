# Read timestamps, not landing pages

> The agent-browser scoreboard. Updated every release cycle — see [Update log](#update-log).

Every project in this space has a landing page, a star count, and a demo GIF. None of
those measure engineering. The only honest benchmark for "did you actually build the
web platform" is the **date a corner of the spec got closed** — a public commit hash,
a public ticket, a public release.

This page is our scoreboard against the other browsers built for agents. Rules:

1. **Timestamps only, no adjectives.** Every row links to something public: their
   ticket or release, our commit. Click everything.
2. **Both sides must have public dates.** A capability with no dated counterpart on
   the other side doesn't get a row — it goes in the "not races" section instead.
3. **Rows we lose stay in.** A scoreboard that only wins is marketing.

We're AginxBrowser — a single-binary Rust agent browser (obscura-lineage core, like
Cloudflare's Kitesurf, plus the layout/paint stack we maintain for screenshots). If
you want the landing-page version: [browser.aginx.net](https://browser.aginx.net).
This page is the other thing.

---

## vs Lightpanda (35k★, Zig, no rendering by design)

Their 0.4.1 shipped Sep 15, 2026. Reading it against our git log:

| Capability | Them | Us | Reading |
|---|---|---|---|
| Click dispatches the full pointer/mouse sequence, not one bare `click` | shipped in [0.4.1](https://github.com/lightpanda-io/browser/releases), Sep 15 | [`ab2baa7`](https://github.com/yinnho/aginxbrowser/commit/ab2baa7), Sep 6 | we led by 9 days |
| Redirect rewrites POST→GET: body + its headers must drop (fetch redirect semantics) | [#3526](https://github.com/lightpanda-io/browser/issues/3526), filed Sep 15 — closed Sep 16 | [`3d5f0bc`](https://github.com/yinnho/aginxbrowser/commit/3d5f0bc) + [`48fa1ec`](https://github.com/yinnho/aginxbrowser/commit/48fa1ec), Sep 11 | our fix predates their ticket by 4 days |
| Which box scrolls when `<body>` has `overflow` — css-overflow-3 §3.3 propagation | [#3523](https://github.com/lightpanda-io/browser/issues/3523), filed Sep 15 — closed Sep 16 | [`4f26dec`](https://github.com/yinnho/aginxbrowser/commit/4f26dec), Sep 14 | we shipped before the ticket even existed |
| CSS custom properties | "basic support" in [0.4.1](https://github.com/lightpanda-io/browser/releases), Sep 15 | [`208518a`](https://github.com/yinnho/aginxbrowser/commit/208518a), Sep 4 — `var()` substitution + CSSOM readback | we led by 11 days |
| Fetch Referrer Policy (document header, `<meta>`, per-request) | [#3485](https://github.com/lightpanda-io/browser/issues/3485), filed Sep 10 — closed Sep 17 | [`cb0c34e`](https://github.com/yinnho/aginxbrowser/commit/cb0c34e) + [`42deef0`](https://github.com/yinnho/aginxbrowser/commit/42deef0), Sep 14–15 | **they filed first; we shipped first.** Both dates on record |
| Storage state on context creation | [#1550](https://github.com/lightpanda-io/browser/issues/1550), open since Feb 14 | [v0.2.7](https://github.com/yinnho/aginxbrowser/releases/tag/v0.2.7), Sep 4 — session export/import incl. localStorage | open 7 months |

Credit where due, scope kept precise: their 0.4.1 shipped an experimental **full
CORS** implementation (`--experimental-features cors`). Ours covers CORS preflight
enforcement. On breadth, **they lead there**. Two teams grinding the same specs hit
the same corners — that's why dates are a fair benchmark: nobody's copying anybody,
the spec is public, and so are the closures.

## vs browser-use (114k★, Python over Chromium)

| Capability | Them | Us | Reading |
|---|---|---|---|
| A stable device fingerprint per profile (identity continuity across sessions) | [#5119](https://github.com/browser-use/browser-use/issues/5119), filed Jul 1 — **open** | [`bd30283`](https://github.com/yinnho/aginxbrowser/commit/bd30283), Sep 13 — per-account device persona (UA + hardware seed per identity) | open 2.5 months vs shipped |
| Surviving bot walls | [#5447](https://github.com/browser-use/browser-use/pull/5447), merged Aug 10: patch two CDP detection leaks + **integrate Bright Data's paid CAPTCHA solver** | TLS-layer Chrome-145 impersonation since the [first public commit](https://github.com/yinnho/aginxbrowser/commit/31102e4) (Jun 19, [`d478bdb`](https://github.com/yinnho/aginxbrowser/commit/d478bdb)), in-engine byte-WAF PoW solver [`94c2ced`](https://github.com/yinnho/aginxbrowser/commit/94c2ced) (Aug 27) | don't leak at the TLS layer; solve challenges in-engine; no paid external service |
| Long-running sessions without leaking Chrome processes | [#5484](https://github.com/browser-use/browser-use/issues/5484), filed Aug 17 — **open** (tab accumulation); [#5770](https://github.com/browser-use/browser-use/issues/5770), filed Sep 10 — **open** (teardown hangs, orphaned Chrome) | [`26b35e9`](https://github.com/yinnho/aginxbrowser/commit/26b35e9), Sep 6 — persistent sessions survive idle eviction *and process restarts* | there is no Chrome to orphan |

Honest note on this matchup: browser-use is an orchestrator over real Chromium, and
that buys them web-platform coverage we're still closing corner by corner (their
pages run full Chrome). The receipts above are about the price of that architecture,
timestamped.

## Kitesurf (Cloudflare): no rows, by method

Kitesurf is closed source ("hopefully soon" for open), so it has no public tickets
and no public commit dates — it cannot enter a timestamp scoreboard, ours or anyone's.
What's public is their launch post, which itself lists what it can't do: real TLS
fingerprints, long authenticated sessions, and (at launch) self-hosting. Those are
our rows 2–3 above and our license. Same obscura lineage — Cloudflare picking this
core validated the route for everyone in it.

## Not races

Three axes where the projects aren't competing on dates at all:

- **Pixels.** Lightpanda renders zero pixels *on purpose*. browser-use delegates to
  Chrome. We ship real screenshots, PDF, and video from the Rust layout/paint stack
  we maintain: a full page costs **532 ms / 227 MB** in our pipeline where Chrome
  needs **4053 ms / 2.1 GB** ([method](https://browser.aginx.net/#benchmarks)).
- **Fingerprint.** libcurl (Lightpanda) and stock Chrome-via-CDP (browser-use) both
  answer "what TLS stack are you?" honestly. Half the production web answers wrong
  JA3s with a wall. Ours impersonates Chrome 145.
- **License.** AGPL-3.0 (Lightpanda), closed (Kitesurf), MIT-over-Chromium
  (browser-use — but the Chrome it drives is a 500MB binary with its own terms) vs
  Apache-2.0, one 227MB-RAM binary.

## Steal this

This page is a method, not a dunk. If you're evaluating an agent browser: pick the
five web-platform corners your agents actually touch, and go read the dates. Ours
are public — every link above resolves, and the whole log is
[right here](https://github.com/yinnho/aginxbrowser/commits/main/).

And it's Apache-2.0. If you're building your own and a commit above saves you a
week — take it.

---

## Update log

- **2026-09-15** — first audit. Lightpanda 0.4.1 receipts (6 rows), browser-use
  architecture receipts (3 rows), Kitesurf excluded by method (closed source).
