# x-reply

Post a tweet or a reply on X with the full write-op anti-automation header set
(`x-client-transaction-id` + `x-twitter-session-signature` trio), generated
in-page. Four steps, all `eval`:

1. `p256` — install the pure-JS P-256 library (engine `crypto.subtle` has no
   ECDSA) + sign/verify self-test.
2. `bearer` — refresh `localStorage.abx_bearer` from the live `main.*.js`
   when older than 12h.
3. `ctx_bind` — ensure txid context (key + animation frames, cached 72h in
   `localStorage.abx_txid_ctx`) and the web session binding (`abx_bind`,
   ~3h TTL; re-established via `/i/session_binding/challenge` +
   `CreateWebSessionBind` when stale).
4. `post` — CreateTweet with txid + signature trio. Root tweet when
   `reply_to` is empty; reply otherwise. Result saved as `post`:
   `{ok, status, tweet_id, err_code, err_msg}`.

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
