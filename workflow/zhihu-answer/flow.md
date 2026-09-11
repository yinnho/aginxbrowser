# zhihu-answer — zhihu question pages (403 logged-out, by design)

This sample is installed **failing on purpose**. Logged-out question pages
hard-403 at the network layer: the document request comes back a 650-byte
403, the `zse-ck` challenge script (522KB) loads but never trades for a
passing cookie, and the flow dies at step 0 with a receipt. It stays here
because the failure is the demo: a wall is a receipt, not a mystery — and
the composition recipe below is the documented way around it.

```
POST /flow/run {"name":"zhihu-answer"}          # → failed, step 0, 403
POST /flow/run {"name":"zhihu-answer","vars":{"question_id":"19550361"}}
```

The 403 holds through every logged-out path we probed: session navigation,
re-navigation after challenge settle, and one-shot `/fetch` obscura. Same
verdict as the engine's WAF ladder — zhihu sits above it today.

## Composition recipe: import a logged-in session

```
import_curl(curl="<Chrome 'Copy as cURL' of any zhihu.com request>")
flow_run(name="zhihu-answer", session_id="<that session_id>",
         vars={"question_id":"603518666"})
```

The flow's create block is skipped; the steps run in the authenticated
page. Same handoff `xcom-profile` documents for logged-in X reads.

## Engine observation (not chased)

The 403 page leaves a degraded document behind — `document.querySelectorAll`
is `undefined`, `contentType` empty, `childNodes` missing. Recording it
here so the observation survives; whether that document should exist at
all is an engine question for another batch.
