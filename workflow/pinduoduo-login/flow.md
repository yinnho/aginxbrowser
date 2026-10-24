# pinduoduo-login — 拼多多商家扫码登录

对齐 `doudian-login` / `taobao-login`：**出码 → 人扫 → 回执 `session_id` + `live_qr`**。

## 人只需要做一件事

**打开回执 `live_qr`（`/session/<id>/qr/live`），用手机【多多商家 App】扫并确认。**
禁止递静态 PNG / `qr_shot`。

## 结构

```
create account pdd-login-{ts}（keepalive）
  → navigate mms /login/?redirectUrl=/home/
  → wait QR canvas ready
  → screenshot（仅归档）
  → wait 离开 /login（{{login_timeout_ms}}）
  → eval landing
```

引擎 `GET /session/<id>/qr` 会从方 canvas 抠当前码；`/qr/live` 每秒重读并在「已过期」时点刷新。

**永不包装 `window.fetch`**（PDD fbeZ 指纹）。

## 用法

```bash
ENGINE=http://127.0.0.1:8199
curl -sS -X POST "$ENGINE/flow/run" -H 'content-type: application/json' \
  -d '{"name":"pinduoduo-login","vars":{"login_timeout_ms":180000}}'
# 回执：
#   session_id  → 给 pinduoduo-publish
#   live_qr     → /session/<id>/qr/live  （打开给人扫）
```

短超时先拿活码再自己轮询：

```bash
curl -sS -X POST "$ENGINE/flow/run" -H 'content-type: application/json' \
  -d '{"name":"pinduoduo-login","vars":{"login_timeout_ms":1000}}'
# 打开 http://127.0.0.1:8199$live_qr ；扫完后：
# POST /session/<id>/wait  predicate=离 /login
# POST /account/verify {name, url: https://mms.pinduoduo.com/home/, predicate: ...}
```

## 与 publish

`pinduoduo-publish` 要已登录 `session_id`（或同 account 热 jar）。先本 flow，再 publish。

## Keepalive / anti-tear

PDD has **no** fetch/XHR probe shield (never wrap `window.fetch` — fbeZ).
Session keep-alive is verify write-back only:

```bash
curl -sS -X POST "$ENGINE/account/verify" -H 'content-type: application/json'   -d '{"name":"pdd-249-20261024-154659","url":"https://mms.pinduoduo.com/home/","predicate":"location.hostname.indexOf('"'"'mms.pinduoduo.com'"'"')>=0 && location.pathname.indexOf('"'"'/login'"'"')<0"}'
```

On `logged_in:false`: `DELETE /accounts/:name` + re-run this flow (`live_qr`),
no stack-scan onto a torn jar.
