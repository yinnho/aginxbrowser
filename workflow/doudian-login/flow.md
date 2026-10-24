# doudian-login — 抖店扫码登录流（页内切换版）

抖店（fxg.jinritemai.com）扫码登录出码流。**2026-10-06 翻案后走页内切换器**，
不再绕 open.snssdk.com SSO（那是过渡方案，见文末历史）。

## 结构（7 步）

```
create login/common（钉 1440×900）
  → wait 全表单面（.login-switcher--cell ×1 + .account-center-switch-button ≥2 + input）
  → click_xy (1322,136)（76×76 切换器中心，真鼠标链）
  → wait .account-center-image-content 的 background-image 变 data:png（200×200）
  → eval qr_data（页面手里的码原样抠出，零截图裁剪）
  → screenshot .account-center-image-content（人看的裁剪，对不齐时以 qr_data 为准）
  → wait 落地：hostname=fxg && pathname 不以 /login 开头（{{login_timeout_ms}}）
  → eval landing {url,title}
```

**给人扫码一律发回执 `live_qr`**（`/session/<id>/qr/live`）：自刷新页每秒重读登录页当前码。
**禁止**把 `qr_data` / `qr_shot` / 静态 PNG 递给人（60–100s 过期）。`qr_data`/`qr_shot` 只留作调试归档。

抖店 App 扫一扫 + 手机确认。页面自己轮询 check_qrconnect，确认后 302 进 fxg，末步 predicate 命中。
回执的 `session_id` 就是登录态导出柄（cookie 落 .jinritemai.com：sessionid / sessionid_ss /
sid_guard / sid_tt / passport_csrf_token / toutiao_sso_user_doudian /
sso_uid_tt_doudian / ucas_sso_c0_doudian / ffa_goods_ewid / ecom_gray_shop_id /
odin_tt / msToken / ttwid）。doudian-publish 用 `session_id` 复用直接组合。

## 递码的坑（2026-10-09 实战）

- **码只有 60-100s 寿命**：发 `receipt.live_qr`（活码自刷新），不要抠码落盘。
  静态图来回一延迟就过期；活码页过期会点刷新并换新图。
- **要干净登录态就开新 account**（`/session/create {"account":"新名字"}`）：
  共享 jar 的会话开登录页会被自己的旧 cookie 静默 302 进工作台，根本到不了
  码。10-09 实测：`scan-1029` 新 jar 一次成。
- 长等待可以放心让 flow 自己 `wait` 到底——**#238 已修**（2026-10-09）：
  flow_run 现在逐命令走全局管理器、应答在途不持锁（#193 同款），等待
  期间实例其余端点照常响应（实测 30s wait 中 /sessions 0.77s、
  /state 0.01s）。要提前结束 wait 仍可用 `vars.login_timeout_ms:1000` +
  自己轮询 `/session/:id/wait`，人扫的仍是回执 `live_qr`，不是 qr_data。

## 真机 receipt

- **2026-10-09**：`scan-1029` 干净 jar，页内切换出码 → 人扫+确认 → 落地
  `/ffa/mshop/homepage/index`；前两枪过期纯因递码慢（教训如上）。
- **2026-10-06**：全表单面等齐后 settle-gated 点击 **313ms** 翻转出码；
  扫码确认后 **9.7s** 落地。
- 2026-10-02：SSO 直连时代的首绿（`/tmp/qr-test/run-doudian-login.json`），
  `login_timeout_ms:5000` 短超时验证黄金路径。

## 踩坑史（都在本机实测过）

- **早测页内切换器「全死」是点击太早**：cell 先于 account-center SDK 渲染，
  那时点上的是没接线的元素，翻转永不挂载。等全表单面（cell+tab pills+input
  三者齐）再点。DOM `.click()` 没验证过，走真坐标 click_xy。
- SSO 时代：client_key 抄错一位 = 10003 配置无效（key 是 `ttae0f96cae89a91`，
  ta 开头 15 位）；fxg 登录页自带切换器在 SSO 前链下试过三条路全死。这些
  只在回 SSO 时有用。
- 码是**客户端生成**的 data:png，不走网络图——probe 按
  `img[src^=data:image]`/background-image 找，别拦 get_qrcode 之类的 URL。

## 登录要用新 account（2026-10-21 踩坑）

默认共享 jar 里留着前几次扫码留下的 SSO cookie，之后再开登录页扫码，**cookie 会写进去但页面不跳转**
（URL 一直停在 /login/common，看起来像"扫了没反应"）。连扫三次都落不了地，换成每次一个全新 account
（`POST /session/create {account:"dd-fresh-<ts>", width:1440, height:900}` → navigate 登录页 → 跑本 flow 传 `session_id`）
就 16 秒落地。给人扫的码发回执 `live_qr`（引擎自带 `/session/<id>/qr/live`），别发静态图。

## 活码 + 新 jar（2026-10-21，已写进 flow/引擎）

- `create.account = "dd-login-{ts}"`：每次登录都是全新 cookie 库（引擎把 `{ts}` 换成 unix 秒）。共享 jar 里前次扫码留下的 SSO cookie
  会让后来的扫码"cookie 写进去了、页面不跳转"。
- 回执带 `live_qr`（`/session/<id>/qr/live`）：自刷新扫码页，每秒重读登录页**当前**的码。**给人扫码一律发这个链接，不发静态图**
  （静态图 60–100 秒过期，来回延迟就赶不上）。`/session/<id>/qr` 是当前码的图片，`?json=1` 回 `{url, qr, landed}`。
- 回执带 `elapsed_ms` 和 `timings`（逐步耗时），发布 flow 的速度直接从回执统计。

## Keepalive / anti-tear

No known cookie-deleting widget recipe for fxg today — keep alive via
`account/verify` (cookie write-back + remembered spec):

```bash
curl -sS -X POST "$ENGINE/account/verify" -H 'content-type: application/json'   -d '{"name":"scan-1029","url":"https://fxg.jinritemai.com/ffa/mshop/homepage/index","predicate":"location.hostname===\"fxg.jinritemai.com\" && !location.pathname.startsWith(\"/login\")"}'
```

On `logged_in:false`: delete account + fresh `dd-login-{ts}` / `live_qr`
(shared jar SSO cookies make re-scan look like "scanned, nothing happens").
