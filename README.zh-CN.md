# AginxBrowser

**Agent 的浏览器。看 live web，读它，操作它，记住它。**

[![skills.sh](https://skills.sh/b/yinnho/aginxbrowser)](https://skills.sh/yinnho/aginxbrowser)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![MCP](https://img.shields.io/badge/MCP-compatible-brightgreen)](https://browser.aginx.net/mcp)
[![Hosted](https://img.shields.io/badge/hosted-browser.aginx.net-4dd0ff)](https://browser.aginx.net/)

[English](README.md) | **中文文档**

不是给人用的浏览器改吧改吧给 Agent 用。是从第一行代码就为 AI Agent 设计的——看世界、读世界、搜世界、操作世界，还要记得住读过的东西：一个 Rust 二进制，内置 V8，不依赖 Chromium。

> 人有 Chrome，Agent 有 AginxBrowser。

一个二进制，零依赖，启动即服务。HTTP API + MCP + CDP 三种协议——Agent 拿来就能用，已有的 Playwright / Puppeteer / browser-use 代码一行直连。

*下面这些真实页面，都是 AginxBrowser 的 diting 引擎渲染的（无 Chromium）——Wikipedia、本仓库、Rust 官网。想亲自截图？[看这里](docs/API.md#screenshot)*

![AginxBrowser 渲染真实页面](docs/demo.gif)

## 为什么 Agent 需要专属浏览器

同一批 20 个页面、同一网络环境下对着 headless Chrome 实测（[bench](bench/README.md)，2026-08-28）：出可用文本**快 7.6 倍**（p50 532 ms vs 4 053 ms），**内存省约 10 倍**（整个进程 227 MB vs Chrome 每页 ~2.1 GB），Chrome `--dump-dom` 40 次加载 5 次连 DOM 都没出来，我们 0 次硬失败。Agent 的总成本 = 浏览器效率 × 模型效率，这是浏览器那一半。

现有的"浏览器自动化"方案都是为人或为一次性抓取设计的，不是为 Agent：

| | AginxBrowser | Puppeteer/Playwright | Firecrawl | Browser-use |
|---|---|---|---|---|
| 为谁设计 | **Agent-first** | 人调试 | 抓取服务 | LLM 套壳 |
| 依赖 | 单二进制，无 Chromium | Chromium ~500MB | Docker ~1GB | Chromium |
| 看得见（截图） | ✅ 内置 diting 渲染引擎 | 需 Chromium | ❌ | 需 Chromium |
| 读得懂 | markdown + js_extract + fetch 回执 | 要自己写 | markdown | 要自己写 |
| 找得到（搜索） | ✅ 15 引擎 7 分类聚合 | ❌ | ❌ | ❌ |
| 操得了 | session 索引化交互 | DevTools API | ❌ | LLM 驱动 |
| 记得住 | ✅ 本地 fetch/搜索缓存（SQLite FTS5） | ❌ | 爬虫缓存 | ❌ |
| 协议 | HTTP + MCP 原生 + CDP | Node API | HTTP | Python |
| TLS 指纹 | ✅ Chrome/Firefox/Safari/Edge | 需插件 | ❌ | ❌ |
| CAPTCHA | ✅ 识别 + 自动等待 + 可选 2captcha | 要自己接 | ❌ | ❌ |
| 交互式 Session | ✅ 持久化 | ✅ | ❌ | ✅ |

Agent 用浏览器要的是五件事：**看得见、读得懂、找得到、操得了、记得住。** 一个二进制全包，systemd 守护，MCP 直连 Claude/Cursor，零依赖启动即服务。

**核心优势：不依赖 Chromium。** AginxBrowser 内联了完整的浏览器引擎（V8 + Rust HTTP 栈 + 自有的 diting CSS/布局/绘制渲染引擎，以 Blitz/Stylo/Taffy 谱系为参照实现），不需要 Puppeteer、不需要 Chrome、不需要 Docker。一个 Rust 二进制挂 systemd，就是 agent 的浏览器基础设施。

## 三件事：无状态渲染器干不了

刚冒出来的「agent 浏览器」大多是**无状态、无指纹**的一次性渲染引擎——抓公开页很轻，碰上 Cloudflare 或要登录的站就死。AginxBrowser 走相反的路：

- **🔐 真实 TLS 指纹** — stealth 模式用 BoringSSL 复刻 Chrome145 / Firefox133 / Safari / Edge 的完整 TLS 握手（不是只改 UA），可按请求切换；Cloudflare Turnstile 挑战页自动等 `cf_clearance`。无指纹引擎碰反爬就是 403，我们穿过去。
- **🤝 有状态交互 Session** — 持久化浏览器会话（8 分钟空闲保活），登录态可注入可导出（`session_create(cookies=...)` ↔ `session_cookies`），跨翻页、跨多步流程不断。一次性引擎抓完即弃，做不了「登录 → 操作 → 再操作」。
- **🔌 MCP 原生** — 17 个工具是一等公民（不是 CDP 套壳），Claude Code / Cursor / Claude Desktop 一行接入。HTTP + MCP 双协议之外还有 CDP 桥，DevTools 生态照样能用。

> 参照：Cloudflare 的 Kitesurf 明确不做真实 TLS 指纹协商、不做持久认证会话——反爬与登录正是 AginxBrowser 的地盘。

Apache-2.0 开源、单二进制，想自托管现在就能跑，不锁任何云。

## 每次 fetch 都是一张回执

Agent 是照着浏览器说的话行事的，所以响应里要写清楚实际发生了什么，而不是只给个 "200"：

- **`tier`** — 页面是哪条路出的：纯 HTTP（~100 ms）还是 V8 渲染的浏览器层。Agent 能看懂这次 fetch 为什么快、为什么慢。
- **`redirected_from`** — 完整重定向轨迹。`redirected_from[0]` 是你要的 URL，`url` 是内容实际来自的 URL——请求地址配实际地址，每一跳都看得见。
- **`content_hash` + `changed_since_prev`** — 每次 fetch 都算哈希；同一 URL 前后两次采样可以 diff。被限频的源连续好几天吐同一个 200 冻结体，读出来就是 `changed_since_prev: false`——最便宜的测谎器。
- **`captcha_event`** — 碰到挑战页时（配了解算器就含解算结果），响应里写明，而不是把挑战页当内容递给你。

[本地缓存](#核心能力)是同一个思路的延伸：搜索命中带 `[§ 标题]` 小节前缀，Agent 知道命中落在页面哪个位置；排序把关键词相关性跟新鲜度融合起来。

## 核心能力

- **分层渲染**：静态页面纯 HTTP 直取（~100ms），需要 JS 渲染才启动 V8（~1-2s）——[bench](bench/README.md) 页面集里 90% 根本不用拉起 V8；每次响应带 `tier` 字段说明走的哪层
- **多引擎聚合搜索**：通用网页（百度/Bing/搜狗/搜狗微信/Google/DuckDuckGo）、新闻（Bing News）、代码（Stack Overflow/GitHub）、包（npm/PyPI）、学术（arXiv）、AI 模型（Hugging Face）——15 引擎 7 分类，并发查询、合并去重；运维还可把私有 Meilisearch 索引接入同一 `/search`。Agent 一步完成"搜→读"
- **图片搜索**：`categories=images` 接百度图片/必应图片，返回 `image_url` 二进制直链（可直接下成 jpg/png）+ `source_url` 溯源
- **交互式 Session**：持久化浏览器会话，索引化交互（state/click/input/scroll/eval），Agent 像人一样浏览；`session_export` 把 Agent 摸索出来的操作导出成能直接跑的 curl 回放脚本，重放零模型 token
- **CDP 桥**：`/json/version` + `/devtools/{kind}/{id}` WebSocket——Playwright / Puppeteer / browser-use 的 `chromium.connectOverCDP()` 一行接入（[集成指南](docs/integrations.md)）。兼容 DevTools 生态，但自己不做 CDP 套壳
- **文件下载**：流式落盘（不吃内存）、SHA-256 校验、断点续传——二进制、压缩包、数据集用这个
- **记得住本地缓存**：每次 fetch/搜索自动进 SQLite（FTS5），落 `~/.aginxbrowser/cache.db`。`cache` 工具从 Agent 已读过的内容里找答案，不再重付网络时间：全文检索支持中文逐字匹配、关键词×新鲜度融合排序、`[§ 标题]` 小节感知摘要、每 URL 内容哈希测漂移、TTL 有界、共享部署可按 session 隔离
- **CAPTCHA 处理**：类型识别 + Cloudflare 挑战自动等待 + 可选 2captcha 解算，搜索不卡验证页
- **JS 数据提取**：`js_extract` 参数，从 SPA 提 `window.__INITIAL_STATE__` 等结构化数据
- **截图渲染**：`/screenshot` 端点（`--features screenshot`），JS 渲染后的 DOM 用自有的 diting 引擎出 PNG——纯 CPU，无 Chromium，agent 的视觉输入
- **TLS 指纹伪装**：stealth 模式模拟 Chrome145/Firefox133/Safari/Edge，可按请求切换
- **MCP Server**：`--mcp` 模式暴露 17 个工具（fetch/eval/click/search/download/cache + session + 截图工具），Claude Code / Claude Desktop / Cursor 直接调用
- **Firecrawl 兼容**：`/v1/scrape` 端点，现有 Firecrawl 客户端改 base URL 即可迁移
- **DNS 重绑定防护**：内置 SSRF 防护 + 解析后 IP 校验

## 是浏览器，不是爬虫

AginxBrowser 干的是**实时��信息**：agent 带着问题来，读几页，拿着答案走。它不是爬虫工具，而且产品形态上就让它变不成爬虫：

- **robots.txt 默认不查。** RFC 9309 解析器是内置的，但实时取信息不是爬虫，不默认守爬虫的规矩；想守的运维设 `AGINXBROWSER_HONOR_ROBOTS=1` 自行打开。
- **没有"抓全站"的 API。** 没有 crawl 端点，没有链接递归——每一页都是 agent 明确要的那一页。
- **内置限额。** 单域名每分钟 20 页；每个交互 session 一共 200 页。`AGINXBROWSER_DOMAIN_RATE_PER_MIN` / `AGINXBROWSER_SESSION_PAGE_LIMIT` 可调（`0` 关闭，自己的实例自己定）。agent 啃文档站、跑后台足够宽裕；一页接一页遍历的爬虫套路、包括换子域名轮着爬（同一个注册域，共用一个额度），直接卡死。
- **托管实例（browser.aginx.net）限额更紧。** 所有用户共用一个出口 IP，让站点对这个 IP 保持好感是服务的一部分。想要不一样的数字，自己部署。
- 想批量爬站？请用爬虫工具。这里不是，以后也不会是。

## 适合做什么

不是 demo，是真实有人在用 agent 浏览器干的事：

- **啃烂后台** — AWS / App Store Connect / Google Play，点穿几十层菜单才干完一件事。让 agent 代你点，到要授权时再回来确认。
- **登录后批量操作** — 往购物车加一打商品、翻历史订单找小票、查只对登录态开放的页面。注入 cookie 即用，操作完导出复用。
- **穿反爬站点** — Cloudflare 防护、Turnstile 挑战、TLS 指纹检测，stealth 模式硬穿，不是碰 403 就退。
- **中国互联网** — 百度 / 搜狗 / 微信公众号多引擎聚合搜索，中文页正确渲染，不只懂英文 web。
- **现场写脚本** — 让 agent 看页面、写一段 JS、`eval` 执行——高亮对比表、重排内容、按隐藏参数过滤商品。GreaseMonkey-on-steroids。
- **多模态视觉** — 截图当视觉输入，做「看图判断」的流程：找好座位、辨认页面布局、确认渲染对不对。

## 快速开始

想先体验？直接用托管实例 **https://browser.aginx.net/**。

**一键全装**（SKILL.md 触发面 + MCP 工具 + 验活）：

```bash
# 下载 -> 先看一眼内容 -> 确认无误再执行（不要盲跑网络脚本）
curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/skill.sh -o skill.sh
less skill.sh
bash skill.sh
```

**只注册 MCP**：

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

**通过 [skills.sh](https://www.skills.sh) 装触发面**：

```bash
npx skills add yinnho/aginxbrowser
```

自己部署：

```bash
# macOS / Linux 用 Homebrew
brew install yinnho/aginxbrowser/aginxbrowser
aginxbrowser doctor   # 特性 + 字体 + 出口自检

# Docker（Docker Hub，GHCR 有镜像）
docker run -p 8089:8089 yinnho/aginxbrowser:latest
# （或 ghcr.io/yinnho/aginxbrowser:latest）

# 或预编译二进制（平台识别 + sha256 校验 + 镜像回退 + doctor 自检）
# 稳妥起见：下载 -> 看一眼 -> 再跑（不要盲跑网络脚本）
curl -fsSL https://browser.aginx.net/install.sh -o install.sh
less install.sh && bash install.sh
# 信得过仓库直接来也行：
#   curl -fsSL https://browser.aginx.net/install.sh | sh
# GitHub 慢/被墙？AGINXBROWSER_GH_PROXY=https://ghfast.top/ bash install.sh
aginxbrowser doctor   # 特性 + 字体 + 出口自检

# 或源码构建（--features stealth,screenshot，不带会掉这两项能力）
cargo build --release --features stealth,screenshot

# 启动服务
./target/release/aginxbrowser
# → Listening on 0.0.0.0:8089

# 验证
curl http://127.0.0.1:8089/health
# → {"status":"ok","engine":"diting"}

# 抓页面
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'

# 搜索
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","max_results":5}'

# 建交互式会话
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
│   ├── API.md            # 完整 API 参考（HTTP + MCP）
│   └── integrations.md   # CDP 桥：Playwright / Puppeteer / browser-use
├── bench/                # 基准测试（对 headless Chrome）
│   ├── README.md         #   方法论 + 数字
│   ├── pages.txt         #   固定 20 页集合
│   ├── run.py            #   跑测脚本
│   ├── summarize.py      #   TSV → 结果表
│   └── results/          #   原始数据
└── src/
    ├── main.rs              # HTTP 服务入口与路由
    ├── server.rs            # 业务层（fetch/click/eval/search）
    ├── session.rs           # 交互式浏览器会话
    ├── mcp.rs               # MCP Server（17 个工具）
    ├── render.rs            # 分层渲染（HTTP 直取 → diting 浏览器引擎）
    ├── store.rs             # 本地 fetch/搜索缓存（SQLite FTS5、漂移哈希）
    ├── download.rs          # 流式文件下载（sha256、断点续传）
    ├── robots.rs            # RFC 9309 robots.txt 解析器（opt-in 门）
    ├── rate.rs              # 单域名 + 单 session 限额
    ├── captcha.rs           # CAPTCHA 识别与自动解算
    ├── firecrawl_compat.rs  # Firecrawl 兼容 /v1/scrape 端点
    ├── diting_cdp/          # CDP 桥（DevTools HTTP + WebSocket）
    ├── doctor_cli.rs        # `aginxbrowser doctor` 自检
    ├── browser.rs           # 顶层 API：Browser、BrowserBuilder
    ├── page.rs              # 顶层 API：Page、Element
    ├── config.rs            # BrowserConfig
    ├── cookie.rs            # CookieStore
    ├── error.rs             # Error 类型
    ├── search/              # 15 个原生搜索引擎，7 个分类
    │   ├── mod.rs           #   SearchEngine trait、Registry、合并去重、渐进退避
    │   ├── baidu.rs         #   百度（JSON API，wreq stealth）
    │   ├── baidu_images.rs  #   百度图片（acjson API，images 类）
    │   ├── bing.rs          #   Bing（HTML 解析，plain reqwest）
    │   ├── bing_images.rs   #   必应图片（images/async 端点，images 类）
    │   ├── bing_news.rs     #   必应新闻 RSS（news 类；走代理）
    │   ├── sogou.rs         #   搜狗通用（HTML 解析，plain reqwest）
    │   ├── sogou_wechat.rs  #   搜狗微信（HTML 解析 + /link 解析）
    │   ├── duckduckgo.rs    #   DuckDuckGo（html.duckduckgo.com，general 类；直连优先）
    │   ├── google.rs        #   Google（HTML 解析，wreq stealth + proxy）
    │   ├── stackexchange.rs #   Stack Overflow（SE API v2.3，code 类）
    │   ├── github_repos.rs  #   GitHub 仓库（api.github.com，code 类）
    │   ├── arxiv.rs         #   arXiv（Atom API，academic 类）
    │   ├── huggingface.rs   #   HF 模型/数据集/Spaces（ai 类）
    │   ├── npm.rs           #   npm 包（npms.io API，packages 类）
    │   ├── pypi.rs          #   PyPI 包名解析（JSON API，packages 类）
    │   └── meilisearch.rs   #   私有索引适配器（env 配置）
    │
    ├── diting_dom/          # HTML 解析、DOM 树、CSS 选择器
    ├── diting_css/          # CSS 解析 + 级联
    ├── diting_net/          # HTTP 客户端、Cookie、编码、代理
    ├── diting_js/           # V8 运行时、JS ops、模块加载
    ├── diting_layout/       # 基于 Taffy 的布局、浮动、命中测试
    ├── diting_fonts/        # 内置 CJK 字体子集、回退
    └── diting_browser/      # 页面导航、生命周期、浏览器上下文
```

## 构建

```bash
# 普通构建（不含 stealth，TLS 指纹等功能不生效）
cargo build --release

# 含 stealth（需 go + cmake + C++ 工具链，启用 TLS 指纹伪装）
cargo build --release --features stealth

# 含截图渲染（启用 /screenshot，加入渲染栈，二进制 +30-40MB）
cargo build --release --features screenshot

# 全功能（推荐生产部署）
cargo build --release --features stealth,screenshot
```

依赖：Rust 1.78+，首次编译自动下载 V8 静态库（需网络）。启用 stealth 需额外 `go`、`cmake`、C++ 编译器。启用 screenshot 自带 CJK 字体子集（GB2312 + 常用符号）——正确渲染中文无需服务器装任何字体。

## 运行时环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | 监听地址 |
| `AGINXBROWSER_STEALTH` | 启用 | `0` 关闭 stealth（诊断用） |
| `AGINXBROWSER_UA` | Linux Chrome145 | 伪装 UA |
| `AGINXBROWSER_ACCEPT_LANGUAGE` | `zh-CN,zh;q=0.9,en;q=0.8` | Accept-Language 头 |
| `AGINXBROWSER_PROXY` | 无 | 可选回退代理。被墙源引擎（Google/Bing News/Hugging Face）先直连、失败才走此代理——海外部署无需配置；单次请求也可传 `use_proxy:true` 走代理。浏览器/session/CDP 导航到已知被墙域名（wikipedia.org、github.com 等）会自动走代理。引擎故意无视标准 `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`（给别的工具设没问题），启动时见到会打警告 |
| `AGINXBROWSER_NAV_CHAIN_LIMIT` | `10` | JS 导航链上限：页面经 `location`/表单连跳多少个文档后导航中止。计数含最初文档（10 = 首文档 + 9 跳）。���法长链（跨提供商 SSO 跳转）可调高；HTTP 3xx 重定向单独算额度（20，按 Fetch spec/浏览器对齐） |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` 进程内缓存 TTL，`0` 禁用 |
| `AGINXBROWSER_HONOR_ROBOTS` | 未设 | `/fetch`、`/screenshot`、`/download` 和 MCP 工具默认不查 robots.txt；设 `1` 打开（运维自选） |
| `AGINXBROWSER_ROBOTS_TTL_SECS` | `3600` | 每主机 robots.txt 策略缓存 TTL |
| `AGINXBROWSER_DOMAIN_RATE_PER_MIN` | `20` | 单注册域每分钟页面数上限（子域名共额度���，超限返回 429；`0` 关闭。见「是浏览器，不是爬虫」 |
| `AGINXBROWSER_SESSION_PAGE_LIMIT` | `200` | 单个交互 session 可走的页面总数上限（换页的点击也计），超限后续导航被拒，当前页仍可操作；`0` 关闭 |
| `AGINXBROWSER_MCP_ALLOWED_HOSTS` | 无 | `/mcp` 额外放行的 `Host`（逗号分隔）。传输层的 DNS 重绑定防护默认只认回环地址，局域网 IP 或 Docker 主机名调用本实例时需加上 |
| `AGINXBROWSER_STORE` | 开 | 本地 fetch/搜索缓存；`0`/`false`/`off` 关闭 |
| `AGINXBROWSER_STORE_PATH` | `~/.aginxbrowser/cache.db` | SQLite 数据库位置（0600 权限创建） |
| `AGINXBROWSER_STORE_TTL_HOURS` | `720` | 缓存页面 TTL |
| `AGINXBROWSER_STORE_SEARCH_TTL_HOURS` | `168` | 缓存搜索结果集 TTL |
| `AGINXBROWSER_STORE_SCOPE` | `global` | `session` 让每个 MCP 客户端会话有独立缓存作用域——公共多客户端部署设这个 |
| `CAPTCHA_SOLVER_API_KEY` | 无 | 2captcha API Key，设置后自动解算验证码 |
| `CAPTCHA_SOLVER_SERVICE` | `2captcha` | 验证码解算服务 |
| `AGINXBROWSER_MEILI_URL` | 无 | Meilisearch 地址；设置后启用私有索引引擎 |
| `AGINXBROWSER_MEILI_INDEX` | 无 | 要查询的 Meilisearch index |
| `AGINXBROWSER_MEILI_KEY` | 无 | 可选 Bearer key |

## API 文档

**完整 API 参考** → [`docs/API.md`](docs/API.md)
**CDP 集成指南** → [`docs/integrations.md`](docs/integrations.md) — Playwright / Puppeteer / browser-use 一行接入
**安全审计说明** → [`docs/skills-sh-audit.md`](docs/skills-sh-audit.md) — 为什么 skills.sh 上显示 Critical Risk，每条告警对应的真实产品功能

包含：
- 全部 25 个 HTTP 端点（`/fetch`、`/search`、`/screenshot`、`/download`、`/v1/scrape`、`/doctor`、11 个 session 端点、CDP 发现、MCP 传输）
- MCP Server 的 17 个工具及参数
- Claude Code / Claude Desktop / Cursor 客户端配置
- 环境变量、错误码、站点抓取示例

## 作为外挂接入其他系统

AginxBrowser 定位是**纯外挂基础设施**——像真实浏览器一样作为独立服务挂在系统里，谁需要谁调用，不嵌入宿主代码、不污染宿主配置。同机部署一个实例（systemd 守护），所有需要"渲染 + 抓取"能力的应用共享它。

三个接入口：

- **HTTP** — `/fetch`、`/search`、`/screenshot`、`/download`，任何有 HTTP 客户端的语言都能调
- **MCP** — 一行接进 Claude Code / Cursor / Claude Desktop（见上）
- **CDP** — 把 Playwright / Puppeteer / browser-use 指到 `ws://your-host:8089/devtools/browser/<id>`；见 [`docs/integrations.md`](docs/integrations.md)

集成方式：读环境变量 `AGINXBROWSER_URL=http://127.0.0.1:8089`。未设 → 行为不变；设了 → 风控站自动调 AginxBrowser 渲染抓取，失败自动回退。

## 已知限制

1. **截图需 opt-in feature**：`/screenshot` 需 `cargo build --release --features screenshot` 启用（加入渲染栈，二进制 +30-40MB）。默认渲染引擎为 diting（自有 CSS+布局+绘制栈）；传 `engine: "blitz"` 可切回 Blitz 参照管线。两者对复杂站点 CSS 均为近似渲染（非 Chromium 像素级精准）
2. **元素坐标已支持**：`/screenshot` 带 `selector` 返回元素页面坐标（`selector_rects`，CSS px），`selector` 单独传就直接截该元素。默认 diting 引擎下纯行内元素（`<a>文字</a>`）也有矩形——是行内内容拍平后的并集，按元素自身 `line-height` 撑到行盒高，与 Chrome 对只含替换元素的行内（`<a><img></a>`）的报告方式一致（返回行盒高而不是图片高）。空行内元素仍无矩形——选块级祖先
3. **JS 交互已大幅可用，重指纹页仍可能失败**：React/Vue 事件委托已正常（`src`/`href` 等 URL 反射属性解析绝对 URL 后，Next.js/webpack 能完成 hydration，点击可触发 handler）。但 WorkOS/Cloudflare 认证页等重指纹站点会探测 `navigator.plugins`、WebGL canvas 等，在 stealth 指纹补齐前仍可能崩
4. **代理支持**：HTTP/HTTPS/SOCKS5，通过 `AGINXBROWSER_PROXY` 配置
5. **强风控站点**：百度文库不支持；知乎专栏文章需有效 `__zse_ck`

## 许可证

Apache-2.0。
