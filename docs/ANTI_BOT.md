# AginxBrowser 反爬能力与站点实战

记录 AginxBrowser 对各类反爬站点的实测结论、根因分析、以及未来的改进方向。
基于实战调试（非理论），作为后续维护者的决策依据。

---

## 反爬检测的层级

反爬是**多层信号叠加**，按"离浏览器内核远近"排序，任一层穿帮都可能被拦：

1. **IP 层**：住宅 IP vs 数据中心 ASN（代理可解）
2. **TLS/JA3/JA4 指纹**：ClientHello 的 cipher/extension 顺序（HTTP 客户端栈决定）
3. **HTTP/2 指纹**：SETTINGS 帧、WINDOW_UPDATE、header 顺序
4. **JS 运行时指纹**：`navigator`、canvas、WebGL、字体
5. **自动化协议指纹**：CDP 握手序列（`Runtime.enable` 泄漏等）
6. **行为指纹**：鼠标轨迹、键盘节奏
7. **签名加密**：服务端要求特定 header（如知乎 `x-zse-96`）或 cookie 由客户端 JS 算

AginxBrowser 的定位（V8 + 内联 Obscura 自研 DOM + wreq stealth）覆盖 2/4，部分覆盖 1（代理）。

---

## AginxBrowser 的反爬栈

| 层 | 实现 | 状态 |
|----|------|------|
| TLS 指纹 | wreq（BoringSSL）Chrome145 emulation，`EmulationOS` 从 UA 推导保持自洽 | ✅ |
| HTTP/2 指纹 | wreq emulation 内置 | ✅ |
| HTTP header | `ObscuraHttpClient`/`StealthHttpClient` 注入 Chrome sec-ch-ua / sec-fetch | ✅ |
| JS navigator | `bootstrap.js` 的 `userAgent`/`platform`/`language` 从 `__obscura_ua` 动态推导 | ✅ |
| 代理 | per-call `use_proxy`（国内直连、国外走 `OBSCURA_PROXY`） | ✅ |
| Cookie | per-call `cookies` 注入 + CookieJar 自动 Set-Cookie | ✅ |
| DOM | Obscura 自研 DOM 子集（**不完整**，重 SPA 会崩） | ⚠️ 限制 |

---

## 实测站点结论

### ✅ 微信公众号文章（mp.weixin.qq.com/s/...）

- **无需任何 cookie**（公开静态页，风控只看 HTTP 层指纹）
- stealth + macOS UA + zh-CN 直连即可
- 关键修复历史：stealth wreq 的 `Proxy::all`（非 `Proxy::http`，后者不拦截 https）、JS 层 UA 用 context 的而非硬编码 Linux、`navigator.platform` 从 UA 推导

### ✅ 知乎专栏（zhuanlan.zhihu.com/p/...）

- **只需 `__zse_ck` 一个 cookie**，无需 d_c0 / 登录 / 住宅代理
- 同一个 `__zse_ck` 对所有专栏通用，**有效期约一年**
- 正文从初始 HTML 的 `<script id="js-initialData">` JSON 提取（SSR），不依赖 JS 渲染，绕过 DOM 不完整
- `__zse_ck` 首次获取需过易盾验证码（aginxbrowser 不会自动过，人工一次后长期有效）
- **曾误判**为 IP 问题（折腾住宅代理无效），实为 cookie；曾误判需 d_c0+登录，实为只需 `__zse_ck`
- 脚本：`examples/zhihu.sh`

### ✅ 百度搜索 / GitHub / 爱站网 等

- 百度搜索结果页：去掉 reqwest 自动解压（百度返回的编码与声明不符会解码失败），直接解析
- GitHub：`use_proxy:true` 走 socks5，async eval 提取动态渲染的仓库列表
- 爱站网：静态表格，直接抓

### ❌ 百度文库

- 触发"百度安全验证"（`captcha.gtimg.com` 腾讯验证码）
- 即使过验证码，**正文是图片/Canvas 渲染**，文本不在 HTML 里，任何不截图方案都拿不到
- 解法：真浏览器渲染 + 截图 + **PaddleOCR / ddddocr**（ONNX 模型，Rust 可加载）

### ⚠️ 知乎首页 / 其他需 d_c0 流程

- 知乎首页（www.zhihu.com）要求登录（真实浏览器清 cookie 也跳登录）
- `d_c0` 首次靠知乎 JS 在客户端生成；aginxbrowser 访问 `zhihu.com/signin` 能自动生成 d_c0（zse_ck JS 跑通），证明 V8+stealth 足以执行知乎风控 JS
- 但 `__zse_ck` 需过验证码才能拿 → 自动化卡在这步

---

## 架构限制：自研 DOM

Obscura 自研的 DOM 是子集，重 SPA 站点（知乎专栏页、百度文库预览）会因缺 DOM API 崩溃
（如 `insertAdjacentElement`、`innerHTML` on null）。

**这是架构性限制**，不是单个 bug。后面还有 MutationObserver、Shadow DOM、ResizeObserver、
IntersectionObserver、Web Components… 无穷无尽。

两条出路（研究结论）：
1. **外挂真 Chromium via CDP**（推荐）：aginxbrowser 变编排层，内核用真 Chrome（白拿 TLS/DOM/V8 指纹），Obscura/V8 专职跑签名脚本。对标 nodriver
2. **DOM 换 Servo**：纯 Rust 完整 DOM，但指纹≠Chrome，仍会被 Chrome-shaped 风控识别

> 基准测试参考：`curl_cffi`（6.4MB 纯 HTTP+TLS 指纹）在 31 站点打平 `CloakBrowser`（130MB 49 处 C++ patch 真 Chromium）。对不需要 JS 渲染的站点，TLS+HTTP2 指纹就够，"真浏览器"非万能。

---

## 改进方向（优先级）

### P1（已做）TLS 指纹自洽
wreq `EmulationOS` 从 `AGINXBROWSER_UA` 动态推导（macOS UA → MacOS TLS 指纹）。

### P2（已做）知乎专栏免登录
只需 `__zse_ck`（一年有效），从 `js-initialData` 提取正文。

### P3 自动过验证码（解锁知乎 __zse_ck 自动获取 + 百度文库）
- 易盾/腾讯滑块、点选验证码 → **ddddocr**（目标检测 + 分类 ONNX 模型），Rust 用 onnxruntime 加载
- 百度文库图片正文 → 真浏览器截图 + PaddleOCR

### P4 SessionPool（抄 Crawlee）
`(UA, Sec-CH-UA, TLS profile, cookie jar)` 四元组绑定，被风控标记后整 session 退役。
住宅代理轮换 > 浏览器选型（对持续任务是更大杠杆）。

### P5 外挂真 Chromium（架构升级）
若 DOM 不完整的限制持续阻碍，走 CDP 路线。需注意反检测细节：
- 绝不在每个 frame 自动 `Runtime.enable`；用 `Page.createIsolatedWorld` isolated context
- `navigator.webdriver` 在 isolated world 置 `undefined`，别用 Proxy 覆盖（能被检测）
- 驱动系统真实 Chrome（非 bundled Chromium），版本对齐收益 ≈ patch 本身

---

## 关键参考

- [ianlpaterson 反检测浏览器基准测试](https://ianlpaterson.com/blog/anti-detect-browser-benchmark-patchright-nodriver-curl-cffi/)
- [rebrowser: 修复 Runtime.Enable CDP 检测](https://rebrowser.net/blog/how-to-fix-runtime-enable-cdp-detection-of-puppeteer-playwright-and-other-automation-libraries)
- [Castle.io: 从 Puppeteer-stealth 到 Nodriver 的演进](https://blog.castle.io/from-puppeteer-stealth-to-nodriver-how-anti-detect-frameworks-evolved-to-evade-bot-detection/)
- [nodriver](https://github.com/ultrafunkamsterdam/nodriver) · [patchright](https://github.com/Kaliiiiiiiiii-Vinyzu/patchright)
- [cv-cat/ZhihuApis（知乎 x-zse-96 逆向）](https://github.com/cv-cat/ZhihuApis)
- [ddddocr](https://github.com/sml2h3/ddddocr) · [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)
- [curl_cffi](https://github.com/lexiforest/curl_cffi) · [wreq](https://github.com/0x676e67/wreq)
- [Crawlee: 避免被拦截指南](https://crawlee.dev/js/docs/guides/avoid-blocking)
