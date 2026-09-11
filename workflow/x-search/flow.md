# x-search

Search X via the GraphQL `SearchTimeline` (POST-only — the GET form 404s) and
return tweets deduped and sorted by view count. Two steps, all `eval`:
`bearer` → `search`.

## Run

```bash
curl -sS -X POST http://127.0.0.1:8129/flow/run -H 'Content-Type: application/json' -d '{
  "name": "x-search",
  "session_id": "s_8",
  "vars": { "args_json": "{\"q\": \"obscura rust browser\", \"count\": 20, \"top\": 5}" }
}'
```

`args_json`: `{"q": "<raw query>", "count": 20, "top": 10}` — `count` is how
many results to fetch, `top` how many to return after view-sorting. Saved
output (`search`): `{ok, count, tweets:[{id, sn, views, fav, rt, text}]}` —
`views` from `core.views.count`, text truncated to 200 chars.

## Use it for

- **Target discovery**: the promo-campaign loop starts here — search the
  topic keyword, sort by views, read the top tweets, reply to the ones worth
  a technical data-point comment.
- **Handle resolution**: searching a product name surfaces the official
  account (how the 09-12 follow batch found `playwrightweb`/`Stagehanddev`
  when direct handle guesses 404'd). Feed a hit's `sn` to `x-read` user mode
  for the rest_id + follow state.
- X search operators work in `q` (`from:`, `since:`, `min_faves:`, …).
- Product `product:"Top"` (ranked) is baked in — what you want for finding
  high-view tweets; switch to `"Latest"` in the step script for a timeline
  feed.
