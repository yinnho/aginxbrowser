# taobao-login — 淘宝扫码登录流（login.jhtml 直连版）

淘宝扫码登录出码流。同抖店一个病根：www.taobao.com 首页 9.79s + 点
登录再 3.7s ≈ 整链 13.5s+；直连 `login.taobao.com/member/login.jhtml`
（302 到 havanaone）总墙钟 **3.73s**，其中 `qrCode/generate.do` 168ms
发出、**QR canvas 815ms 渲染**。

## 结构（5 步，线性）

```
preload everyhelp 拦截（fetch/XHR 层拒绝 everyhelp.(cdn.)taobao.com）
  → create login.jhtml?redirectURL=www.taobao.com 1440×900（302 → havanaone）
  → wait .qrcode-img canvas（页面唯一 canvas，212×212）
  → screenshot .qrcode-img crop（qr_shot）
  → wait 落地判定：hostname 不含 login.taobao.com（{{login_timeout_ms}}）
  → eval landing {url,title,nick}
```

页面默认双 tab（密码/短信 + 扫码），**canvas 无需任何点击**就在右侧。
`qrCode/query.do` 从 758ms 起每 ~2.1s 自动轮询扫码态；扫+确认后
havana 跳离 login.taobao.com（中途登录 hop 留在该域，所以 predicate
命中的是真落地）。redirectURL 钉死落 www.taobao.com，末步的 `nick`
（`.site-nav-login-info-nick` 锚）就是登录判据：true=活。cookie 落
`.taobao.com`，任何淘宝会话可导入。

## 2026-10-07 三条血泪纪律（登录态保不住的根因与仪式）

1. **everyhelp 处决墙**：`everyhelp.taobao.com/version/getWidgetVersion`
   响应 200 成功但 Set-Cookie 删 unb/sn/uss（服务端处决，无报错面）。
   step-1 的 preload 把它在 fetch/XHR 层拒绝；实例级保险=
   `AGINXBROWSER_INIT_SCRIPT` 挂同一脚本（本机 8791 已挂）。其余
   taobao flow（orders/shop-collect/live/publish）都带同款。
2. **弗兰肯罐=假修复**：被处决过的账户在死 jar 上直接叠扫码，登录
   「看起来活了」但卖家授予没建立，第一次特权写（草稿自动保存
   previewDraftSubmit）服务端就拆会话（另一把刀，同样删 unb）。
   **被处决过的账户必须先 `DELETE /accounts/:name`（连活 jar 一起清）
   再重扫**。扫完先验卖家面再干活：导航
   `mai.taobao.com/seller_admin.htm`，落 `myseller 千牛商家工作台`=
   卖家会话在（扫码后立刻去可能撞 SSO 握手半程弹一次登录页，重导
   一次就过），弹登录=别发。
3. **探针纪律**：登录判据只认 www.taobao.com 的 nick 锚。
   `i.taobao.com/my_taobao` 是坏探针——那页没有 nick 锚、还会弹回
   havana，2026-10-07 观测脚本在它身上出过假死判决。另外卖家面
   （SellManage 等）**只走 flow 既定路由**，探索性导航=风控簇级拆
   会话（同 IP 多账户十分钟内连坐双灭的实测教训）。

## 真机 receipt（2026-10-02，/tmp/qr-test/run-taobao-login.json）

`login_timeout_ms: 5000` 短超时验证：`qr_ready` 命中 CANVAS、
`qr_shot` 240×240 2.4KB（二维码双色压缩比高，非空图——空图只会剩
几百字节）、第 2 步按预期 timeout、会话 s_15 保活。真登录只差人扫
码确认。

跟 doudian-login 凑一对：两个都是「出码交给人的 handoff 流」，回执
`saved.qr_shot` 直接可递，或 `/live?session=<id>` 给人盯着扫（活页
码不会像静态截图那样两三分钟就死）。
