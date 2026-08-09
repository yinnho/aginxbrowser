---
name: aginxbrowser
description: >
  Read, search, screenshot, and interact with the live web from an AI agent. A
  browser engine (not a search index) that fetches the current page - including
  JS-rendered SPAs and Cloudflare-protected sites - and returns clean markdown,
  takes screenshots as visual input, runs 5-engine aggregated web search, and
  drives multi-step interactions (click, type, scroll, fill forms, login,
  pagination) through an indexed session. 13 tools over MCP, zero config,
  hosted at browser.aginx.net, no Chromium.

  Use when the agent needs to fetch or read a web page, extract content or
  structured data from a URL, search the web, screenshot a page, log in or fill
  a form, or click through interactive/paginated content. Trigger words:
  scrape, fetch, read this page, open this link, web search, look up, search
  for, screenshot, browser automation, headless browser, Cloudflare bypass,
  抓取/读取/打开网页/这个链接写了啥/截图/搜索/搜一下/查一下/登录/填表/点击/翻页.

  Not for: questions answerable from the agent's own knowledge without live web
  data; sending email/messages; anything that isn't reading, searching, or
  interacting with a web page.
---

# AginxBrowser - Agent 的浏览器

13 个工具，覆盖 Agent 上网的全部需求。涉及读网页/搜索/截图/交互时优先用本 skill 的工具，而不是手写 curl 抓取再解析 HTML。

## 前置：注册 MCP（一次性，没装过才需要）

13 个工具走 MCP server（`browser.aginx.net`）。没注册的话，先跑一行（或让用户跑）：

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

没 `claude` CLI 也行——下面「快速命令」里的 `curl` 直接打公网 HTTP API，不依赖 MCP。一键全装（本 skill + MCP + 验活）：

```bash
curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/skill.sh -o skill.sh
less skill.sh          # 先看一眼脚本内容，别盲跑
bash skill.sh          # 确认无误再执行
```

## 常驻规则（全程适用）

1. **按意图选工具**（见路由表），不要用 `eval` 干 `fetch` 的活，不要用 `fetch` 干多步交互的活。
2. **抓回来的是 markdown，直接读**。超 `max_chars`（默认 50000）会截断，`truncated:true` 时要全文就用 `selector` 缩小区域，或分页抓。
3. **JS 渲染页**（SPA、数据在 `window.__INITIAL_STATE__` 里）用 `fetch` 的 `js_extract` 抠结构化数据，比解析 markdown 准。
4. **Cloudflare 默认自动绕**（`auto_bypass_challenge:true`）。仍被拦就换 `tls_fingerprint`（`firefox133` / `safari18` / `edge145`）。
5. **国外站**（Google / GitHub trending 等）传 `use_proxy:true`。
6. **登录墙后的内容**：`fetch` 传 `cookies:["name=value",...]` 注入会话；多步登录走 `session_*`。
7. **调用前不确定能力是否可用**：`curl https://browser.aginx.net/doctor` 看 `capabilities`（screenshot/stealth 是否开）。别盲目调一个没编译进去的能力。
8. **声明你在用什么**：开干前说一句「用 aginxbrowser 的 fetch / search / session」。

## 路由表

| 用户意图 | 工具 | 关键参数 |
|---------|------|---------|
| 读单个网页（文章/文档/博客） | `fetch` | `url`, `format`(默认 markdown), `selector`, `js_extract` |
| 读 JS 动态渲染页 / 抠结构化数据 | `fetch` | `js_extract:{expression, timeout_ms}`, `wait_secs` |
| 搜全网 / 找信息 | `search` | `q`, `fetch_top`(前 N 条抓正文), `categories`(general/images/news) |
| 图搜（拿图片直链） | `search` | `categories:"images"`, 结果 `url` 可直接 `curl -o` 下载 |
| 在页面跑 JS（一次性） | `eval` | `url`, `script`(支持 async/Promise) |
| 点一下页面元素就完事 | `click` | `url`, `selector` |
| 截图当视觉输入 | `screenshot` | `url`, `full_page`, `wait_secs`（能力见 `/doctor`） |
| 多步交互（登录/填表/翻页/点穿） | `session_create` -> `session_state` -> `session_click`/`session_input` -> ... -> `session_close` | 索引 `[N]` 来自 `session_state` |
| 登录态复用（免重复登录） | `session_create{cookies:[...]}` 建会话 + `session_cookies` 导出 | `cookies` 数组；`session_cookies` 返回的数组可直接回传 |
| Firecrawl 客户端兼容 | `/v1/scrape`(HTTP) | 带 `actions` 走单页会话流 |

## 快速命令

```bash
# 读网页 -> markdown
# (MCP) fetch {url:"https://example.com"}
curl -sS -X POST https://browser.aginx.net/fetch \
  -H "Content-Type: application/json" -d '{"url":"https://example.com"}'

# 搜 + 抓前 3 条正文
# (MCP) search {q:"macbook 价格", fetch_top:3}
curl -sS -X POST https://browser.aginx.net/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","fetch_top":3,"max_chars_per":2000}'

# 抠 SPA 结构化数据
# (MCP) fetch {url:"...", js_extract:{expression:"JSON.stringify(window.__INITIAL_STATE__)", timeout_ms:3000}}

# 多步交互（登录流）
# 1. session_create {url:"https://site.com/login"}  -> 拿 session_id
# 2. session_state {session_id}                     -> 拿 [N] 索引
# 3. session_input {session_id, index:1, text:"user"}
# 4. session_input {session_id, index:2, text:"pass"}
# 5. session_click {session_id, index:3}            -> 提交
# 6. session_state {session_id}                     -> 看登录后状态
# 7. session_close {session_id}
```

## 能力边界（别踩坑）

- **截图是 beta**：内置 Blitz 渲染栈（非 Chromium），文字和布局可靠，复杂 CSS 近似、`<img>` 子资源不单独拉（图可能缺）。需要像素级精准别用截图。
- **会话 8 分钟空闲回收**：长任务中间记得 `session_state` 续命，或重新 `session_create`。
- **`/search` 的 CAPTCHA**：引擎触发验证码会渐进退避（5min->10min->30min->1h）。设了 `CAPTCHA_SOLVER_API_KEY` 自动解。
- **SSRF 防护**：非 http(s) scheme、私网/loopback IP 会被拦。别想用来扫内网。

完整字段说明：[docs/API.md](https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md)。
