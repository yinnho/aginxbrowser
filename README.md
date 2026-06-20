# AginxBrower

轻量级服务端浏览器引擎，内置 Obscura 浏览器内核，用于快速页面抓取和 JS 交互。

## 定位

- **AginxBrower**：服务端浏览器，内置 V8 引擎，支持 JS 执行、CSS 选择器、页面导航
- **复杂场景 fallback 到 Chromium**：本 PoC 不实现 Chromium fallback，后续可在调度层根据失败类型切换

## 目录结构

```
aginxbrower/
├── Cargo.toml
├── build.rs              # V8 snapshot 生成
├── js/
│   └── bootstrap.js      # V8 启动脚本
├── README.md
└── src/
    ├── main.rs              # HTTP 服务入口与路由
    ├── server.rs            # 业务层（fetch/click/eval）
    ├── browser.rs           # 顶层 API：Browser、BrowserBuilder
    ├── page.rs              # 顶层 API：Page、Element
    ├── config.rs            # BrowserConfig
    ├── cookie.rs            # CookieStore
    ├── error.rs             # Error 类型
    │
    ├── obscura_dom/         # HTML 解析、DOM 树、CSS 选择器
    ├── obscura_net/         # HTTP 客户端、Cookie、编码、代理
    ├── obscura_js/          # V8 运行时、JS ops、模块加载
    └── obscura_browser/     # 页面导航、生命周期、浏览器上下文
```

## Features

| Feature | 默认 | 说明 |
|---------|------|------|
| `stealth` | 关闭 | TLS/JA3 指纹伪装（依赖 BoringSSL，需 `go` + C++ 工具链） |

## 依赖

- Rust 1.78+
- 首次编译会自动下载 V8 静态库（需网络，较慢）
- 启用 `stealth` feature 需额外安装 `go`、`cmake`、C++ 编译器
- 如需代理下载 V8，设置环境变量：
  ```bash
  export OBSCURA_PROXY=socks5://127.0.0.1:8800
  ```

## 构建

```bash
# 普通构建（不含 stealth）
cargo build --release --no-default-features

# 含 stealth（需 go + cmake + C++ 工具链）
cargo build --release
```

release 二进制预计在 70MB 左右。

## 运行

```bash
export OBSCURA_PROXY=socks5://127.0.0.1:8800   # 可选
./target/release/aginxbrower
```

默认监听 `0.0.0.0:8089`，可通过 `AGINXBROWER_BIND` 修改：

```bash
AGINXBROWER_BIND=0.0.0.0:8090 ./target/release/aginxbrower
```

## HTTP API

### GET /health

```bash
curl http://127.0.0.1:8089/health
```

响应：

```json
{"status":"ok","engine":"obscura"}
```

### POST /fetch

抓取页面并返回内容。

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | 是 | 目标 URL |
| format | string | 否 | `markdown` / `html` / `text`，默认 `markdown` |
| selector | string | 否 | CSS 选择器，仅提取选中区域 |
| wait_secs | u64 | 否 | 页面加载后额外等待秒数 |
| use_proxy | bool | 否 | 走 `OBSCURA_PROXY` 代理，默认 `false`（国内站点直连；国外站点设 `true`） |
| cookies | string[] | 否 | 导航前注入的 cookie（`["name=value", ...]`），用于需登录态的站点 |
| max_chars | usize | 否 | 截断 `content` 到指定字符数，默认 `50000`，`0` 不限 |

响应字段：

| 字段 | 类型 | 说明 |
|------|------|------|
| url | string | 最终 URL（重定向后） |
| title | string? | 页面标题 |
| content | string | 抓取内容（markdown/html/text） |
| truncated | bool | `content` 是否被 `max_chars` 截断 |

**缓存**：`/fetch` 有进程内缓存（key 含 url/format/selector/cookies/use_proxy/max_chars），TTL 由 `AGINXBROWER_CACHE_TTL_SECS` 控制（默认 600s，`0` 禁用）。重复抓取同一 URL 命中缓存（~0.01s vs 首次 ~1s）。

示例：

```bash
cat <<EOF | curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" -d @-
{"url":"https://github.com/trending","format":"text","selector":"article","use_proxy":true}
EOF
```

响应：

```json
{
  "url": "https://github.com/trending",
  "title": "Trending  repositories on GitHub today · GitHub",
  "content": "...",
  "truncated": false
}
```

**安全**：内置 SSRF 防护（`validate_url` 拦截非 http(s/file) scheme、私网/loopback IP），robots.txt、tracker 拦截（stealth 模式）。

### POST /click

使用 JS `element.click()` 点击指定元素。

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | 是 | 目标 URL |
| selector | string | 是 | CSS 选择器 |
| wait_secs | u64 | 否 | 页面加载后额外等待秒数 |
| use_proxy | bool | 否 | 走 `OBSCURA_PROXY` 代理，默认 `false` |
| cookies | string[] | 否 | 导航前注入的 cookie |

示例：

```bash
cat <<EOF | curl -sS -X POST http://127.0.0.1:8089/click \
  -H "Content-Type: application/json" -d @-
{"url":"https://github.com/trending","selector":"article:first-of-type h2 a"}
EOF
```

响应：

```json
{
  "url": "https://github.com/trending/",
  "selector": "article:first-of-type h2 a",
  "clicked": true,
  "text_after": "..."
}
```

### POST /eval

在页面上执行任意 JavaScript 并返回结果。

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | 是 | 目标 URL |
| script | string | 是 | JS 表达式或 async IIFE（支持 awaitPromise，可等动态渲染） |
| wait_secs | u64 | 否 | 页面加载后额外等待秒数 |
| use_proxy | bool | 否 | 走 `OBSCURA_PROXY` 代理，默认 `false` |
| cookies | string[] | 否 | 导航前注入的 cookie |

> `/eval` 支持 **async 脚本**（返回 Promise 会被 await），适合抓取 React/Vue 等动态渲染页面：`script: "(async()=>{await new Promise(r=>setTimeout(r,3000));return document.body.innerText})()"`。

示例：

```bash
cat <<EOF | curl -sS -X POST http://127.0.0.1:8089/eval \
  -H "Content-Type: application/json" -d @-
{"url":"https://example.com","script":"document.title"}
EOF
```

响应：

```json
{
  "url": "https://example.com/",
  "result": "Example Domain"
}
```

## 错误处理

API 返回不同 HTTP 状态码区分错误类型：

| 状态码 | 场景 |
|--------|------|
| 400 | CSS 选择器语法错误 |
| 404 | 元素未找到 |
| 502 | 目标网站不可达（DNS/连接失败） |
| 504 | 请求超时 |
| 500 | 其他内部错误 |

## 已知限制

## 站点抓取示例

`examples/` 下提供了针对不同风控类型站点的抓取脚本。

### 动态渲染页面（GitHub Trending 等）

JS 异步渲染的内容用 `/eval` 的 **async 脚本**（`evaluate_async` 支持 awaitPromise），等渲染完再提取：

```bash
curl -s -X POST http://127.0.0.1:8089/eval -H 'Content-Type: application/json' -d '{
  "url": "https://github.com/trending",
  "script": "(async()=>{await new Promise(r=>setTimeout(r,4000));return Array.from(document.querySelectorAll(\"article.Box-row\")).slice(0,5).map(a=>a.querySelector(\"h2 a\")?.textContent?.trim())})()",
  "use_proxy": true
}'
```

### 微信公众号文章（公开，无需登录）

stealth 指纹 + macOS UA 即可，**不需要 cookie**：

```bash
curl -s -X POST http://127.0.0.1:8089/eval -H 'Content-Type: application/json' -d '{
  "url": "https://mp.weixin.qq.com/s/xxxxx",
  "script": "({title:document.querySelector(\"#activity-name\")?.textContent?.trim(), body:document.querySelector(\"#js_content\")?.innerText})"
}'
```

### 知乎专栏（需 __zse_ck cookie）

知乎专栏是公开内容（无需登录），但知乎要求带 `__zse_ck` cookie 才放行（否则 403）。**只需 `__zse_ck` 一个 cookie，不需要 d_c0 / 登录态 / 住宅代理**；同一个 `__zse_ck` 对所有专栏文章通用，有效期约一年。正文在初始 HTML 的 `<script id="js-initialData">` JSON 里（SSR），直接解析即可，不依赖 JS 渲染，绕过 DOM 完整性问题。

```bash
./examples/zhihu.sh "<文章URL>" "<__zse_ck值>"
```

`__zse_ck` 获取（约一年一次）：浏览器打开任意知乎专栏 → F12 → Application → Cookies → `.zhihu.com` → 复制 `__zse_ck`。首次访问知乎会弹易盾验证码，人工过一次后 `__zse_ck` 长期有效。

## 已知限制

1. **无法截图**：没有 layout/paint 引擎，不支持截图。
2. **无元素坐标**：只能做 JS click，不能做基于屏幕坐标的点击。
3. **JS 复杂组件可能失败**：React/Vue 等框架的事件委托可能不响应原生 `click()`，需要针对具体站点写 JS。
4. **代理支持**：支持 HTTP/HTTPS/SOCKS5 代理，通过 `OBSCURA_PROXY` 传入；国内站点默认直连（`use_proxy:false`），国外站点请求时传 `use_proxy:true`。
5. **强风控站点**：百度文库（安全验证 + 正文图片化）暂不支持；知乎专栏需提供有效 `__zse_ck`。

## 后续优化方向

1. 增加 `POST /session` 会话保持，复用浏览器实例，减少启动开销。
2. 增加 `POST /form`：自动填充 input 并 submit。
3. 增加失败重试 + 超时细粒度控制。
4. 在调度层实现 Chromium fallback 策略。
5. 暴露 Prometheus /healthz 等运维端点。

## 与 Chromium 对比

| 项目 | AginxBrower | Chromium |
|------|------------|----------|
| 二进制体积 | ~70MB | ~256MB+ |
| 启动速度 | 快 | 慢 |
| 截图 | ❌ | ✅ |
| 坐标点击 | ❌ | ✅ |
| JS click / scraping | ✅ | ✅ |
| 复杂 SPA 兼容 | 中等 | 高 |
| 代理 | ✅ | ✅ |
| Cookie 持久化 | ✅ | ✅ |
| TLS 指纹伪装 | ✅ (stealth) | ✅ |

## 许可证

与 OpenCarrier 主项目保持一致。
