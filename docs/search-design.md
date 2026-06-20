# `/search` 设计：搜索 + 抓取一体化

> AginxBrowser 的第四个端点。把"搜索"和"抓正文"合并成一个调用，
> 让 Agent 一步完成"搜 → 读"。OpenCarrier 侧删除 SearXNG MCP，
> 搜索能力下沉到 AginxBrowser，OpenCarrier 回归 Agent 本身。

## 定位与决策

### 为什么把搜索放进 AginxBrowser

搜索和抓取是 Agent 最常见的连续动作：**搜关键词 → 读前几个结果的正文 → 总结**。
现在这两步分裂在两个系统（SearXNG MCP 搜、web_fetch 抓），Agent 要多次往返，
且搜索结果指向的页面经常是动态渲染/风控页（公众号、知乎），只有 AginxBrowser 抓得动。

把搜索下沉到 AginxBrowser，`/search` 一步返回"结果 + 正文"，且正文由 AginxBrowser
的 stealth 抓取能力保证质量。

### 关键决策：调用 SearXNG，不重写

SearXNG 是**元搜索引擎**——自己不爬，聚合百度/Google/Bing/Brave 等。它的核心价值
是几百个引擎结果页解析器（社区十年维护），搜索引擎改版它跟着改。

**重写 SearXNG 是自杀**（一个人维护几百个解析器）。AginxBrowser 直接调用本机已部署的
SearXNG（`127.0.0.1:8888`）作为搜索后端，白嫖它的聚合能力，自己只做"抓正文"这件
它擅长的事。

```
Agent → POST /search?q=macbook&fetch_top=3
         │
         ├─① AginxBrowser 调 SearXNG (127.0.0.1:8888/search?format=json)
         │     SearXNG 聚合百度/Google/Bing，返回 results[]
         │
         └─② AginxBrowser 对前 3 个 url 并发调自己的抓取能力(stealth+JS渲染)
               拿到每条的正文 content

         → 返回 [{title,url,snippet,content}, ...]
```

各司其职：**SearXNG 聚合（它的十年积累），AginxBrowser 抓取（它修好的 stealth/JS）**。

## 依赖关系

- **SearXNG 作为 AginxBrowser 的依赖**，同机部署，`127.0.0.1:8888`。
- 地址由环境变量 `SEARXNG_URL` 配置（默认 `http://127.0.0.1:8888`）。
- **降级**：SearXNG 不可用时，`/search` 返回 503（搜索后端不可用），**不影响 `/fetch` `/eval` `/click`**——它们不依赖 SearXNG。
- 部署清单：SearXNG（systemd/已有）+ AginxBrowser（systemd/已有），同机。

## API

### `POST /search`

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| q | string | 是 | 搜索关键词 |
| fetch_top | usize | 否 | 对前 N 条结果抓正文，默认 `0`（只返回 SearXNG 的 title/url/snippet，不抓） |
| categories | string | 否 | SearXNG 分类，默认 `general`（可选 `images`/`news`/`it` 等） |
| language | string | 否 | 语言，默认 `zh-CN` |
| max_results | usize | 否 | 返回结果上限，默认 `10` |
| max_chars_per | usize | 否 | 每条正文字符截断，默认 `4000`，`0` 不限 |
| wait_secs | u64 | 否 | 抓正文时每页的 JS 渲染等待秒数，默认 `3` |
| use_proxy | bool | 否 | 抓正文时是否走代理（国外站），默认 `false` |

> `fetch_top=0` 时，`/search` 退化为纯搜索（等价原 SearXNG MCP 的能力），
> 毫秒级返回。`fetch_top>0` 才触发抓取，此时耗时 = max_results 内前 N 个页面
> 的抓取时间（并发，受 `wait_secs` 影响）。

响应字段：

```json
{
  "query": "macbook",
  "number_of_results": 1000,
  "results": [
    {
      "title": "MacBook Air - Apple",
      "url": "https://www.apple.com/mac/",
      "snippet": "The most powerful Mac laptops...",
      "engines": ["bing", "google"],
      "score": 8.5,
      "content": "...(正文,仅当该条在 fetch_top 范围内才有,否则 null)...",
      "content_truncated": false,
      "fetch_error": null
    }
  ],
  "search_backend": "searxng"
}
```

- `content`：正文。仅在 `index < fetch_top` 的结果上有值，其余为 `null`。
- `content_truncated`：该条正文是否被 `max_chars_per` 截断。
- `fetch_error`：该条抓取失败时的错误信息（成功为 `null`）。**单条失败不影响其他条**。

## 实现要点

### 1. 调 SearXNG（步骤①）

```rust
// 伪代码
let searx_url = env::var("SEARXNG_URL").unwrap_or("http://127.0.0.1:8888".into());
let resp: serde_json::Value = http_client.get(format!(
    "{}/search?q={}&format=json&categories={}&language={}&pageno=1",
    searx_url, urlencode(q), categories, language
)).send().await?.json().await?;
let results: Vec<ResultItem> = resp["results"].as_array()...
```

SearXNG 的 JSON API 已验证可用（`?format=json`），返回 `results[]` 每条含
`url/title/content(snippet)/engines/score`。

### 2. 并发抓正文（步骤②）

对 `fetch_top` 条结果**并发**抓取（不要串行，否则 N 个页面 × wait_secs 太慢）：

```rust
// 伪代码: 对前 fetch_top 条并发抓取
let fetch_futures = results.iter().take(fetch_top).map(|r| {
    let url = r.url.clone();
    tokio::spawn(async move {
        // 复用 do_fetch 的逻辑(stealth + JS渲染 + cookie)
        // 但要走独立的 current-thread runtime(和 /fetch 一样, V8 !Send)
        fetch_one(url, wait_secs, use_proxy, max_chars_per).await
    })
});
```

**关键约束**：V8 是 `!Send`，每个抓取任务要在独立的 current-thread runtime +
LocalSet 里跑（和现有 `/fetch` 的 `run_on_local_runtime` 一致）。并发抓取 = 起 N
个 blocking 线程，每个内部各跑一个 local runtime。`tokio::task::spawn_blocking`
天然适合（每个 blocking 线程独立）。

并发上限建议 `min(fetch_top, 5)`，避免一次起太多 V8 实例吃内存。

### 3. 复用现有抓取能力

抓正文直接复用 `server.rs` 的 `do_fetch` 逻辑（stealth browser + rendered_text），
不重写。把 `do_fetch` 的核心抽成可被 `/search` 调用的函数即可。

### 4. 超时与降级

- 整体 `/search` 超时：SearXNG 调用 10s + 单页抓取沿用现有 30s。
- SearXNG 挂 → 直接 503，`{"error":"search backend unavailable"}`。
- 单条抓取失败 → 该条 `content=null, fetch_error="..."`，其他条正常返回。
- SearXNG 慢/部分引擎无响应 → SearXNG 自己处理（它有 `unresponsive_engines` 字段）。

## 与 `/fetch` 的关系

- `/fetch`：抓**已知 URL**。
- `/search`：**搜关键词** →（可选）抓正文。

`/search` 内部抓正文时复用 `/fetch` 的能力，不重复实现。两者共享 stealth、rendered_text、
cookie 注入等。

## 与 OpenCarrier 的关系（替代 SearXNG MCP）

OpenCarrier 侧：
- **删除** `mcp_searxng_web_search` 工具 + SearXNG MCP 配置（`config.toml` 的 searxng 条目）
- Agent 需要搜索时，调 AginxBrowser 的 `/search`（通过 `web_fetch` 或新增工具）

OpenCarrier 不再背"搜索基础设施"，回归 Agent 本身。SearXNG 从 OpenCarrier 的依赖
变成 AginxBrowser 的依赖，但物理上还是同一个进程、同一台机器，只是归属变了。

## 缓存

`/search` 复用 `/fetch` 的缓存思路：
- 搜索结果（`fetch_top=0`）按 `(q, categories, language)` 缓存，短 TTL（如 300s）。
- 抓取的正文走 `/fetch` 已有的缓存（按 URL）。

## 不做的事

- **不重写 SearXNG 的引擎解析器**（百度/Google 改版维护是 SearXNG 的事）。
- **不做自己的搜索排序算法**——用 SearXNG 的 `score`，原样透传。
- **不支持翻页深度抓取**——`fetch_top` 只抓首页结果的前 N 条。深度搜索让 Agent 多调几次。

## 验证清单（实现后）

- [ ] `fetch_top=0`：毫秒级返回 SearXNG 结果（title/url/snippet）
- [ ] `fetch_top=3`：前 3 条带正文，其余 `content=null`
- [ ] 单条抓取失败：该条 `fetch_error` 有值，其他条正常
- [ ] SearXNG 停掉：`/search` 返回 503，`/fetch` 仍正常
- [ ] 抓微信公众号结果：正文能拿到（验证 stealth 在 `/search` 里也生效）
- [ ] 并发抓取：3 条总耗时 ≈ 单条最慢的耗时（非 3 倍）
