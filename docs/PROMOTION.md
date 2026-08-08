# Agent 的浏览器，看世界和世界交互

> 人有 Chrome，Agent 有 AginxBrowser。

## 一个尴尬的现状

你在用 Claude、GPT、Cursor，让 Agent 帮你查资料、订机票、填表单。Agent 很聪明，但它是个瞎子——它看不见网页长什么样，只能拿到一堆 markdown 文字；它也摸不着网页，想点个按钮得靠你喂坐标或者套一层 LLM 去"猜"。

现在的"浏览器自动化"方案，要么是 Puppeteer/Playwright（给人调试用的，要装 Chromium，500MB 起步），要么是 Firecrawl（抓取服务，只会读不会操作），要么是 Browser-use（给 LLM 套个 Chromium 壳，重且贵）。

**没有一个是从第一行代码就为 Agent 设计的浏览器。**

Agent 用浏览器到底要什么？想清楚这件事，会发现就五件事：

- **看得见** —— 能截图，把页面变成视觉输入
- **读得懂** —— 能把页面变成结构化文字/markdown
- **找得到** —— 能搜索，不用人喂 URL
- **操得了** —— 能点击、输入、滚动，像人一样交互
- **跑得动** —— 轻量、稳定、好部署，不是动不动 Docker 起一堆

## AginxBrowser 的回答

一个 Rust 二进制，内置 V8，**不依赖 Chromium**。systemd 守护，HTTP API + MCP，启动即服务。

```
./aginxbrowser
→ Listening on 0.0.0.0:8089
```

五感，全包：

### 看得见 —— 截图，但不用 Chromium

`POST /screenshot` 把页面 JS 渲染后的 DOM 画成 PNG。关键在于：**没有 Chromium**。

我们内联了一个纯 Rust 渲染栈（Blitz：Servo 的 Stylo 做 CSS、Taffy 做布局、vello_cpu 做绘制），CPU 渲染，服务器无显存也能跑。Agent 拿到 base64 PNG，直接当多模态视觉输入，或者 `data:image/png;base64,...` 嵌进自己的上下文。

```bash
curl -sS -X POST http://127.0.0.1:8089/screenshot \
  -H "Content-Type: application/json" \
  -d '{"url":"https://cn.bing.com/search?q=蔚来ES8内饰","full_page":true}' \
  | jq -r .image_base64 | base64 -d > cabin.png
```

这里有个真实的故事。Blitz 是 beta，我们一开始喂它真实站点（百度、GitHub），出来的截图全白——一片空白。排查到最后，根因是 Blitz 假设"总会有网络层去拉 CSS/字体"，遇到我们这种"V8 渲染完直接喂 DOM、不拉子资源"的集成就卡死了：head 里的 stylesheet 永远等不到加载完成的回调，paint 阶段直接跳过整页。

我们 fork 了 Blitz，一行 patch 修掉——当网络层是空操作时，不把 stylesheet 标记成"阻塞渲染的关键资源"。修完，百度从 1 种颜色（全白）变成 1653 种颜色，搜索框、结果列表全出来了。**已经提了 PR 给上游。**

这就是"在他的基础上再研发"——上游做通用渲染器，我们做 agent 视角的集成，盲区只有我们这种用法才会撞到，自己修。

### 读得懂 —— markdown + JS 提取

`/fetch` 默认返回 markdown，Agent 直接消化。分层渲染：静态页面纯 HTTP 直取（~100ms），需要 JS 渲染才启动 V8（~1-2s），80% 页面加速 10 倍。

遇到 SPA，`js_extract` 直接抠 `window.__INITIAL_STATE__` 这类结构化数据，不用正则苦哈哈地解析。

### 找得到 —— 5 引擎聚合搜索

`/search` 并发查百度/Bing/搜狗/搜狗微信/Google，合并去重，可选自动抓前 N 条正文。Agent 一步完成"搜→读"。

```bash
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"蔚来ES8 酒红内饰 后排视角","categories":"images","max_results":10}'
```

`categories=images` 还能图搜，返回图片二进制直链（`curl -o` 直接下成 jpg/png），不是图片所在网页的 URL。Agent 能自己找真实素材补材料。

### 操得了 —— 索引化 Session

持久化浏览器会话，Agent 像人一样：打开页面 → 看状态 → 点击/输入 → 拿结果。

```
[0] <a href="/home">Home</a>
[1] <input type=email placeholder=Email />
[2] <input type=password placeholder=Password />
[3] <button id=submit>Sign In</button>
```

索引化交互——Agent 不用猜屏幕坐标，`click index:3` 就点登录。比 LLM 盲点坐标靠谱得多。

### 跑得动 —— 单二进制，MCP 直连

一个二进制 ~70MB（含截图功能 ~104MB），不需要 Node、不需要 Chromium、不需要 Docker。systemd 守护，就是基础设施。

`--mcp` 模式直接暴露 12 个工具，Claude Code / Claude Desktop / Cursor 配一下就能调，不用写 HTTP 客户端：

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "command": "/path/to/aginxbrowser",
      "args": ["--mcp"]
    }
  }
}
```

## 不依赖 Chromium，为什么是核心

这不是"我们更轻"的体量炫耀，是架构选择。

Chromium 是为人设计的浏览器，它要处理 GPU 合成、音频、扩展、多进程沙箱——这些 Agent 一个都用不到，但每一个都是部署负担和故障面。Agent 要的是"把页面变成我能消化的输入"，不是"复刻一个人的浏览器"。

所以 AginxBrowser 的内核（Obscura）只做 agent 要的：HTTP 抓取（带 TLS 指纹伪装、stealth）、V8 跑 JS、DOM 操作、布局绘制。砍掉一切人用浏览器才需要的重量。

代价是诚实说的：复杂 CSS 渲染是近似的（Blitz beta，非 Chromium 像素级精准），截图里的 `<img>` 不单独拉（风控站图片可能缺）。但 agent 看个页面布局、读个文字、定位个按钮，够用——而且省掉 400MB 依赖和一堆 Docker 配置。

## 谁该用

- **做 Agent 应用的** —— 给你的 agent 配一双眼睛和一双手，HTTP 或 MCP 接入
- **做 RAG / 知识抓取的** —— 分层渲染 + 搜索聚合，比 Firecrawl 轻、比 Puppeteer 简单
- **做 LLM 工具链的** —— 一个 MCP 配置，Claude/Cursor 直接有浏览器能力
- **受够了 Chromium 部署的** —— 单二进制 systemd，同机部署，所有应用共享

## 接入

```bash
# 全功能构建
cargo build --release --features stealth,screenshot
./target/release/aginxbrowser

# 验证
curl http://127.0.0.1:8089/health
# → {"status":"ok","engine":"obscura"}
```

GitHub：https://github.com/yinnho/aginxbrowser

完整 API 文档见 `docs/API.md`。

---

Agent 互联网正在成形。Agent 要互相协作、要访问世界、要被人访问。

AginxBrowser 是其中"看世界"这一层——给每一个 Agent 装上眼睛和手。

> 人有 Chrome，Agent 有 AginxBrowser。
