# AginxBrowser：让 AI Agent 拥有一颗"轻量浏览器心脏"

> 一个 70MB 的 Rust 服务端浏览器，让 Agent 不装 Chrome 也能抓微信公众号、跑 JS、过风控。

## 痛点：Agent 想看个网页，怎么就这么难？

你给 AI Agent 接上 `web_fetch`（本质是 reqwest 直连），让它去抓一篇微信公众号文章。结果——

- 拿回来的是 6.6MB 的空壳 HTML，正文 JS 还没渲染；
- 想抓 GitHub Trending，全是异步渲染的，静态抓取一片空白；
- 知乎专栏？直接 403，因为缺风控要的 cookie。

怎么办？传统答案是**上 Playwright / Puppeteer，挂一个真 Chromium**。

但真浏览器的代价是残酷的：

- Chromium 二进制 256MB+，进程动辄吃几百 MB 内存；
- 启动慢，每个会话都要拉起浏览器实例；
- 服务器上跑几十个 Agent，光是浏览器就能把机器拖垮。

有没有可能，**既要能渲染 JS、过风控，又不要 Chromium 那么重**？

这就是 **AginxBrowser** 想解决的问题。

## AginxBrowser 是什么

一句话：**一个用 Rust 写的、内置 V8 引擎的轻量服务端浏览器**。

它不是 headless Chromium，而是一个"够用就好"的浏览器内核——

- 内置 V8（完整 JavaScript 运行时，能跑页面脚本、等异步渲染）
- 内置 HTML 解析 + CSS 选择器（能 querySelector、提取正文）
- 可选 **stealth 指纹伪装**（wreq + BoringSSL，模拟 Chrome 的 TLS/JA3 指纹）
- 整个二进制 **~70MB**，单进程，systemd 守护

对外只暴露三个 HTTP 端点，简单到不能再简单：

```bash
POST /fetch   # 抓页面，返回 markdown/html/text
POST /eval    # 在页面里执行任意 JS（支持 async/await）
POST /click   # JS 点击元素
```

## 它能做什么（实战验证）

这是我们在真实站点上测出来的，不是 PPT：

| 站点 | 传统 reqwest | AginxBrowser |
|------|-------------|-------------|
| 微信公众号文章 | ❌ 风控验证码 | ✅ **免 cookie，直接拿正文** |
| GitHub Trending | ❌ 异步渲染，空 | ✅ eval 等 4 秒，拿到仓库列表 |
| 百度搜索结果 | ⚠️ 解码偶发失败 | ✅ 稳定 |
| 知乎专栏 | ❌ 403 | ✅ 一个 cookie（一年有效） |
| 任意静态页/API | ✅ | ✅（自动直连，不走浏览器，快） |

**最硬核的一点**：微信公众号文章，AginxBrowser **不需要任何 cookie、不需要登录态**就能抓到完整正文。

原因是它有 stealth 指纹（TLS 层伪装成 Chrome）+ 一致的 navigator 指纹（UA / platform / language 自洽）。微信的风控只看"你像不像真浏览器"，AginxBrowser 装得够像，就放行了。

## 一个反直觉的设计：它是"外挂"，不是"库"

大多数抓取方案是"把浏览器嵌进你的代码"——`pip install playwright`，然后 `from playwright.sync_api import sync_playwright`。

AginxBrowser 反过来：**它是一个独立服务，挂在系统里，谁需要谁调**。

```
┌─────────────────────────────────────────┐
│              你的服务器                  │
│                                         │
│  ┌──────────┐  ┌──────────┐  ┌────────┐ │
│  │ Agent A  │  │ Agent B  │  │ 脚本 C │ │
│  └────┬─────┘  └────┬─────┘  └───┬────┘ │
│       │             │            │      │
│       └─────────────┼────────────┘      │
│                     ▼                   │
│           ┌──────────────────┐          │
│           │   AginxBrowser    │  ← 70MB  │
│           │   (HTTP :8089)   │    单进程 │
│           └──────────────────┘          │
└─────────────────────────────────────────┘
```

好处是什么？

1. **一个实例，所有应用共享**。不用每个 Agent 各拉一个浏览器。
2. **宿主零改动**。以 OpenCarrier（我们的 Agent 运行时）为例——它的内置 `web_fetch` 工具默认走 reqwest。接入 AginxBrowser 只需要一个环境变量：

   ```bash
   export AGINXBROWSER_URL=http://127.0.0.1:8089
   ```

   设了，`web_fetch` 遇到微信这类风控站就自动转给 AginxBrowser；没设，行为完全不变，可随时回退。**Agent 的接口、参数、语义都不用改**，Agent 根本不知道浏览器存在。

3. **失败降级**。AginxBrowser 挂了？`web_fetch` 自动回退到 reqwest，整个系统不瘫。

这种"外挂基础设施"的定位，让 AginxBrowser 可以无痛接入任何已有的 Agent / 爬虫 / RAG 系统，而不用重写它们的抓取层。

## 它怎么做到这么轻？

关键决策：**不实现完整的浏览器，只实现"够用的浏览器"**。

| 组件 | Chromium | AginxBrowser |
|------|----------|-------------|
| JS 引擎 | V8 | V8（同一个） |
| 渲染/layout/paint | 完整（Blink） | **没有** |
| DOM | 完整 | 自研子集（够 querySelector/innerText） |
| 网络 | 完整 | reqwest + wreq（stealth） |
| 二进制 | 256MB+ | ~70MB |
| 截图 | ✅ | ❌ |

代价是诚实的：**没有 layout/paint 引擎，不能截图；DOM 是子集，重 SPA 偶尔会因缺 API 崩**。

但换来的是——它快、它轻、它能跑在 1 核 1G 的小机器上不掉链子。对于"Agent 抓网页内容"这个场景（要的是文本，不是视觉），这个取舍非常划算。

> 一个基准测试的启示：`curl_cffi`（6.4MB，纯 HTTP+TLS 指纹）在 31 个风控站点上的通过率，和 `CloakBrowser`（130MB，49 处 C++ patch 的真 Chromium）打平。**对不需要视觉的抓取场景，"真浏览器"从来不是唯一解。**

## 几个真实踩过的坑（给想自研的人）

开发 AginxBrowser 的过程里，有几个反直觉的发现，值得分享：

1. **"抓不到"经常不是 IP 问题**。
   知乎专栏 403，我们一度以为是数据中心 IP 被封，折腾了住宅代理、IP 轮换都无效。最后发现：**只需一个 `__zse_ck` cookie（有效期一年）**，跟 IP、TLS 指纹都无关。风控的真相往往比你以为的简单。

2. **stealth 指纹要"自洽"，不是"高级"**。
   一开始 TLS 指纹伪装成 Chrome/Linux，HTTP UA 却写 Chrome/macOS——这种矛盾本身就是强风控信号。修成 macOS UA 配 macOS TLS 指纹，微信立刻放行。**一致性 > 单点逼真度**。

3. **innerText 在自研 DOM 里不等于真浏览器**。
   真浏览器的 `innerText` 自动排除 `<script>` 文本，自研 DOM 不排除——微信 44 个 inline script 把 `body.innerText` 撑到 260 万字符。这种"看起来标准、实则坑"的差异，是自研 DOM 的持续成本。

## 适合谁，不适合谁

**适合：**
- AI Agent 平台，需要一个共享的、轻量的"看网页"能力
- RAG 系统，要抓网页做知识库
- 中轻量爬虫，目标是要文本内容而非视觉
- 想给现有系统无痛加一个浏览器外挂

**不适合：**
- 需要截图、视觉识别（没有 paint 引擎）
- 目标是百度文库这类"正文是图片"的站（要 OCR，得另配）
- 需要完整 DOM API 兼容的重 SPA 自动化（自研 DOM 子集会陆续踩坑）

## 上手

```bash
git clone https://github.com/yinnho/aginxbrowser
cd aginxbrowser
cargo build --release --features stealth   # 含指纹伪装
./target/release/aginxbrowser               # 监听 :8089

# 试一下
curl -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" \
  -d '{"url":"https://mp.weixin.qq.com/s/xxxxx","format":"markdown","wait_secs":4}'
```

仓库：**https://github.com/yinnho/aginxbrowser**
文档：README（API 全字段）+ `docs/ANTI_BOT.md`（反爬实战与架构权衡）

## 写在最后

AginxBrowser 不是一个"Chromium 杀手"，它是一个**务实的中间态**——在"reqwest 太弱"和"Chromium 太重"之间，给 Agent 一个能用的浏览器内核。

它背后是一个更朴素的理念：**基础设施应该像水电一样，拧开阀门就有，而不是每个应用自己打口井**。一个 70MB 的服务挂在那里，所有 Agent 共享，谁需要谁调。

如果你也在做 Agent、在为"怎么让 Agent 看懂网页"发愁，欢迎试试，或者来聊聊你的场景。

---

*AginxBrowser 是 [OpenCarrier / Aginx](https://github.com/yinnho) 生态的一部分——Agent 互联网基础设施，访问 Agent 就像访问网站一样简单。*
