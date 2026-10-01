[English](install.md) | [中文](install.zh-CN.md)

# 安装 AginxBrowser

> 这份文档是写给 **AI Agent 自己读**的。用户把下面这句话丢给 Agent，Agent 读完后能自己完成接入：
>
> ```
> 帮我安装 AginxBrowser：https://raw.githubusercontent.com/yinnho/aginxbrowser/main/docs/install.md
> ```

AginxBrowser 是为 AI Agent 设计的浏览器引擎。一个 Rust 二进制，内置 V8，不依赖 Chromium。能读网页、搜全网、截图、交互（点击/输入/滚动）。**HTTP API 就是全部接口**：装到本机、把服务跑起来，所有能力都是一条 `curl` POST 的事。

---

## 0. 前置

- 一台能跑它的机器（macOS / Linux / Windows）。
- `curl`（用来验证和首次调用）。

不需要 Node、Chromium、Docker，也不需要 API Key。

---

## 1. 安装并启动

### 方式 A：Homebrew（macOS / Linuxbrew）

```bash
brew install yinnho/aginxbrowser/aginxbrowser
aginxbrowser          # 启动 HTTP 服务，监听 0.0.0.0:8089
```

### 方式 B：一行安装器

```bash
# 先下载审查再执行——不要盲管道跑网络脚本
curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/install.sh -o install.sh
less install.sh
bash install.sh
```

自动识别平台、下载预编译二进制、SHA-256 校验、装到 `/usr/local/bin` 或 `~/.local/bin`（`AGINXBROWSER_BIN_DIR=...` 改路径、`AGINXBROWSER_VERSION=v0.5.x` 钉版本），收尾跑一次自检。

### 方式 C：手动下载预编译二进制

各 release 提供预编译二进制（macOS Apple Silicon / Linux x86_64 / Windows x86_64；Windows 资产从 v0.3.1 起提供——v0.2.10 砍掉了 Intel-macOS 资产，它烤进去的 V8 snapshot 架构不对）：

```bash
VER=v0.5.26
OS=$(uname -s); ARCH=$(uname -m)
case "$OS-$ARCH" in
  Darwin-arm64)  T=aarch64-apple-darwin ;;
  Linux-x86_64)  T=x86_64-unknown-linux-gnu ;;
  MINGW*-x86_64) T=x86_64-pc-windows-msvc ;;   # git-bash；PowerShell 用户手动挑资产
  *) echo "unsupported: $OS-$ARCH"; exit 1 ;;
esac
curl -fsSL -o aginxbrowser.tar.gz \
  "https://github.com/yinnho/aginxbrowser/releases/download/${VER}/aginxbrowser-${VER}-${T}.tar.gz"
tar xzf aginxbrowser.tar.gz && cd aginxbrowser-${VER}-${T}
./aginxbrowser   # 默认监听 0.0.0.0:8089（Windows 下是 .exe）
```

同一 release 下有对应 `.sha256` 文件可校验下载完整性。release 二进制带全量特性（stealth TLS + 截图）；`doctor` 会报编译进去的特性集。

压缩包里还带公众号文章拼装工具：`workflow/wechat-oa-post/`（`md_to_args.py` + `templates/`），在你自己的机器上把 markdown 文章拼成内置 `wechat-oa-post` flow 要的 `args_json`。

### 方式 D：源码构建

```bash
git clone https://github.com/yinnho/aginxbrowser.git
cd aginxbrowser
cargo build --release --features stealth,screenshot   # 约 4 分钟
./target/release/aginxbrowser                          # 默认监听 0.0.0.0:8089
```

---

## 2. 验证

```bash
# 服务活着吗
curl -sS http://127.0.0.1:8089/health

# 看能力清单（不触发网络抓取，秒回）
curl -sS http://127.0.0.1:8089/doctor | jq .

# 想确认抓取链路真的通？跑一次真实探活（会抓一次 example.com）
curl -sS 'http://127.0.0.1:8089/doctor?probe=true' | jq .
```

`/doctor` 返回 `capabilities`（screenshot / stealth / captcha_solver 是否可用）、`search_engines`、`endpoints`。`?probe=true` 额外跑一次真实 fetch，报 `ok` / `latency_ms`。

---

## 3. 首次调用

```bash
BASE=http://127.0.0.1:8089

# 读网页 -> markdown
curl -sS -X POST $BASE/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'

# 搜索（百度/Bing/搜狗/搜狗微信/Google 聚合）
curl -sS -X POST $BASE/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","max_results":5}'

# 多步交互：建 session，按序号操作，关闭
# 1. POST $BASE/session/create {"url":"https://site.com/login"} -> session_id
# 2. GET  $BASE/session/$ID/state                              -> [N] 序号
# 3. POST $BASE/session/$ID/input  {"index":1,"text":"user"}
# 4. POST $BASE/session/$ID/click  {"index":3}                 -> 提交
```

装好下面的 skill 后，也可以直接用自然语言使唤 Agent："帮我读一下这个网页"、"搜一下 macbook 价格"、"帮我登录这个网站并翻到第二页"。

---

## 4.（可选）装 SKILL.md，让 Agent 主动触发

API 对任何进程都可用，但 Agent 不一定知道**何时**该用。把 `SKILL.md` 放进 skills 目录，Agent 就会在"读网页/搜索/截图/交互"类任务上主动调用：

```bash
bash skill.sh   # 仓库根目录；下载 SKILL.md 并验证本机实例
# 或手动：
mkdir -p ~/.claude/skills/aginxbrowser
curl -sS https://raw.githubusercontent.com/yinnho/aginxbrowser/main/SKILL.md \
  -o ~/.claude/skills/aginxbrowser/SKILL.md
```

---

## 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | 监听地址（绑 `127.0.0.1:8089` 即只允许本机访问） |
| `AGINXBROWSER_TOKEN` | 无 | 设了之后所有路由都要求这个 bearer token |
| `AGINXBROWSER_PROXY` | 无 | 代理地址（`use_proxy:true` 时用；browser/session 页面导航遇到已知被墙域名（wikipedia.org、github.com 等）也会自动走它）。注意：`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` 这些标准代理变量引擎一律不认，代理只看 `AGINXBROWSER_PROXY` 这一个开关 |
| `CAPTCHA_SOLVER_API_KEY` | 无 | 2captcha Key，设了自动解验证码 |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` 缓存 TTL（秒），`0` 禁用 |
| `AGINXBROWSER_DOMAIN_RATE_PER_MIN` | `20` | 单域名每分钟页面上限（超限返回 429；`0` 关闭）——本工具做实时查询，不做爬虫 |
| `AGINXBROWSER_SESSION_PAGE_LIMIT` | `200` | 单个交互 session 可走的页面总数（`0` 关闭） |
| `AGINXBROWSER_STORE_SCOPE` | `global` | 本地持久缓存范围；`session` = 只留内存 |

---

## 故障排查

- **连接被拒**：服务没跑——先 `aginxbrowser` 启动，再 `curl http://127.0.0.1:8089/health`。
- **工具调不通**：先 `curl 'http://127.0.0.1:8089/doctor?probe=true'`，看 `probe.ok` 和 `probe.error`。
- **截图不可用**：`/doctor` 的 `capabilities.screenshot` 为 false，说明编译时没开 screenshot feature（源码构建加 `--features stealth,screenshot`）；先用 `fetch` 或 `/v1/scrape` 代替。
- **国外站读不到**：`fetch` / `search` 传 `use_proxy: true`（需实例设了 `AGINXBROWSER_PROXY`）。
- **被 Cloudflare 拦**：默认自动绕；仍被拦可换 `tls_fingerprint`（firefox133 / safari18 等）。
- **登录墙后的内容**：`fetch` 传 `cookies: ["name=value", ...]` 注入会话 cookie。

完整路由说明见 [API.md](https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md)。

---

© 2026 OpenCarrier · Apache-2.0 开源
