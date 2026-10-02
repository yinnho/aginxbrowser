# doudian-login — 抖店扫码登录流（SSO 直连版）

抖店（fxg.jinritemai.com）扫码登录出码流。慢的根源是前链不是出码：
fxg 首页落地 7.1s + 登录页安定 10s+，整链 12-20s 才见码，而码本身
只有 60-100s 寿命。本流直连 SSO authorize URL，干净会话** 1.78s 出码**。

## 结构（4 步，线性）

```
create SSO authorize(open.snssdk.com, redirect_uri=fxg /login/common)
  → wait .qrcode-box img[src^=data:image]（客户端生成的 data:png 码）
  → screenshot .qrcode-box 282×240 crop（qr_shot）
  → wait 落地判定：hostname=fxg && pathname 不以 /login 开头（{{login_timeout_ms}}）
  → eval landing {url,title}
```

出码后把 `saved.qr_shot` 或 `/live?session=<id>` 递给人，抖店 App 扫一
扫 + 手机确认。SSO 页自己轮询扫码态，确认后 302 到 redirect_uri 带
code，fxg 换 code 完登、离开 /login，第 3 步 predicate 命中。回执的
`session_id` 就是登录态导出柄（cookie 落 .jinritemai.com）。

## 真机 receipt（2026-10-02，/tmp/qr-test/run-doudian-login.json）

`login_timeout_ms: 5000` 短超时验证黄金路径：`qr_ready` 命中 IMG、
`qr_shot` 282×240 8.6KB（非空图）、第 2 步按预期 timeout 报 failed、
回执带现场截图 + hint、会话 s_14 保活可接管——全按设计。真登录只差
人扫码确认这一下。

## 踩坑（都是本机实测）

- **client_key 抄错一位 = 10003 配置无效**。key 必须是
  `ttae0f96cae89a91`（ta 开头 15 位）；之前以为「SSO 服务端回归」，
  其实是 URL 少打了个 `9`，报错页长得跟服务端挂了一样。
- **fxg 登录页自带的「扫码登录」切换器是死路**：toggle 后 daren 容器
  空壳，`get_qrcode` 根本不发（DOM click、真坐标 click、先踩
  authorize 再进页，全试过）。页内 `.type` 其他登录方式三个 icon 点
  了也全无反应（window.open 钩验证过没弹窗）。别绕回这条路。
- SSO 页的码是**客户端生成**的 data:png img，不走网络图，probe 要按
  `img[src^=data:image]` 找，别拦 get_qrcode 之类的 URL。
