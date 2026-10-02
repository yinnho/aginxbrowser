# taobao-login — 淘宝扫码登录流（login.jhtml 直连版）

淘宝扫码登录出码流。同抖店一个病根：www.taobao.com 首页 9.79s + 点
登录再 3.7s ≈ 整链 13.5s+；直连 `login.taobao.com/member/login.jhtml`
（302 到 havanaone）总墙钟 **3.73s**，其中 `qrCode/generate.do` 168ms
发出、**QR canvas 815ms 渲染**。

## 结构（4 步，线性）

```
create login.jhtml 1440×900（302 → havanaone/login.htm?bizName=taobao）
  → wait .qrcode-img canvas（页面唯一 canvas，212×212）
  → screenshot .qrcode-img crop（qr_shot）
  → wait 落地判定：hostname 不含 login.taobao.com（{{login_timeout_ms}}）
  → eval landing {url,title}
```

页面默认双 tab（密码/短信 + 扫码），**canvas 无需任何点击**就在右侧。
`qrCode/query.do` 从 758ms 起每 ~2.1s 自动轮询扫码态；扫+确认后
havana 跳离 login.taobao.com（中途登录 hop 留在该域，所以 predicate
命中的是真落地）。cookie 落 `.taobao.com`，任何淘宝会话可导入。

## 真机 receipt（2026-10-02，/tmp/qr-test/run-taobao-login.json）

`login_timeout_ms: 5000` 短超时验证：`qr_ready` 命中 CANVAS、
`qr_shot` 240×240 2.4KB（二维码双色压缩比高，非空图——空图只会剩
几百字节）、第 2 步按预期 timeout、会话 s_15 保活。真登录只差人扫
码确认。

跟 doudian-login 凑一对：两个都是「出码交给人的 handoff 流」，回执
`saved.qr_shot` 直接可递，或 `/live?session=<id>` 给人盯着扫。
