# AginxBrowser

**Agent 的浏览器。看世界，和世界交互。**

不是给人用的浏览器改吧改吧给 Agent。是从第一行代码就为 AI Agent 设计的——看世界、读世界、搜世界、操世界，一个 Rust 二进制，内置 V8，不依赖 Chromium。

> 人有 Chrome，Agent 有 AginxBrowser。

一个二进制，零依赖，启动即服务。HTTP API + MCP，Agent 拿来就能用。

## 为什么 Agent 需要专属浏览器

现有的"浏览器自动化"方案都是为人或为抓取设计的，不是为 Agent：

| | AginxBrowser | Puppeteer/Playwright | Firecrawl | Browser-use |
|---|---|---|---|---|
| 为谁设计 | **Agent-first** | 人调试 | 抓取服务 | LLM 套壳 |
| 依赖 | 单二进制，无 Chromium | Chromium ~500MB | Docker ~1GB | Chromium |
| 看得见（截图） | ✅ 内置 Blitz 渲染 | 需 Chromium | ❌ | 需 Chromium |
| 读得懂 | markdown + js_extract | 要自己写 | markdown | 要自己写 |
| 找得到（搜索） | ✅ 5 引擎聚合 | ❌ | ❌ | ❌ |
| 操得了 | session 索引化交互 | DevTools | ❌ | LLM 驱动 |
| 协议 | HTTP + MCP 原生 | Node API | HTTP | Python |
| TLS 指纹 | ✅ Chrome/Firefox/Safari | 需插件 | ❌ | 需插件 |
| CAPTCHA | ✅ 自动解决 | 需自己接 | ❌ | 需自己接 |
| 交互式 Session | ✅ | ✅ | ❌ | ✅ |

Agent 用浏览器要的是五件事：**看得见、读得懂、找得到、操得了、跑得动。** 一个二进制全包，systemd 守护，MCP 直连 Claude/Cursor，零依赖启动即服务。

**核心优势：不依赖 Chromium。** AginxBrowser 内联了 Obscura 浏览器内核（V8 + 自研 HTTP 栈 + 内置 Blitz 渲染栈），不需要 Puppeteer、不需要 Chrome、不需要 Docker。一个 Rust 二进制，systemd 守护，就是 agent 的浏览器基础设施。

## 核心能力

- **分层渲染**：静态页面纯 HTTP 直取（~100ms），需要 JS 渲染时才启动 V8（~1-2s），80% 页面加速 10x
- **5 引擎聚合搜索**：百度/Bing/搜狗/搜狗微信/Google 并发查询，合并去重，可选自动抓正文，Agent 一步完成"搜→读"
- **图片搜索**：`categories=images` 接百度图片/必应图片，返回 `image_url` 二进制直链（`curl -o` 可直接下成 jpg/png）+ `source_url` 溯源，Agent 自主补真实素材
- **交互式 Session**：持久化浏览器会话，索引化交互（state/click/input/scroll/eval），AI Agent 像人一样浏览网页
- **CAPTCHA 自动解决**：检测验证码类型，可选 2captcha 自动解决，搜索不再卡死
- **JS 数据提取**：`js_extract` 参数，从 SPA 提取 `window.__INITIAL_STATE__` 等结构化数据
- **截图渲染**：`/screenshot` 端点（`--features screenshot`），JS 渲染后的 DOM 喂给内置 Blitz 渲染栈（Stylo/Taffy/vello_cpu，纯 CPU，无 Chromium）出 PNG，agent 的视觉输入
- **Cloudflare 自动绕过**：检测 "Just a moment..." 挑战页，自动等待 `cf_clearance`
- **TLS 指纹伪装**：stealth 模式模拟 Chrome145/Firefox133/Safari/Edge，可按请求切换
- **MCP Server**：`--mcp` 模式暴露 12 个工具（fetch/eval/click/search + 8 个 session 工具），Claude Code / Claude Desktop / Cursor 直接调用
- **Firecrawl 兼容**：`/v1/scrape` 端点，现有 Firecrawl 客户端改 base URL 即可迁移
- **DNS 重绑定防护**：内置 SSRF 防护 + DNS 解析后 IP 校验

## 快速开始

```bash
# 构建
cargo build --release

# 启动服务
./target/release/aginxbrowser
# → Listening on 0.0.0.0:8089

# 验证
curl http://127.0.0.1:8089/health
# → {"status":"ok","engine":"obscura"}

# 抓取页面
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'

# 搜索
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","max_results":5}'

# 创建交互式会话
curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'
# → {"session_id":"s_1","url":"https://example.com/"}

# MCP 模式（给 AI Agent 用）
./target/release/aginxbrowser --mcp
```

## 目录结构

```
aginxbrowser/
├── Cargo.toml
├── build.rs              # V8 snapshot 生成
├── js/
│   └── bootstrap.js      # V8 启动脚本
├── README.md
├── docs/
│   └── API.md            # 完整 API 参考（HTTP + MCP）
└── src/
    ├── main.rs              # HTTP 服务���口与路由
    ├── server.rs            # 业务层（fetch/click/eval/search）
    ├── session.rs           # 交互式浏览器会话
    ├── captcha.rs           # CAPTCHA 检测与自动解决
    ├── render.rs            # 分层渲染（HTTP 直取 → obscura 浏览器）
    ├── mcp.rs               # MCP Server（12 个工具）
    ├── firecrawl_compat.rs  # Firecrawl 兼容 /v1/scrape 端点
    ├── browser.rs           # 顶层 API：Browser、BrowserBuilder
    ├── page.rs              # 顶层 API：Page、Element
    ├── config.rs            # BrowserConfig
    ├── cookie.rs            # CookieStore
    ├── error.rs             # Error 类型
    ├── search/              # 原生搜索引擎
    │   ├── mod.rs           #   SearchEngine trait、Registry、合并去重、渐进退避
    │   ├── baidu.rs         #   百度（JSON API，wreq stealth）
    │   ├── bing.rs          #   Bing（HTML 解析，plain reqwest）
    │   ├── sogou.rs         #   搜狗通用（HTML 解析，plain reqwest）
    │   ├── sogou_wechat.rs  #   搜狗微信（HTML 解析，plain reqwest + /link 解析）
    │   └── google.rs        #   Google（HTML 解析，wreq stealth + proxy）
    │
    ├── obscura_dom/         # HTML 解析、DOM 树、CSS 选择器
    ├── obscura_net/         # HTTP 客户端、Cookie、编码、代理
    ├── obscura_js/          # V8 运行时、JS ops、模块加载
    └── obscura_browser/     # 页面导航、生命周期、浏览器上下文
```

## 构建

```bash
# 普通构建（不含 stealth，TLS 指纹等功能不生效）
cargo build --release

# 含 stealth（需 go + cmake + C++ 工具链，启用 TLS 指纹伪装）
cargo build --release --features stealth

# 含截图渲染（启用 /screenshot，拉入内置 Blitz 渲染栈，二进制 +30-40MB）
cargo build --release --features screenshot

# 全功能（推荐生产部署）
cargo build --release --features stealth,screenshot
```

依赖：Rust 1.78+，首次编译自动下载 V8 静态库（需网络）。启用 stealth 需额外 `go`、`cmake`、C++ 编译器。启用 screenshot 需服务器装 CJK 字体（`fonts-noto-cjk`）以正确渲染中文。

## 运行时环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | 监听地址 |
| `AGINXBROWSER_STEALTH` | 启用 | `0` 关闭 stealth（诊断用） |
| `AGINXBROWSER_UA` | Linux Chrome145 | 伪装 UA |
| `AGINXBROWSER_ACCEPT_LANGUAGE` | `zh-CN,zh;q=0.9,en;q=0.8` | Accept-Language |
| `OBSCURA_PROXY` | 无 | 代理地址，`use_proxy:true` 时使用 |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` 缓存 TTL，`0` 禁用 |
| `CAPTCHA_SOLVER_API_KEY` | 无 | 2captcha API Key，设置后自动解决验证码 |
| `CAPTCHA_SOLVER_SERVICE` | `2captcha` | 验证码解决服务 |

## API 文档

**完整 API 参考** → [`docs/API.md`](docs/API.md)

包含：
- 所有 HTTP 端点（`/fetch`、`/click`、`/eval`、`/search`、`/v1/scrape`、8 个 Session 端点）
- MCP Server 的 12 个工具及参数
- Claude Code / Claude Desktop / Cursor 客户端配置
- 远程服务器 SSH 接入方式
- 环境变量、错误码、站点抓取示例

## 作为外挂接入其他系统

AginxBrowser 定位是**纯外挂基础设施**——像真实浏览器一样作为独立服务挂在系统里，谁需要谁调用，不嵌入宿主代码、不污染宿主配置。同机部署一个实例（systemd 守护），所有需要"渲染 + 抓取"能力的应用共享它。

接入方式：读环境变量 `AGINXBROWSER_URL=http://127.0.0.1:8089`。未设 → 行为不变；设了 → 风控站自动调 AginxBrowser 渲染抓取，失败自动回退。

## 已知限制

1. **截图需 opt-in feature**：`/screenshot` 需 `cargo build --release --features screenshot` 启用（拉入内置 Blitz 渲染栈，二进制 +30-40MB）。内置 Blitz 是 beta，复杂站点 CSS 渲染近似（非 Chromium 像素级精准），图片子资源不单独拉取
2. **无元素坐标**：只能做 JS click，不能做基于屏幕坐标的点击（`screenshot` feature 下 Blitz 内部已算出 `final_layout()` 坐标，尚未暴露给 API）
3. **JS 复杂组件可能失败**：React/Vue 事件委托可能不响应原生 `click()`
4. **代理支持**：HTTP/HTTPS/SOCKS5，通过 `OBSCURA_PROXY` 传入
5. **强风控站点**：百度文库暂不支持；知乎专栏需有效 `__zse_ck`

## ���可证

与 OpenCarrier 主项目保持一致。
