# AginxBrowser

轻量级服务端浏览器引擎，内置 Obscura 浏览器内核，用于快速页面抓取、JS 交互和聚合搜索。

## 定位

**轻量级服务端浏览器 + 原生搜索引擎**——内置 V8 引擎，支持 JS 执行、CSS 选择器、页面导航；内置 Rust 原生搜索引擎（百度/Bing/搜狗/搜狗微信/Google），聚合搜索 + 抓正文一体化。定位是**纯外挂基础设施**：作为独立服务挂在系统里，谁需要谁调，不嵌入宿主代码。

- **抓取**：渲染 JS、过风控（微信公众号免 cookie）、提取正文
- **搜索**：`/search` 原生多引擎聚合（百度/Bing/搜狗/搜狗微信/Google），并可对前 N 条结果自动抓正文，Agent 一步完成"搜→读"
- **TLS 指纹伪装**：stealth 模式使用 BoringSSL 模拟 Chrome145 指纹，绕过搜狗微信等基于 TLS 指纹的反爬检测
- **复杂场景 fallback 到 Chromium**：本 PoC 不实现 Chromium fallback，后续可在调度层根据失败类型切换

## 目录结构

```
aginxbrowser/
├── Cargo.toml
├── build.rs              # V8 snapshot 生成
├── js/
│   └── bootstrap.js      # V8 启动脚本
├── README.md
└── src/
    ├── main.rs              # HTTP 服务入口与路由
    ├── server.rs            # 业务层（fetch/click/eval/search）
    ├── browser.rs           # 顶层 API：Browser、BrowserBuilder
    ├── page.rs              # 顶层 API：Page、Element
    ├── config.rs            # BrowserConfig
    ├── cookie.rs            # CookieStore
    ├── error.rs             # Error 类型
    ├── search/              # 原生搜索引擎
    │   ├── mod.rs           #   SearchEngine trait、Registry、合并去重、CAPTCHA 暂停
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

## 运行时环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | 监听地址 |
| `AGINXBROWSER_STEALTH` | 启用 | `0` 关闭 stealth（诊断用） |
| `AGINXBROWSER_UA` | Linux Chrome145 | 伪装 UA（stealth 下应设为 macOS Chrome145 保持指纹自洽） |
| `AGINXBROWSER_ACCEPT_LANGUAGE` | `zh-CN,zh;q=0.9,en;q=0.8` | Accept-Language |
| `OBSCURA_PROXY` | 无 | 代理地址，`use_proxy:true` 时使用 |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` 缓存 TTL，`0` 禁用 |

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
./target/release/aginxbrowser
```

默认监听 `0.0.0.0:8089`，可通过 `AGINXBROWSER_BIND` 修改：

```bash
AGINXBROWSER_BIND=0.0.0.0:8090 ./target/release/aginxbrowser
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

**缓存**：`/fetch` 有进程内缓存（key 含 url/format/selector/cookies/use_proxy/max_chars），TTL 由 `AGINXBROWSER_CACHE_TTL_SECS` 控制（默认 600s，`0` 禁用）。重复抓取同一 URL 命中缓存（~0.01s vs 首次 ~1s）。

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

### POST /search

原生聚合搜索 + 可选自动抓正文。Agent 一步完成"搜→读"。

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| q | string | 是 | 搜索关键词 |
| fetch_top | usize | 否 | 对前 N 条结果抓正文，默认 `0`（只返回 title/url/snippet，毫秒级） |
| categories | string | 否 | 搜索分类，默认 `general` |
| language | string | 否 | 语言，默认 `zh-CN` |
| max_results | usize | 否 | 返回结果上限，默认 `10` |
| max_chars_per | usize | 否 | 每条正文字符截断，默认 `4000`，`0` 不限 |
| wait_secs | u64 | 否 | 抓正文时每页 JS 渲染等待秒数，默认 `3` |
| use_proxy | bool | 否 | 抓正文时是否走代理（国外站），默认 `false` |

#### 搜索引擎

内置 5 个搜索引擎，并发查询、合并去重：

| 引擎 | 分类 | HTTP 客户端 | 说明 |
|------|------|------------|------|
| Baidu | general | wreq stealth | 百度 JSON API，中国最常用 |
| Bing | general | plain reqwest | Bing HTML 解析，稳定可靠 |
| Sogou | general | plain reqwest | 搜狗通用搜索 |
| Sogou WeChat | general, news | plain reqwest | 搜狗微信搜索 |
| Google | general | wreq stealth + proxy | Google HTML 解析，国内需代理 |

- **合并去重**：多引擎返回的同一 URL（归一化后）合并为一条结果，`engines` 字段列出所有来源引擎，`score` 累加
- **CAPTCHA 暂停**：引擎触发验证码后自动暂停（搜狗微信 60 分钟，其他 30 分钟），不影响其他引擎
- **stealth 优势**：wreq 使用 BoringSSL 模拟 Chrome145 TLS 指纹，绕过基于 TLS 指纹的反爬检测

示例（纯搜索，快）：

```bash
curl -s -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","max_results":5}'
```

示例（搜索 + 抓前 3 条正文，一步到位）：

```bash
curl -s -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","fetch_top":3,"max_chars_per":2000}'
```

响应：

```json
{
  "query": "macbook 价格",
  "number_of_results": 1000,
  "results": [
    {
      "title": "MacBook Air - Apple",
      "url": "https://www.apple.com/mac/",
      "snippet": "...(搜索摘要)...",
      "engines": ["bing", "baidu"],
      "score": 8.5,
      "content": "...(正文,仅 index<fetch_top 才有,否则 null)...",
      "content_truncated": false,
      "fetch_error": null
    }
  ],
  "search_backend": "native"
}
```

- `fetch_top=0`：纯搜索，毫秒级
- `fetch_top>0`：前 N 条并发抓正文（复用 `/fetch` 的 stealth + JS 渲染），单条失败不影响其他（`fetch_error` 标记）

> 设计细节见 [`docs/search-design.md`](docs/search-design.md)。

## 错误处理

API 返回不同 HTTP 状态码区分错误类型：

| 状态码 | 场景 |
|--------|------|
| 400 | CSS 选择器语法错误 |
| 404 | 元素未找到 |
| 502 | 目标网站不可达（DNS/连接失败） |
| 504 | 请求超时 |
| 500 | 其他内部错误 |

## 作为外挂接入其他系统（以 OpenCarrier 为例）

AginxBrowser 定位是**纯外挂基础设施**——像真实浏览器一样作为独立服务挂在系统里，谁需要谁调用，不嵌入宿主代码、不污染宿主配置。同机部署一个实例（systemd 守护），所有需要"渲染 + 抓取"能力的应用共享它。

**OpenCarrier 的接入方式**：OpenCarrier 的内置 `web_fetch` 工具默认走 reqwest 直连，但微信公众号、知乎专栏等风控/JS 渲染页面只能拿到空壳。OpenCarrier 不把 AginxBrowser 写进自己的配置体系，而是读一个环境变量：

```bash
export AGINXBROWSER_URL=http://127.0.0.1:8089
```

- **未设** → `web_fetch` 纯 reqwest，行为完全不变（可随时回退）
- **设了** → `web_fetch` 内部识别到已知风控站时，调 AginxBrowser 的 `/fetch` 渲染抓取；失败自动回退 reqwest，不报错

宿主侧零新增配置字段、Agent 侧零接口变化——AginxBrowser 是一个环境变量挂上去的"浏览器外挂"。OpenCarrier 同时提供独立的 `browser_*` 工具集（`browser_navigate`/`browser_evaluate`/`browser_click`），给需要点击/滚动/执行 JS 的交互场景显式调用。

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

stealth 模式可直接抓取，**不需要 cookie**：

```bash
curl -s -X POST http://127.0.0.1:8089/eval -H 'Content-Type: application/json' -d '{
  "url": "https://mp.weixin.qq.com/s/xxxxx",
  "script": "({title:document.querySelector(\"#activity-name\")?.textContent?.trim(), body:document.querySelector(\"#js_content\")?.innerText})"
}'
```

通过 `/search` 搜索微信文章并自动抓正文（一步完成"搜→读"）：

```bash
curl -s -X POST http://127.0.0.1:8089/search -H 'Content-Type: application/json' \
  -d '{"q":"AI人工智能","categories":"news","fetch_top":3,"max_chars_per":2000}'
```

### 知乎专栏（需 cookie）

知乎专栏是公开内容（无需登录），但需要提供有效 cookie 才能访问：

```bash
./examples/zhihu.sh "<文章URL>" "<cookie值>"
```

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

| 项目 | AginxBrowser | Chromium |
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
| 内置搜索 | ✅ (5 引擎) | ❌ |

## 许可证

与 OpenCarrier 主项目保持一致。
