# 性能 / 兼容记分板（证据页）

[English](scoreboard.md) | [中文](scoreboard.zh-CN.md)

> 快照：**2026-10-19 Asia/Shanghai** · AginxBrowser **0.6.0** · `/health` commit **`0c302d9`**  
> 二进制：本机 `target/debug`（`cargo build --features stealth` + `AGINXBROWSER_BUILD_COMMIT`）— **不是** Homebrew cellar，**不是** `--release`  
> 原始回执：[`bench/scoreboard-20261019/`](../bench/scoreboard-20261019/) · [`INDEX.json`](../bench/scoreboard-20261019/INDEX.json)

本页是带提交回执的 **tip 微基准**，补充 [`bench/README.md`](../bench/README.md) 里的历史 20 页 harness（2026-08-28 / 2026-09-29），以及线级 [Stealth/TLS 对比](stealth-tls-comparison.zh-CN.md)。debug 构建上的绝对毫秒 **不是** release 营销数字。

---

## 方法

### 测什么

| 指标 | 做法 | 用意 |
|------|------|------|
| **冷启动 → 首次 `/health`** | 拉起进程并轮询到 200 | Agent 进程启动成本 |
| **热 `/health`** | ready 后 3 次采样 | 控制面开销 |
| **`POST /fetch` markdown** | `render_tier:"auto"`，`format:"markdown"`，缓存 TTL 0 | 到可用文本的墙钟时间 |
| **Peak RSS** | shortset 期间 `ps` 采样 | 单进程内存 |
| **Session snapshot** | `create` → `navigate` → `GET /snapshot` | 可行动 refs 延迟 |
| **curl / Chrome headless** | 同机基线 | 裸 HTTP vs 真 Chromium |

### 引擎

| 引擎 | 状态 | 说明 |
|------|------|------|
| **aginxbrowser 0.6.0** | **已跑** | tip `0c302d9`，stealth，端口 `18091` |
| **curl** | **已跑** | macOS curl |
| **Chrome 155 headless** | **已跑**（仅 example.com） | `--headless=new --dump-dom`，冷 profile |
| **Lightpanda** | **未跑** | 本机无二进制 — [releases](https://github.com/lightpanda-io/browser/releases) |
| **Obscura** | **未跑** | 无独立二进制；通过 [agent-browser](https://github.com/vercel-labs/agent-browser) CDP 引擎框定 |

### 限制（必读）

- **Debug profile。** 数字来自未优化的 stealth debug 构建；未编造 `--release` 成绩。
- **含网络。** 墙钟含 Asia/Shanghai 出口 RTT；只在同机列之间比。
- **不编竞品数字。** Lightpanda / Obscura 格子保持 **未跑**。
- **不含商户 / 登录生产 API。**
- **短集 ≠ 全量 harness。** 5 个 URL 来自 [`bench/pages.txt`](../bench/pages.txt)。

### 复现

```bash
AGINXBROWSER_BUILD_COMMIT=$(git rev-parse --short HEAD) cargo build --features stealth
AGINXBROWSER_CACHE_TTL_SECS=0 AGINXBROWSER_BIND=127.0.0.1:18091 \
  AGINXBROWSER_RESTORE_SESSIONS=0 ./target/debug/aginxbrowser &
curl -sS http://127.0.0.1:18091/health
```

---

## 矩阵 A — 进程与控制面

| 指标 | aginxbrowser | 回执 |
|------|--------------|------|
| 冷启动 → 首次 `/health` | **2025 ms** | [`cold-start.summary.json`](../bench/scoreboard-20261019/cold-start.summary.json) |
| 热 `/health` 中位（n=3） | **0.47 ms** | [`health-warm.summary.json`](../bench/scoreboard-20261019/health-warm.summary.json) |
| commit / stealth / TLS | `0c302d9` / stealth **true** / `chrome145` | 同上 |
| shortset 期间 Peak RSS | **58.3 MB** | [`aginx-fetch-shortset.summary.json`](../bench/scoreboard-20261019/aginx-fetch-shortset.summary.json) |

---

## 矩阵 B — Fetch → markdown（短集）

同一批 5 个公开 URL，串行，关缓存。aginx 出 markdown；curl 下的是原始 HTML 字节。

| URL | aginx ms | tier | chars | curl ms | curl ok |
|-----|----------|------|-------|---------|---------|
| `https://example.com` | **587** | http | 156 | 726 | 是 |
| `https://www.rust-lang.org/` | 2179 | http | 4916 | 4094 | 是 |
| `https://www.python.org/` | 2725 | http | 7762 | 247 | 是 |
| `https://nodejs.org/en` | 8273 | http | 4929 | 30017 | **否**（超时） |
| `https://doc.rust-lang.org/book/title-page.html` | 801 | http | 1573 | 779 | 是 |

| 汇总 | 值 |
|------|----|
| aginx 中位（5/5 成功） | **2179 ms** |
| curl 中位（仅 4/5 成功） | 753 ms |
| example.com：aginx vs Chrome `--dump-dom` | **587 ms** markdown vs **1617 ms** DOM |

**怎么读**

1. 本机 tip 上，example.com 的 `/fetch` markdown 快过 curl HTML 与冷启动 Chrome `--dump-dom`。
2. 某些静态站 curl 更快（如 python.org）——它停在字节，不做 HTML→markdown。
3. 本轮 curl 对 nodejs.org **超时**；aginx 仍返回 markdown。成功率也是记分项。
4. 不要把「5 成功的 aginx 中位」和「4 成功的 curl 中位」当成纯速度对比。

---

## 矩阵 C — Session snapshot（example.com）

navigate 与 snapshot 分开计时（首次用 `create(url)` 的探测把导航叠进了第一次 snapshot；见 `INDEX.json` caveat）。

| 步骤 | ms | 说明 |
|------|----|------|
| `POST /session/create`（无 url） | 8.6 | 空会话 |
| `POST …/navigate` → example.com | 2635 | 浏览器会话路径（不是 `/fetch` http 层） |
| `GET …/snapshot`（json） | **6.2** | 1 个元素：`Learn more` `[ref=e0]` |
| `GET …/snapshot?format=text` | 2.1 | 类 a11y 文本 |
| 热 `GET …/snapshot` | 1.6 | 第二次 json |

回执：[`aginx-snapshot-example.com.summary.json`](../bench/scoreboard-20261019/aginx-snapshot-example.com.summary.json)。

文档加载后，refs 构建是毫秒级；只读 Agent 仍应优先 `/fetch` http 层。

---

## 矩阵 D — 竞品（未跑）

| 引擎 | 状态 | 指针 |
|------|------|------|
| Lightpanda | **未跑** | [GitHub releases](https://github.com/lightpanda-io/browser/releases) |
| Obscura | **未跑** | [agent-browser](https://github.com/vercel-labs/agent-browser) CDP（`--engine obscura`） |

只公布我们自己的数字与方法，方便以后同机补列。

相关：[agent-browser 适配说明](agent-browser-engine.zh-CN.md) · [Stealth/TLS 对比](stealth-tls-comparison.zh-CN.md) · 历史 [bench/README.md](../bench/README.md)。

---

## 兼容性框定（不是合成「% 分」）

| 能力 | Aginx tip | 证据 |
|------|-----------|------|
| 本地 HTTP API（无 MCP） | 有 | [`docs/API.zh-CN.md`](API.zh-CN.md) |
| 交互式 snapshot + `ref=eN` | 有 | 矩阵 C |
| Stealth TLS 预设 | 有 | [stealth-tls-comparison](stealth-tls-comparison.zh-CN.md) |
| 对接 agent-browser 的 CDP | 仅设计稿 | [`cdp-serve-design.md`](cdp-serve-design.md) |
| 即插即用 Chromium 替代 | 否 | 有意 — HTTP 优先 |

---

## 变更记录

| 日期 | Commit | 说明 |
|------|--------|------|
| 2026-10-19 | 二进制 `0c302d9` | 首份公开 tip 记分板回执 |
