# xhs-qianfan-login — 千帆（商家）扫码登录

对齐 `doudian-login`：**出码 → 人用千帆 App 扫 → 回执 `session_id`**。

> **不是** `workflow/xhs-login`（C 端 www / edith `sns/.../qrcode/create` → `www.xiaohongshu.com/mobile/login`）。
> 商家路径只走 beer-login → CAS ` /api/cas/customer/web/qr-code` → `customer.xiaohongshu.com/loginconfirm`。

## 人只需要做一件事

**打开回执 `live_qr`（`/session/<id>/qr/live`），用手机【千帆 App】扫并确认。**
禁止递 `qr_data` / `qr_shot` 静态图。

## 路径（2026-10-13 见证）

```
preload CAS qr-code hook
  → navigate customer.xiaohongshu.com/login?service=https://ark.xiaohongshu.com/ark
  → wait .beer-login-container img.css-wemwzq
  → eval click 右上角二维码图标
  → wait「APP扫一扫登录」+ img.css-1lhmg90
  → capture meta（断言 loginconfirm + 通道含千帆）
  → save qr_data / qr_shot
  → wait ark 落地（无 ticket= 查询串、无「凭证已过期」）
  → seller/info/v2 验活
```

CAS 状态枚举（beer ScanLogin）：`CREATE=2` → `WAIT_CONFIRM=3` → `LOGIN_SUCCESS=1`；`EXPIRED=4`。

## 用法

```bash
# Reuse ONE stable account name (never xhs-qf-$HHMMSS) so persona fp_seed stays one device.
MAC_UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36'
SID=$(curl -sS -X POST "$ENGINE/session/create" -H 'content-type: application/json' \
  -d "{\"account\":\"xhs-qf-stable\",\"width\":1440,\"height\":900,\"persona_ua\":\"$MAC_UA\"}" | jq -r .session_id)

curl -sS -X POST "$ENGINE/flow/run" -H 'content-type: application/json' \
  -d "{\"name\":\"xhs-qianfan-login\",\"session_id\":\"$SID\",\"vars\":{\"login_timeout_ms\":180000}}"
# 回执 live_qr=/session/$SID/qr/live — 千帆 App 扫（勿发 qr_data/qr_shot）
```

干净 `account` jar：旧 cookie 会静默把你送进半登录态。**同一店只复用 `xhs-qf-stable`（或 `xhs-merchant-ark`）**，不要每次扫码换 `xhs-qf-HHMMSS`（会烧新 fp_seed，易 300015）。Darwin 上即使不传 `persona_ua` 也会默认 Mac Chrome/145。

## 闸门（flow 内 throw）

| 检查 | 要求 |
|------|------|
| QR URL | `customer.xiaohongshu.com/loginconfirm?...` |
| 拒绝 | `www.xiaohongshu.com/mobile/login`（edith C 端） |
| 通道 | `support_qr_code_channel_infos` 含 **千帆** |
| 落地 | ark host、无 `ticket=`、无「登录凭证已过期」、`seller/info/v2` success |

## 已知坑

- 引擎 CSS 常把 beer-login 卡片画歪/画没；**以 DOM + CAS 回执为准**，全页截图可能只有 GROW 底图。
- `click_xy` 对错位图标不可靠 → 用 DOM `.click()`。
- 扫码成功后若长时间停在 `ark?...&ticket=ST-...` 且弹「凭证已过期」，说明 **ST 换票失败/过期**——重跑本 flow，扫码后尽快让页面自己完成 SSO（勿中途 navigate 打断）。
- `service-ticket` 在未登录时常见 HTTP 406；登录态建立后由页面自己换票。

## 与发布的衔接

`workflow/xhs-merchant-publish` 复用本 flow 回执的 `session_id`（同一 account）。先登录见证 `landing.seller.success`，再跑发布。
