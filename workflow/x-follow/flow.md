# x-follow

Follow / unfollow an X account via the v1.1 `friendships/{create,destroy}`
endpoint, with the full write-op anti-automation header set
(`x-client-transaction-id` + `x-twitter-session-signature` trio). Four steps,
all `eval`: `p256` → `bearer` → `ctx_bind` → `follow` (same self-healing
prefix as `x-reply`, shared verbatim).

## Run

```bash
curl -sS -X POST http://127.0.0.1:8129/flow/run -H 'Content-Type: application/json' -d '{
  "name": "x-follow",
  "session_id": "s_8",
  "vars": { "args_json": "{\"action\": \"follow\", \"user_id\": \"2065154071112060928\"}" }
}'
```

`args_json`: `{"action": "follow"|"unfollow", "user_id": "<rest_id>"}`.
`unfollow` maps to `friendships/destroy.json`; anything else maps to `create`.
The `follow` step saves `{ok, status, action, id, screen_name, following,
err_head}`.

## Gotchas learned on live runs

- **The response `following` field echoes the PRE-action state** (unfollow of
  a followed account returns `following:true`; the follow after it returns
  `following:false`), even though the mutation did apply in both directions —
  verified via `x-read`'s `relationship_perspectives.following` after a
  round-trip. Don't trust it as a post-condition; use `x-read` on the
  screen_name for the authoritative state.
- `user_id` is the numeric **rest_id**, not the handle. Resolve a handle with
  `x-read {screen_name}` first, or find the account via `x-search`.
- Batch follow/unfollow should keep the 350-600ms gap between calls that the
  manual 09-12 follow batch used — the flow runs one target per invocation on
  purpose (rate-limit + bot-profile hygiene).
- Same prerequisites as `x-reply`: a logged-in x.com session passed as
  `session_id`, localStorage seeded with `abx_ct_features`.
