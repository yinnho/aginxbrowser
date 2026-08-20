# Launch 发布稿（Kitesurf 热点期）

> 四渠道草稿，直接复制粘贴用。发布前最好先发我 review 一遍（或自行调整）。
> 背景：Cloudflare Kitesurf（agent 浏览器）刚发布，博客文章已就位：
> - 中文：https://browser.aginx.net/blog/launch-zh.html
> - English：https://browser.aginx.net/blog/launch-en.html
> - 技术复盘：https://browser.aginx.net/blog/rendering-en.html / -zh.html

---

## 1. V2EX（中文，分享创造区）

**标题**：做了个给 AI Agent 用的浏览器，不依赖 Chromium，欢迎来怼

**正文**：

```
起因是 Cloudflare 上周发了 Kitesurf——一个给 AI Agent 用的浏览器。我把它发布博客读了好几遍，里面有段话：
「如果你需要真实的 TLS 指纹去跟反爬握手，或者开一个需要持久状态的登录会话——Kitesurf 还不是合适的工具，用 Chromium 吧。」
它公开承认的这三件干不了的事，恰好是我这个项目从第一天就在啃的。

项目：AginxBrowser —— 一个 Rust 二进制的浏览器引擎，给 Agent 用。
- 内置 V8，不依赖 Chromium（对比：Puppeteer 要拖 500MB Chromium）
- stealth 模式用 BoringSSL 复刻 Chrome145/Firefox133/Safari/Edge 的完整 TLS 握手，按请求切换；Cloudflare 挑战页自动等 cf_clearance
- 5 引擎聚合搜索：百度/必应/搜狗/微信/Google，中文互联网是一等公民
- 截图：纯 CPU 渲染（Blitz：Stylo+Taffy+vello_cpu），无 GPU 也能跑，附带元素坐标
- 有状态会话：cookie 注入/导出，登录 → 翻页 → 操作不断流
- 13 个 MCP 工具，HTTP + MCP 双协议，Claude Code/Cursor 一行接入

托管实例直接可用：https://browser.aginx.net
开源 Apache-2.0，可自托管：https://github.com/yinnho/aginxbrowser
一键装（skill + MCP）：npx skills add yinnho/aginxbrowser

说实话的部分：
- skills.sh 上安全审计显示 Critical Risk——文档里如实写了每条告警对应什么（eval 执行 JS、cookie 导出这些是浏览器本质，不是恶意代码）：https://github.com/yinnho/aginxbrowser/blob/main/docs/skills-sh-audit.md
- 内置渲染是 beta，复杂 CSS 是近似不是像素级；截图能用别指望 Chrome 级
- 极强风控的站（百度文库）暂时不支持

写了个发布博客对比 Kitesurf 的三件事：https://browser.aginx.net/blog/launch-zh.html
还有一篇纯 Rust 截图的技术复盘：https://browser.aginx.net/blog/rendering-zh.html

欢迎来怼技术选型、架构、或者直接试用后反馈。
```

---

## 2. Hacker News（English，Show HN）

**Title**：`Show HN: AginxBrowser – a browser for AI agents (no Chromium, real TLS fingerprints)`

**Body**：

```
Show HN: AginxBrowser — a browser for AI agents

A single Rust binary with an embedded V8 that works as a browser engine for
agents: HTTP API + MCP (13 tools), hosted at https://browser.aginx.net,
self-hostable (Apache-2.0), no Chromium.

Why it exists: Cloudflare just shipped Kitesurf, an agent browser, and their
launch post openly listed the three things it can't do — real TLS fingerprint
negotiation, persistent authenticated sessions, and (implicitly) MCP-native
tooling. Those are exactly what this project was built around.

What's inside:
- Own V8 + HTTP stack (obscura core), TLS fingerprinting via BoringSSL
  (Chrome145/Firefox133/Safari/Edge), Cloudflare challenge auto-wait
- Screenshots via Dioxus Blitz (Stylo + Taffy + vello_cpu, pure CPU, no GPU),
  with element coordinates — no Chromium anywhere
- 5-engine aggregated search (Baidu/Bing/Sogou/WeChat/Google)
- Stateful sessions: cookie inject/export, login → operate → continue
- 13 MCP tools: fetch/search/eval/click/screenshot + 9 session tools

Honest limitations:
- The Blitz renderer is beta; complex-site CSS is approximate, not
  pixel-perfect (fine for "see what's on the page")
- skills.sh flags it as Critical Risk — we documented why each flagged
  category maps to a core browser feature, not malware:
  https://github.com/yinnho/aginxbrowser/blob/main/docs/skills-sh-audit.md
- We hit and fixed a real blank-page bug upstream (blitz #636) and are pinned
  on a known-good parley revision while a CJK hang (linebender/parley#752) is
  fixed upstream. Post-mortem: https://browser.aginx.net/blog/rendering-en.html

Try it (hosted, zero setup): https://browser.aginx.net
Install for Claude Code: `claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp`
```

---

## 3. X / Twitter（English thread）

```
1/ Cloudflare shipped a browser for agents (Kitesurf) and in the launch post
publicly admitted the three things it can't do:
• real TLS fingerprints vs anti-bot
• persistent authenticated sessions
(their docs say "use Chromium" for both)

Those are exactly the three things we built AginxBrowser around. Same
lineage — both from the obscura Rust engine. Different bet: they built the
lightest browser; we built the one that gets through.

2/ What AginxBrowser does:
• TLS fingerprinting via BoringSSL — Chrome145/Firefox133/Safari/Edge, per
  request. Cloudflare "Just a moment" auto-waits cf_clearance.
• Stateful sessions — inject cookies to start logged-in, export when done.
  Login → paginate → act → act again, the chain never breaks.
• MCP-native — 13 tools, one line into Claude Code/Cursor. Not a CDP wrapper.

3/ The "no Chromium" constraint is the interesting engineering part.
Screenshots run on Dioxus Blitz (Stylo + Taffy + parley + vello_cpu) — pure
CPU, no GPU. We run the page's JS in our own V8, then hand the final DOM to
Blitz purely for layout + paint.

War stories we hit:
• blank-screen bug: upstream assumed "there's always a net provider" →
  fixed + merged (blitz #636)
• CJK hang in parley 0.11 → minimized repro, filed upstream (#752), pinned
  known-good revision

4/ Honest caveats:
• Blitz is beta — complex CSS is approximate, not pixel-perfect
• skills.sh audit shows Critical Risk — we documented exactly why (it's the
  nature of a browser, not malware)

Full post-mortem: https://browser.aginx.net/blog/rendering-en.html
Try the hosted instance: https://browser.aginx.net
Open source: https://github.com/yinnho/aginxbrowser

Humans have Chrome. Agents have AginxBrowser.
```

---

## 4. 掘金（中文长文）

**标题**：Cloudflare 做了个 Agent 浏览器，却在博客里公开承认了三件它干不了的事

**正文**：直接使用博客文章的完整版（已含 SEO 友好标题/小标题），首段前加一段导语：

```
> 上周 Cloudflare 发布了自己的 Agent 浏览器 Kitesurf，12 周做出，免费公测。
> 但读完他们的发布博客，最有意思的不是他们做了什么，而是他们公开承认的三件做不到的事——
> 恰好是我们这个开源项目从第一天就在啃的。以下是完整对比和我们的技术复盘。

[接 https://browser.aginx.net/blog/launch-zh.html 全文，可适当改写首段和结尾]

结尾加 CTA：
> 托管实例：https://browser.aginx.net（零配置，Claude Code 一行接入）
> 开源：https://github.com/yinnho/aginxbrowser
> 技术复盘（纯 Rust 截图，无 Chromium）：https://browser.aginx.net/blog/rendering-zh.html
```

---

## 发布顺序建议

1. **X 线程**（最快，先声）
2. **HN Show HN**（英文技术圈，Kitesurf 热度期 24-48h 内效果最好）
3. **V2EX**（中文开发者，热度跟着来）
4. **掘金/知乎/公众号**（长文，慢热但持久，避开 HN/V2EX 同日撞车）

> 各平台账号需要你登录发布。发之前如果想让我微调任一稿，直接说。
