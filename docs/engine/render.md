# obscura-render 摸底报告 — 渲染认领 Phase 0

> 2026-08-23。Phase 2 双轨决策的慢线第一步：摸底上游自研渲染器
> `/tmp/obscura-upstream/crates/obscura-render`（最新 main 39fe4d2），
> 为「style/paint/text 三层自研」定批次节奏。主线 Blitz（钉 2fa6434d）
> 不受影响，切换硬条件 = parity + CJK 验证（见 browser.md §8.4）。

## 0. 一句话定位

obscura-render 是上游 2026 年 3 月起从零手写的 CPU 渲染层：taffy 骨架布局 +
手写 cascade + cosmic-text 文本 + tiny-skia 光栅。设计目标「确定性渲染」
（只捆内嵌字体、关系统字体扫描），**代价是零 CJK 字体——中文页面全是豆腐块**。
它证明了两件事：①不用 Stylo 也能把真实站点（Wikipedia/Tailwind/MDN 在测试里
被点名）画出来；②这条路的长尾极长（240 commits 还在磨 sticky/动画/webfont）。

## 1. 规模对比

| | 我们 (Blitz) | 上游 obscura-render |
|---|---|---|
| 总行数 | screenshot.rs 646 行（集成层；引擎在外部 git 依赖） | **66,942 行**（含 vendor fork） |
| css.rs | —（Stylo 全量 CSS） | 10,350 行（537 个属性名分发） |
| style.rs | —（Stylo cascade） | 10,924 行 |
| dom.rs（DOM→布局桥） | blitz-dom（外部） | 20,283 行 |
| inline.rs（文本） | parley（外部，CJK 挂死 bug） | 4,983 行 |
| paint.rs | blitz-paint+vello_cpu（外部） | 17,018 行 |
| border.rs | — | 452 行 |
| 测试 | 我们管线 8 个 | **469 个 #[test]**（dom 132/paint 129/style 86/css 79/inline 33） |

**vendor fork（关键发现）**：`vendor/taffy`（11 commits 改动：grid fit-content、
auto margin 溢出、float clearance、collapsed margins…）和 `vendor/cosmic-text`
（4 commits：CSS 断行语义 491 行重写 shape.rs、variable font instances、
fallback face variations）。**上游连 taffy 和 cosmic-text 都是改过的 fork**，
不是原版依赖。

## 2. 架构总览

```
obscura_dom::DomTree
   │ layout_dom(_with_images/_with_resources)   [dom.rs]
   ▼
收集 <style>/ShadowRoot → 手写 cascade（styles: HashMap<NodeId, LayoutStyle>）
   → taffy 树构建 → compute_layout × 8+ 轮修复链
   ▼
DomLayout { rects, text_runs, clip_rects, transforms, … }
   │ prepare_dom_* 家族                          [paint.rs]
   ▼
PreparedRender { ScrollTree, StickyLayout, viewport_fixed, … }（可复用的保留态）
   │ paint_prepared / screenshot_prepared_*
   ▼
tiny-skia Pixmap → PNG/JPEG/WebP
```

### 2.1 dom.rs — DOM→布局桥（20k 行）

- **入口家族** `layout_dom → _with_images → _with_resources`；核心
  `layout_dom_once`：解析样式表（含 ShadowRoot 各自编译）→ cascade_walk →
  继承遍历（em/rem/vw 解析）→ taffy 树 → 多轮布局 → 后处理修复。
- **Retained rendering**：三种 damage 输入（Attribute/Tree/Style Mutation）
  + Animation/Resource 变体。样式表侧 InvalidationMap（`:has()` 按 Gecko 式
  向上 anchor 失效）；属性侧三档分类 Selector/Subtree/Full；不可表达则回退
  全量。shadow DOM 存在即放弃增量。
- **状态机三个**（没有页面生命周期——渲染器是无状态纯函数，生命周期由调用方
  PreparedRender 复用表达）：动画时间线 AnimationTimelineState、容器查询定点
  迭代（七种终止态，振荡时静默禁用条件规则降级）、滚动 ScrollTree 纯数据。
- **布局覆盖面**：float/clearfix 三套策略（最重投入区）、table 映射为 grid
  （fixed/auto 两算法，Wikipedia infobox 有专测）、subgrid 仅 column 子集、
  multicol 仅 atomic 子块、SVG 作 atomic replaced box、原生控件内在尺寸特判。
- **与 taffy 关系**：只做 flex/grid/block 骨架；inline 文本完全自研
  （**每词一个 taffy leaf** + cosmic-text 整容器 shaping 双轨）；table 用
  「测 min/max→反算列宽→固定 track→再布局」外层协商；text-align 把 block
  提升为 flex-column 实现；calc 经 resolver 注入。主流程是 8+ 次
  compute_layout 的修复链。

### 2.2 css.rs / style.rs — 手写 CSS 层（21k 行）

- css.rs：手写 tokenizer/规则切分（非 cssparser 完整语法树），at-rule 分发：
  @media/@supports/@layer/@container/@keyframes(含 -webkit-)/@property 进规则体；
  @font-face/@import 跳过（字体走 DynamicFontFace 另路）。选择器直接复用
  obscura_dom::selector（与 diting_dom 同源，但**上游已领先**：1824 行 vs
  我们 860 行，`:has()` 相对选择器解析 + shadow host/scope 匹配已实现——
  渲染认领前先把 selector.rs 的差距吸收进 diting_dom）。StylesheetCache 按
  「有序 CSS 源+viewport 相同则复用解析结果」。
- style.rs：**537 个不同属性名**的分发处理，UA 样式表、继承语义、逻辑属性、
  动画声明、颜色/渐变/grid calc 全在手写 match 里。86 个测试锁行为。
- 对比 Stylo：覆盖面窄一个数量级，但胜在**每条都看得懂改得动**，且测试密度
  不低。

### 2.3 inline.rs / paint.rs / border.rs — 文本与光栅（22.5k 行）

**paint.rs（17k）**：
- `prepare_dom` 变体族（2119–2449）是纯参数累积链，每加一个功能派生一个入口：
  `_at_animation_time` → `_with_dynamic_fonts` → `_and_stylesheet_cache` →
  `_with_animation_state` → `_for_media(print)` → `_internal`(2482，唯一真实
  入口)；`_with_retained_styles` 再叠加「保留上次级联、只对变更节点重算」。
  说明渲染器以截图/无头为核心迭代，API 靠外层包装演进。
- PreparedRender（830）= 「不可变布局 + 可重复绘制」快照：viewport、动画采样、
  content_size、fixed/sticky 节点集、ScrollTree、选中图片、usvg 字体库、DomLayout。
- 绘制主循环 `paint_laid_dom_scrolled`（3332，约 1400 行）。region 截图支持非 1x
  原生光栅（受限样式白名单 2967），否则 Lanczos3 重采样。
- 特性位置：位图 image crate 七格式；SVG resvg/usvg 0.47 整体光栅；woff/woff2
  用 **wuff 0.2.7** 解码；linear/radial/conic 渐变（conic 逐像素）；box-shadow；
  clip-path 仅 polygon 掩膜；canvas 经 CanvasSurfaceSource trait 注入外部 RGBA。
- 动画：WAAPI 仅 transform/opacity 快路径（containing-block 拓扑变化即放弃全量
  重排）；CSS 声明式动画布局期采样，几何型逐帧重排、Paint 级复用几何。时间源
  DocumentTime（真实时钟）/LocalOverride（确定性测试）双轨。

**inline.rs（5k）**：
- cosmic-text fork + swash 0.2 光栅；rustybuzz 强制 Advanced 整形（Basic 会错
  映射 span 边界）；UAX#14 断行按 span 携带 CssLineBreak{wrap,word_break,
  overflow_wrap} 三策略。
- 字体两级解析：`resolve_font_family`（CSS generic/常见名→内嵌 Liberation/
  DejaVu）+ `resolve_loaded_font`（webfont 表 token 选择，含 CSS 不对称字重
  搜索）。webfont：@font-face 解析→wuff 解码→fontdb Binary 注册→选择时钉死
  font_id。可变字体独立 VariableSwashCache，wght/opsz/ital/slnt 自动 clamp，
  假斜体 14° skew。首行缩进探针整形、balance(≤6 行)、ellipsis 独立缓冲。

**border.rs（452）**：纯数据模型 Sides<T> + CSS 1–4 值展开，区分 specified/
used 宽度；绘制在 paint_css_border（5476）。

### 2.4 接线面（谁在消费 render）

三层全接，且是**活状态**而非批处理：

1. **obscura-js**（最深）：ObscuraState 直接持有 `prepared_render:
   Option<PreparedRender>` + animation_timeline + stylesheet_cache +
   dynamic_fonts + RenderResourceCache + pending_style_mutations 等 ~10 个
   render 类型字段（ops.rs:178-230）。bootstrap.js 里
   getBoundingClientRect/IntersectionObserver 出现 37 处——JS 几何 API 由
   retained 布局真值驱动，DOM/style 变更清 prepared_render 待下次几何读重建。
2. **obscura-browser page.rs**：`prepare_screenshot_resources()`（3171）经
   页面自己的 HTTP 客户端并发预取图片/字体/CSS（保留 cookie/代理/拦截/CORS）；
   `screenshot()` 主路径走 js.screenshot_prepared_with_surface_color（活
   runtime），无 runtime 才退 screenshot_png_scrolled 兼容路径；
   screenshot_region() 文档空间任意区域裁剪。
3. **obscura-cdp**：Page.captureScreenshot 完整 CDP 形状（png/jpeg/webp、
   clip、captureBeyondViewport、screencast 限流 MAX_SCREENCAST_FRAMES=2、
   MAX_LONG_PNG_PIXELS=32M）。

## 3. 与我们 Blitz 管线逐层对比

| 层 | 我们 | 上游 | 差距本质 |
|---|---|---|---|
| DOM→布局桥 | blitz-dom（黑盒，#636 我们修过 pending resources 门控） | dom.rs 20k 行全透明 | 上游可读可改；我们等上游 |
| CSS | Stylo（全覆盖但巨依赖） | 手写 537 属性（覆盖窄但可控） | 正确性上限 Stylo 高；可维护性反转 |
| 布局 | taffy 原版 | taffy fork + 8-pass 修复链 + float/table/multicol 自研外环 | 真实站点 parity 主要靠这条修复链，blitz 没有 |
| 文本 | parley（钉 0.10，CJK 挂死） | cosmic-text fork（UAX14+Blink 语义对齐） | 上游文本栈无已知挂死；CJK 字符断行类已实现 |
| 光栅 | vello_cpu | tiny-skia + resvg + image/wuff | 同级；上游 SVG/图片格式更全 |
| 集成模型 | 序列化 HTML→from_html 重解析（批处理） | V8 realm 内活 PreparedRender（增量） | 架构代差：上游 JS 几何/截图同源，我们是两套真相 |
| CJK | ✅ 可用（服务器 fonts-noto-cjk + parley system fonts） | ❌ 零 CJK 字体（确定性设计使然） | 我们唯一的实质领先项 |

## 4. CJK 专项（我们的主战场）

- 上游内嵌字体仅 Liberation/DejaVu/Noto Color Emoji；`load_system_fonts`
  显式关闭（inline.rs:993）。中文页面上游自己画不了。
- **文本引擎本身不是 CJK 障碍**（已核实到代码级）：
  - 光栅：swash Outline/ColorOutline/ColorBitmap 三源（inline.rs:725–728），
    CJK 轮廓字形无障碍；缺字形有逐词 glyph 级 fallback 迭代器
    （cosmic shape.rs:389–430），末段扫库内所有 face——塞 CJK ttf 进
    `new_with_fonts` 即被兜底命中；开 system fonts 则 macOS/Linux 的
    PlatformFallback 各有 PingFang/Noto Sans CJK 脚本列表。
  - 断行：fork 的 css_break_data（shape.rs:1051）走完整 UAX#14 pair 表，
    Ideographic/CJ starter/Hangul jamo 分类齐备；keep-all 禁断列表正确；
    break-all 有 Blink 式 tailor。CJK 默认逐字断行天然正确。
- **两处真障碍（自研必改点）**：
  ①`font_face_covers_ascii`（paint.rs:7205–7228）：collect_web_fonts 静默丢弃
  与 ASCII 无交集的 @font-face——**中文子集化 webfont 全被忽略**，须改过滤器
  或改走内嵌字节注入；
  ②遗留 ab_glyph 词切分回退路径（draw_text/measure_text，paint.rs:6695）无
  fallback，混合行 CJK 必 tofu（仅影响无法折叠为纯文本 IFC 的行）。
- 次要坑：locale 硬编码 "en-US"（inline.rs:1049），Han unification 偏日文字形；
  无垂直排版；逐像素 CPU 合成对高密度中文页偏慢（功能正确）。
- 我们的验证基准现成：百度 1533 色/2s（Blitz 钉 rev 基线），自研必须先过这条线。

## 5. 认领路径建议（批次草案）

- **批次 −1（顺手先做）**：diting_dom/selector.rs 吸收上游 `:has()` + shadow
  匹配（860→1824 行的差距）——这是渲染 cascade 的前置依赖，且属于 dom 模块
  认领的自然延伸。
- **批次 0（特征测试）**：把我们 screenshot.rs 管线的输出锁成基线图集
  （baidu/github/people + 本地回归页，像素哈希+色彩数双指标）。
- **批次 1（cascade 只读复刻）**：css.rs/style.rs 是最独立的可移植单元。
  独立 crate + 与 Stylo 输出对照测试，不接产品。CJK 改造点①
  （ascii webfont 过滤器）在此层一并修掉。
- **批次 2（布局桥）**：dom.rs 的 float/table/multicol 修复链是最难啃也最
  有价值的部分（真站点 parity 来源，blitz 没有）。按主题拆批；taffy/cosmic-text
  fork 同步引入并接手维护责任。
- **批次 3（paint+text）**：inline.rs 文本引擎替换 parley（顺手解决 CJK 挂死
  依赖）+ tiny-skia 光栅 + CJK 字体资产捆入。此步完成才谈切换。
- **切换判据**：§4 百度级基准达 parity + 元素坐标/screenshot API 面不变 +
  二进制体积可接受（vendor fork 使依赖反而变纯 crates.io）。

## 6. 已知风险

- **工程量以月计**：67k 行 vs 我们认领过的最大模块 js（307 测试/数千行），
  dom.rs 单文件就超整个 diting_browser。
- **上游还在高速演化**（240 commits/2 月）：今天抄的明天又变。认领策略应
  是「理解+按需移植」，不做整仓跟随。
- **vendor fork 维护责任转移**：拿 taffy/cosmic-text fork 就要自己背它们的
  上游同步。
- **活状态架构差距**：上游渲染挂在 V8 realm 内（增量、与 JS 几何同源）；
  我们若只做截图管线可以批处理，但 getBoundingClientRect 级几何就得学它挂
  realm——那是 diting_js 的改造，超出渲染本身。
- **批次 −1 是唯一近期动作项**：selector.rs 差距吸收独立、低风险、dom 认领
  延伸；其余批次在 Blitz 主线健康的前提下无时间压力。

---
*摸底方法：两路并行探读代理（dom.rs 一路；paint+inline+border 一路）+
主线程核实接线面/vendor fork/CJK 断行代码/selector 同源性。所有 file:line
锚点均出自 /tmp/obscura-upstream main 39fe4d2。*
