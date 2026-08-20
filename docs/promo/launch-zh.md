# Cloudflare 刚做了个 Agent 浏览器,却在博客里公开承认了三件干不了的事

> 备选标题
> ① 人有 Chrome,Agent 有 AginxBrowser
> ② 为什么我们不抢着做最轻的浏览器,而去做最难的那三个

上周 Cloudflare 发了个东西叫 **Kitesurf**——一个专门给 AI Agent 用的浏览器。12 周做出来,跑在他们全球边缘网络的 V8 isolate 上,不用 Chromium,免费公测。

Cloudflare 是个 400 亿美元的公司。他们下场,只说明一件事:**「Agent 需要自己的浏览器」这个判断,成立了。**

但我把他们的发布博客读了好几遍,因为里面有段话特别有意思。大意是:

> 如果你需要播放视频、渲染 WebGL、**用真实的 TLS 指纹去跟反爬握手**,或者**开一个需要持久状态的十分钟登录会话**——Kitesurf 还不是合适的工具,用 Chromium 吧。

这段对我有意思,是因为它公开承认的这几个 Kitesurf 干不了的事,恰好是 **AginxBrowser** 从第一天就在啃的。

## 先说大判断

Agent 现在能写代码、改文档、管项目,但你让它去网上看一眼,它就抓瞎了。它手里的工具,每一个都差一口气:

- `curl` 拿不到 JS 渲染的页面,SPA 抓回来一堆空壳。
- 搜索 API 给的是索引里的旧快照,可能几天前的,价格库存早变了。
- Puppeteer / Playwright 要装一整个 Chromium,服务器扛不住,而且本就是给人调试用的,不是给 Agent。
- Firecrawl 这类抓取服务只读不会动,不能登录、不能翻页、不能点。

Agent 要的不是其中某一个。Agent 要的是五件事都齐:**看得见、读得懂、找得到、操得了、跑得动。**

AginxBrowser 就是干这个的。一个 Rust 二进制,内置 V8,不依赖 Chromium。systemd 守护,HTTP API + MCP,Claude Code 一行接入。

> 人有 Chrome,Agent 有 AginxBrowser。

有意思的是,它和 Kitesurf **同源**——都从一个叫 obscura 的 Rust 无头引擎起家,都用 Blitz 做 HTML/CSS。区别在于:**Kitesurf 选了「做最轻、最便宜的那个」,我们选了「做最难、别人干不了的那三个」。**

## 哪三个?

### 一、真实 TLS 指纹,硬穿反爬

Kitesurf 公开说自己不做 TLS 指纹协商——这其实是必然的:Cloudflare 自己就是全球最大的反爬厂商,他们不可能让自己的浏览器去伪装指纹绕反爬,这是结构性利益冲突。所以 Kitesurf 的流量永远老老实实带个「我是 bot」的签名。

AginxBrowser 不一样。stealth 模式用 BoringSSL 把 Chrome145、Firefox133、Safari、Edge 的**整个 TLS 握手**复刻出来(不是只改个 User-Agent),能按请求切换;Cloudflare 的 "Just a moment..." 挑战页自动等 `cf_clearance`。别人碰反爬就是 403 的站,我们能穿过去。

**这不是劣势,是我们的地盘。**

### 二、有状态的长会话

Kitesurf 是无状态的——抓完即弃,官方建议需要登录态的场景「用 Chromium」。

AginxBrowser 有持久化会话,8 分钟空闲保活。你可以注入 cookie 直接以登录态开局(`session_create(cookies=...)`),操作完再把 cookie 导出来复用(`session_cookies`)。登录 → 翻页 → 操作 → 再操作,整条链不断。

一次性引擎做不了「先登录,再做一串操作」。这个能。

### 三、MCP 原生,不是 CDP 套壳

Kitesurf 主打 Chrome DevTools Protocol,要接 MCP 得再套一层 `chrome-devtools-mcp`。

AginxBrowser 的 13 个工具本身就是 MCP 一等公民。Claude Code、Cursor、Claude Desktop 一行接入,不用先去学一套 DevTools 协议。Agent 拿来就能用。

## 拿来干什么

不是 demo,是真有人在用 agent 浏览器干的事:

- **啃烂后台**——AWS、App Store Connect、Google Play,点穿几十层菜单才干完一件事。让 agent 代你点,到要授权时回来确认。
- **登录后批量操作**——往购物车加一打商品、翻历史订单找小票、查只对登录态开放的页。
- **现场写脚本**——让 agent 看页面、写一段 JS、`eval` 执行:高亮对比表、重排内容、按网站不给的隐藏参数过滤商品。GreaseMonkey-on-steroids。
- **中国互联网**——百度、搜狗、微信公众号,5 引擎聚合搜索 + 中文页正确渲染,不是只懂英文 web。

## 说点实话

别只听好的:

- 内置 Blitz 渲染栈还在 beta,复杂站点的 CSS 是近似,不是 Chromium 那种像素级精准。截图能用,但别指望和 Chrome 一模一样。
- 现在只能 JS 点击,基于屏幕坐标的点击还没有(Blitz 内部坐标已算出来,没暴露给 API)。
- 极强风控的站(百度文库等)暂时不支持。
- **开源 Apache-2.0,单二进制**,想跑在哪台机器上都行,不锁任何云——这一点 Kitesurf 即使开源也做不到,它只能部署到你自己的 Cloudflare 账号。

## 一行接入

Kitesurf 验证了方向,Cloudflare 把最难的三个山头让了出来。AginxBrowser 就守在这三个山头上。

```bash
claude mcp add aginxbrowser --transport http https://browser.aginx.net/mcp
```

托管实例直接用:**browser.aginx.net** · 开源自部署:**github.com/yinnho/aginxbrowser**

Agent 互联网正在成形。给每一个 Agent 装上眼睛和手——这件事,我们做了。
