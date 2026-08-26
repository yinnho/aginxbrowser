[English](install.md) | [中文](install.zh-CN.md)

# 安装 AginxBrowser

> 这份文档是写给 **AI Agent 自己读**的。用户把下面这句话丢给 Agent，Agent 读完后能自己完成接入：
>
> ```
> 帮我安装 AginxBrowser：https://raw.githubusercontent.com/yinnho/aginxbrowser/main/docs/install.md
> ```

AginxBrowser 是为 AI Agent 设计的浏览器引擎。一个 Rust 二进制，内置 V8，不依赖 Chromium。能读网页、搜全网、截图、交互（点击/输入/滚动）。

**本项目运行着一个公网托管实例**，绝大多数情况你不需要自己部署--一行命令接上即可。

---

## 0. 前置

- 一个支持 MCP HTTP transport 的客户端：Claude Code / Claude Desktop / Cursor / Windsurf 等。
- 能访问 `https://browser.aginx.net`（公网）。

不需要 Node、Chromium、Docker，也不需要 API Key。

---

## 1. 一行接入（托管实例，推荐）

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

如果是其他客户端，等价配置（写入对应的 settings 文件）：

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "type": "http",
      "url": "https://browser.aginx.net/mcp"
    }
  }
}
```

---

## 2. 验证

```bash
# 看到 aginxbrowser 在列表里
claude mcp list

# 看托管实例的能力清单（不触发网络抓取，秒回）
curl -sS https://browser.aginx.net/doctor | jq .

# 想确认抓取链路真的通？跑一次真实探活（会抓一次 example.com）
curl -sS 'https://browser.aginx.net/doctor?probe=true' | jq .
```

`/doctor` 返回 `capabilities`（screenshot / stealth / captcha_solver 是否可用）、`search_engines`、`endpoints`。`?probe=true` 额外跑一次真实 fetch，报 `ok` / `latency_ms`。

---

## 3. 首次调用

接上 MCP 后，直接让 Agent 用自然语言调，或显式调工具：

- "帮我读一下这个网页：https://example.com" → `fetch`
- "搜一下 macbook 价格" → `search`
- "截个图看看这个页面长啥样" → `screenshot`（需托管实例开了 screenshot feature；`/doctor` 会告诉你）
- "把这个 zip / 这个 pdf 下载保存" → `download`（流式写盘、SHA-256 校验、断点续传）
- "帮我登录这个网站并翻到第二页" → `session_create` + `session_state` + `session_click`/`session_input`

HTTP API 也能直接调（不走 MCP）：

```bash
curl -sS -X POST https://browser.aginx.net/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'
```

---

## 4.（可选）装 SKILL.md，让 Agent 主动触发

MCP 接上后工具就可用，但 Agent 不一定知道**何时**该用。把仓库根的 `SKILL.md` 放进 skills 目录，Agent 就会在"读网页/搜索/截图/交互"类任务上主动调用：

```bash
mkdir -p ~/.claude/skills/aginxbrowser
curl -sS https://raw.githubusercontent.com/yinnho/aginxbrowser/main/SKILL.md \
  -o ~/.claude/skills/aginxbrowser/SKILL.md
```

---

## 5.（可选）自己部署

托管实例够用就跳过这步。

### 方式 A：一行安装器（最快）

```bash
# 先下载审查再执行——不要盲管道跑网络脚本
curl -fsSL https://raw.githubusercontent.com/yinnho/aginxbrowser/main/install.sh -o install.sh
less install.sh
bash install.sh
```

自动识别平台、下载 v0.2.0+ 预编译二进制、SHA-256 校验、装到 `~/.local/bin`（`PREFIX=...` 改路径、`VERSION=v0.2.0` 钉版本），收尾跑一次自检：

```bash
aginxbrowser doctor   # 编译特性 + 内置字体 + 环境态势 + 一次出口探针
```

### 方式 A′：手动下载预编译二进制

v0.2.0 提供三平台预编译二进制（macOS Apple Silicon / macOS Intel / Linux x86_64）：

```bash
VER=v0.2.0
OS=$(uname -s); ARCH=$(uname -m)
case "$OS-$ARCH" in
  Darwin-arm64) T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64) T=x86_64-unknown-linux-gnu ;;
  *) echo "unsupported: $OS-$ARCH"; exit 1 ;;
esac
curl -fsSL -o aginxbrowser.tar.gz \
  "https://github.com/yinnho/aginxbrowser/releases/download/${VER}/aginxbrowser-${VER}-${T}.tar.gz"
tar xzf aginxbrowser.tar.gz && cd aginxbrowser-${VER}-${T}
./aginxbrowser   # 默认监听 0.0.0.0:8089
```

同一 release 下有对应 `.sha256` 文件可校验下载完整性。

### 方式 B：源码构建

```bash
git clone https://github.com/yinnho/aginxbrowser.git
cd aginxbrowser
cargo build --release --features stealth,screenshot   # 约 4 分钟
./target/release/aginxbrowser                          # 默认监听 0.0.0.0:8089
```

环境变量：

| 变量 | 默认 | 说明 |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | 监听地址（公网部署建议绑 127.0.0.1 + nginx 反代） |
| `AGINXBROWSER_PROXY` | 无 | 代理地址（`use_proxy:true` 时用，抓国外站） |
| `CAPTCHA_SOLVER_API_KEY` | 无 | 2captcha Key，设了自动解验证码 |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` 缓存 TTL（秒），`0` 禁用 |

---

## 能力清单（14 个 MCP 工具）

| 工具 | 用途 |
|------|------|
| `fetch` | 读网页 → markdown/html/text（分层渲染、stealth、js_extract） |
| `search` | 多引擎聚合搜索（百度/Bing/搜狗/搜狗微信/Google），可图搜 |
| `eval` | 在页面执行 JS（支持 async/Promise） |
| `click` | 加载页面并点击 CSS 选择器 |
| `download` | HTTP(S) 流式下载到磁盘（SHA-256 校验、断点续传）— 二进制/压缩包/数据集 |
| `session_create` | 创建持久交互会话（多步登录/填表/翻页），支持 `cookies` 注入登录态 |
| `session_navigate` / `session_state` / `session_click` / `session_input` / `session_scroll` / `session_eval` / `session_cookies` / `session_close` | 会话操作（`session_cookies` 导出登录态复用） |

完整字段说明见 [API.md](https://github.com/yinnho/aginxbrowser/blob/main/docs/API.md)。

---

## 故障排查

- **工具调不通**：先 `curl https://browser.aginx.net/doctor?probe=true`，看 `probe.ok` 和 `probe.error`。
- **截图不可用**：`/doctor` 的 `capabilities.screenshot` 为 false，说明托管实例没开 screenshot feature；用 `fetch` 或 `/v1/scrape` 代替。
- **国外站读不到**：`fetch` / `search` 传 `use_proxy: true`。
- **被 Cloudflare 拦**：默认自动绕；仍被拦可换 `tls_fingerprint`（firefox133 / safari18 等）。
- **登录墙后的内容**：`fetch` 传 `cookies: ["name=value", ...]` 注入会话 cookie。

---

© 2026 OpenCarrier · Apache-2.0 开源 · 托管于 browser.aginx.net
