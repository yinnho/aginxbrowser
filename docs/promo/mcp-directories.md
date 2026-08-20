# MCP 目录注册套件

> 一次性任务：把 aginxbrowser 的 MCP server 登记到主流的 MCP 目录。所有目录都需要账号登录 + 网页表单提交（无法 API 自动化），本文件是统一提交素材。

## 服务器信息（所有目录通用）

```json
{
  "name": "aginxbrowser",
  "url": "https://browser.aginx.net/mcp",
  "type": "http",
  "description": "Browser engine for AI agents. Fetch JS-rendered and Cloudflare-protected pages as clean markdown, 5-engine aggregated web search (Baidu, Bing, Sogou, WeChat, Google), screenshots with element coordinates, stateful sessions with cookie inject/export for logged-in workflows. 13 tools. Single Rust binary, no Chromium."
}
```

**工具列表**（提交时若需填）：`fetch` `search` `eval` `click` `screenshot` `session_create` `session_navigate` `session_state` `session_cookies` `session_click` `session_input` `session_scroll` `session_eval` `session_close`

**特性要点**（填描述/标签用）：no Chromium · TLS fingerprinting (Chrome145/Firefox133/Safari/Edge) · Cloudflare bypass · 5-engine Chinese+Western search · screenshots · element coordinates · stateful sessions · CAPTCHA solving · MCP-native (13 tools) · hosted + self-hostable (Apache-2.0)

## 各目录提交方式

| 目录 | 提交入口 | 备注 |
|---|---|---|
| **mcpm.sh** | `npx mcpm publish`（CLI）或 web 表单 mcpm.sh | registry 在 pathintegral-institute/mcp-registry，app 提交流，需登录 |
| **mcp.so**（中文） | mcp.so/submit | 中文目录，表单填中文描述更佳 |
| **glama.ai/mcp** | glama.ai 登录 → "Add server" | 支持从 GitHub repo 导入；有 tool-definition-quality 评分，README 质量影响展示 |
| **smithery.ai** | smithery.ai → Submit server | 需账号 |
| **pulsemcp.com** | pulsemcp.com → Submit | curl 403（bot 拦截），用浏览器手动 |

## 中文描述（mcp.so 用）

> AginxBrowser — 给 AI Agent 用的浏览器引擎。不依赖 Chromium 的单一 Rust 二进制：抓 JS 渲染和 Cloudflare 防护的页面为干净 markdown，5 引擎聚合搜索（百度/必应/搜狗/微信/Google），纯 CPU 渲染截图 + 元素坐标，有状态会话支持登录态注入与复用。13 个 MCP 工具，HTTP + MCP 双协议，托管实例即用，也可 Apache-2.0 自托管。

## 提交清单

- [ ] mcpm.sh：`npx mcpm publish`（或 web 表单）
- [ ] mcp.so：填表单（用上面中文描述）
- [ ] glama.ai：登录 → Add server → 填 URL/描述（可尝试 GitHub repo 导入）
- [ ] smithery.ai：Submit server 表单
- [ ] pulsemcp.com：浏览器手动提交

> 提交后各目录会拉取 MCP 端点验证（`initialize` 握手 + `tools/list`），browser.aginx.net/mcp 已通过 13 工具验证，应能直接通过。
