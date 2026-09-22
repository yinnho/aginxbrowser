# AginxBrowser 淘宝/天猫兼容性问题报告

## 建议 Issue 标题

`[Bug] 淘宝店铺页白屏：g.alicdn.com 子资源加载失败，但相同 URL 通过顶层 /fetch 可成功`

## 摘要

在 macOS arm64 上使用 AginxBrowser 0.2.8 访问淘宝店铺时，桌面店铺页会正常被淘宝重定向到登录页，但登录 UI 没有渲染出来；移动端公开店铺页返回 HTTP 200，也能从原始 HTML 中读到 `shopId`、`sellerId` 和 `pageId`，但最终正文为空、截图接近空白，无法加载商品列表。

关键现象是：页面中的 `g.alicdn.com` 外部脚本由 `aginxbrowser::diting_browser::page` 加载时持续报 `Network error`；把完全相同的脚本 URL 作为顶层 URL 交给 `/fetch` 时，Tier 1 虽然失败，但 `smart_fetch` 会回退到 Tier 2 并成功取得脚本内容。这更像是页面子资源加载链路没有复用顶层请求的回退/网络能力，而不只是淘宝要求登录。

## 测试目的

访问以下公开店铺并统计店铺商品数量：

```text
https://shop115719829.taobao.com/
```

## 测试环境

- 测试时间：2026-09-06（Asia/Shanghai）
- 操作系统：macOS 26.6.2（Build 25G83）
- CPU 架构：Apple Silicon / arm64
- 安装方式：Homebrew 官方 tap
- AginxBrowser：0.2.8
- 可执行文件：`/opt/homebrew/bin/aginxbrowser`
- 功能：`screenshot+stealth`
- 代理：未配置
- CAPTCHA solver：未配置
- 启动绑定：`127.0.0.1:8089`

## 前置检查

`aginxbrowser doctor` 全部通过：

```text
[ok] features  screenshot+stealth
[ok] fonts     bundled CJK bundle inks 汉字
[ok] egress    https://example.com -> 200
all checks passed (0 warning(s))
```

健康接口也正常：

```json
{
  "status": "ok",
  "engine": "diting",
  "version": "0.2.8",
  "capabilities": {
    "screenshot": true,
    "stealth": true,
    "captcha_solver": false
  }
}
```

同一台机器、同一网络中的普通 Chrome 可以打开淘宝并由用户正常完成登录，因此不是整机无法连接淘宝或阿里 CDN。

## 最小复现步骤

### 1. 启动服务

```bash
AGINXBROWSER_BIND=127.0.0.1:8089 aginxbrowser
```

### 2. 抓取桌面店铺首页

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://shop115719829.taobao.com/"}'
```

实际结果（登录跳转中的临时参数已省略）：

```json
{
  "url": "https://login.taobao.com/...<redacted>",
  "title": "登录",
  "content": "网站无障碍\n \"登录页面\"改进建议",
  "truncated": false,
  "tier": "browser"
}
```

重定向到登录页本身可以是淘宝的正常策略，但页面主要登录 UI、二维码和表单都没有被渲染出来。

### 3. 抓取移动端公开店铺页

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://shop.m.taobao.com/shop/shop_index.htm?shop_id=115719829"}'
```

实际结果摘要：

```json
{
  "title": null,
  "content": "",
  "truncated": false,
  "tier": "browser"
}
```

该 URL 的原始 Document 请求实际返回 HTTP 200、约 3000 bytes。HAR 中可见：

```html
<script>
var __vmGlobalData__ = {
  "shopId": 115719829,
  "sellerId": 2360607136,
  "pathInfo": "shop/index2",
  "pageId": 324596025,
  "protocolType": "mini"
};
</script>

<script src="//g.alicdn.com/tb/tracker/index.js"></script>
<script src="//g.alicdn.com/cell/cell-lib-cps/0.0.6/index.js"></script>
<script src="//g.alicdn.com/tb-shop/shop-page-webapp/0.1.129/web/index.js"></script>
```

说明 HTML 文档和店铺标识已成功到达，失败发生在后续应用脚本/页面渲染阶段。

### 4. 使用交互会话复现

```bash
curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://shop.m.taobao.com/shop/shop_index.htm?shop_id=115719829"}'

curl -sS http://127.0.0.1:8089/session/s_1/network
curl -sS http://127.0.0.1:8089/session/s_1/console
curl -sS -X POST http://127.0.0.1:8089/session/s_1/screenshot
```

实际结果：

- `network.total = 1`，只记录了 1 个 Document 请求；状态 200，大小约 3000 bytes。
- `console.total = 0`。
- 截图接口返回有效 PNG（测试中为 1440 x 820），但页面内容为空白，店铺应用未出现。
- 服务端 stderr 同时明确记录多个外部脚本加载失败；这些失败没有出现在会话 network/HAR 中。

## 关键日志

移动店铺页的应用脚本加载失败：

```text
WARN aginxbrowser::diting_browser::page:
Failed to fetch script https://g.alicdn.com/tb-shop/shop-page-webapp/0.1.129/web/index.js:
Network error: ... error sending request for url (...)

WARN aginxbrowser::diting_browser::page:
Failed to fetch script https://g.alicdn.com/tb/tracker/index.js:
Network error: ... error sending request for url (...)

WARN aginxbrowser::diting_browser::page:
Failed to fetch script https://g.alicdn.com/cell/cell-lib-cps/0.0.6/index.js:
Network error: ... error sending request for url (...)
```

登录页脚本也失败：

```text
WARN aginxbrowser::diting_browser::page:
Failed to fetch script https://g.alicdn.com/mtb/lib-windvane/3.0.6/windvane.js: Network error

WARN aginxbrowser::diting_browser::page:
Failed to fetch script https://g.alicdn.com/??mtb/lib-promise/3.1.3/polyfillB.js,mtb/lib-windvane/3.0.7/windvane.js:
Network error

WARN aginxbrowser::diting_browser::page:
Failed to fetch script https://g.alicdn.com/vip/havana-nlogin/0.10.36/index.js: Network error
```

另观察到一次脚本解析错误：

```text
WARN aginxbrowser::diting_browser::page:
Inline script error: JS error: Uncaught SyntaxError: Invalid or unexpected token
at <script>:1:19
```

## 最重要的对照实验

将上面失败的脚本 URL 直接传给 `/fetch`：

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://g.alicdn.com/tb-shop/shop-page-webapp/0.1.129/web/index.js"}'
```

结果如下：

| 资源 | 作为页面子资源 | 作为顶层 `/fetch` |
|---|---|---|
| `tb-shop/shop-page-webapp/0.1.129/web/index.js` | Network error | 成功，返回 JS；`content_length=50000`，因上限截断 |
| `??mtb/lib-promise/3.1.3/polyfillB.js,mtb/lib-windvane/3.0.7/windvane.js` | Network error | 成功，`content_length=6989` |
| `vip/havana-nlogin/0.10.36/index.js` | Network error | 成功，返回 JS；`content_length=50000`，因上限截断 |

顶层请求日志显示：

```text
WARN aginxbrowser::render: smart_fetch: Tier 1 error for <same g.alicdn.com URL>, trying Tier 2
```

随后 Tier 2 成功返回内容。页面子资源加载日志中没有看到相同的 Tier 2 回退，只直接报失败。

## 实际结果

1. 无 Cookie 的桌面店铺页被淘宝重定向到登录页。
2. 登录页依赖的阿里 CDN 脚本未加载，无法显示可操作的登录 UI。
3. 移动端店铺页的基础 HTML 和店铺 ID 已到达，但应用脚本未加载，最终正文和截图为空。
4. 商品组件和真实商品列表请求 `/i/asynSearch.htm` 没有被页面触发，因此无法统计商品数。
5. 会话 network/HAR 没有记录 stderr 中出现的失败子资源，诊断信息不完整。

## 预期结果

至少满足以下一项：

1. 页面外部脚本在 Tier 1 失败后，像顶层 `/fetch` 一样回退到 Tier 2，并完成店铺 SPA 渲染；或
2. 登录页能完整渲染二维码/表单，允许用户在持久会话中手动完成登录；或
3. 返回结构化错误，明确指出哪些关键子资源失败，而不是返回成功但正文为空的页面。

在资源加载成功后，会话应继续等待店铺应用初始化并记录商品列表请求，最终能够读取商品组件或 `/i/asynSearch.htm` 响应。

## 初步原因判断

以下为根据日志作出的推测，需要维护者结合实现确认：

1. `diting_browser::page` 的子资源下载器没有复用 `render::smart_fetch` 的 Tier 1 -> Tier 2 回退逻辑。
2. 页面子资源网络栈与顶层 `/fetch` 使用的 TLS/HTTP 客户端、DNS、请求头或重试策略不同。
3. 协议相对 URL（`//g.alicdn.com/...`）或阿里 CDN combo URL（路径以 `??` 开头并含逗号）在子资源链路中可能存在兼容问题。
4. 外部脚本失败后，页面仍被当作已完成并返回空正文/空截图，缺少“关键资源失败”状态。
5. 失败的脚本请求没有进入 session network/HAR，导致调用方只能从 stderr 发现真实原因。

## 建议修复方向

1. 让 `<script src>`、stylesheet、XHR/fetch 等页面子资源复用顶层 `smart_fetch` 的网络策略和 Tier 2 fallback。
2. 为协议相对 URL和 `https://g.alicdn.com/??a.js,b.js` combo URL增加回归测试。
3. 检查子资源请求是否正确继承 UA、Accept-Language、Cookie、Referer、重定向策略和 stealth TLS 配置。
4. 对 SPA 页面增加资源加载完成/应用就绪等待；关键脚本失败时返回明确的 partial/error 状态。
5. 将失败的子资源也写入 session network/HAR，至少包含 URL、资源类型、错误类别和最终重试结果。
6. 为淘宝登录页和移动店铺页增加可选集成测试；测试只需验证登录组件或店铺根组件出现，不需要自动登录或绕过验证。

## 安全与复现说明

- 本报告未包含账号、密码、Cookie、登录令牌或验证码。
- 淘宝登录跳转 URL 中的 `_lgt_`、UUID 等临时参数均已删除。
- 未尝试绕过淘宝登录、验证码或访问控制。
- 移动端公开店铺页即可稳定复现“Document 成功、外部脚本失败、页面为空”的核心问题，无需提供淘宝账号。

