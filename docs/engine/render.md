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
