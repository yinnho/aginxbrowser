# aginxbrowser 需求反馈 — 来自 AginxOS 手机上屏链路实测

来源：AginxOS 团队（手机 host OS，Pixel 5 / SM7250，musl 静态）。
部署：aginxbrowser v0.2.8 + `screenshot` feature，aarch64-unknown-linux-musl 全静态，
服务手机 DRM 面板（1080×2340）。链路：本机 HTTP 页面 → diting 渲染 → 帧上屏，
/dev/input/event2 触摸 → 滚动/点击。以下全部为设备实测发现，按优先级排列。

---

## P0 — 视口帧流（性能根因，最痛）

**现状**（源码定位）：
- `Page.startScreencast` 是 no-op（`src/diting_cdp/domains/page.rs:515`）。
- `Page.captureScreenshot` 无视所有 CDP 参数（clip/format/quality/full_page 一律不收），
  固定流程：取 `documentElement.outerHTML` → 重新解析 → **整页**重排 → 全页栅格化 PNG
  （`page.rs:484-510`，`screenshot.rs:526-700`）。渲染宽度钉死 1280，full_page 恒 true。

**后果（实测数字）**：
- 1280×2298 页：约 2.1 fps（勉强）。
- 1280×9054 页：**0.6–0.7 fps，每帧 ~1.4 s**，拖动滚动明显卡顿。
  成本随页高线性涨（重排+重绘+编码都乘页高），与视口无关。

**需求**：
1. `Page.startScreencast` 真实现：内容变化时按**视口**推帧（非全页），滚动时引擎走自身
   scroll offset（合成/视口滚动，不整页重排），帧尺寸 = 视口尺寸。
2. `Page.captureScreenshot` 支持 CDP 标准参数：`clip{x,y,width,height,scale}`、
   `format(png/jpeg)+quality`、`captureBeyondViewport:false`（只渲当前视口）。
3. 滚动不要被迫烤进布局：我们目前用「body 负 marginTop」模拟滚动（因为引擎抓帧只有
   全页模式），副作用是 outerHTML 序列化把 offset 带进重渲——滚动越多，抓出的图页顶被裁
   越多。引擎自身可滚动 + 抓帧视口化之后，此 hack 退役。

## P1 — 坐标世界统一（点击命中前提）

**现状**：
- 引擎真实 viewport 实测 2560×1360（`innerWidth/innerHeight`），且两次导航间测得
  2560×1360 → 1728×1037 漂移，触发条件不明。
- 抓帧世界恒 1280 宽（另一套坐标系）。
- `Emulation.setDeviceMetricsOverride` 被接受但是空壳 stub。
- `Page.getLayoutMetrics` 返回硬编码常量 1280×720（`page.rs:24-25, 462-463`）。

**后果**：触摸点击坐标要从面板坐标 ×2.37 换算进引擎世界，抓帧世界又是另一比例，
自动化无法把「屏上看到的元素」可靠映射成点击坐标。

**需求**：
- `Emulation.setDeviceMetricsOverride` 真实现（至少 `innerWidth/innerHeight` 与
  `Page.getLayoutMetrics` 反映真实值且可设）。
- `Input.dispatchMouseEvent` 与抓帧/视口同一坐标系，devicePixelRatio 语义明确。

## P2 — file:// 与本地内容导航 ✅ 已闭环（2026-09-09）

**当时的现状**：file:// 被硬禁，报错文案指引 `--allow-file-access`，但该开关无效（误导）。
data: URL 可用，但大页面 base64 很笨重。

**闭环方式**：`--allow-file-access` 真生效（需求的前半）。开关是进程级的，等价 env
`AGINXBROWSER_ALLOW_FILE_ACCESS=1`（ musl 侧不传 CLI 参数时用 env 更顺手）。落地细节：

- 网络层单一卡点：`fetch_file_url`（reqwest 主管线、stealth 文档请求、stealth 重定向
  跳转三条路都汇到这里）进门先查开关。这同时修掉一个真洞：改之前**网络层对 file://
  无门禁**——绕过三个 CDP gate 的入口（MCP session_create、/fetch、子资源）都能直接
  读本地文件，而服务默认绑 0.0.0.0。现在默认全拒，错误文案点名真实开关。
- 三个 CDP gate（`Page.navigate`/`Target.createTarget`/`DOM.setFileInputFiles`）改读
  同一开关；`BrowserContext.allow_file_access` 死字段（从未有任何入口能置 true）删除。
- 开关打开后：file:// 导航、相对路径子资源（css/js/img 同目录解析）、`/fetch` 本地
  文件全可用；file 页面只能引 file 子资源（跨 scheme 钉死：http 页引 file:// 子资源
  一律拒，`subresource_allowed` 矩阵原有测试钉着）。截图渲染链路同批补齐：预取收集器
  原来只收 http/https，本地页的 css/img 根本进不了预取清单，抓出来的图是"黑标题+灰占位"
  （live DOM 却是红的——探针只查 computed style 查不出这个）。现在 file 页的子资源按
  同 scheme+开关开收进预取，截图与 http 页逐像素等价（红 h1 像素数 1262 两边一致）。
  原来记的"http/file 共有老洞：img 相对 src 不归一化成绝对 URL、解码 miss 画占位框"
  已闭环（#317）：`resolve_img_source` 拿文档 base 把相对 src join 成绝对 URL（空/
  data:/blob: 直通，无 base 保原样），静态截图、live 带画 missing 检查、CDP 带抓
  三条消费链共用这一个归一化点。同一 fixture http/file 双跑，dot.png 蓝色像素
  256（=16×16 整图）两条传输都在，红 h1 1262 无回归。
- 边界（与 Chrome 对齐故意的）：页面 JS 的 `fetch()/XHR/import` 走 reqwest，reqwest
  不做 file://，所以**页面脚本**读不了本地文件（Chrome 里 file:// fetch 也被 CORS
  挡）；`file://` 带 remote host（`file://example.com/x`）拒。POST 到 file:// 按 GET
  读。agent 想让页面带本地内容 → 导航 file:// 或 setContent（#299 已落）。
- 顺手闭环同类洞：`--allow-private-network` 也是五处注释声称、实际从未接线的纸面开关
  （只有 env 生效）。同批接线，`/doctor` 对两个开关都加了打开即警的检查行。

## P3 — 实测 OK、不用改（给团队的正面反馈）

- 中文排版与栅格质量很好；bundled CJK 字体在无 fontconfig 的 musl 环境直接工作，
  零系统依赖。
- data: URL 导航 + outerHTML 全页渲染链路稳定；1280×2298 页 <2s 出图。
- CDP over WS（/json/version、flatten session）与手写 python 客户端兼容良好。

## 附 — musl 交叉编译补丁（可直接上游）

`screenshot` feature 在 aarch64-unknown-linux-musl 下编译失败：
`yeslogic-fontconfig-sys` build.rs 需要 pkg-config 交叉 sysroot。
修法（两处小改，已在本地下验证通过）：

```toml
[dependencies]
fontique = { version = "0.10", optional = true }   # 新增 optional 依赖

[features]
screenshot = [ ..., "fontique/fontconfig-dlopen" ] # feature 数组加一行
```

注意：仅设 env `RUST_FONTCONFIG_DLOPEN=1` **不够**——fontique 用自己的
`fontconfig-dlopen` feature gate `ffi_dispatch!` 路径。dlopen 后运行时无
libfontconfig 也能跑（bundled CJK 字体兜底），已在设备上验证。

---

## 更新 2026-09-07 — 截图构建零 blitz（回应"构建日志提到 blitz"）

**根因**：AginxOS 构建拉 main rev + `--features screenshot`，与 0.2.x 版本号无关。
当时 `screenshot` feature 仍内含 blitz 参照管线（blitz 四件套 + Stylo 族），
故构建日志出现 blitz。diting 本就是默认渲染路径，属 feature 拆分欠账非引擎行为。

**已修**（main `962abd3`）：`screenshot` = 纯 diting 栈，`cargo tree` 全图零
blitz；参照管线独立 opt-in `blitz-reference`（设备构建永不开）。设备构建少背
54 个包（blitz 四件套 + stylo 九件 + usvg/svg 工具链），依赖计数 416 → 362。
构建命令不变。fontique/fontconfig-dlopen patch 仍需（fontique 是 diting 运行时
依赖）。二进制体积降幅未测，AginxOS 下次出包记录 strip 后对比。交接细节见
`~/Documents/aginx/aginxbrowser-zero-blitz-build.md`。

---

## 更新 2026-09-08 — P1 坐标世界收尾（dpr 语义落地）

P1 四条全部闭环。getLayoutMetrics 真值已随视口帧流上 v0.2.11；本批补上最后一块：
`Emulation.setDeviceMetricsOverride` 的 `deviceScaleFactor` 从"校验后丢弃"改为真
生效——>0 钉住 `window.devicePixelRatio`（跨导航存活），=0 回 persona 默认
（Chromium 的"0 = default"）。同批加了两个钉回归的测试：dpr 钉定/回退、
gBCR 圆心点 dispatchMouseEvent 命中（= 设备触摸→点击链路的引擎侧验收）。

坐标语义说明（四坐标消费者同源、Pixel 5 面板 1:1 配方、已知差异）见交接文档
`~/Documents/aginx/aginxbrowser-coordinate-world.md`。要点：override 之后
innerWidth/gBCR/elementFromPoint/抓帧/getLayoutMetrics 全读同一个视口，
dpr 只影响脚本可见的报告值、不影响成像（恒 1 CSS px = 1 图像像素）；
screen.* 跟 persona 不跟 override（stealth 立场），screenWidth/screenHeight
参数校验后暂不生效。
