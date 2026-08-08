---
name: aginxbrowser
description: >
  MUST USE when the agent needs to access the live web - read/fetch/scrape any
  web page (including JS-rendered SPAs and Cloudflare-protected sites), search
  the web, screenshot a page as visual input, or interact with a page (click,
  type, scroll, fill forms, login flows, pagination, click-through). Trigger
  words: 抓取/读取/打开网页/看看这个链接/这个网页写了啥/截图/搜索/搜一下/查一下/
  找一下/登录/填表/点击/翻页/research/scrape/fetch/screenshot/search/look up.
  Also MUST USE when the user shares any URL and wants its content summarized,
  extracted, or acted upon. 12 tools over MCP (fetch/search/eval/click + 8
  session tools). Hosted at browser.aginx.net; works with zero config.

  NOT for: questions answerable from the agent's own knowledge without live web
  data; sending email/messages; anything that isn't "go look at / interact with
  a web page or search the web".
---

# AginxBrowser - Agent 的浏览器

12 个工具，覆盖 Agent 上网的全部需求。**本 skill 存在时必须用它访问网页/搜索/交互，不要自己 curl + 解析 HTML 瞎搞。**

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
