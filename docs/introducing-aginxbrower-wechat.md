# 为了让 AI Agent 看懂一篇微信文章，我们写了 70MB 的浏览器

> Agent 时代最大的荒诞：让 AI 读个网页，居然要请出 256MB 的 Chrome。

## 一个尴尬的现实

你给 AI Agent 接好了大模型、配好了工具，让它去读一篇公众号文章。

它卡住了。

回来的是一堆空壳代码。因为公众号正文是 JavaScript 动态渲染的，而你给它的 `web_fetch`（说白了就是发个 HTTP 请求）根本不执行 JS。

那怎么办？传统答案：**上 Playwright，挂一个真浏览器**。

听起来合理。但你算过账吗——

- 一个 Chromium 二进制 **256MB** 起步
- 每开一个会话，进程吃 **几百 MB 内存**
- 你的服务器上跑着 5 个 Agent，光浏览器就能把机器榨干

让 AI 看一眼网页，代价是把一台服务器变成浏览器托管中心。

这不对劲。

## 我们造了个"够用就好"的浏览器

于是我们做了 **AginxBrowser**——一个 **70MB** 的 Rust 服务端浏览器。

它不是 headless Chrome，而是一个被精准阉割过的浏览器内核：

- ✅ **保留**：V8 引擎（能跑 JS、等异步渲染）、HTML 解析、CSS 选择器、TLS 指纹伪装
- ❌ **砍掉**：layout、paint、截图、完整的 DOM 兼容层

翻译成人话：**它能渲染 JS、能过风控、能抓正文，但它不能截图，也不装 Chrome 那一套**。

代价是诚实的。换来的是——它快、它轻、它跑在 1 核 1G 的小机器上不掉链子。

对"Agent 读网页"这件事，你要的是**文字内容**，又不是要看它长什么样。那为什么要为用不上的视觉渲染买单？

## 最硬核的一点：微信公众号，免登录直接抓

这是我们实测出来的，不是画饼。

| 站点 | 传统方式 | AginxBrowser |
|------|---------|-------------|
| 微信公众号文章 | ❌ 验证码拦截 | ✅ **免 cookie，直接拿正文** |
| GitHub Trending | ❌ 异步渲染全是空 | ✅ 等 4 秒，拿到完整列表 |
| 知乎专栏 | ❌ 403 | ✅ 一个 cookie 搞定（管一年） |
| 百度搜索 | ⚠️ 时灵时不灵 | ✅ 稳定 |

重点说说微信。

公众号文章明明是公开的，谁都能看，可 Agent 一抓就是验证码。为什么？因为微信的风控在问一个问题：**"你像不像真浏览器？"**

传统 `reqwest` 一上去就露馅——TLS 握手指纹是 Rust 的，不是 Chrome 的。微信一看，机器人，验证码伺候。

AginxBrowser 的解法叫 **stealth 指纹伪装**：在 TLS 层把自己伪装成 Chrome（用 BoringSSL，和真 Chrome 同一个加密库），HTTP 头和 navigator 指纹也对齐成 macOS 版 Chrome。

微信一看，"哦，是个 Chrome"，放行。

**不需要登录，不需要 cookie，不需要扫码**。Agent 直接抓到完整正文。

## 反直觉的设计：它是"外挂"，不是"库"

这一点我觉得是最有意思的。

大多数抓取工具是"嵌进你的代码"——`pip install playwright`，然后一堆 API 调用。

AginxBrowser 反过来：**它是一个独立服务，挂在系统里，谁需要谁调**。

```
你的服务器
├── Agent A ──┐
├── Agent B ──┼──► AginxBrowser（70MB，单进程，HTTP 服务）
└── 脚本 C ──┘
```

一个实例，所有应用共享。不用每个 Agent 各拉一个浏览器。

接入方式更是简单到离谱。以我们自己的 Agent 运行时 OpenCarrier 为例，原本的 `web_fetch` 工具走 reqwest，要接入 AginxBrowser **只需要一个环境变量**：

```bash
export AGINXBROWSER_URL=http://127.0.0.1:8089
```

- **设了** → 遇到微信这种风控站，自动转给 AginxBrowser 抓
- **没设** → 行为完全不变，随时可回退
- **AginxBrowser 挂了** → 自动降级回 reqwest，系统不瘫

Agent 的接口不用改，Agent 根本不知道浏览器存在。这种"水电式"的基础设施，才是它该有的样子——**拧开阀门就有，而不是每个应用自己打口井**。

## 三个我们踩过的坑

开发过程中有几个反直觉的发现，分享给同样在搞爬虫/Agent 的朋友：

**坑一："抓不到"经常不是 IP 的锅。**

知乎专栏一直 403，我们一度以为是数据中心 IP 被封，买了住宅代理、搞 IP 轮换，折腾半天全无效。最后发现：**只要一个叫 `__zse_ck` 的 cookie（有效期整整一年）**，跟 IP 一毛钱关系没有。

风控的真相，往往比你以为的简单。

**坑二：指纹要"自洽"，不是"高级"。**

一开始我们 TLS 指纹伪装成 Chrome/Linux，HTTP 的 User-Agent 却写着 Chrome/macOS。结果？微信秒拦。

为什么？因为**这种自相矛盾本身就是最强的机器人信号**。一个真浏览器不会声称自己是 macOS 却发着 Linux 的 TLS 握手。

修成 macOS UA + macOS TLS 指纹，立刻通了。**一致性，比单点逼真重要得多。**

**坑三：自研 DOM 的 `innerText`，和真浏览器不一样。**

真浏览器的 `innerText` 自动排除 `<script>` 里的文本。我们自研的 DOM 不排除——结果微信页面 44 个 inline script，把 `body.innerText` 撑到了 **260 万字符**，正文淹没在里面。

这种"看着标准、实则暗坑"的差异，是自研 DOM 的长期成本。我们用"读之前先清空 script 文本"绕过了，但这类坑会持续冒出来。

## 适合谁，不适合谁

说实在的，AginxBrowser 不是银弹。

**适合：**
- AI Agent 平台，想要个共享的轻量"看网页"能力
- RAG 系统，抓网页做知识库
- 中轻量爬虫，目标是要文字内容
- 想给现有系统**无痛加**一个浏览器外挂

**不适合：**
- 要截图、视觉识别的（我们没有 paint 引擎）
- 目标是百度文库这类"正文是图片"的站（得另配 OCR）
- 重度 SPA 自动化（自研 DOM 子集会持续踩坑）

边界讲清楚，是对读者负责。

## 想试一下？

三行命令：

```bash
git clone https://github.com/yinnho/aginxbrowser
cd aginxbrowser && cargo build --release --features stealth
./target/release/aginxbrowser
```

然后：

```bash
curl -X POST http://127.0.0.1:8089/fetch \
  -d '{"url":"https://mp.weixin.qq.com/s/xxxxx","format":"markdown","wait_secs":4}'
```

GitHub 仓库：**github.com/yinnho/aginxbrowser**

---

## 写在最后

AginxBrowser 背后是一个很朴素的判断：

**在"reqwest 太弱"和"Chromium 太重"之间，存在一个巨大的中间地带。** Agent 读网页这件事，绝大多数时候要的是文本，不是视觉。为这个场景扛一个完整浏览器，是杀鸡用牛刀。

我们选了一条务实路：砍掉用不上的，把剩下的做到够好。

如果你也在做 Agent，也在为"怎么让 AI 看懂网页"头疼，欢迎试试，或者来聊聊你的场景。

---

👉 **AginxOS**，专注 AI Agent 实战。

我们相信，**Agent 互联网应该像访问网站一样简单**。AginxBrowser 是这块基础设施的一小块拼图——让每一个 Agent，都拥有一颗轻量的浏览器心脏。

下一篇，我们会拆解 AginxBrowser 怎么用 stealth 指纹骗过微信风控的技术细节，**关注不迷路**。
