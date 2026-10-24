# taobao-keepalive — verify-only ping (anti-tear)

Thin flow that navigates `www.taobao.com` under an existing account jar with
`taobao-block-everyhelp` mounted, then witnesses the buyer nick anchor.

**Preferred cron path** (same shield, plus cookie write-back + remembered
spec) is the engine face, not this flow:

```bash
ENGINE=http://127.0.0.1:8199
curl -sS -X POST "$ENGINE/account/verify" -H 'content-type: application/json' \
  -d '{"name":"taobao-personal","url":"https://www.taobao.com/","predicate":"!!document.querySelector('"'"'.site-nav-login-info-nick'"'"')"}'
# → {logged_in, probe_shields:["taobao-block-everyhelp"], …}
```

`account/verify` (and `account/login`) now preload host-matched probe
shields before navigate, so a bare verify can no longer silently let
everyhelp delete `unb`/`sn`/`uss`.

## When to use this flow

Hand-check / compose with an existing `session_id`, or when you want the
`alive` save blob without teaching the account verify spec.

```bash
curl -sS -X POST "$ENGINE/flow/run" -H 'content-type: application/json' \
  -d '{"name":"taobao-keepalive","vars":{"account":"taobao-personal"}}'
```

## On `logged_in: false`

1. **DELETE** the account (`DELETE /accounts/:name`) — do not stack-scan
   onto a torn jar (franken jar = fake fix; seller grants never establish).
2. Re-run `taobao-login`, hand the human `live_qr` only.
3. Re-stamp verify on www nick, then seller face
   (`mai.taobao.com/seller_admin.htm` → myseller 千牛) before privilege writes.

## Other platforms (same pattern)

| Platform | Probe shield on verify/login | Ping URL + predicate |
|---|---|---|
| Taobao / Tmall | `taobao-block-everyhelp` | `https://www.taobao.com/` + nick selector |
| XHS www / creator / ark | `xhs-sign` (logout fuse) | explore / ark home (see xhs-*-login) |
| PDD mms | **none** (never wrap `fetch`) | `https://mms.pinduoduo.com/home/` + off `/login` |
| Doudian fxg | none today | `…/ffa/mshop/homepage/index` + off `/login` |

Cron: call `POST /account/verify` periodically; alert on `logged_in:false`
→ delete + `live_qr`, no stack-scan.
