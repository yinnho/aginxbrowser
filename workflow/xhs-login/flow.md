> **商家 / 千帆请用 [`xhs-qianfan-login`](../xhs-qianfan-login/)**（beer-login → CAS `loginconfirm`）。本 flow 是 **C 端** www / edith 扫码。

# xhs-login — 小红书登录（全在 flow 里）

对齐 `doudian-login` / `taobao-login`：**出码 → 人扫 → 回执 `session_id`**。
不依赖 `scripts/xhs-qr-login.py` 画码；SVG 二维码在 flow 内用 vendored
`xhs-qrcode` recipe 生成（`saved.qr_data` + `#__xhs_qr_panel` 截图 `qr_shot`）。

## 人只需要做一件事

**用手机小红书 App 扫 `qr_data` / `qr_shot` / `/live?session=` 上的码并确认。**

其余（签名 preload、出码、画码、status 轮询、写回 jar）都在
`POST /flow/run {"name":"xhs-login"}` 里。

## 两种 mode

### A. `mode=qr`（默认）— 引擎内扫码

```
preload xhs-sign + xhs-qrcode + QR hook
  → navigate login
  → ensure_qr（优先页内 create）
  → render_qr → saved.qr_data（data:image/svg+xml;base64,…）
  → screenshot #__xhs_qr_panel → saved.qr_shot
  → 签名轮询 status 至 code_status=2
  → landing
```

```bash
# 干净 account 会话（推荐）
SID=$(curl -sS -X POST "$ENGINE/session/create" -H 'content-type: application/json' \
  -d '{"account":"xhs4","width":1440,"height":900}' | jq -r .session_id)

curl -sS -X POST "$ENGINE/flow/run" -H 'content-type: application/json' \
  -d "{\"name\":\"xhs-login\",\"session_id\":\"$SID\",\"vars\":{\"login_timeout_ms\":120000}}"
# 回执 saved.qr_data / saved.qr_shot — 扫码；或打开 /live?session=$SID
```

### B. `mode=cookies` — Chrome 导入（App「环境异常」时）

不另跑脚本：把 Chrome DevTools 的 cookie 当成 **flow 输入**。

```bash
curl -sS -X POST "$ENGINE/flow/run" -H 'content-type: application/json' -d '{
  "name": "xhs-login",
  "vars": {
    "mode": "cookies",
    "import_cookies": [
      "web_session=…; Domain=.xiaohongshu.com; Path=/; Secure; HttpOnly",
      "a1=…; Domain=.xiaohongshu.com; Path=/"
    ],
    "land_url": "https://www.xiaohongshu.com/explore"
  }
}'
```

`create.cookies` 吃 `import_cookies`（整叶替换，保持数组类型）→ preload →
navigate `land_url` → 签名 `user/me` 验活。形态等同 `import_curl`，只是
收进 flow 的 vars，而不是外部 helper。

从 Chrome 拷 cookie：开 `creator.xiaohongshu.com` 扫码登录 → DevTools →
任意请求 Copy as cURL → 把 `-b` / `cookie:` 拆成 Set-Cookie 字符串数组
（或先 `POST /import/curl` 得到 account 会话，再 `session_id` 复用跑
`xhs-post`，不必再跑本 flow）。

## 结构（14 步）

| id | op | 作用 |
|---|---|---|
| preload | preload | `recipes=[xhs-sign,xhs-qrcode]` + QR hook（两种 mode 都跑） |
| route | branch | `mode=cookies` → cookies_nav |
| nav_login…poll | … | QR 出码 / 画码 / 轮询 |
| render_qr | eval | vendored `xhs-qrcode` recipe → SVG data URL + DOM 面板 |
| qr_shot | screenshot | `#__xhs_qr_panel`（同 doudian/taobao 的 `qr_shot`） |
| cookies_* | … | 导入 jar 验活 |
| end | eval | 收束 |

## 为什么不用页内 canvas / 外部 Python

- 引擎里页面 QR `canvas.toDataURL` 曾出 **空白 PNG**（ledger 09-25）。
- 本 flow 用 **SVG 模块码**（`qrcode-generator`）写进 `qr_data`，并挂到
  `#__xhs_qr_panel`，`/live` 与 `qr_shot` 都能扫。
- `scripts/xhs-qr-login.py` 仅作可选 CLI；自动化请只打本 flow。

## 环境异常

status `471` / 超时且 App 显示环境异常 → flow **抛结构化错误**，文案要求
`vars.mode=cookies` + `import_cookies`（或 Chrome `import_curl` 后只带
`session_id` 去跑 `xhs-post`）。不另开脚本文件。

## vars

| var | default | |
|---|---|---|
| `mode` | `"qr"` | `"qr"` \| `"cookies"` |
| `login_url` | www login | QR 模式登录页 |
| `login_timeout_ms` | `120000` | 扫码确认预算（帽 120s） |
| `import_cookies` | `null` | cookies 模式：Set-Cookie 字符串数组 |
| `land_url` | `/explore` | cookies 模式落地页 |

## 纪律

1. 优先页内 QR（`source=page`）；同会话少 `create`。
2. status GET-only，参数 `qr_id`/`code`，必须 `_webmsxyw` 签名。
3. 干净 `account` jar；主号别进引擎。
4. 码寿命短：出码后立刻扫 `qr_data` / `/live`。
