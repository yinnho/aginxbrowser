# XHS creator signer (#56) — postmortem, 2026-09-22

Issue #56 claimed the creator.xiaohongshu.com page signer "never wires" in this
engine (`window.mnsv2` family never lands), so signed XHRs go out bare and 406.
Batch 179 re-investigated from scratch on HEAD (v0.5.3, c247e66). Outcome: issue
closed — the original observation point was wrong, and the signer installs
natively today. Filed #78 for four real stealth gaps found along the way.

## 1. The observation point was wrong

`mnsv2` is an internal module name inside the jsvmp VM. Real Chrome does not
expose it as a global either — on a genuine Chrome session of the same page,
`typeof window.mnsv2` is `"undefined"`, exactly as in our engine. A verdict
built on that symbol was unfalsifiable from the start. The correct observable
is `window._webmsxyw` — the actual x-s/x-t signing entry the page's own code
calls.

## 2. What actually works on HEAD

Fresh session, plain navigation to `creator.xiaohongshu.com/publish/publish`
(no eval, no patches, no masks; the 401 bounce to /login still runs the bundle):

- `typeof window._webmsxyw` → `"function"`
- Calling it for real:

  ```
  _webmsxyw("/api/sns/web/feeds", JSON.stringify({…homefeed payload}))
  → {"X-s":"XYW_eyJzaWduU3ZuIjoiNTYiLCJzaWduVHlwZSI6…","X-t":"1790073313738"}
  ```

  X-s is the `XYW_` + base64 envelope, decodes to `{"signSvn":"56",…}`; X-t is
  a ms timestamp. Correct shape.
- The jsvmp also wraps `window.fetch` at runtime
  (`function(t){return N(t,arguments.length>1?td(arguments[1]):{})}`), while
  `XMLHttpRequest.prototype.open/send/setRequestHeader` remain
  `[native code]` — the signing layer rides the fetch path, and the wrap is
  present.
- Server acceptance was already proven in the original #56 workaround (manual
  `_webmsxyw` signing → note create `200 success:true`); engine-produced
  signatures are accepted. Not re-proven end-to-end this time (needs login
  cookies) — regression-check on the next real-send batch.

Most probable fix candidate: #55 (8f150dd, custom element upgrade chain —
ctor this-rebinding + `new` authorization), which landed 32 minutes after #56
was filed; the launcher's install path depends on that chain. Not bisected;
the outcome is what mattered for closing.

## 3. VM behavior map (byproducts worth keeping)

- **Origin gate**: on a 127.0.0.1 probe page the VM reads one property
  (`per.now`) and bails without installing anything — in real Chrome too. Any
  Chrome-parity comparison must run on the real origin.
- **Environment scan** (real origin, our engine, no masks, 26 reads then
  silent completion): `per.now` → `per.timeOrigin` → `doc.addEventListener` →
  `doc._listeners` ×4 → `doc.cookie` → `nav.__proto__` → `nav.webdriver` →
  `HAS nav.webdriver` → `doc.documentElement` → `HAS` on 10 selenium/webdriver
  markers → `per.now`/`per.timeOrigin` again → `doc.cookie` ×2. Reads
  stopping ≠ VM crash — it finished judging.
- **Blob location**: launcher.js (~909 KB) carries the 288 KB jsvmp payload in
  a double-quoted template string after `__makeTemplateObject(["`; the
  environment slots at the tail use strict `!==` spelling
  (`typeof navigator!=="undefined"?navigator:undefined`).

## 4. Filed separately: #78 stealth gaps

All four are on the VM's actual read path, with Chrome reference values:

| Property | Ours | Chrome |
|---|---|---|
| `typeof document._listeners` | `"object"` (registry readable) | `"undefined"` |
| `typeof navigator.webdriver` | `"undefined"` | `"boolean"` (false) |
| `performance.now()` integral | yes | no (sub-ms float) |
| `Object.prototype.toString.call(navigator)` | `"[object Object]"` | `"[object Navigator]"` |

## 5. Instrumentation traps hit along the way

- edith responses for rejected requests (bad/missing signature, no login)
  carry no `Access-Control-Allow-Origin`, so page-context fetch only ever
  reports `CORS error` with status 0 — don't read that as a transport failure.
- `/api/redcaptcha/v2/getconfig` is signature-exempt (bare XHR also gets 200) —
  it cannot serve as signing evidence despite living on edith.
- `/session/:id/har` records response headers but not request headers.
- The page's fetch wrapper (minified, from the VM) and our own bootstrap fetch
  (`async (input, init = {})`, unminified) are distinguishable by shape —
  check `String(window.fetch)` before attributing a wrapper to the page.
- Local engine API shapes used throughout: `POST /session/create` body
  `{"url":…}`; `POST /session/:id/eval` body `{"script":…}` returns
  `{"result":"<string>"}`; eval queues behind page load (kick returning fast
  is normal, poll `window` state separately).
