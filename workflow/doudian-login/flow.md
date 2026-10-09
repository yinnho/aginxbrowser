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

出码后把 `qr_data` 解码（剥 data: 前缀 → base64 → .gif/.png）落到本地文件，
`open` 给人扫，抖店 App 扫一扫 + 手机确认。页面自己轮询 check_qrconnect，
确认后 302 进 fxg，末步 predicate 命中。回执的 `session_id` 就是登录态导出柄
（cookie 落 .jinritemai.com：sessionid / sessionid_ss / sid_guard / sid_tt /
passport_csrf_token / toutiao_sso_user_doudian / sso_uid_tt_doudian /
ucas_sso_c0_doudian / ffa_goods_ewid / ecom_gray_shop_id / odin_tt / msToken /
ttwid）。doudian-publish 用 `session_id` 复用直接组合。

## 递码的坑（2026-10-09 实战）

- **码只有 60-100s 寿命，出码到人扫要一气呵成**：抠码→落盘→open 全链要
  <5s，先码后闲聊必过期。过期别救，重跑 flow（~15s 重铸一张）。
- **要干净登录态就开新 account**（`/session/create {"account":"新名字"}`）：
  共享 jar 的会话开登录页会被自己的旧 cookie 静默 302 进工作台，根本到不了
  码。10-09 实测：`scan-1029` 新 jar 一次成。
- 长等待别让 flow 自己 `wait` 到底——**flow_run 全程持 SESSIONS 锁**，
  等待期间这台实例所有 HTTP 端点全堵（引擎已知 bug，另立单）。递码模式用
  `vars.login_timeout_ms:1000` 拿回执里的 qr_data，自己轮询
  `/session/:id/wait`。

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
