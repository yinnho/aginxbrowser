# xhs-login — 小红书扫码登录流（引擎内 QR + 签名轮询）

小红书（xiaohongshu.com）扫码登录出码 / 确认流。对应原
`scripts/xhs-qr-login.py`（09-25 ledger），形态对齐 `doudian-login` /
`taobao-login`：**flow 出码 → 人扫确认 → 回执 `session_id` 即登录态**。

引擎**不再**按域名自动挂 `xhs-sign`；本 flow 显式
`preload {recipe:xhs-sign}` + QR create 钩子。

## 结构（6 步）

```
create 裸会话（1440×900，无 url）
  → preload recipe=xhs-sign + QR create hook（document-start）
  → navigate {{login_url}}（默认 https://www.xiaohongshu.com/login）
  → wait _webmsxyw 或 __xhs_qr
  → eval ensure_qr → saved.qr {url, qr_id, code, source}
  → eval 签名轮询 status 至 code_status=2（{{login_timeout_ms}}，帽 120s）
  → eval landing 证据
```

## 怎么跑

**推荐（账户位持久 jar）：**

```bash
# 1) 干净 account 建会话（死 jar / 共享 jar 易风控）
curl -sS -X POST "$ENGINE/session/create" \
  -H 'content-type: application/json' \
  -d '{"account":"xhs4","width":1440,"height":900}'
# → session_id

# 2) 跑 flow（复用 session_id；create 块被跳过）
curl -sS -X POST "$ENGINE/flow/run" \
  -H 'content-type: application/json' \
  -d '{"name":"xhs-login","session_id":"s_…","vars":{"login_timeout_ms":120000}}'
```

出码后立刻把 `saved.qr.url` 画成二维码给人扫（见下）。确认后
`saved.login_done.code_status == 2`；cookie 在会话 jar（带 `account` 时
引擎按命令写回账户位）。再 `flow/run {"name":"xhs-post","session_id":…}`。

**递码（人侧，flow 外）：**

```bash
python3 - <<'PY'
import json, sys
# pip install qrcode ; 或沿用 /tmp/qrvenv
import qrcode
receipt = json.load(sys.stdin)   # flow 回执或中途取出的 saved
url = json.loads(receipt["saved"]["qr"])["url"]  # 若 saved.qr 已是对象则直接 ["url"]
qrcode.make(url).save("/tmp/xhs-qr.png")
print("/tmp/xhs-qr.png")
PY
open /tmp/xhs-qr.png
```

或盯着 `/live?session=<id>` 扫页内码（活页不会像静态图那样两三分钟就废）。

**快失败 / 自己轮询（对齐 doudian 笔记）：**
`vars.login_timeout_ms: 5000` → 回执带 `saved.qr` → 本地渲染扫码 → 用同一
`session_id` 自行 `eval` 签名 status，或加长超时再跑（`__xhs_qr` 仍在页上）。

## Flow 内 vs 仍在外部

| 在 flow 里 | 仍在外部 / 人侧 |
|---|---|
| preload `xhs-sign` + QR hook | 账户位 `POST /session/create {account}`（flow create 不传 account） |
| navigate 登录页 | 把 `saved.qr.url` **渲染成 PNG**（python `qrcode` 等） |
| 捕获页内 create 或一次手动签名 create | 手机 App 扫码 + 确认 |
| `_webmsxyw` 签名轮询 `qrcode/status` | App 报「环境异常」时改走 **Chrome import_curl**（正路兜底） |
| landing 证据 | `GET /session/:id/cookies` 验收 `web_session` |

## 纪律（从 python / ledger 搬过来）

1. **优先用页面自己的 QR**（`source=page`）。同会话反复
   `qrcode/create` 是 09-22 风控指纹；ensure_qr 只在钩子 20s 内没抓到才
   手动补一枪。
2. **status 必须签名且 GET-only**：参数名是 `qr_id` / `code`（不是登录 URL
   里的 `qrId` / `xhs_code`）。页内轮询在引擎里不跑（visibility），所以
   flow 自己签着打。
3. **码寿命短**：出码 → 渲染 → 扫要一气呵成；过期（3/471）别救，重跑
   flow。
4. **干净 jar**：要稳就新 `account`；主号别进引擎。www cookie 进引擎曾
   触发 300011 — 出问题回 Chrome 扫码 + `import_curl`。
5. **create 不带 url**：先 preload 再 navigate，避免无签首航（与 xhs-post
   同款）。

## 与脚本的关系

`scripts/xhs-qr-login.py` 保留为 CLI 薄封装（本地画码 + `open`），内部应
优先打本 flow；新自动化请直接 `flow/run name=xhs-login`。

## vars

| var | default | 含义 |
|---|---|---|
| `login_url` | `https://www.xiaohongshu.com/login` | 登录页 |
| `login_timeout_ms` | `120000` | 扫码确认预算（eval 帽 120s） |
