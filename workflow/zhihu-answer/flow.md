# zhihu-answer — zhihu question pages (burst-403 aware, loops with a budget)

Logged-out question pages hard-403 at the network layer: the document
request comes back a 650-byte 403, the `zse-ck` challenge script (522KB)
loads but never trades for a passing cookie. The 403 holds through every
logged-out path we probed — session navigation, re-navigation after
challenge settle, one-shot `/fetch`. What changed in batch 175
(2026-09-22): the flow no longer dies at step 0 with a bare wait
timeout. It **loops, classifies, and gives up with a receipt that says
exactly what happened** — `steps_done=34` executions ≈ 5 wall retries,
then `zhihu wall persists after 34 step executions — verdict=unknown
facts.doc_status=403 … Burst 403 self-heals in ~15 min: park, then
rerun this flow with the same session_id (cookies stay warm)`.

```
POST /flow/run {"name":"zhihu-answer"}          # → failed at giveup, ~24s, receipt with facts
POST /flow/run {"name":"zhihu-answer","vars":{"question_id":"19550361"}}
```

## Structure (9 steps, a jump-back loop)

```
load(id) → settle(1.2s) → verdict→v → branch landed→read
                                  → branch steps_done≥30→giveup
                                  → space(2.5s) → branch steps_done truthy→load   # back-jump
giveup: throw (receipt names the wall + the caller-side remedy)
read:   wait .List-item (the ONLY DOM wait) → eval answers → expect /question/
```

`max_steps: 40` — a loop flow declares a tight budget, not the 1000
default.

## Three lessons the dogfood run paid for (all engine-side, all fixed)

1. **A 403 wall has no DOM.** The 650-byte 403 body never parses into a
   document in this engine (`document.documentElement` is undefined), so
   a `readyState` predicate burns its full timeout on exactly the wall
   this flow exists to classify. Rule: **no DOM-dependent wait before
   the classifier** — settle on time (`setTimeout`), put DOM waits on
   the landed path only, where a DOM is guaranteed to exist.
2. **Page-context state resets on wall navigation.** The first loop
   design counted retries in `window.__zhihu_tries`; every 403
   navigation wiped the JS context, the counter silently restarted at 1,
   and the loop never converged (dogfood receipt: `tries: 1` on every
   round, killed only by curl's 180s timeout). Fix: the executor's
   reserved `steps_done` var — executor-maintained, immune to page
   resets. Branch `steps_done at_least 30` for retry budgets.
3. **Whole-leaf `{{var}}` keeps type.** `"value": "{{max_tries}}"` used
   to splice the STRING "3", which made `at_most` error and `equals`
   silently read false. `substitute` now returns the value itself for a
   clean whole-leaf placeholder (numbers stay numbers).

## Composition recipe: import a logged-in session

```
import_curl(curl="<Chrome 'Copy as cURL' of any zhihu.com request>")
flow_run(name="zhihu-answer", session_id="<that session_id>",
         vars={"question_id":"603518666"})
```

The flow's create block is skipped; the steps run in the authenticated
page. Same handoff `xcom-profile` documents for logged-in X reads.

## The 15-minute backoff belongs to the caller

Burst 403 self-heals in ~15 min. A flow holding a session asleep for 15
minutes is a session wasted; a cron rerun with the same `session_id`
(cookies stay warm) costs nothing. The giveup receipt says exactly
this — the flow's job is to fail fast and hand the decision up.
