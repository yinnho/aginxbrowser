# xcom-profile — read an X profile logged-out

Recorded 2026-09-11 against `x.com/aginxbrowser` through the engine proxy
(foreign egress + stealth TLS), then curated. Logged-out profile pages
render fully in diting — handle, stats, up to ~10 posts — no login wall.
Nonexistent handles bounce to `/i/flow/login`, which is what the `expect`
on this flow catches.

```
POST /flow/run {"name":"xcom-profile"}                        # default handle
POST /flow/run {"name":"xcom-profile","vars":{"handle":"elonmusk"}}
```

Both verified green: `aginxbrowser` (3 followers, 4 posts) and `elonmusk`
(1,406 following, 10 posts, posts from 4h before the run). A bogus handle
returns `status:failed` at step 0 with the login-redirect URL and a
viewport screenshot in the receipt.

## Engine notes learned while recording

- `data-testid` attributes (the usual `article[data-testid=tweet]`,
  `[data-testid=UserName]` addressing) are absent from the DOM our renderer
  produces — target `article`, `[role=tab]`, `a[href*="/followers"]` instead.
- The `create` block goes through `{{var}}` substitution — a bug found right
  here on the first replay (the raw `{{handle}}` navigated literally and
  died in the login redirect); fixed with a regression test in
  `src/flow.rs` (`run_flow_substitutes_create_block`).
- The proxy must be the foreign-egress one (`use_proxy: true` is baked into
  the create block). x.com is unreachable from CN direct.

## Logged-in composition

Reading a timeline, mentions, or DMs needs login: import a logged-in
session first, then hand its id to the flow —

```
import_curl(curl="<Chrome 'Copy as cURL' of any x.com request>")
flow_run(name="xcom-profile", session_id="<that session_id>")
```

The flow's create block is skipped entirely; the flow's steps run in the
authenticated page.
