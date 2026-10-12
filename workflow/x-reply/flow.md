# x-reply

> 2026-10-12：P-256 收成 named recipe `x-p256`（`preload` + `apply_now`），本 flow 不再内联库正文。

> 2026-10-12：bearer 探针收成 named recipe `x-bearer`（与 `x-p256` 同 preload）；本 flow 不再内联 843B 探针。

> 2026-10-12：txid ctx+bind / b64 / derEncode / authHeaders 收成 named recipe `x-auth-ctx`（与 `x-p256`/`x-bearer` 同 preload）；本 flow 的 `ctx_bind` 改为薄封装，写路径 eval 改调 `abxB64keep`/`abxDerEncode`/`abxTxidGen`。


Post a tweet or a reply on X with the full write-op anti-automation header set
(`x-client-transaction-id` + `x-twitter-session-signature` trio), generated
in-page. Preload mounts `x-p256` + `x-bearer` + `x-auth-ctx`, then five
`eval` steps:

1. `p256` — smoke sign/verify against recipe `x-p256` (`globalThis.P256`).
2. `bearer` — thin call into recipe `x-bearer` (`abxProbeBearer`).
3. `ctx_bind` — thin call into recipe `x-auth-ctx` (`abxEnsureAuthCtx`):
   txid context (72h `abx_txid_ctx`) + web session binding (`abx_bind`).
4. `post` — CreateTweet with txid + signature trio (helpers from
   `x-auth-ctx`). Root tweet when `reply_to` is empty; reply otherwise.
   Result saved as `post`: `{ok, status, tweet_id, err_code, err_msg}`.
5. `verify` — after a reply (skipped for root tweets): sleeps 1.5s, pulls
   `TweetDetail` on the parent, and reports whether our reply is actually in
   the thread: `{ok, verified, mine_in_thread}`. This is the only reliable
   post-check — `UserTweetsAndReplies` lags and doesn't show replies to
   strangers. A `verified:false` after `post.ok:true` means visibility lag,
   not necessarily a lost reply; re-run the flow's verify step (or
   `x-read {tweet_id}`) before concluding.

## Prerequisites

- A session on an x.com page that is logged in (six-cookie style B login) —
  pass its id as `session_id`. No cookies are stored in this flow by design.
- The session's localStorage must carry `abx_ct_features` (the 42-key
  features map). Any prior read/write setup on the session seeds it.
- Steps 1-3 are self-healing; on a cold session only step 3 can be slow
  (fetches `/home` for the txid key/frames — eval budget is ~5s, prefer a
  session that already visited x.com).

## Run

```bash
curl -sS -X POST http://127.0.0.1:8129/flow/run -H 'Content-Type: application/json' -d '{
  "name": "x-reply",
  "session_id": "s_8",
  "vars": { "args_json": "{\"text\": \"reply text here\", \"reply_to\": \"2097684761405735333\"}" }
}'
```

`args_json` is a string whose content is JSON: `{"text": "...", "reply_to":
"<tweet id>"}` (empty `reply_to` = root tweet). It is spliced into the script
as a JS object literal, so any quotes/newlines inside `text` are fine as long
as the value is JSON-encoded.

## Error codes worth knowing

- `187` duplicate — the same status already exists (posted recently). Also
  means the request passed the auth gate: a "failed" 187 after an earlier
  timeout usually means the first attempt DID land (eval timeout cuts the
  result, not the request). Verify before re-running; never blind-retry
  replies to strangers.
- `186` over 280 chars, `88` rate limited, `401 code 32/89` — stale bearer
  (step 2 normally fixes this on the next run).
