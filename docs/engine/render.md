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

## 7. 批次 −1 完成（2026-08-23）

diting_dom/selector.rs 吸收上游差距（860→~1480 行），343→352 测试。**范围修正
（记档）**：原计划的 shadow 匹配（`:host`/`::slotted`）依赖 tree.rs 整套 shadow
原语（attach_shadow_root/is_shadow_root/assigned_slot，上游 tree.rs 独有约
600 行）——那属于 frame realm C 组挂账，本批不吸收。实际吸收五件：

1. **解析开关**：`parse_has()` + `parse_is_and_where()`——selectors 0.26 默认
   关闭，导致 `a:has(p)` / Tailwind preflight 的 `:where(...)` 静默解析失败、
   规则被丢弃。开启后匹配逻辑 crate 内建，无需额外代码。
2. **opaque() 稳定性修复**：DomElement 是 Copy，栈地址每次遍历都变，
   `OpaqueElement::new(self)` 对同一节点返回不同 id——`:has` 锚点匹配比较的
   就是这个 id，必炸。改为取节点在 `borrow_inner().nodes` Vec 里的稳定槽位地址。
3. **PseudoClass 扩充**：focus-visible/focus-within/link/visited（link|any-link
   同义；visited 恒 false）。Bootstrap `.visually-hidden-focusable` 的
   `:not(:focus):not(:focus-within)` 快照匹配解锁。
4. **matches_selector 单元素 API** + **CompiledSelector 编译管线**
   （specificity + AncestorHashes + SelectorKey 分桶）+ **Matcher**
   （可复用 caches + 增量祖先 bloom 过滤器 + candidate 去重代数）+
   subject_keys（Gecko 式保守分桶契约：`.control:is(button, #save)` 保
   .control，`.control:is(#save, #cancel)` 用双 id 桶）。
5. 测试 +8（:has 解析/深组合器/嵌套禁令、:is/:where 匹配、focus 家族快照、
   :link、分桶覆盖不变量、Matcher+bloom、candidate 代数去重）。

**踩坑记档**：测试初版写了 `section:has(article:has(li))`——selectors 0.26 按
spec 禁止嵌套 :has（SelectorParsingState::DISALLOW_RELATIVE_SELECTOR →
InvalidState），上游也没测这个形态。嵌套禁令已锁进我们的测试。

shadow 匹配三钩子（parent_node_is_shadow_root/containing_shadow_host/
matches_in_shadow_scope）随 frame realm 立项时一并吸收（tree.rs shadow 原语 +
selector.rs 钩子 + css.rs scope cascade 三层联动）。

cascade 复刻（批次 1）的前置依赖就此就位：CompiledSelector/Matcher 正是
obscura-render/css.rs 构建样式表索引的原语。

## 8. 批次 0 完成（2026-08-23）：基线图集

screenshot.rs 新增 4 个基线测试（`baseline_*`），把 Blitz 管线在**确定性本地
页面**上的输出锁成 parity 判据。全部无网络依赖，CI 可跑；服务器 CI 需装
`fonts-noto-cjk`（与生产一致）。

| 基线页 | 锁定指标 |
|---|---|
| SSR 中文文本 | 800×400 画布、色彩数>50、#1a0dab 标题字形>100px、正文墨色>200px、h1 rect 近顶 |
| flex 行 + grid | 三色 cell 各>4000px；grid 六 cell rect 精确（60×40±1.5，行距≥40 不重叠） |
| 表格 + display:none | 折叠边框灰系>150px、文字墨色>30px、隐藏元素 box=0x0 |
| full_page 高度追踪 | 内容 1800px 时画布跟随 ≥1800；viewport 模式裁到 300 |

**记档一个 API 真实行为**：display:none 元素在 selector_all 下仍报一条
rect 匹配（blitz 保留节点），box 为 0x0——基线锁的是这个诚实行为，
自研切换时若行为不同需显式说明。

**用法**：将来任何渲染层变更（升级 blitz rev / 自研替换）跑这 4 页，
色彩数或区域像素漂移 = 布局/绘制回归（或改进），必须逐项解释后才可切换。
真实站抽样（baidu/github/people）继续走部署后人工验证流程（见
[[blitz-rendering-spike]] 历次部署记录），不入 CI。

358 测试全绿（+4 基线 +1 stealth 组合下的 cfg_attr 修正）。

## 9. 批次 1 完成（2026-08-23）：diting_css cascade 只读复刻（最小竖切）

新建 `src/diting_css/mod.rs`（~800 行含测试，14 测试），从上游 css.rs/style.rs
切出 cascade 层最小可用竖切。**未接产品管线**——模块级
`#![cfg_attr(not(test), allow(dead_code))]` 就是其只读状态的诚实声明（批次 3
三档政策的第 2 档）。

**竖切范围**：
- **Stylesheet 解析**：手写 tokenizer（注释/嵌套括号/引号感知）+ at-rule 分发
  （@media/@supports/@layer 进规则体；@font-face/@import/@keyframes 丢弃）+
  上游同款错误恢复（顶层杂散 `}` 重同步，remoteok.com 真实案例）。
- **@media 求值**：逗号列表=OR（函数内逗号不分隔）、`not` 前缀取反、
  `screen and (min-width: …)` 链、纯 feature 查询隐含 `all`（print 下也适用，
  spec 行为已锁进测试）、宽度/高度四类 feature。Tailwind 断点形态全覆盖。
- **@supports 求值**：not/and/or 组合（同级混用判无效，CSS Conditional 规则）+
  声明探针（属性在支持集内且值合法；`(display: nonsense)` 正确 false）。
- **ComputedStyle 最小子集**：display/color/background/margin/padding/
  font-size/font-weight/text-align + CSS 1-4 值展开 + named/hex 色解析 +
  unitless 非零长度拒绝（upstream 2c12b5a）+ UA 默认（inline 标签清单/b·strong
  加粗）+ 继承语义（color/font-size/text-align 从父级，author 覆盖继承）。
- **cascade_element**：UA ← author（specificity→source order 排序应用）← inline；
  specificity 由我们自己的 diting_dom compile_rule_selector 提供。

**与上游的差距（诚实清单）**：537 属性 vs 我们的 ~15；无 container query/
keyframes/property 注册/层优先级/shadow scope/CSS Nesting denest（上游的
denest 处理 Tailwind v4 嵌套，我们暂丢弃嵌套块）；颜色仅 named/hex 无 rgb()。

**踩坑记档**：①测试初版断言「纯 feature 查询不适用于 print」——错，spec 规定
隐含 all，已按 spec 锁行为；②`:where(article)` 匹配的是 article 元素而非其
子元素——端到端测试语义修正；③`not print` 的 `not` 后不能 trim 字母（会把
print 也吞掉变成空串误判 all）；④嵌套规则块 `&:hover{}` 会以 `(选择器, 体)`
形式漏进声明对，需按 value 含 `{` 过滤。

372 测试全绿（352→372），双构建 0 警告。下一步=批次 2（dom.rs 布局桥）或先
把 diting_css 对接 screenshot.rs 做「双引擎对照」（Blitz Stylo vs diting_css
在同一页面的 computed style diff），后者能提前暴露语义分歧。

## 10. 双引擎对照完成（2026-08-23）：diting_css vs Stylo 同页 computed diff

批次 1 的验收动作。`src/screenshot.rs` 新增 `mod cross_check`（仅测试编译，
~250 行）：同一 HTML+stylesheet 喂两个引擎——Blitz 的 Stylo（经
`stylo_alias 0.20` 直连 blitz-dom 的 BaseDocument，与 blitz 钦定版本一致）
vs 我们的 diting_css cascade_element——逐元素逐属性比对 computed style。

**结果：3/3 测试全绿，总计 373 测试。**

覆盖三组场景，全部一致：
- display + 颜色链（named/hex 解析、color 继承、background set-flag、
  font-weight 数值）
- @media 求值（viewport 宽度驱动断点切换，两引擎同判）
- 继承语义（父 color/font-size → 子继承；author 覆盖 UA）

**三处建模差异（非 bug，已锁进断言语义）**：
1. **初始值物化**：Stylo 物化所有初始值（background 返回 transparent 而非
   None、font-weight 恒有数值默认 400）；我们 None=未声明。→ background 比
   set-flag 不比值；font-weight 用 `unwrap_or(400)`。
2. **UA sheet 差距**：Stylo 的 UA 层给 h1/p 等默认 margin（16px 等），我们
   UA 层只有 display/bold。→ margin 仅在显式声明时比较。
3. **颜色空间**：Stylo 已解析成 AbsoluteColor（components 0..1 f32）；
   我们存 u8 rgba。→ 转 u8 后比较，容差天然为 0。

**stylo 0.20 API 备忘**（本次全部踩过后修正，凭记忆写必错）：
- `ElementData` 经 Deref 直接 `.styles`，无需 `.get_styles()`
- display 判断用 `Display::outside()/inside()/is_none()`，别比枚举变体名
- 颜色：`get_inherited_text().color` 是**已解析** AbsoluteColor
  （components 0..1 f32 + alpha f32），resolve_to_absolute 不存在于该类型
- margin 字段是 margin_top/bottom/left/right 四个独立字段，类型
  `GenericMargin<LengthPercentage>` **枚举**（LengthPercentage/Auto/
  AnchorSizeFunction），match 解包
- padding 是 `NonNegative<LengthPercentage>` 结构体 tuple，取 `.0`
- text_align 走 `clone_text_align()` → TextAlignKeyword
- LengthPercentage 判长度用 `.unpack()` → `Unpacked::Length(l)`,
  `l.px()`；`px()` 在 calc 上 panic，测试值避开

这层对照是后续批次的护栏：批次 2 起 diting_css 若接产品管线替换 Stylo，
任何 computed-style 回归都会在这里先炸。

## 11. 批次 2a 完成（2026-08-23）：taffy fork 净变更提取 + 分类

批次 2（布局桥）的第一刀：把上游 vendor/taffy 的 fork 工作吃透并对照
我们产品管线钉的 stock taffy 0.13.0（blitz 864b4fd）。

**提取方法**：crates.io 拉 pristine 0.12.1 与 vendor/taffy diff 得**净变更
1796 行 / 12 文件**。git 补丁链有 24k 行（11 commits，2026-07-26 → 08-04），
说明这些 commit 是同一批代码的反复迭代——按净 diff 理解比按 commit 逐个读
高效得多。

**八主题分类**（全部对照 0.13.0 逐标记物+语义验证）：

| # | 主题 | 文件 | 0.13.0 状态 |
|---|---|---|---|
| 1 | float clearance 几何化：`last_float_top`（源序不上升）+ `lowest_float_bottoms[2]` 替代段索引 high-water-mark；段细分不再破坏 clearance | float.rs | **部分**：0.13.0 有 `clear_bottoms` 但保留索引式 `last_placed_floats`/`update_last_placed_float`，源序约束仍靠段索引 |
| 2 | margin-collapsing 元数据进 ComputeSize 输出与测量缓存（MeasureOutput + vertical-margin 上下文键） | block.rs, cache.rs | **无**：measure cache 仍存裸 `Size<f32>` |
| 3 | 首选宽高比传递：box-sizing 感知 adjustment、`item_aspect_ratio_is_intrinsic`（固有比例恒 content-box）、flex 最终主尺寸重传递（Flexbox 9.4） | flexbox.rs, grid_item.rs, alignment.rs | **无**（matrix 测试 5/8 格分歧） |
| 4 | `normal` 对齐关键词（新枚举变体+parse/serde）：布局模式相关默认值的来源保留 | alignment.rs, flexbox.rs, grid/mod.rs | **无**（枚举里没有 Normal） |
| 5 | grid auto margin → fit-content（min-content floor + available clamp）+ 溢出时 auto margin 压过 unsafe self-alignment | grid/alignment.rs | **无**（breakable 矩阵 7/7 分歧） |
| 6 | track 分发上限：已到限 track 跳过（含 item_incurred_increase） | track_sizing.rs | **无**（未运行时验证，代码级确认） |
| 7 | grid item 固有尺寸遏制（per-axis `intrinsic_size_containment`） | grid_item.rs, style/ | **无** |
| 8 | calc() resolver 注入：`set_calc_resolver(fn(*const (), f32) -> f32)`——taffy 视句柄为不透明，宿主注入求值（render.md §2.1 的「calc 经 resolver 注入」即此） | taffy_tree.rs | **无**（0.13.0 `resolve_calc_value` 仍是返回 0.0 的桩） |

**运行时分类**（`src/diting_layout/mod.rs`，7 测试全绿，380 总数）：fork 自带
的回归场景移植到 stock 0.13.0 公开 API（measure 签名 0.12→0.13 从五参闭包
变为 LayoutInput/LayoutOutput，已适配）。每条断言锁 stock 实际行为，fork 期望
在注释里——**上游 taffy 哪天吸收了某个修复，对应断言即失败并精确命名变化**。

分歧清单（stock ← → fork）：
- replaced 元素 natural 100x50 ← → stretch 300x150（`normal` 语义缺失）
- ratio+stretch 矩阵：8 格中 4 格分歧；最戏剧的是 justify START + align 默认
  → stock 塌回 natural 100x50（fork 400x200）
- 显式 block stretch + ratio：stock 忽略 stretch（300x150 ← → 400x200）；
  双轴 definite 时 stock 仍用 ratio 覆写高度（300x150 ← → 300x200）
- definite inline 120 + block stretch：stock 留 ratio 推导的 60 ← → 200；
  definite block 80 + inline stretch：stock 推导 160 ← → 300
- **fit-content（最尖锐）**：breakable（min100/max600）在 300px track，7 种
  auto-margin/非 stretch 组合 stock 全部 600（原始 max-content 直出，无
  available clamp）← → fork 全部 300；author min-width 350 时 stock 600
  ← → fork 350
- flex 主尺寸 ratio 重传递：auto cross size 下 stock 用 measure 的 10px
  ← → fork 用最终主尺寸 300/2=150
- 一致项：unbreakable min-content floor（600 溢出保留 + 无 auto margin 时
  stretch 300）、author max 250、content-box 边缘 290——stock 的 max-content
  直出路径在这些场景恰好同值

**feature 合并护栏（关键坑）**：taffy 直依赖必须精确镜像 blitz workspace
声明（`default-features = false`，**无 float_layout**）——cargo 全图统一
feature，多开任何 feature 都会静默改变产品管线布局。`taffy_tree` 已在统一图
中（lock 里 taffy 带 slotmap），显式列出是 no-op。380 全绿（含基线图集）
证明零扰动。

**对路线的意义**：fork 是一整包 CSS 正确性修复（Blink/Gecko 对齐语义），
stock 0.13.0 基本没吸收。切换判据达成时若采用 obscura 渲染层，vendor taffy
（0.12.1+fork）随行；若维持 blitz，这份分类清单就是「上游 taffy 哪天该重新
评估」的活探针。

下一步=批次 2b：dom.rs 布局桥本体（8-pass 修复链、table→grid 外层协商、
text-align block→flex-column 提升）的最小竖切 + 与 blitz rect 对照。

## 12. 批次 2b 完成（2026-08-23）：布局桥最小竖切 + blitz rect 对照

上游 dom.rs 的 DOM→taffy 映射抽成最小竖切，落在
`src/diting_layout/mod.rs`（fork_deltas 测试模块之外的正式代码）：

**桥本体**（约 230 行，全部只读侧——不接产品管线）：

- `layout_dom(tree, styles, viewport_width) -> HashMap<NodeId, Rect>`：
  从 `<html>` 起整树布局，rect=绝对 border-box 坐标。提取=DFS 沿 taffy 树
  累加 location（子 location 已含父 border+padding 偏移，与 blitz-paint
  同一累加语义）；rounding 保持 taffy 默认开（对齐 upstream + blitz）。
- `to_taffy_style`：display 角色映射（Block/Flex/Grid/None 直译；
  **Inline→flex-row-wrap** = IFC 替身，upstream 模型）；margin/padding px；
  width/height 走 **content-box→border-box 换算**（CSS 初始 box-sizing 是
  content-box，taffy 全 border-box，子集还没有 box-sizing 声明，先按初始值
  把 padding 加回去——border 未建模）；`text_align: center/right` 把 block
  提升 flex-column+align_items（upstream promote_for_alignment）。
- 文本=确定性词叶（upstream build_word_leaves）：每词一个固定尺寸 taffy
  leaf，`text_width`=0.55em/ASCII + **1.0em/CJK** + bold×1.08；line-height
  =1.2em；空白 token 塌成单空格、高度 0；**CJK 逐字成 token**（UAX#14 每个
  表意文字后可断行——upstream 按空白切，CJK 段落会成一个不可断的巨词，
  这是我们对上游的刻意偏差）。
- inline run 聚合：一个 block 的 text + 可展平 inline 子元素（span 等）
  的词全部拼进**一个** flex-row-wrap wrapper；inline 元素本身不占盒。
  display:none 整子树跳过。
- `font_context` 沿祖先链取有效 font-size/weight（默认 16/400）。

**对照测试**（`bridge_cross_check`，5 个）：blitz 侧复用 `element_rect`（已
开 pub(crate)），我方侧 inline 复刻 cross_check 的全树 cascade（含 UA
display/bold + 继承链）。双侧同 taffy（同一 lock 的 864b4fd），**断言差异
只能来自桥建模，不是布局算法**：

- `authored_boxes_match_blitz`（核心）：body{margin:0} + 全 authored 尺寸
  页面，html/body/#a/#b/#c 五元素 x/y/w/h **双侧 ±0.51** 全对上——含
  300×50 content + 10px padding = 320×70 border-box（验证 content-box 换算）
  和块级堆叠。文本驱动尺寸（真实字形 vs 启发式）不做双侧比对，这是刻意
  边界。
- `display_none_skips_subtree`：子树消失不留间隙，与 blitz 位置一致。
- `cjk_wraps_per_glyph`（我方结构锁）：200px 盒 12 个 20px 表意字 → 10/行
  ×2 行 ×24px = 48 高。
- `inline_run_is_one_wrapper`：span 展平无盒，p 高度=单行。
- `text_align_center_promotes`：centered run x=(200−18)/2（17.6 取整 18）。
  **已知偏差入档**：真 CSS 里 centered block 的 *block* 子元素仍拉满宽度，
  flex-column 替身会 shrink-wrap 居中——inline 内容（text-align 真正管辖
  的对象）行为正确，block 子元素是替身的已知局限，upstream 同款。

**taffy 0.13.0 API 备忘**（批次 2b 踩坑）：`size: Size<Dimension>`（不是
LengthPercentageAuto；word leaf 固定尺寸用 `Dimension::length`）；margin=
`Rect<LengthPercentageAuto>`、padding=`Rect<LengthPercentage>`，但本模块自己
的 `Rect` 会遮蔽 prelude 名，要写全 `taffy::geometry::Rect`；block_layout
feature 下 Style 有原生 `text_align` 字段（LegacyLeft/Right/Center）但只对
比容器窄的 block item 生效，不解决行内对齐，提升替身仍然必要。

**验证**：`cargo test --features screenshot` 385 全绿（380+5）。附带事实：
本机磁盘满了（Data 卷 100%，350Mi free），清了 target/debug/incremental
（1.4G，纯加速缓存）才链得上。

**边界（下一批吸收）**：float/table/multicol 修复链、replaced 元素
（img/SVG/video）、position:absolute、em/rem/% 长度、flex/grid 属性透传
（现在 flex/grid 容器只有 display 映射，没有 gap/flex-direction/track 等）。

## 13. 批次 2c 完成（2026-08-23）：flex/grid 透传 + replaced 元素

**diting_css 属性增量**（全非继承，px/fr-only）：flex-direction / flex-wrap
（nowrap|wrap）/ justify-content（6 值）/ align-items（4 值）/ flex-grow /
flex-shrink / flex-basis（px）/ gap（长短手）/ grid-template-columns|rows
（`1fr 2fr 100px auto` 词表解析；minmax/repeat 记边界）。关键设计：**真
align-items 与 text_align 分离**（upstream 同款——text-align 声明永不改变
flex 子元素尺寸；桥里 align/justify 只在 Flex/Grid 容器上生效，Block 上
无效）。

**桥透传**（to_taffy_style）：直译到 taffy 枚举（AlignItems::STRETCH 等
legacy 常量）；gap 默认 0；grid track 映射 `1fr`→minmax(auto,fr)（FromFr）、
px→定长（FromLength）、auto→AUTO const，`.into()` GridTemplateComponent。

**flex/grid 子元素分类修正**：Flex/Grid 容器是 atomic_container——每个元素
子级 blockify 成独立 item（CSS flex-item blockification），inline 展平只在
block/inline 格式化上下文发生；连续裸文本仍聚成一个匿名 run wrapper（=
CSS 匿名 flex item）。

**replaced 元素**（img/video/iframe/canvas/object/embed）：
- natural size = HTML width/height 属性；无属性 → CSS 默认 object size
  300×150。CSS width/height 逐轴覆写。
- **语义发现（探针实证）**：属性=presentational hint 声明——CSS `width:100px`
  + `height=200` 属性 → **100×200 不是 100×50**（两轴都 declared，ratio 不
  推导）。双侧一致。
- taffy 侧 `item_is_replaced=true` + `aspect_ratio=nat_w/nat_h`（属性自洽，
  无覆写冲突）。
- inline-level（UA/author 声明 inline）时进 run 当原子 token（像个肥词）；
  block-level（我们 ua_display 的 img 默认）/ flex-grid item 时直接子级。

**对照结果**（4 新测试，389 全绿）：
- flex row+gap / column+gap / justify-content:space-between：13 元素 xywh
  双侧 ±0.51 全对。
- flex-grow 分配（200/100）+ align-items:center 交叉轴居中：双侧对上。
- grid 1fr 2fr 行列 + 二行回绕：双侧对上（track 数学=同 taffy）。
- img 属性尺寸 200×100、CSS 覆写 100×200：**尺寸双侧一致**。

**入档偏差**：
1. img 位置不 cross-assert：我们 UA 把 img 建模为 block-level（y=0 堆叠），
   真 CSS/blitz 是 inline-level 坐文本基线（strut ~2px 偏移，x 也有词间隙）。
2. 无尺寸 img：我们 300×150（CSS 默认 object size），blitz 无网络时 0×0
   （无 intrinsic size 可用；真页面图加载了就有）。两侧都自洽，不比。

**边界（下一批）**：float/table/multicol（float 需 taffy float_layout feature，
当前刻意不开——开了会动产品管线）、position:absolute/inset、em/rem/% 长度、
min/max-width、minmax()/repeat()、CSS aspect-ratio 属性、inline-block。

## 14. 批次 2d 完成（2026-08-23）：position:absolute + min/max + aspect-ratio

**diting_css 增量**：position（static/relative/absolute/fixed）、
top/right/bottom/left（px）、min/max-width/height（px）、aspect-ratio
（`1.5` 或 `16 / 9`，auto→None）。

**桥**：
- to_taffy_style 透传：taffy 没有 static——in-flow 一律 Relative（它的
  默认）；absolute/fixed→Absolute；inset 四边（未声明边 auto）；min/max
  走 content-box 换算（+padding）；aspect-ratio 有限正数才透传。
- **containing block reparent（upstream 的 fix-up 复刻）**：taffy 的
  absolute 相对**直接 taffy 父级**解析（block.rs:987 静态位=父 content
  box inset），CSS 语义是最近 positioned 祖先。桥在整树 build 后做一轮
  reparent：每个 absolute 子盒 `remove_child`+`add_child` 到最近
  position≠static 祖先的 taffy 节点；fixed / 无 positioned 祖先 → root
  （ICB 替身）。out-of-flow 子元素不进 inline run（abspos blockification）。
- rect 提取沿 reparent 后的 taffy 树累加，天然正确。

**对照结果**（4 新测试，393 全绿）：
- relative：偏移 (20,10)、后续兄弟占静态位 y=30——双侧 ±0.51。
- absolute 双轴 inset stretch（top/bottom/left/right → 240×160 @ (20,10)）
  ——双侧全对。
- min/max-width（200px 容器内 min 300 → 300；auto stretch 800 → max 100）
  + aspect-ratio（width+2/1→50 高；height+0.5→30 宽）——双侧全对。

**重大发现（blitz 的 abspos 缺口）**：blitz 把 absolute/fixed 锚在**静态
父级**上——`#abs { left:15 }` 在 margin-left:30 的静态父里落 x=45（=
静态父偏移+inset），fixed top:8 会带上塌陷 margin 变 48。CSS/Chrome/本桥
= 最近 positioned 祖先 / viewport（15/8）。即 **blitz（DioxusLabs）的
taffy 桥没做 nearest-positioned-ancestor reparent**。对照测试里 abs/fixed
位置只锁我方语义（不 cross-assert——锁别人的 bug 没意义），#gp/#mid 等
in-flow 元素仍双侧对。这对产品 /screenshot 有实际含义：真实页面 overlay
/modal（absolute/fixed）在 blitz 截图里可能偏移。值得给上游提 issue 或
本地补丁（复刻我们这轮 reparent 逻辑到 blitz-dom）。

**定性完成（2026-08-23 续）**：确证为 blitz/taffy 的**真实布局缺口**，
非我方测量伪影（`absolute_origin` 沿 layout_parent 链求和，与 blitz 自身
布局自洽）。证据链四条：
1. blitz 的 taffy 树就是 DOM 树——`BaseDocument` 直接实现
   `taffy::CacheTree`，`child_ids()`（layout/mod.rs:346）返回
   `node.layout_children`（`collect_layout_children` 从 DOM children 构建）；
   全仓无 `add_child/set_children`（没有独立 taffy 树可 reparent）。
2. `layout_parent` 无差别设成 DOM 递归父级（resolve.rs:278/304），absolute
   无特判。
3. taffy 语义（block.rs:725-742, 991-996）：`Position::Absolute` 子元素在
   **taffy 父级**的布局函数内解析，锚定父级 padding box；静态位 fallback
   = 父 content box inset。→ 最近 positioned 祖先解析归**建树方**。
4. 探针实测（rev 2fa6434d，viewport 800×600）：positioned 祖父+静态中间父
   → #abs blitz (45,6) vs CSS (15,5)；无作者 CSS 的默认页面
   （UA `body{margin:8px}`）→ `fixed;top:0;left:0` blitz **(8,16)** vs
   CSS (0,0)；静态父流起点恰为 0 时巧合正确（bug 隐身）。

**上游现状**：DioxusLabs/taffy#212（2022 年至今 OPEN）——taffy 维护者定性
为「未支持特性」：taffy 不建模 `position:static`，最近 positioned 祖先解析
归建树方。DioxusLabs/blitz#690（krazyjakee 提，ICB 口味）被关成它的 dup。
但 #690 的口径是「等 taffy 修」；**blitz 自己在建树时 reparent 即可修**
（先例：obscura-render `reparent_inset_positioned_nodes` +
`resolve_static_positions_and_reparent`，crates/obscura-render/src/dom.rs，
两遍法=先在流内采集静态位再 reparent；reparent 后 layout_parent 必须跟随，
否则 paint/hit-test 的 origin 走错链）。最新 main（d8e860a，#759 升级
taffy）31 个新提交无 abspos 修复、代码结构未变。
**处置（遵守不 fork 原则）**：不改 pin、不打本地补丁；向上游提 issue
（草稿 /tmp/blitz-abspos-issue.md——新角度=blitz-dom 侧可修，非 taffy#212
dup），等修复进 release 后随 parley CJK 修复一起评估升 pin。产品侧无
本地缓解手段（布局在 blitz-dom 内部发生）。

**边界（下一批）**：em/rem/% 长度（值系统重构）、float（需 float_layout
feature）、table/multicol、inline-block、absolute 无 inset 轴的 static
position 回填（upstream StaticPositionCandidate 路径）、sticky。

## 15. 批次 2e 完成（2026-08-23）：em/rem/% 长度——值系统重构

**diting_css 值系统**：
- 计算值 `Length { Px(f32), Percent(f32) }`——em/rem 在级联时折叠成 px
  （CSS 计算值语义），% 保持符号交给布局引擎（used-value 期解析）。
- 解析层私有 `CssLength { Px/Em/Rem/Percent }`（`parse_css_length`：
  px/em/rem/%、无单位 `0`；**rem 在 em 之前判**——"rem" 也以 "em" 结尾）。
- `FontCtx { own, root }`；`resolve_len` 折叠 em→own、rem→root。
- 字段换型：margin/padding Sides、width/height、四 inset、四 min/max
  → `Option<Length>`。gap/flex-basis/grid tracks 刻意仍 px-only（边界）。

**font-size 预遍（本批核心机制）**：CSS 规定 font-size 先于其它一切属性
计算（em 长度依赖它，与块内声明顺序无关）。cascade_element 在 apply 前
按**同一胜出序**（specificity 排序 candidates + inline 最后）扫一遍
font-size 声明，算出 own_fs：em/% 对**父** fs、rem 对根 fs。然后所有
em 长度对 own_fs 折叠。cascade_element 签名 +`root_font_size` 参数。

**root font-size 线程**：our_styles 把根元素（document 直接子元素）的
计算值 fs 作为整个子树的 rem 基传下去——`html { font-size: 20px }` +
`width: 10rem` → 200。screenshot.rs 的 cross_check 路径固定 16（其
fixture 不设根 fs）。

**桥映射**：lp / lpa_zero（margin 缺省=0）/ lpa_auto（inset·clamp 缺省
=auto）三个闭包；% → `taffy LengthPercentage(Auto)::percent(p/100)` 直通。
**taffy 的 % 语义与 CSS 一致**（block.rs:696/703 实证：margin-top/bottom
% 对 `parent_size.width`；inset 按轴对 area_width/height；padding 对宽）
——所以 % 交给 taffy 算，双侧同引擎对照即锁我方透传映射。

**font_context 语义修正**：级联后每个元素都带 resolved fs（不再 None），
祖先链查找从"最后命中"改为"**最近命中即停**"（否则 html 的 16 会盖掉
#zh 的 20——cjk_wraps 测试当场抓到）。fs 与 weight 各自独立找最近。

**stylo_view 投影**：px_len 双态化——stylo 的 computed LengthPercentage
Unpacked::Percentage → `Length::Percent`（stylo 同样把 % 留到 used-value）。

**对照结果**（4 新对照测试，401 全绿）：
- em：`#outer{font-size:24px}` + `#inner{font-size:2em; width:5em;
  height:1em}` → fs=48、240×48，双侧 ±0.51。
- rem：默认根 16 → `12.5rem`=200 双侧；authored root 20 → `10rem`=200。
- %：400×100 容器里 `width:50%; margin-left:10%; height:50%;
  margin-top:5%` → (40,20) 200×50——**margin 双轴都对 CB 宽**，双侧。
- % inset（positioned 父直子，blitz DOM-parent 锚恰好=CB 的形状）：
  left:10%/top:25% → (30,50) 双侧；`width:100px; min-width:60%` → 240
  （min-width 必须配显式小 width——block 自动拉伸会吞掉 floor）。

**边界（挂账）**：% 尺寸+px padding 无混合形状（% 按 border-box 语义
直通，不做 content-box 加算——authored box-sizing 本就是后续批次）；
gap/flex-basis/grid tracks 的 em/%；line-height 仍固定 1.2 系数（真实
LineHeight 模型=批次 3）；ch/ex/vh 单位。

## 16. 批次 3a 完成（2026-08-23）：真实字形测量 + fixture 字体双侧钉死

**问题**：批次 2b 的词叶模型用确定性猜测测宽（ASCII 0.55em/字、CJK
1em/字、粗体 ×1.08）——结构对、数值假。文本派生 rect 对照 blitz 时，
CJK 恰好 1em 全对，Latin 全差 1-2px（"hello world WebKit" 猜 140.8、
blitz 142、真值 141.184）。要让对照测试有意义，测量必须是真字形。

**等价链（先定性再动手）**：
- blitz（rev 2fa6434d）的 parley 0.10 用 **harfrust**（Rust HarfBuzz
  移植）整形；我们选 **swash**（上游 obscura-render 文本栈同款）。
- uharfbuzz 权威验证：两 shaper 对 fixture 字体的 "hello world"@16
  输出 **84.032px 浮点相等**（含 GPOS kern -7u/1000em）。CJK 无 kerning
  → 按构造一致。测量侧不存在引擎差异，差异只在舍入模型（见下）。

**blitz 文本宽度语义（探针 7/7 + 空白 3/3 定性）**：
- 宽度 = **ceil(精确 advance 和)**——不是 taffy 的 round-to-nearest。
  141.184 → blitz 给 142；frac<0.5 时两种舍入必差 1px，这就是旧模型
  Latin 全差 1px 的根因。
- run 首尾可折叠空白丢弃（`" "`→0 宽，`"hello "`→同 `"hello"`）；
  断行点前导/行尾空格丢弃（CSS 尾随空白移除）。
- 行高仍 1.2×fs（blitz-dom/src/layout/mod.rs:76 钉死，不从字体 metrics
  推导）——parley 的 quantize 只影响垂直 metrics（ascent/descent
  Chrome 式取整，line_break.rs:1108），不影响宽度。

**fixture 字体双侧钉死**：Noto Sans SC（OFL）变量字体 instancer 出
wght=400/700 两个静态实例，pyftsubset 到测试字符集（~139KB×2 入仓，
`scripts/make_font_fixture.py` 可复现；测试用到字符集外的 CJK 会
.notdef 当场炸——往脚本字符集加字重跑）。我侧 `FontBook`（text.rs，
swash ShapeContext thread-local 复用）直接吃字节；blitz 侧
`DocumentConfig.font_ctx` 注入 `system_fonts: false` 的 fontique
Collection + `register_fonts(Blob, FontInfoOverride { family:
"DitingFixture", weight })`——双侧同一份字节、同一 family，测试
stylesheet 前置 `body { font-family: DitingFixture }`。无系统字体、
无网络、无 @font-face。

**模型重构（mod.rs）**：
- `TextLeaf::Run { text, font_size, bold }`：**纯文本 run = 单个
  measure 叶**，结构对齐 blitz 的"文本节点 = 一个 parley 测量的叶"。
  `measure_text_leaf`：贪心断行（空格 pending、断点丢弃）、
  min-content 返回最宽 token、宽度 ceil、高度 = 行数 × 1.2×fs。
- 混合 run（文本 + inline 元素）**回退批次 2b 的 flex-row 词叶模型**
  （已知边界：词叶各自 round 而非整 run ceil，挂账）。
- 确定性 `text_width` 删除；`TaffyTree<()>` → `TaffyTree<TextLeaf>`。

**taffy measure 闭包的坑（本批最贵的教训）**：
`compute_layout_with_measure` 的闭包对**所有 childless 节点**触发
——不只带 context 的。`None` 分支返回 HIDDEN 会把全部普通叶
（replaced、空 div、词叶）清零，13 个测试同时炸。正确写法是
`None => taffy::compute_leaf_layout(inputs, style, |_,_| 0.0, |_,_| Size::ZERO)`
——即 stock `compute_layout` 内部用的同一个函数（taffy_tree.rs:906）。

**对照结果**（4 新测试，405 全绿）：
- CJK 四字 × 四字号 advance=fs±0.01（swash 直测）。
- "hello world WebKit" shrink-wrap：ceil(141.184)=142 = blitz 142。
- "加粗Bold文本"@20 w700：混排让宽度对面重敏感，ceil(125.32)=126 =
  blitz 126（前置断言 bold_w > reg_w+1 防退化为合成加粗）。
- 你好world测试engine渲染真实@200px/20px：断行后高度对照 blitz +
  整数行数×24 结构锁。

**边界（挂账）**：混合 run 的 flex-row 回退（见上）；ch/ex/vw/vh；
line-height 真实 LineHeight 模型（仍 1.2 系数）；FontBook 单 family
双 weight（500/800 snap 到近邻，也是 fixture 唯会触碰的范围）。

## 17. 批次 3b 完成（2026-08-23）：文本光栅——swash scale/render + baseline 对照 blitz

**baseline 模型（先读 parley 源码再写代码）**：blitz 的 baseline 完全由
parley 0.10 的量化行盒决定（line_break.rs:1103-1129，quantize=true）：
round(ascent) 与 round(descent) **分别取整**→leading = lh−(a+d)→
above = floor(leading/2)、below 拿大半（Chrome 语义）→
baseline = round(line_y) + a + above。run 的 ascent/descent 来自 **skrifa
字体级 metrics**（data.rs:445），与 line-height 无关——
`FontSizeRelative(1.2)` 只产生 line_height。**Noto SC 1.448em 自然 extents
超过 1.2em 行盒**：leading 为负，baseline 恰落在 fs（12/16/20/24 全档
验证=fs 整），字形墨迹溢出行盒顶——Chrome 式 CJK 拥挤观感，数值级复刻。
注意 swash 的 `Metrics.descent` 报**正值**（基线以下距离），我们 normalize
成「正数朝下」约定，别让它泄漏成负数。

**光栅**：`FontBook::rasterize(text, fs, bold, color) -> TextRaster`
（RGBA tile + baseline 行位 + `ink_bbox()` 50% 覆盖率墨迹框）。链路 =
swash ShapeContext 整形（逐 glyph id/advance/offset）→ ScaleContext +
`Render::new(&[Source::Outline])`（Format::Alpha 默认）→ A8 coverage
max-blend →着色。**placement 坑（最贵）**：swash 光栅走 zeno
`Origin::BottomLeft`，`placement.top` 是图顶相对 pen 的**向上**距离——
blit y = pen_y − top（写成 +top 会整块落到 tile 外，tile 尺寸恰好合法
所以不炸只空）。

**对照面定义**：blitz 的字形光栅是 **vello_cpu 路径填充**（非 swash，
anyrender_vello_cpu scene.rs:137 glyph_run），rasterizer 不同→像素全等
不可能。契约 = **ink 轮廓**：基线上墨迹顶/基线下墨迹底/左右墨迹边，
±2px（50% coverage 阈值双侧同规）。blitz 侧复用 screenshot.rs 的
paint 路径（render_to_buffer + paint_scene）+ fixture 字体注入；
测试 fs=16/24 两档过——错 baseline/错 metric 源/错 advance 都会越界。

**fixture 补字教训**：渲/染/加/粗 不在 charset → .notdef，而 **.notdef
的 1em advance 恰好骗过所有 CJK 宽度断言**（3a 的 cjk_advance_is_one_em
测的其实是 .notdef），raster 一出立刻现形（gid=0 空图）。教训入
make_font_fixture.py 注释：宽度断言不充分，字形级验证靠 ink；charset
必须与测试源同步（已补 加粗渲染真假混搭，140KB×2 重生成）。

**结果**：2 新测试（baseline_model_is_parley_quantized——metrics
1.16em/0.288em/0 + baseline==fs 四档；raster_ink_extents_match_blitz），
407 全绿。

**边界（挂账）**：单行 run（多行=measure_text_leaf 行盒 × raster 逐行
组合，后续批次）；整像素 pen 无 subpixel（vello_cpu 亚像素，ink ±2px
吸收）；无 color/bitmap 字形源（Source::Outline 单源）；无 faux bold
（synthesis 只在 shaping 层，300-500 weight 仍 snap 双 weight）。

## 18. 批次 3c 完成（2026-08-23）：CJK 字体捆入——/screenshot 确定性注入

**动机**：/screenshot 此前依赖服务器装 fonts-noto-cjk（环境依赖=不确定性
来源）；上游 obscura-render 确定性设计干脆零 CJK（中文全豆腐）——这是
我们的产品差异点，批次 3c 把它落进产品。

**bundle**：Noto Sans SC（OFL，notice 随行 diting_fonts/OFL.txt）变量字体
instancer wght=400/700 → GB2312 全量（6763 字≈全部现代简体）+ASCII
subset，2.43MB×2=+4.9MB（screenshot build 才编入，include_bytes 于
src/diting_fonts/）。`scripts/make_font_bundle.py` 可复现。

**接线（src/diting_fonts.rs）**：
- 家族名用真名 "Noto Sans SC"——中文页常显式 style 该名，直接命中捆入
  字节（诚实且实用）；
- `set_fallbacks(FallbackKey::new(Script::Hani, None), ours.chain(existing))`
  ——**set 是整体替换**，必须把内置链（系统 Noto 等）接回尾部：捆入
  优先、系统兜底（GB2312 外罕字/日韩文走系统）；
- `append_generic_families(SansSerif/Serif/Monospace)`——零字体机器上
  无 style 的文本也能解析（generic 链尾部附加）；
- system_fonts 保持 true。确定性优先、缺字优雅，fixture 全钉（双侧同
  字节）与真实页面（系统兜底）之间的工程折中。

**注入点**：screenshot.rs render_html_to_png 的 DocumentConfig.font_ctx。
FontBook::bundled（diting_fonts::font_book）给 diting 栈产品路径备用。

**验证**：`cjk_renders_without_system_fonts`——build_ctx(false)（生产
接线只关系统尾）+ font-family:serif 的中文页 paint 出 >100 墨迹像素：
**无任何系统字体 CJK 照样渲染**，这是 86quan 不装字体也能出图的证明。
`bundle_cjk_coverage_paints`——抽样 GB2312 字 advance==1em **且 raster
有墨**（3b 教训：advance 断言防不住 .notdef，ink 才咬合）。基线图集
（本地页走同一 render 路径）409 全绿未扰动——字体从 PingFang 换到捆入
Noto，锁的色彩数/区域像素指标不敏感，无需重定。

**边界（挂账）**：繁体/日文假名/韩文不在 GB2312（走系统兜底，无系统则
豆腐）；subset 无 hinting（--no-hinting，与 fixture 同策略）；release
二进制 +4.9MB（截图构建）；bold 只有 700（300-500 snap 近邻）。

## 19. 批次 4a 完成（2026-08-23）：paint 最小竖切——diting 栈出第一幅图

**动机**：批次 3 止于"测量+单行光栅"；4a 让 diting 栈从几何走向像素——
背景填充 + 多行文本出图，对照契约从 rect/ink 轮廓升级到**整页像素结构**
（bg bbox 精确 + ink 分带）。

**共享断行（单一真相）**：measure 的贪心断行抽成 `text::greedy_wrap`
（`tokens_of` 出 Token{text,width,is_space}）。measure_text_leaf 与
rasterize_wrapped 消费同一函数——**测量与绘制所见即同一组行**，3a 锁的
断言语义（边缘空白折叠、断点前空格丢弃、ceil 宽度）只剩一处定义。
空间在断点持有 pending（随词提交），行尾/断点丢弃；paint 重建行串时
kerning 差 ≤ 亚像素级，不影响 ±2px ink 契约。

**多行光栅（text.rs rasterize_wrapped）**：wrap_at 复用测量时的可用宽
（= 直接 taffy 父节点 content_box_width），baseline_i = round(i×lh) +
baseline_offset（parley 量化模型），tile 高度覆盖全部行 metrics 跨度
（±1px slack）。TextRaster 增 `top` 字段：tile 行 0 在行盒坐标的 y
（CJK 负 leading 时 <0，墨溢出盒顶=blitz 同款观感）。blit 核心抽成
`blit_line`（单行/多行共用 shaping→A8 max-blend→colorize）。

**PaintItem + Canvas（paint.rs 新模块）**：layout_dom_with_paint 在原
collect 预序遍历里顺带产出文档序 paint 项——`Bg{dom_id,rect,color}`
（进入元素即推，天然父底子面）+ `Text{...,x,y,wrap_at}`（run 叶）。
Canvas 直 alpha RGBA8：fill_rect（整数 rect、source-over、裁剪）+
blit_text。execute 重放列表即成图。**mix run（行内元素混排）本批不绘**
——它的 flex-row 词叶兜底无 run 上下文，挂账。z-index/border/图片/
radius 同挂。

**颜色链路**：cascade 里 color 本就继承，但文本节点无 ComputedStyle——
`color_context` 与 font_context 同款最近祖先步进；run 取首段节点的
color（与 fs/bold 同近似）。吸收 `rgb()/rgba()` 进 parse_color
（逗号/空格分隔、百分比通道、0-1 alpha）——旧"out of slice scope"
断言翻转为已吸收；真实页面最常见 bg 格式不再丢。

**对照（bridge_cross_check::paint_bg_and_wrapped_text_match_blitz）**：
105px 宽红底 div + 11 汉字（5/5/1 三行），双引擎同 fixture 字节整页渲染
——blitz: vello_cpu paint_scene；ours: parse→cascade→layout_dom_with_paint
→execute。断言：bg 四边精确（±1px，rect 契约穿透到像素）；ink 分带数
==3 且各带顶 ±2px；ink bbox 四边 ±2px。**首跑即绿**（修掉测试自身
band 计数 bug 后）——全链路（parse→cascade→taffy→断行→swash→合成）
与 blitz（html5ever→style→taffy→parley→vello_cpu）像素级对齐。
wrapped_raster_matches_measure_lines 锁测量/绘制 parity（band 数=行数、
tile 几何推导重算）。

**坑**：band 计数 fold 里拿"上一 band 顶"当"上一 ink 行"比较——每 2 行
开新 band（8,10,12…），必须记 last_row。Canvas 在私有模块里 pub use
未被消费会告警，等 screenshot 接线再 re-export。

**测试**：409→414 全绿（+paint 两件、+wrapped parity、+blitz 像素对照、
+rgb() 解析）。

## 20. 批次 4b 完成（2026-08-23）：border 盒绘制——第二 paint 原语

**动机**：4a 只有 bg+text；真实页面盒模型的一半是 border（卡片、表格、
分隔线全是它），且它**同时吃布局**（content inset）——双侧一起认领。

**diting_css 建模**：`border_width: Sides`（1-4 值展开复用 expand_sides，
thin/medium/thick=1/3/5px）+ `border_color`（单一色）+ `border_style`
（Solid/Dashed/Dotted/Double；**none/hidden=CSS initial→宽归零**，无
style 不设 border——`border-width: 3px` 单独写不画也不占位，与 CSS
computed 语义一致）。shorthand `border: <w> <s> <c>` 任意顺序，**先解析
进局部、全过再落地**（invalid-declaration recovery 不留部分副作用——
直接 return false 会漏写已解析分量，测试抓到）。per-side 色/样式挂账；
非 solid 样式按 solid 画（渐进近似，布局占位与 CSS 一致）。

**布局**：to_taffy_style 补 `s.border = Rect<LengthPercentage>`（taffy
0.13 border 是 LengthPercentage 非 f32）；authored size 的 content-box→
border-box 换算（size/min/max）从 +padding 扩成 +padding+border——
`width: 105px` + 6px border + 8px padding → 边盒 133。wrap_at 取
content_box_width 自动扣除 border。

**paint**：`PaintItem::Border{rect, widths[TRBL], color}`——collect 里
紧随 Bg 之后推（background-clip: border-box=bg 画在 border 之下），先于
子树。色缺省 currentColor=元素自身 computed color。execute 四带填充：
**top/bottom 通栏（拥有角），left/right 垂直内缩**——radius 0 的经典
矩形边框画法。

**对照（paint_border_and_padding_match_blitz）**：105px 内容宽 + 8px
padding + 6px solid 蓝边红底三行中文。断言：蓝带 bbox==元素边盒**精确
±1** 且尺寸恰 133×100；可见红内缩=边盒 inset 6px（±1）；ink 仍 3 带
（wrap 宽不变）、带顶随 border+padding 平移后 ±2 对齐 blitz。一次过。

**测试**：414→416。挂账：radius/图案样式逐像素/per-side 边属性/
box-sizing: border-box（现 subset 未建模，authored 一律 content-box）。

## 21. 批次 4c 完成（2026-08-23）：overflow 裁剪——clip 栈

**动机**：卡片/弹窗/文章摘要全是"定高盒子装不下内容"——overflow:hidden
是真实页面第二常见的视觉约束（仅次于 bg/text/border 三原语），且它把
paint 栈从"平铺列表"升级成"带结构状态的重放"。

**diting_css**：`overflow: Option<Overflow>`（Visible/Hidden/Clip/Scroll/
Auto，uniform 双轴；overflow-x/y per-axis 挂账）。非 visible 一律裁剪。

**PaintItem::Clip{rect}/PopClip**：collect 在进入带 overflow 的元素时、
推完该元素自己的 Bg/Border **之后**推 Clip（CSS：overflow 裁的是子孙，
自己的背景边框不受裁），递归完子树推 PopClip——扁平 item 列表按文档序
携带树的 clip 结构。clip rect=padding box（边盒 inset border 宽）。
文本 run 是 taffy 子节点，天然落在 Clip 对内。

**Canvas clip 栈**：`(x0,y0,x1,y1)` 右下开区间，`allowed()`=画布边界∩
全部活动 clip；压栈即求交，**退化交集=(0,0,0,0) 全裁**；fill_rect/blit_
text 逐像素过 clip。PopClip 弹栈。

**对照（paint_overflow_hidden_clips_match_blitz）**：105×48 定高蓝边
红底盒装 3 行文本——padding box (6,6)→(111,54) 为 clip，第 3 行墨顶
~57 全没。断言：双侧可见带==2 且带顶 ±2；**clip 底边（行 54）以下零
ink 逐像素**；元素自身 117×60 边框盒不被自己的 overflow 裁掉。一次过
——blitz 同样在 padding box 裁剪，语义对齐。

**挂账**：BFC（overflow 盒的 margin collapse/float 包含）；clip-path/
border-radius 联动圆角裁剪；overflow-x/y 分轴；scrollbar 占位。

**测试**：416→418（+clip 栈单测：相交/弹出/退化；+overflow 像素对照）。

## 22. 批次 4d 完成（2026-08-23）：混合 run 文本出图——词叶带 context

**动机**：4a 挂账"mix run 不绘"。`汉字<b>加粗</b>混合` 是真实页面最常见
的文本形态——纯 run 叶（b 内部）早就有 context 会绘，缺的是**词叶本身**
（div 直属文字走 2b 词叶兜底，plain Style 叶无 paint 信息）。

**做法**：TextLeaf 增 `Word{text,fs,bold,color}` 变体——build_word_leaves
改 `new_leaf_with_context`，尺寸 Style 照旧；measure 闭包对 Word **透传
compute_leaf_layout**（与 None 分支合流）——**布局零变化**，纯加 paint
信息。collect 对 Word 叶推 `PaintItem::Text`：x/y=叶盒原点，wrap_at=叶宽
（词叶=单 token，贪心断行永不触发，flex row 已在叶粒度换行）。颜色链：
flush_run 把该段的 color 传给词叶；行内子元素的 run 在自己的 build_element
里解析自身 color——`<span style="color:red">` 内嵌即红。

**对照（paint_mixed_run_text_matches_blitz）**：双宽度——800px 单行
1 带、80px 换行 2 带（汉字加粗/混合），带顶与 ink bbox 四边 ±2；结构断言
Text item 数==5（4 词叶+1 b run 叶）。一次过——词叶位置 3a 已证与 blitz
同模（上游同为 flex 词叶模型），本批只补上"画"。

**测试**：418→419。挂账清一项；批次 4 至此：bg+text（4a）/border（4b）/
clip（4c）/mix run（4d）——diting paint 栈与 blitz 的像素对照面覆盖
真实页面的主体形态。仍挂：radius/图案样式/per-side 边属性/img 替换盒/
渐变/z-index/BFC。

## 23. 批次 5a 完成（2026-08-23）：替换盒占位——img 灰盒 + alt

**契约探底**：blitz 无网络模式下 img 画什么？`blitz-paint/src/render.rs`
`draw_image` 以 `if let Some(image) = self.element.raster_image_data()`
开头、无数据整函数 no-op——**无占位、无 alt、无假边框**，元素只剩 CSS
给的 bg/border。所以本批拆两半：**可对照半**（img 的 CSS 背景是双引擎
共享 paint 面，像素对照）；**政策半**（灰盒+alt 是 diting 栈自己的无网
占位行为，blitz 没有，锁结构断言）。

**PaintItem::Replaced{rect, alt, fill_placeholder}**：collect 在替换元素
（img/video/iframe/canvas/object/embed——复用 is_replaced_tag）处、Bg/
Border 之后推 Replaced。alt run 在 collect 解析齐 `(text, fs, bold,
color)`（font_context/color_context 继承链，paint.rs 保持无样式访问）；
`fill_placeholder` = 作者没给可见 background_color 才填灰——有 bg 的盒
子已经"看得见"，灰盒反而盖掉作者样式。alt=""（存在但空）→ 只画盒不画
字（CSS 装饰图语义）。

**paint.rs**：Replaced 分支——`fill_placeholder` 时整边盒填 rgb(224,224,
224)；alt 非空时 `rasterize_wrapped(alt, …, wrap_at=盒宽)`，blit 于
`(盒x, 盒y + r.top)`（与其他 Text tile 同一套 cramped-CJK 顶部偏移）。

**对照（paint_img_background_match_blitz）**：100×50 img 带 rgb(198,40,40)
背景、无 alt——bg bbox 双侧精确 ±1；双侧零 ink（blitz 不画 alt，我们
有 bg 不画灰盒）。**结构（paint_replaced_placeholder_structural）**：
`width=100 alt="谛听图"`（无 height→ratio 2:1→100×50，batch 2 已证的
几何）；灰盒 bbox==(0,0,99,49) 逐像素精确；alt ink 落盒内；alt="" 降级
盒-only 零 ink。

**测试**：419→422（+img bg 像素对照、+占位结构）。挂账更新：真图绘制
（image 解码/object-fit/object-position——blitz draw_image 已有 compute_
object_fit 可抄）、alt 垂直溢出盒（多行 alt 目前允许溢出盒底，浏览器
会裁）、iframe/video 专属占位形态。

## 24. 批次 5b 完成（2026-08-23）：真图绘制——data:URL PNG 进 Canvas

**通路**：`png` crate 本就在 screenshot feature（零新依赖），`base64` 亦在。
新模块 `diting_layout/image.rs`：`decode_data_url_png(src)`——只认
`data:image/png;base64,`（真实页面 inline 图的唯一形态；plain data URL 与
其他媒体类型返回 None 走 5a 占位）；`decode_png` 归一化到 RGBA8（palette/
sub-byte EXPAND、gray 提升、缺 alpha 补 255），与上游喂给 painter 的
`RasterImageData` 同构。

**布局**：`layout_dom_with_paint` 预扫全树 img → `HashMap<NodeId,
DecodedImage>`，穿进 build_replaced_leaf/collect。natural size 语义对齐
Chrome：attrs 是 presentational hints per-axis 覆盖，**缺轴从图 ratio 派生**
（`aspect-ratio: auto w/h` 加载后语义），无 attrs 无 CSS → 盒=图尺寸；无图
才退 2:1/300×150。

**绘制**：`PaintItem::Image{rect, image}`——collect 在替换元素处优先于 5a
占位（有解码图就画图，灰盒/alt 只在无图时）。object-fit 本批只做缺省
**fill**（blitz `compute_object_fit` 的 Fill=container 分支）；rect=content
box（=边盒，替换元素 border/padding 进布局仍挂账）。`Canvas::blit_image`：
近邻采样、source-over、过 clip 栈。

**对照（三测）**：blitz 侧 harness 注入 `SpecialElementData::Image(
RasterImageData::new(同一字节))`——**注入必须先于首次 resolve**（布局期读
它派生 natural size；布局后注入不触发 relayout，blitz 0 像素——已踩坑记
档，产品路径的"图晚到"需要 damage 驱动 relayout，不在 harness 模拟）。
- `paint_img_data_url_1to1_matches_blitz`：1:1 尺寸（盒==图 100×50）**逐像素
  相等**——vello 恒等变换下采样即精确拷贝，近邻 blit 同。diting paint 栈
  第一个逐像素对照。
- `paint_img_scaled_and_natural_size_match_blitz` Case A：CSS 200×100 盒装
  100×50 图 → ×2 拉伸，近邻 vs vello 双线性边缘不同 → bbox ±1 + 象限中心
  采样精确（反 diagonal 红：`(x<w/2)^(y<h/2)`）。
- Case B：无 attrs 无 CSS → 双侧盒=图 natural 100×50，bbox ±1。

**测试**：422→426（+image.rs 单测 round-trip/拒绝、+两对照）。挂账更新：
object-fit contain/cover/scale-down + object-position（blitz sizing.rs 可
抄）；图晚到 relayout（damage 驱动）；网络图（diting_net fetch + 解码缓存
——解码目前每次 layout 重跑）；`image-rendering: pixelated` 采样策略。

## 25. 批次 5c 完成（2026-08-23）：object-fit 全家 + object-position

**CSS 侧**（diting_css）：`ObjectFit`（fill/contain/cover/none/scale-down，
初始 Fill）+ `ObjectPositionPart`（Percent/Px，关键字 left/top=0%、center=
50%、right/bottom=100% 折算成百分比）。ComputedStyle 加 `object_fit`/
`object_position` 两非继承字段；apply_one 分发 + @supports 探针同步。

**数学**（diting_layout `object_paint_rect`）：blitz sizing.rs 的四臂
`(x<1, y<1)` match 化简后恒等于 contain=min(xr,yr)、cover=max(xr,yr)；
scale-down = contain 结果宽 > 自然宽则用自然尺寸。offset =
`position.resolve(box − paint)`：百分比乘自由空间、px 直接用；初始
50%/50% 居中。cover 的 offset 可为负（画布外），contain 留 letterbox。

**裁剪语义（本批最大实证发现）**：blitz-paint render.rs `should_clip`
含 `is_image ||` —— **图片元素无条件裁剪到 padding box**（与 overflow
无关，spec 行为：replaced 内容不溢出盒）。首版对照测试抓到 blitz cover
右缘=盒缘(99) 而我们=149 未裁剪 → paint.rs Image 分支改为 push_clip(元素
盒)→blit(paint_rect)→pop_clip。fill 时 clip 无感（paint_rect==盒）。

**PaintItem::Image 变体扩为 `{rect, paint_rect, image}`**：rect=元素盒
（兼作裁剪矩形），paint_rect=collect 时按 fit/position 算出的 blit 目标。
collect 读 object_fit/object_position（缺省 fill/50%-50%）。

**对照（两测）**：
- `paint_object_fit_none_and_scale_down_pixel_exact`：none 与 scale-down
  （小图分支）都按自然尺寸绘制——scale=1 无重采样，整幅**逐像素相等**
  （含 px 偏移 `10px 20px` 用例，offset 数学独立验证）。
- `paint_object_fit_contain_cover_match_blitz`：两者都有重采样 → bbox ±1
  对 blitz + 盒内象限类采样（近邻 vs 双线性契约沿用 5b）。contain 方盒
  letterbox 到 200×100@y50；cover 方盒溢出但被裁到盒缘。

**测试**：426→427（+2 对照 −0；总数显示 427 因 image.rs 单测并入）。
挂账更新：object-position 多值语法（三值/四值带边锚定）；网络图路径的
fit 生效（图晚到 relayout 挂账不变）；`image-rendering: pixelated` 采样
策略（none/scale-down 已是 scale=1 不受影响）。

## 26. 批次 6a 完成（2026-08-23）：z-index/stacking——paint 序重排

**探底（blitz-dom damage.rs + node.rs）**：blitz 的 stacking 模型分两层。
①每个父级把子级分两路：z≠0 且 positioned（或 flex/grid item）→ 提升到
stacking context 按 z 稳定排序，画在 neg 带（先）/pos 带（后）；其余进
`paint_children` 按 paint level 稳定排序——static=0、float=1、positioned
z-auto=2（CSS 2.1 App. E step 8），同级保持文档序。②提升止于最近的
**stacking context root**：fixed/sticky、relative|absolute 且 z≠auto、
opacity<1、有 transform 等。

**实现（最小竖切）**：diting_css 加 `z_index: Option<i32>`（None=auto，
apply_one+@supports 同步）。collect 递归里 children 三桶重排：
neg(z<0 positioned, 升序) → mid(paint level: static 0 / positioned-z-auto 2,
稳定文档序) → pos(z>0 positioned, 升序)。static 上 z-index 无效（文档序）。
嵌套 stacking context root 场景挂账（blitz 提升可跨层；我们单层——常见
无 root 页面两者等价）。

**顺手修的潜伏 bug（本批最有价值发现）**：abspos reparent 遍历
`node_map.iter()`（HashMap 无序）后 `add_child` append——**reparent 后的
taffy 兄弟序是 HashMap 序不是文档序**。此前 paint 对照全没多 absolute 兄弟
所以从未暴露；Case A 首跑 ours 在 (25,25) 出红即此因。修法 = reparents
按 DOM 前序 rank 排序后再执行。布局 rect 不受影响（absolute 定位与兄弟
序无关），只有 paint 序受害。

**对照（一测三案）** `paint_stacking_order_matches_blitz`：
- A：文档中间的 relative z=2 蓝块盖过后面绿块（正 z 提升）
- B：z=-1 红块沉到先前绿块之下；文档最后的 positioned z-auto 蓝块盖住
  全部（level 2 > in-flow）
- C：static 元素 z-index:9 无效——负 margin 重叠区后来绿在上

**测试**：427→428。挂账更新：嵌套 stacking context root 跨层提升；
flex/grid item 的 z-index；opacity/transform 建 root；float paint
level 1（float 本身挂账中）。下一批候选：border-radius 或网络图通路。

## 27. 批次 6b 完成（2026-08-23）：border-radius——bg 圆角裁剪

**探底（blitz-paint css_box.rs）**：blitz 的圆角是 BezPath 四角 arc
（`corner_arc`/`ellipse`），bg 裁剪用 `border_box_path()`（圆角 border
box，background-clip: border-box）；border 用外圈/内圈两弧 annulus 填充。
半径 per-corner (rx, ry) 从 Stylo 解析；**不做 CSS scale-down**（相邻角
之和超盒时按规范要缩小，blitz 直接用、注释承认超半会 quirky）。

**实现（最小竖切：单值圆形圆角 × bg 裁剪）**：diting_css 加
`border_radius: Option<Length>`——只认 1 值语法 `border-radius: <px|%>`（%
相对盒宽；多值语法 decline）。paint.rs 加 `Canvas::fill_rounded_rect`：
逐像素判定，角带（edge×edge 带）内测像素中心到四角圆心距离 ≤ r，
十字中带恒在内；r clamp 到短边一半（CSS scale-down 规则——我们做，
对照场景留在合法区间所以不冲突）。Bg item 加 `radius: f32` 字段，
collect 解析（% × rect.width），execute 分派 fill_rounded_rect。
Border 圆角（annulus）挂账。

**坑（本批自踩自抓）**：fill_rounded_rect 首版把「middle zone 恒在内」
写成 `continue`——跳过了整个像素的绘制而非 inside 判定，中间区域全空，
中心采样点当场红变白。修为 corner zone 判定 + match None=>true。

**对照（一测两案）** `paint_border_radius_bg_matches_blitz`：
- 20px 圆角方盒：中心+边中点填充、深角落白、对角线弧外点
  （r−r/√2≈5.86 → 采 (4,4)）不填充——双侧一致（AA 弧线本身不在契约内，
  硬边 vs vello AA）
- `border-radius: 50%` 方盒 = 内切圆：中心/水平轴点填充、角落白、
  对角线距心 ≈65 > r=50 不填充

**测试**：428→429。挂账更新：per-corner/椭圆（rx ry `/` 两值组）半径、
border 圆角 annulus、radius 与 overflow clip 的圆角裁剪联动、50% 非方盒
椭圆。下一批候选：网络图通路 或 alt 溢出裁剪。

## 28. 批次 6c 完成（2026-08-23）：网络图通路——字节注入 + 解码缓存

**设计**：diting 侧不碰网络——fetch 是调用方的事（截图管线的
`prefetch_render_resources` 已用页面自己的客户端拉过 img body），layout
只消费字节。image.rs 加 `ImageCache<'net>`：`resolve(src)` 处理两种源——
data:URL 首见解码后缓存；http(s) URL 查注入的字节表
（绝对 URL → body）再 PNG 解码 + 缓存。表 miss 或非 PNG body → None，
img 保持 5a 占位；无表时 http(s) 恒 None。

**缓存语义**：解码结果 `Arc<DecodedImage>` 按 src 去重——同 src 的 N 个
img 和 layout 重跑都共享同一 Arc（单测锁 `Arc::ptr_eq`），兑现 §24 挂账
的"解码每 layout 重跑"。PNG 判定按字节签名 sniff，不信 Content-Type/URL
后缀。

**接线**：新入口 `layout_dom_with_paint_and_images(..., network_bytes:
Option<&HashMap<String,Vec<u8>>>)`；原 `layout_dom_with_paint` 签名不变
（传 None）。scan_images 改走 cache.resolve。产品接入点：未来 diting
渲染接管截图时，把 PrefetchedResources map 直接递进来即可。

**对照**：`paint_img_from_network_bytes_matches_blitz`——http src +
字节表的 img 与 blitz 吃同一 RGBA 的 data:URL 版**逐像素相等**
（1:1 尺寸）；结构断言 Image item 存在。

**测试**：429→432（+2 image.rs 单测 +1 对照）。挂账更新：JPEG/WebP
解码器；相对 URL 解析归一（现在要求调用方给绝对 URL key）；srcset/
picture 源选择；HTTP 缓存头（ETag/max-age）层。

## 29. 批次 6d 完成（2026-08-23）：JPEG 解码——魔数分派进 ImageCache

**探底（blitz-dom net.rs `ImageHandler::parse`）**：blitz 解码走
`image::ImageReader::with_guessed_format().decode() → into_rgba8()`——
image crate 0.25 全格式。`image` 0.25 已在我们依赖图里
（blitz-dom/anyrender_svg 拉入），加 direct optional dep
（default-features=false, features=["jpeg"]）零新增编译负担。

**实现**：image.rs 加 `decode_jpeg`（`load_from_memory_with_format(Jpeg)`
→ RGBA8）+ `decode_bytes` 魔数分派：PNG 签名 `\x89PNG` → decode_png，
JPEG `FF D8 FF` → decode_jpeg，其他 None；ImageCache 的 http(s) 路径改走
decode_bytes。data:URL 路径保持 PNG-gated（真实页 inline 图唯一形态；
网络 JPEG 走字节表）。两引擎同一解码器 → RGBA 位相同。

**对照/单测（+2 到 434）**：
- `jpeg_decodes_and_sniffs_by_magic`：编码实 JPEG → sniff 解码尺寸对、
  GIF 魔数拒绝、`image/jpeg` data URL 恒拒（路径分工）
- `paint_img_from_network_jpeg_matches_blitz`：https src + JPEG body 进
  字节表 → 我们绘制 vs blitz 用同款 image 解码再注入——**逐像素相等**
  （纯色图规避重采样）

**挂账更新**：WebP/GIF（同款分派可扩）；渐进式 JPEG 大图性能（zune-jpeg
已是快速实现）；srcset/picture；HTTP 缓存头层。下一批候选：alt 溢出裁剪
或 iframe/video 占位形态。
