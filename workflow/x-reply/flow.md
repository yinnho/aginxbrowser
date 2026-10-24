# x-reply

> 2026-10-12：P-256 收成 named recipe `x-p256`（`preload` + `apply_now`），本 flow 不再内联库正文。

> 2026-10-12：bearer 探针收成 named recipe `x-bearer`（与 `x-p256` 同 preload）；本 flow 不再内联 843B 探针。

> 2026-10-12：txid ctx+bind / b64 / derEncode / authHeaders 收成 named recipe `x-auth-ctx`（与 `x-p256`/`x-bearer` 同 preload）；本 flow 的 `ctx_bind` 改为薄封装，写路径 eval 改调 `abxB64keep`/`abxDerEncode`/`abxTxidGen`。

> 2026-10-22：`post` 点页面上的 Post 按钮。Draft.js 编辑器走 `ContentState.createFromText` + `EditorState.createWithContent`，再点 `[data-testid="tweetButtonInline"]`（首页）或 `tweetButton`（回复框）。本步不再自己 `fetch` CreateTweet，也**不**包装 `window.fetch` / XHR（确认靠 timeline DOM / composer_cleared / alert）。当天带文章链接的 CreateTweet，即使用了从 `main.js` 解析出的 query id，也被 X 回了 344；同一个正文用这个按钮发出去了。`abx_ct_features` / `abx_ct_toggles` 仍从 `main.js` 写入，verify 还读它们。解析失败不挡点击。

Post a root tweet or a reply by clicking X's composer Post button. Preload
still mounts `x-p256` + `x-bearer` + `x-auth-ctx` so verify has a bearer and
the session binding. The publish step itself does not sign a request.

1. `p256` — smoke sign/verify against recipe `x-p256` (`globalThis.P256`).
2. `bearer` — thin call into recipe `x-bearer` (`abxProbeBearer`).
3. `ctx_bind` — thin call into recipe `x-auth-ctx` (`abxEnsureAuthCtx`):
   txid context (72h `abx_txid_ctx`) + web session binding (`abx_bind`).
4. `plan` — decides whether the session is already on the composer for this
   action. Saved as `plan`: `{url, reply_to, from}`. An empty `url` means
   stay. A root on `/home` or `/compose/post` (no `in_reply_to`) stays.
   Any other page for a root sets `url` to `https://x.com/home`. A numeric
   `reply_to` stays only when the current URL already contains
   `/status/<id>` or `in_reply_to=<id>`; otherwise `url` is
   `https://x.com/i/status/<id>`. A non-numeric `reply_to` does not navigate.
5. `open` — `navigate` to `plan.url` when it is non-empty. This is its own
   step because a navigation inside the post eval drops the realm and kills
   the script. The branch skips it when `plan.url` is empty.
6. `post` — fill the composer and click Post once. Result saved as `post`,
   a JSON string: `{ok, status, tweet_id, err_code, err_msg, via, confirmed,
   href, n_feat, n_tog, features_err}`. `via` is `post_button`. `confirmed`
   is `timeline` (a new `/status/` article on this page contains the text),
   `composer_cleared` (the box emptied and no error toast was seen;
   `tweet_id` may be null), or `alert`. No `window.fetch`/XHR wrap — the
   page owns CreateTweet; `status`/`err_code` from the wire are usually
   null unless an on-page alert surfaced them. Eval budget is 45s.
   For a reply, if the status page has no composer yet, this step clicks
   the focal tweet's `[data-testid="reply"]` and waits for the box.
   It does not click when the box never receives the text, or when Post
   stays `aria-disabled`.
7. `verify` — after a reply (skipped for root tweets): sleeps 1.5s, pulls
   `TweetDetail` on the parent, and reports whether our reply is actually in
   the thread: `{ok, verified, mine_in_thread}`. This is the check for a
   reply whose `post.tweet_id` is still null. A `verified:false` after
   `post.ok:true` means visibility lag, not necessarily a lost reply; re-run
   verify (or `x-read {tweet_id}`) before concluding. `verified:true` can
   also mean an older reply of ours was already in the thread.

## Prerequisites

- A session on x.com that is logged in (six-cookie style B login) — pass
  its id as `session_id`. No cookies are stored in this flow by design.
- Root posts use the composer already on `/home` or `/compose/post`. From
  any other page the flow opens `https://x.com/home` first. The home
  composer is the path that published status `2113174741095940166`.
- Replies use the composer on that tweet. From any other page the flow
  opens `https://x.com/i/status/<id>` first, then clicks Reply if the box
  is not already there. The same Post button sends it. This flow does not
  fall back to a direct CreateTweet call when the box is missing.
- Steps 1-3 are self-healing; on a cold session only step 3 can be slow
  (fetches `/home` for the txid key/frames — eval budget is ~5s, prefer a
  session that already visited x.com). The verify step reads
  `abx_ct_features` written by the post step in the same run. A navigation
  in `open` drops the JS helpers and keeps localStorage.

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
as the value is JSON-encoded. `reply_to` has to be digits.

## Error codes worth knowing

- `187` duplicate — the same status already exists (posted recently). Also
  means the request passed the auth gate: a "failed" 187 after an earlier
  timeout usually means the first attempt DID land (eval timeout cuts the
  result, not the request). Verify before re-running; never blind-retry
  replies to strangers.
- `186` / `88` / `344` historically came from a direct CreateTweet call.
  This flow no longer sends CreateTweet and does not wrap `window.fetch` to
  read the button's wire response, so `post.err_code` is usually null.
  On-page limit toasts still surface as `confirmed: "alert"` / `err_msg`.
  A button that stays disabled comes back as `err_msg: "Post stayed
  disabled"` and was not clicked. Do not blind-retry the same text after
  `composer_cleared` or an alert until you check the thread/profile.
- `confirmed: "composer_cleared"` with `tweet_id: null` means the box emptied
  and this page did not show a new status link before the wait ended. For a
  reply, read `verify`. For a root, look at the profile before posting the
  same text again. The word "posted" on the home page is not a signal.
- An eval timeout after the click can mean the click already landed. Check
  the thread or the profile before running the flow again.
