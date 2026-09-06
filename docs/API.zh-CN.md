# AginxBrowser API 参考

[English](API.md) | [中文](API.zh-CN.md)

> 完整的 HTTP API + MCP Server 接入文档。5 分钟快速接入。

## 快速开始

```bash
# 构建并启动
cargo build --release
./target/release/aginxbrowser

# 验证服务
curl http://127.0.0.1:8089/health
# → {"status":"ok","engine":"diting"}

# 抓取页面
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'

# 创建交互式会话
curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'
```

---

## HTTP API

默认监听 `0.0.0.0:8089`，可通过 `AGINXBROWSER_BIND` 环境变量修改。

### GET /health

健康检查。

```bash
curl http://127.0.0.1:8089/health
```

响应：

```json
{"status":"ok","engine":"diting"}
```

---

### POST /fetch

抓取页面并返回内容。支持分层渲染、Cloudflare 自动绕过、TLS 指纹切换、JS 数据提取。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | ✅ | — | 目标 URL |
| format | string | | `"markdown"` | 输出格式：`markdown` / `html` / `text` |
| selector | string | | `null` | CSS 选择器，仅提取匹配区域 |
| wait_secs | u64 | | `null` | 页面加载后额外等待秒数（等 JS 渲染完成） |
| use_proxy | bool | | `false` | 走 `AGINXBROWSER_PROXY` 代理。国外站点设 `true` |
| cookies | string[] \| object[] | | `[]` | 导航前注入的 cookie：`"name=value"` 字符串（可带 `; Domain=…; Path=/; Secure` 属性）或 CDP 风格对象 `{"name","value","domain","path","secure","httpOnly","sameSite"}`。带 `Domain=` 的条目锚定在它自己声明的域上，跨子域登录态（`.taobao.com` / `.tmall.com` 这种）注入时不会再被 RFC 6265 域校验悄悄丢掉 |
| max_chars | usize | | `50000` | 截断 `content` 到指定字符数。`0` 不限 |
| auto_bypass_challenge | bool | | `true` | 自动检测并绕过 Cloudflare Turnstile 挑战 |
| render_tier | string | | `"auto"` | 渲染策略（见下方说明） |
| tls_fingerprint | string | | `null` | TLS 指纹（stealth 模式），见下方说明 |
| js_extract | object | | `null` | JS 数据提取（见下方说明） |

**render_tier 选项：**

| 值 | 说明 |
|----|------|
| `auto` | HTTP 直取优先，内容不足时自动回退浏览器（**推荐**，默认） |
| `http` | 纯 HTTP，不走浏览器。最快但拿不到 JS 渲染内容 |
| `obscura` | 强制走 obscura 浏览器渲染。最慢但最可靠 |

**tls_fingerprint 选项（需 `--features stealth`）：**

| 值 | 说明 |
|----|------|
| `null` | 默认 Chrome145 |
| `"chrome145"` | Chrome 145 |
| `"firefox133"` | Firefox 133 |
| `"firefox147"` | Firefox 147 |
| `"safari17_5"` | Safari 17.5 |
| `"safari18"` | Safari 18 |
| `"safari26"` | Safari 26 |
| `"edge145"` | Edge 145 |

**js_extract 格式：**

```json
{
  "expression": "JSON.stringify(window.__INITIAL_STATE__)",
  "timeout_ms": 5000
}
```

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| expression | string | — | JS 表达式，在页面上下文中执行 |
| timeout_ms | u64 | `5000` | 等待非 null 结果的超时时间（毫秒） |

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| url | string | 最终 URL（重定向后） |
| title | string? | 页面标题 |
| content | string | 抓取内容（markdown/html/text） |
| truncated | bool | `content` 是否被 `max_chars` 截断 |
| js_extract_result | any? | JS 提取结果（仅 `js_extract` 非空时有值） |
| captcha_event | object? | CAPTCHA 事件（仅检测到验证码时有值；识别 Cloudflare/Google/Baidu 挑战页，以及淘宝/天猫风控信号——`punish` 跳转、`x5sec`、MTop `FAIL_SYS_USER_VALIDATE`/`RGV587` 应答，即使 HTTP 200 也会透出） |

**captcha_event 格式：**

| 字段 | 类型 | 说明 |
|------|------|------|
| engine | string | 触发 CAPTCHA 的搜索引擎名（`/fetch` 时为空） |
| captcha_type | string | `cloudflare_turnstile` / `recaptcha_v2` / `hcaptcha` / `slider` / `unknown` |
| url | string | 触发 CAPTCHA 的 URL |
| auto_solve_attempted | bool | 是否尝试了自动解决 |
| auto_solve_succeeded | bool | 自动解决是否成功 |

**示例 — 基础抓取：**

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com"}'
```

```json
{
  "url": "https://example.com/",
  "title": "Example Domain",
  "content": "# Example Domain\n\nThis domain is for use in illustrative examples...",
  "truncated": false
}
```

**示例 — 提取 SPA 结构化数据：**

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://spa-site.example.com",
    "js_extract": {
      "expression": "JSON.stringify(window.__INITIAL_STATE__)",
      "timeout_ms": 3000
    }
  }'
```

**示例 — 提取特定区域（CSS 选择器）：**

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://github.com/trending","format":"text","selector":"article","use_proxy":true}'
```

**缓存**：`/fetch` 有进程内缓存（key 含 url/format/selector/cookies/use_proxy/max_chars/render_tier/tls_fingerprint），TTL 由 `AGINXBROWSER_CACHE_TTL_SECS` 控制（默认 600s，`0` 禁用）。重复抓取同一 URL 命中缓存（~0.01s vs 首次 ~1s）。

**安全**：内置 SSRF 防护（拦截非 http(s) scheme、私网/loopback IP）、DNS 重绑定防护、robots.txt 遵守、tracker 拦截（stealth 模式）。

---

### POST /click

加载页面并点击指定元素（`element.click()`），返回点击后的页面文本。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | ✅ | — | 目标 URL |
| selector | string | ✅ | — | CSS 选择器 |
| wait_secs | u64 | | `null` | 页面加载后额外等待秒数 |
| use_proxy | bool | | `false` | 走代理 |
| cookies | string[] \| object[] | | `[]` | 导航前注入的 cookie（`"name=value"` 字符串或 CDP 风格对象，语义同 `/fetch`） |
| tls_fingerprint | string | | `null` | TLS 指纹（stealth 模式） |

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| url | string | 最终 URL |
| selector | string | 使用的选择器 |
| clicked | bool | 是否成功点击 |
| text_after | string? | 点击后的页面文本 |

**示例：**

```bash
curl -sS -X POST http://127.0.0.1:8089/click \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com","selector":"a"}'
```

---

### POST /eval

在页面上执行任意 JavaScript 并返回结果。支持 `async`/`Promise`。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | ✅ | — | 目标 URL |
| script | string | ✅ | — | JS 表达式或 async IIFE |
| wait_secs | u64 | | `null` | 页面加载后额外等待秒数 |
| use_proxy | bool | | `false` | 走代理 |
| cookies | string[] \| object[] | | `[]` | 导航前注入的 cookie（`"name=value"` 字符串或 CDP 风格对象，语义同 `/fetch`） |
| tls_fingerprint | string | | `null` | TLS 指纹（stealth 模式） |

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| url | string | 最终 URL |
| result | any | JS 执行结果 |

> `/eval` 的 `script` 参数支持 **async 函数**：返回 Promise 会被自动 await。适合 React/Vue 等动态渲染页面——等渲染完成再提取数据。

**示例 — async 脚本（等动态渲染）：**

```bash
curl -sS -X POST http://127.0.0.1:8089/eval \
  -H "Content-Type: application/json" \
  -d '{
    "url":"https://github.com/trending",
    "script":"(async()=>{await new Promise(r=>setTimeout(r,4000));return Array.from(document.querySelectorAll(\"article.Box-row\")).slice(0,5).map(a=>a.querySelector(\"h2 a\")?.textContent?.trim())})()",
    "use_proxy":true
  }'
```

---

### POST /search

原生聚合搜索 + 可选自动抓正文。Agent 一步完成"搜→读"。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| q | string | ✅ | — | 搜索关键词 |
| fetch_top | usize | | `0` | 对前 N 条结果抓正文。`0` = 只返回摘要 |
| categories | string | | `"general"` | 搜索分类，逗号分隔：`general` / `images` / `news`。`images` 返回图片直链 |
| language | string | | `"zh-CN"` | 语言 |
| max_results | usize | | `10` | 返回结果上限 |
| max_chars_per | usize | | `4000` | 每条正文字符截断。`0` 不限 |
| wait_secs | u64 | | `3` | 抓正文时每页 JS 渲染等待秒数 |
| use_proxy | bool | | `false` | 抓正文时是否走代理（国外站） |

**内置搜索引擎：**

| 引擎 | 分类 | HTTP 客户端 | 说明 |
|------|------|------------|------|
| Baidu | general | wreq stealth | 百度 JSON API |
| Bing | general | plain reqwest | Bing HTML 解析 |
| Sogou | general | plain reqwest | 搜狗通用搜索 |
| Sogou WeChat | general, news | plain reqwest | 搜狗微信搜索 |
| Google | general | wreq stealth + proxy | Google HTML 解析，国内需代理 |
| Baidu Images | images | wreq stealth | 百度图片 `acjson` JSON |
| Bing Images | images | plain reqwest | Bing 图片 `images/async` |

多引擎并发查询，结果合并去重：同一 URL（归一化后）合并为一条，`engines` 列出来源引擎，`score` 累加。

**CAPTCHA 渐进退避**：引擎触发验证码后自动暂停，暂停时长随连续触发次数递增（5min → 10min → 30min → 1h），成功搜索后重置。设置 `CAPTCHA_SOLVER_API_KEY` 环境变量后可自动解决验证码。

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| query | string | 搜索关键词 |
| number_of_results | usize | 结果总数 |
| results | array | 结果列表 |
| captcha_events | array | CAPTCHA 事件列表 |

**results 内每条：**

| 字段 | 类型 | 说明 |
|------|------|------|
| title | string | 标题 |
| url | string | 链接 |
| snippet | string | 搜索摘要 |
| engines | string[] | 来源引擎 |
| score | float | 综合得分 |
| content | string? | 正文（仅 `fetch_top` 范围内有值） |
| content_truncated | bool | 正文是否被截断 |
| fetch_error | string? | 抓正文失败原因 |
| image_url | string? | 图片二进制直链（`curl -o` 可直接下成 jpg/png）。仅 `categories=images` |
| source_url | string? | 图片所在网页 URL（溯源/版权） |
| width | u32? | 图片宽度（px） |
| height | u32? | 图片高度（px） |

> `categories=images` 时，`url` 字段等于 `image_url`（图片直链，便于直接下载）；`snippet` 为空。百度图片优先返回 `objURL`（原图，最高清），拿不到则回退 CDN 代理直链。

**示例 — 搜索 + 抓前 3 条正文：**

```bash
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"macbook 价格","fetch_top":3,"max_chars_per":2000}'
```

**示例 — 图片搜索（返回直链，curl 可直接下载）：**

```bash
curl -sS -X POST http://127.0.0.1:8089/search \
  -H "Content-Type: application/json" \
  -d '{"q":"蔚来ES8 酒红内饰 后排视角","categories":"images","max_results":10}'
```

```json
{
  "query": "蔚来ES8 酒红内饰 后排视角",
  "number_of_results": 20,
  "results": [
    {
      "title": "蔚来ES8 酒红内饰后排实拍",
      "url": "https://n.sinaimg.cn/.../img.jpg",
      "engines": ["baidu_images"],
      "score": 20.0,
      "image_url": "https://n.sinaimg.cn/.../img.jpg",
      "source_url": "https://auto.sina.com.cn/...",
      "width": 1920,
      "height": 1080
    }
  ]
}

# 下载图片
curl -sL -o cabin_ref.jpg "<image_url>"
```

---

### POST /download

把文件从 URL 流式下载到磁盘。与 `/fetch`（返回可读的页面内容）不同，`/download` 保存原始字节——适用于二进制、压缩包、数据集、文档。响应体逐 chunk 落盘（不占内存缓冲），SHA-256 同步增量计算，一次调用即可校验完整性。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | ✅ | — | 文件 URL（仅 `http`/`https`） |
| filename | string | | 自动 | 输出文件名。自动解析顺序：`Content-Disposition` 头 → URL 路径尾段 → `"download"` |
| resume | bool | | `false` | 本地存在未完成文件时续传。通过 `Range: bytes=N-` 探测服务端支持：`206` 追加，`200` 重下 |
| use_proxy | bool | | `false` | 走代理（github.com 等已知被墙域名自动启用） |
| cookies | string[] \| object[] | | `[]` | 随请求发送的 cookie（`["name=value", ...]` 或 CDP 风格对象），用于登录态下载 |

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| url | string | 重定向后的最终 URL |
| path | string | 完成文件的绝对路径 |
| filename | string | 解析出的文件名 |
| size_bytes | u64 | 本次调用写入的字节数（追加只计追加部分） |
| content_type | string? | 响应 Content-Type |
| sha256 | string | 完整文件内容的 SHA-256 |
| resumed | bool | 是否通过 Range/206 续传了已有部分文件 |

**行为说明：**

- 文件落在 `AGINXBROWSER_DOWNLOAD_DIR`（默认当前目录）。下载中数据写入 `<filename>.part`，成功后重命名。
- 与 `/fetch` 相同的 SSRF 策略：环回/私网/链路本地目标默认拒绝，需 `AGINXBROWSER_ALLOW_PRIVATE_NETWORK=1` 放行。
- 重定向最多跟 20 跳，每跳重新过 SSRF 校验。
- 30 秒无数据即中止（防死连接挂起）。单次调用硬上限 4 GB。
- 文件名经过清洗（剥离路径穿越、限长）。

**示例 —— 下载并校验：**

```bash
curl -sS -X POST http://127.0.0.1:8089/download \
  -H "Content-Type: application/json" \
  -d '{"url":"https://github.com/obsidianmd/obsidian-releases/releases/download/v1.5.3/Obsidian-1.5.3-macOS.dmg","resume":true}'
```

### POST /screenshot

把页面 JS 渲染后的 DOM 渲染成 PNG 截图（base64 返回）。**需 `--features screenshot` 构建**（默认不含，见构建章节）。

不走 `/fetch` 的分层渲染——始终驱动 obscura 浏览器跑完 JS，再喂给内置 Blitz 渲染栈（Stylo + Taffy + vello_cpu，纯 CPU，无 Chromium）。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | ✅ | — | 目标 URL |
| width | u32 | | `1280` | 视口宽度（CSS px） |
| height | u32 | | `800` | 视口高度（CSS px；`full_page` 时仅作下限） |
| scale | f32 | | `1.0` | 设备像素比，调高更清晰但 PNG 更大 |
| full_page | bool | | `true` | 截完整滚动页（跟踪内容高度，上限 16000px） |
| wait_secs | u64 | | `null` | 加载后额外等待秒数（等 JS 渲染） |
| selector | string | | `null` | CSS 选择器，截**指定元素区域**而非整页（见下） |
| selector_all | bool | | `false` | 配合 `selector`：不裁剪，返回**所有匹配**的坐标 |
| use_proxy | bool | | `false` | 走 `AGINXBROWSER_PROXY` 代理 |
| cookies | string[] \| object[] | | `[]` | 导航前注入的 cookie（`"name=value"` 字符串或 CDP 风格对象，语义同 `/fetch`） |
| tls_fingerprint | string | | `null` | TLS 指纹（stealth 模式） |

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| url | string | 最终 URL（重定向后） |
| title | string? | 页面标题 |
| width | u32 | 实际渲染的 PNG 像素宽度（`full_page` 跟踪内容高度、`selector` 裁剪时与请求值不同） |
| height | u32 | 实际渲染的 PNG 像素高度 |
| image_base64 | string | PNG 的 base64 编码。`base64 -d` 解码，或 `data:image/png;base64,...` 直接用 |
| format | string | 固定 `"png"` |
| selector_rects | object[]? | 仅当请求带 `selector` 时出现。每个元素 `{x, y, width, height}`，**CSS px，页面左上角为原点**（不是视口坐标） |

**selector 模式（元素级截图 + 坐标）：**

- `selector` + `selector_all=false`（默认）：图像裁剪到第一个匹配元素的边框盒，`selector_rects` 恰好一项（即裁剪区域）。
- `selector` + `selector_all=true`：图像照常整页渲染，`selector_rects` 返回**每个匹配**的坐标，agent 可以只读坐标不要图。
- 坐标来自 Blitz 布局后的 `final_layout`（Taffy 边框盒），沿布局树累加得到页面绝对坐标。

> ⚠️ **行内元素限制**：纯文字行内元素（如 `<a>文字</a>`）没有独立的 Taffy 盒子，crop 模式会报错提示选块级祖先；`selector_all` 模式下返回 `0x0`。含块级/替换内容的行内元素（`<a><img>` 等）会回退到后代盒子的并集。选择器选**块级容器**（div/section/li 等）坐标可靠。

**示例 — 截百度搜索：**

```bash
curl -sS -X POST http://127.0.0.1:8089/screenshot \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.baidu.com/s?wd=蔚来ES8","full_page":true,"wait_secs":2}' \
  | jq -r .image_base64 | base64 -d > baidu.png
```

**示例 — 截第一个搜索结果 + 拿所有结果坐标：**

```bash
# 只截 #content_left 下第一个 .result
curl -sS -X POST http://127.0.0.1:8089/screenshot \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.baidu.com/s?wd=蔚来ES8","selector":"#content_left .result"}' \
  | jq -r .image_base64 | base64 -d > first-result.png

# 不要图，只要 9 条结果的页面坐标
curl -sS -X POST http://127.0.0.1:8089/screenshot \
  -H "Content-Type: application/json" \
  -d '{"url":"https://www.baidu.com/s?wd=蔚来ES8","selector":"#content_left .result","selector_all":true,"full_page":false,"width":100,"height":100}' \
  | jq -c '.selector_rects'
# [{"x":150,"y":2843,"width":608,"height":153}, {"x":150,"y":3016,"width":608,"height":69}, ...]
```

```json
{
  "url": "https://www.baidu.com/s?wd=蔚来ES8",
  "title": "蔚来ES8_百度搜索",
  "width": 1280,
  "height": 800,
  "image_base64": "iVBORw0KGgo...",
  "format": "png"
}
```

> 截图是 agent 的"视觉输入"——但内联 Blitz 是 beta，复杂站点的 CSS 渲染近似（非 Chromium 像素级精准）。图片等子资源不单独拉取（截图里 `<img>` 可能缺），文字和布局可靠。

---

### POST /v1/scrape（Firecrawl 兼容）

[Firecrawl](https://github.com/mendableai/firecrawl) 兼容端点。现有 Firecrawl 客户端只需改 base URL 即可迁移。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | ✅ | — | 目标 URL |
| formats | string[] | | `["markdown"]` | 输出格式：`["markdown"]` / `["html"]` / `["markdown","html"]` |
| onlyMainContent | bool | | `false` | 仅主内容（接受参数，暂未实现） |
| waitFor | u64 | | `null` | 等待 JS 渲染毫秒数 |
| timeout | u32 | | `null` | 超时（毫秒，接受参数） |
| actions | object[] | | `[]` | 抓取前动作（见下方） |
| selector | string | | `null` | CSS 选择器 |
| tls_fingerprint | string | | `null` | TLS 指纹（stealth 模式） |

**actions 格式：**

```json
[
  {"type": "click", "selector": "button.accept"},
  {"type": "wait", "milliseconds": 1000}
]
```

| type | 字段 | 说明 |
|------|------|------|
| `click` | `selector` | 点击元素（锚点链接会导航到目标页） |
| `wait` | `milliseconds` | 等待指定毫秒 |
| `screenshot` | — | 截图渲染后的页面，返回 base64 data-URI（需 `screenshot` feature） |
| `scroll` | — | 滚动页面 |
| `writeText` | `text`, `selector` | 向匹配元素输入文本 |
| `pressKey` | `key` | 按下按键（Enter 会提交所在 GET 表单） |

带任意 `actions` 时，`/v1/scrape` 走**单页会话流程**：导航一次 → 按序执行动作 → 从该页面的最终状态提取。所有动作作用于同一页面。请求里带 `screenshot` 动作（或 `formats` 含 `"screenshot"`）时，响应 `data.screenshot` 返回 `data:image/png;base64,...` 形式的截图；未启用 `screenshot` feature 时该字段省略。

**响应（Firecrawl 格式，成功/失败均返回 HTTP 200）：**

```json
{
  "success": true,
  "data": {
    "markdown": "...",
    "html": "...",
    "metadata": {
      "title": "Example Domain",
      "sourceURL": "https://example.com/",
      "description": "...",
      "statusCode": 200
    }
  }
}
```

---

## Session API（交互式浏览器会话）

持久化浏览器会话，支持索引化交互。每个会话有独立的 V8 运行时 + 页面上下文，8 分钟无操作自动回收。

适合 AI Agent 像"人"一样浏览网页：打开页面 → 查看状态 → 点击/输入 → 获取结果。

### POST /session/create

创建交互式浏览器会话。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | | `null` | 初始 URL（可选） |
| use_proxy | bool | | `false` | 走代理 |
| cookies | string[] | | `[]` | 导航前注入的 cookie（`["name=value",...]`），让会话创建即登录态 |
| persistent | bool | | `false` | 登录态落盘：会话闲置过期甚至服务重启后，下一次调用同一个 `session_id` 会带着登录态原地复活（`session/{id}/close` 会删掉快照，空闲过期则保留） |

**响应：**

```json
{"session_id": "s_1", "url": "https://example.com/"}
```

### POST /session/{id}/navigate

导航到新 URL。

**请求字段：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | ✅ | 目标 URL |

**响应：**

```json
{"url": "https://example.com/page2", "title": "Page 2"}
```

### POST /session/{id}/state

获取当前页面状态，返回索引化的交互元素列表。

**响应格式（紧凑文本）：**

```
url=https://example.com/login
title=Login
viewport=1280x800

[0] <a href="/home" rect=[24,16,52x19]>Home</a>
[1] <input type=email placeholder=Email rect=[24,60,232x22] />
[2] <input type=password placeholder=Password rect=[24,100,232x22] />
[3] <button id=submit rect=[24,140,88x28]>Sign In</button>
[4] <a href="/forgot" rect=[120,144,110x19]>Forgot password?</a>
```

索引号 `[N]` 用于 `click` / `input` 操作。`rect=[x,y,w,h]` 是元素相对当前视口的坐标（y 随滚动变化）——用它判断元素是否在视口内、需要先 `scroll` 再 `click`。

### POST /session/{id}/click

按索引点击交互元素。

**请求字段：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| index | usize | ✅ | 元素索引（从 `/state` 获取） |

**响应：**

```json
{"url": "https://example.com/dashboard", "clicked": true}
```

### POST /session/{id}/input

按索引在输入框中填入文本。

**请求字段：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| index | usize | ✅ | 元素索引 |
| text | string | ✅ | 要输入的文本 |

**响应：**

```json
{"filled": true}
```

### POST /session/{id}/scroll

滚动页面。

**请求字段：**

| 字段 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| direction | string | | `"down"` | `up` 或 `down` |
| amount | u32 | | `3` | 滚动视口高度数 |

**响应：**

```json
{"scrolled": true}
```

### POST /session/{id}/eval

在会话中执行 JavaScript。

**请求字段：**

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| script | string | ✅ | JS 代码（支持 async） |

**响应：**

```json
{"result": "..."}
```

### POST /session/{id}/close

关闭会话，释放资源。对 persistent 会话，这一步会顺带删掉落盘的登录快照——空闲过期会保留快照，显式关闭不会。

**响应：**

```json
{"ok": true}
```

### GET /session/{id}/cookies

导出会话的 cookie，完整 Set-Cookie 形式（`["name=value; Domain=example.com; Path=/", ...]`，标志位都带）。用于把登录态持久化——存下来，下次 `session_create` 传 `cookies` 直接以登录态起会话，不用重新登录。导出用完整形式而不是裸 `name=value`，是为了跨子域登录态能扛住一个来回：`.taobao.com` 的 cookie 回灌时重新锚定在它自己的域上；裸键值对只会绑在你恰好打开的那个页面上。

**响应：**

```json
{"url": "https://example.com/dashboard", "cookies": ["sessionid=abc123; Domain=example.com; Path=/", "csrftoken=xyz; Domain=example.com; Path=/; HttpOnly"]}
```

**登录态复用闭环：**

```bash
# 1. 正常登录一个会话（session_create -> input -> click）
# 2. 导出 cookie
curl -sS http://127.0.0.1:8089/session/$SID/cookies | jq -r .cookies[]

# 3. 下次直接带 cookie 建会话，免登录
curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/dashboard","cookies":["sessionid=abc123; Domain=example.com; Path=/"]}'
```

> 🔒 托管实例**不落盘**任何 cookie——cookie 只在会话内存里，会话 8 分钟空闲回收即清。登录态由调用方自己持有（建议用小号，别用主账号）。

### POST /session/{id}/clone

从现役会话派生一个新会话，完整带走登录态——cookie、`localStorage`/`sessionStorage`、视口设置、弹窗策略、代理和 keepalive 开关——原会话原样不动。危险操作前先克隆存档，或同一登录态并行开多个会话。以前手工「`session_cookies` 导出 → `session_create` 回灌」的路子，手一滑把好的登录态改坏过；这条路不再需要。

**响应：**

```json
{"session_id": "s_2", "cloned_from": "s_1", "url": "https://example.com/dashboard", "viewport": {"width": 390, "height": 844, "mobile": true}, "expires_in_secs": 431}
```

`viewport` 为 `null` 表示源会话没设过视口。

### Session 使用示例

```bash
# 1. 创建会话
SID=$(curl -sS -X POST http://127.0.0.1:8089/session/create \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com/login"}' | jq -r .session_id)

# 2. 查看页面状态
curl -sS -X POST http://127.0.0.1:8089/session/$SID/state

# 3. 输入用户名
curl -sS -X POST http://127.0.0.1:8089/session/$SID/input \
  -H "Content-Type: application/json" \
  -d '{"index":1,"text":"user@example.com"}'

# 4. 输入密码
curl -sS -X POST http://127.0.0.1:8089/session/$SID/input \
  -H "Content-Type: application/json" \
  -d '{"index":2,"text":"mypassword"}'

# 5. 点击登录
curl -sS -X POST http://127.0.0.1:8089/session/$SID/click \
  -H "Content-Type: application/json" \
  -d '{"index":3}'

# 6. 查看登录后状态
curl -sS -X POST http://127.0.0.1:8089/session/$SID/state

# 7. 关闭会话
curl -sS -X POST http://127.0.0.1:8089/session/$SID/close
```

---

## CAPTCHA 自动解决

当搜索引擎或目标网站触发验证码时，AginxBrowser 会：

1. **检测** CAPTCHA 类型（Cloudflare Turnstile、reCAPTCHA v2、hCaptcha、滑动验证码）
2. **上报** `captcha_event` 字段，让调用方知情
3. **自动解决**（如果设置了 `CAPTCHA_SOLVER_API_KEY` 环境变量）

**配置：**

```bash
# 设置 2captcha API Key
export CAPTCHA_SOLVER_API_KEY=your_api_key_here

# 可选：切换验证码解决服务（默认 2captcha）
export CAPTCHA_SOLVER_SERVICE=2captcha
```

设置后，`/fetch` 和 `/search` 遇到验证码会自动提交到 2captcha 并注入 token，无需手动干预。

---

## MCP Server

AginxBrowser 将核心操作包装为 MCP（Model Context Protocol）Server，AI Agent 可直接调用，无需手写 HTTP 客户端。支持两种接入方式：

- **stdio**：`--mcp` 模式，本地/自部署，通过 stdin/stdout 通信
- **streamable HTTP**：HTTP Server 自带 `/mcp` 端点，公网可直接访问（托管实例开箱即用）

### 启动方式

**方式一：托管实例（无需部署，推荐）**

本项目运行着一个公网托管实例，Claude Code 一行接入：

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

HTTP Server 自带 `/mcp` 端点，走 MCP Streamable HTTP 协议（SSE），支持 `GET`（SSE 事件流）和 `POST`（请求/响应）。任何支持 HTTP transport 的 MCP 客户端（Claude Code / Claude Desktop / Cursor）都能连。

**方式二：自部署 stdio**

```bash
./target/release/aginxbrowser --mcp
```

`--mcp` 模式走 stdio 协议，不启动 HTTP 服务器，通过 stdin/stdout 与 MCP 客户端通信。

### 提供的工具（27 个）

#### 基础工具

| 工具 | 说明 |
|------|------|
| `fetch` | 抓取网页（支持分层渲染、stealth、js_extract） |
| `eval` | 在页面上执行 JavaScript（支持 async/Promise） |
| `click` | 点击页面元素（CSS 选择器） |
| `search` | 多引擎聚合搜索（百度/Bing/搜狗/搜狗微信/Google） |
| `download` | 流式下载文件到磁盘（SHA-256 校验、断点续传） |
| `cache` | 查询本地抓取/搜索缓存（全文含 CJK、整页 `get`、统计、按条件清理） |

#### Session 工具

| 工具 | 说明 |
|------|------|
| `session_create` | 创建交互式浏览器会话；`persistent: true` 时登录态落盘，闲置过期甚至服务重启后同一个 `session_id` 带登录态复活 |
| `session_clone` | 从现役会话派生新会话，完整带走登录态（cookie + storage + viewport + 弹窗策略），原会话不动——危险操作前先存档，或同一登录态并行开多会话 |
| `session_list` | 列出存活会话（空闲时长 + 剩余寿命，能复用就别新建） |
| `session_navigate` | 会话内导航到新 URL |
| `session_state` | 获取索引化的页面状态 |
| `session_cookies` | 导出会话当前 cookie，完整 Set-Cookie 形式（`name=value; Domain=…; Path=/`，用于登录态复用——跨子域状态能扛住回灌） |
| `session_storage` | 快照会话的 `localStorage`/`sessionStorage`——cookie 带不走的那半登录态，配 `session_create` 的 `storage` 字段回灌 |
| `session_console` | 读会话最近的页面 console 输出（`log/info/warn/error/dialog` 环形缓冲 500 条，支持 `level`/`since_ts`/`url_contains`/`limit` 过滤）——页面为什么坏，点一下按钮再读它最快 |
| `session_click` | 按索引点击元素 |
| `session_click_xy` | 按页面坐标走真实鼠标链点击（pointerdown→click，逐事件 hit-test）——canvas/地图/自绘控件吃这套；`click_count: 2` 补 `dblclick` |
| `session_drag` | 从 `from` 按下、插值 `mousemove` 滑到 `to` 松开——地图 marker/canvas 选区跟着每一步走 |
| `session_input` | 按索引输入文本（写值后派发 `input`+`change`；`events:"full"` 逐字符派发键盘事件） |
| `session_scroll` | 滚动页面 |
| `session_eval` | 在会话中执行 JavaScript |
| `session_dialog` | 查看/接管弹窗策略（`alert`/`confirm`/`prompt` 永不阻塞：自动作答并记录，`list`/`accept`/`dismiss`） |
| `session_viewport` | 设会话视口（设备模拟）：media query 重算，`mobile: true` 翻 `pointer: coarse`/`hover: none`；设置活过导航 |
| `session_screenshot` | 截会话**当前** DOM 状态（含 click/eval 后的突变）为 base64 PNG；可选 `width`/`height`/`full_page`/`selector` |
| `session_wait` | 等 CSS 选择器命中或 JS 谓词为真，带超时——等待期间页面事件循环照常跑，替代瞎 sleep |
| `session_network` | 读会话网络请求日志；`filter: "media"` 从页面真实发出的请求里提播放/直播链接（m3u8、mp4…）——拿真视频直链靠它 |
| `session_export` | 导出会话录制的动作（默认出可回放的 curl 脚本；`format=jsonl` 出原始日志） |
| `session_close` | 关闭会话（persistent 会话顺带删落盘登录快照——空闲过期保留，显式关闭不留） |

#### fetch 工具参数

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | ✅ | — | 目标 URL |
| format | string | | `"markdown"` | 输出格式：`markdown` / `html` / `text` |
| selector | string | | `null` | CSS 选择器 |
| wait_secs | u64 | | `null` | 页面加载后等待秒数 |
| use_proxy | bool | | `false` | 走代理 |
| max_chars | usize | | `50000` | 截断字符数 |
| auto_bypass_challenge | bool | | `true` | 自动绕过 Cloudflare Turnstile |
| render_tier | string | | `"auto"` | 渲染策略：`auto` / `http` / `obscura` |
| tls_fingerprint | string | | `null` | TLS 指纹 |
| js_extract | object | | `null` | JS 数据提取：`{expression, timeout_ms}` |

#### session_create 参数

| 参数 | 类型 | 必填 | 默认 | 说明 |
|------|------|------|------|------|
| url | string | | `null` | 初始 URL |
| use_proxy | bool | | `false` | 走代理 |
| cookies | string[] \| object[] | | `[]` | 注入 cookie（`"name=value",...` 或 CDP 风格对象），会话创建即登录态。配合 `session_cookies` 复用登录态 |
| storage | object | | `null` | 初始导航落地后注入的 Web Storage：`{"local_storage": {"k":"v"}, "session_storage": {"k":"v"}}`。与 `session_storage` 工具往返 |
| ttl_secs | u64 | | `480` | 空闲回收秒数（钳 60..3600），长流程调大 |
| keepalive | bool | | `false` | 免空闲回收：活到 `session_close` 或进程退出——中间穿插长非浏览器步骤的流程不再丢登录态 |
| persistent | bool | | `false` | 登录态落盘（cookie、`localStorage`/`sessionStorage`、视口、弹窗策略，每次操作后存一份）。会话闲置过期甚至服务重启后，下一次调用同一个 `session_id` 会带着登录态原地复活。`session_close` 会删掉快照，空闲过期则保留（Playwright storageState 的语义，但不用换钥匙——还是你手里那个 session_id） |
| width / height | u32 | | `null` | 初始视口，会话存活期钉住（活过导航） |
| mobile | bool | | `false` | 初始视口的手机模拟（`pointer: coarse`、`hover: none`、`maxTouchPoints = 5`） |

#### session 操作参数

所有 session 操作都需要 `session_id` 参数。`click`/`input` 需要 `index`（从 `session_state` 获取），`input` 还需要 `text`，`eval` 需要 `script`，`navigate` 需要 `url`，`clone` 只要源会话 id。带可选参数的工具：`click_xy` 要 `x`/`y`（可选 `button`、`click_count`）；`drag` 要 `from`/`to`（可选 `steps`、`delay_ms`）；`viewport` 收 `width`/`height`/`mobile`（都可选，缺省保持当前值）；`screenshot` 收 `width`/`height`/`full_page`/`selector`/`selector_all`；`wait` 的 `selector`/`predicate` 二选一，加 `timeout_ms`（默认 10000，上限 120000）；`export` 收 `format`（默认 `bash` / `jsonl`）；`network` 收 `filter: "media"`；`dialog` 收 `action`（`list`/`accept`/`dismiss`）加可选 `prompt_text`；`console` 收 `level`/`since_ts`/`url_contains`/`limit`；`storage`/`cookies` 只要 `session_id`。

### 客户端配置

#### Claude Code

**托管实例（一行命令）**：

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

或在 settings 文件里配置 HTTP transport：

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

**自部署（stdio）**：编辑项目或全局的 settings 文件：

**项目级** `.claude/settings.json`：

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

**全局级** `~/.claude/settings.json`：

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

#### Claude Desktop

编辑 `~/Library/Application Support/Claude/claude_desktop_config.json`（macOS）或 `%APPDATA%\Claude\claude_desktop_config.json`（Windows）：

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

#### Cursor

编辑项目根目录的 `.cursor/mcp.json`：

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

#### 远程服务器（via SSH）

如果 AginxBrowser 部署在远程服务器上，通过 SSH 隧道接入：

```json
{
  "mcpServers": {
    "aginxbrowser": {
      "command": "ssh",
      "args": ["your-server", "/data/www/aginxbrowser/target/release/aginxbrowser", "--mcp"]
    }
  }
}
```

> **注意**：SSH 方式需要本机能免密登录远程服务器（`ssh-copy-id` 配置公钥），且远程服务器上已编译好 AginxBrowser。

---

## 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `AGINXBROWSER_BIND` | `0.0.0.0:8089` | HTTP 服务监听地址 |
| `AGINXBROWSER_STEALTH` | 启用 | `0` 关闭 stealth（诊断用） |
| `AGINXBROWSER_UA` | Linux Chrome145 | 伪装 UA。UA 的浏览器家族/主版本与 TLS 指纹（默认 chrome145）不一致时，启动会打 `fingerprint mismatch` 警告——保持成对一致才能不漏指纹 |
| `AGINXBROWSER_ACCEPT_LANGUAGE` | `zh-CN,zh;q=0.9,en;q=0.8` | Accept-Language |
| `AGINXBROWSER_CACHE_TTL_SECS` | `600` | `/fetch` 缓存 TTL（秒），`0` 禁用 |
| `AGINXBROWSER_DOWNLOAD_DIR` | `.` | `/download` 落盘目录 |
| `AGINXBROWSER_PROXY` | 无 | 代理地址（`use_proxy:true` 时使用；browser/session/CDP 页面导航遇到已知被墙域名时也会自动走它） |
| `CAPTCHA_SOLVER_API_KEY` | 无 | 2captcha API Key，设置后自动解决验证码 |
| `CAPTCHA_SOLVER_SERVICE` | `2captcha` | 验证码解决服务 |

---

## 错误码

| HTTP 状态码 | 场景 |
|------------|------|
| 400 | CSS 选择器语法错误、URL 解析失败 |
| 404 | 元素未找到 |
| 502 | 目标网站不可达（DNS/连接失败） |
| 504 | 请求超时 |
| 500 | 其他内部错误 |

---

## 站点抓取示例

### 微信公众号文章（公开，无需登录）

stealth 模式可直接抓取，**不需要 cookie**：

```bash
# 用 /eval 提取标题和正文
curl -sS -X POST http://127.0.0.1:8089/eval -H 'Content-Type: application/json' -d '{
  "url": "https://mp.weixin.qq.com/s/xxxxx",
  "script": "({title:document.querySelector(\"#activity-name\")?.textContent?.trim(), body:document.querySelector(\"#js_content\")?.innerText})"
}'

# 用 /search 搜索微信文章并自动抓正文
curl -sS -X POST http://127.0.0.1:8089/search -H 'Content-Type: application/json' \
  -d '{"q":"AI人工智能","categories":"news","fetch_top":3,"max_chars_per":2000}'
```

### 交互式登录（Session API）

```bash
# 创建会话 → 查看页面 → 输入 → 点击 → 查看结果
SID=$(curl -sS -X POST http://127.0.0.1:8089/session/create \
  -d '{"url":"https://example.com/login"}' | jq -r .session_id)

curl -sS -X POST http://127.0.0.1:8089/session/$SID/input \
  -d '{"index":1,"text":"user@example.com"}'

curl -sS -X POST http://127.0.0.1:8089/session/$SID/click \
  -d '{"index":3}'

curl -sS -X POST http://127.0.0.1:8089/session/$SID/state
```

### Cloudflare 保护的站点

默认开启 `auto_bypass_challenge`，自动检测 "Just a moment..." 页面并等待 `cf_clearance` cookie：

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch -H 'Content-Type: application/json' -d '{
  "url": "https://cloudflare-protected-site.com"
}'
```

### 提取 SPA 结构化数据

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch -H 'Content-Type: application/json' -d '{
  "url": "https://spa-site.example.com",
  "js_extract": {
    "expression": "JSON.stringify(window.__INITIAL_STATE__)",
    "timeout_ms": 3000
  }
}'
```

### TLS 指纹切换

部分站点会检测 TLS 指纹，Chrome 被拦时可以换 Firefox/Safari：

```bash
curl -sS -X POST http://127.0.0.1:8089/fetch -H 'Content-Type: application/json' -d '{
  "url": "https://strict-site.com",
  "tls_fingerprint": "firefox133",
  "use_proxy": true
}'
```
