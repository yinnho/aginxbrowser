# archify 吸收地图：算法 → 我们怎么写

工作参考，随实现批次演进。credit 链必须随行：**archify（MIT, tt-a1i）**，其本身 based_on
**Cocoon-AI/architecture-diagram-generator v1.0（MIT）**。红线同 obscura 先例：对���只说
"设计参考 archify"，不称自研。全自主开发原则：学契约学算法、Rust 重写、不 vendor 代码。

## 0. 总纲：技术先行 → 流程 → 精美

精美是最便宜的事（常数表+一层皮），前提是骨架已稳；骨架错了精美就要在五处重做五遍。
分层与投资顺序：

1. **技术（机制）**：确定性图布局核心 + 质量门当验收规格 + 统一图模型 spec。
2. **流程**：诊断→自修→升级 LLM 的修复循环；sha256 receipt / 三权合一 MCP 面；
   benchmark 交付门。
3. **精美**：CSS 主题、visual preset、showcase 档门、字号/间距节奏、SIGIL 精修。
   全是数据与常数，最后一层。

archify 自身结构佐证这个分层：14.8k 模板里 4.8k 是纯 CSS；visual_preset 是 data 属性
下拉；quality_profile 是 standard/showcase 旋钮。我们与他们的分岔只在第一步——他们被
零依赖 Node 锁死，布局机制只能外包给 LLM（benchmark 里 correction_rounds 的存在就是
这个外包的失败率计量）；我们是 Rust 二进制 + taffy 在手，机制做进引擎。

**架构决定（2026-09-06 定）**：不做五套渲染器移植，做**一个自动布局引擎 + 每族薄
adapter**。机制与策略分离：

- 机制 = 分层布局（rank/排序/坐标，Sugiyama 族）+ 正交路由 + 标签摆放，~4-6k 行写一次；
- 策略 = 每族 adapter（schema struct + 约束翻译，~300-400 行/族）；
- 五族里只有 sequence 真特殊（时间轴语义）；workflow/architecture/dataflow/lifecycle
  本质都是盒子+正交箭头+不同约束（泳道/边界/相位）；
- 流程反转：archify 是"LLM 写坐标、校验器拒收"；我们是"**引擎提坐标、校验器验收、
  引擎自修（verifiedAutomaticRouteFix 那套机制为参考）、LLM 只管残差**"，保留
  via/pin/fromSide/authored-y 作 escape hatch。spec 零坐标、纯语义，firstPassUsable
  理论上高于 archify（几何失败源头消失）；
- 节点尺寸用 diting 真排版量测（taffy+diting_fonts 真字形步进），不用他们
  widthFactor 0.6 估算——引擎自带的红利。

确定性纪律（receipt/cache 的前提）：无时间戳、无随机、所有启发式 stable 排序
（stableText/stableCompare 照搬其语义）、同输入同字节同 sha256。

## 1. 管线全景（他们的 → 我们的）

```
archify:  LLM 写 JSON(含坐标) → node validate (schema+渲染期检查, receipt) → deliver (原子写)
          → visual-check (Chrome via DevTools pipe, 4 视口量测+截图) → 人工视觉审查
我们:     agent 给 markdown → docgen 解析 (散文 + fenced ```archify JSON 块, 零坐标)
          → 图布局引擎出坐标 → Rust 校验器 (schema + 几何质量门, 诊断数组)
          → 引擎自修 → (残差才升级 LLM) → SVG 合成 + 壳注入 (确定性字节)
          → session 装载 (diting 原生布局) → 视口验收 (scrollWidth/Height 4 档)
          → 截图回传 (agent 视觉判读 = perceptual 门) → receipt+html 返回
```

一次 MCP 调用闭合他们三权分立的后两权：visual-check 不再需要外部 Chrome，perceptual
由带视觉的 agent 判截图。确定性一权由 sha256 receipt 承担，字节确定性使产物可进
SQLite cache。

## 2. 参考件清单（移植目标 vs 参考实现）

| archify 件 | 行数 | 我们的处置 | 落点 |
|---|---|---|---|
| schemas/*.json (5 族+common) | 1.4k | serde 强类型，`deny_unknown_fields`=additionalProperties:false；坐标字段全部降级为可选 override（escape hatch） | `src/docgen/spec.rs` |
| shared/geometry.mjs | 1423 | 纯函数库整体移植（谓词+质量门+路由件共用） | `docgen/geometry.rs` |
| 9 道质量门 (clean-flow/composition) | 散在 geometry | **验收规格**。首期开 standard 档（edge-through-node、proper-crossing、ambiguous-corridor、border-run、endpoint-side、rhythm、label-clearance 里属可读性/正确性者）；showcase 档归精美阶段 | `docgen/checks.rs` |
| shared/text-fit.mjs | 49 | 移植；但节点尺寸优先走 diting 真量测，估算仅兜底 | `docgen/text_fit.rs` |
| utils.mjs textUnits | 233 | UAX#11 Wide/Fullwidth 区间 + variation selector 规则照抄 | 同上 |
| shared/validator.mjs | 86 | schema 错误→诊断加工：annotatedPath（路径注释最近祖先 id/label）+ keyword→supportedFixes 表 | `docgen/spec.rs` |
| sequence 渲染器 | 465 | 参考（其固定列算术+时间轴合同进 adapter）；消息 y 改默认自动堆叠，authored y 兜底时间语义 | `docgen/adapters/sequence.rs` |
| workflow-compiler | 4400 | **参考实现非移植目标**：读 lane/col 约束怎么进坐标、3 轮 readable feedback 收敛什么、verified repairs 机制 | `docgen/engine/` 消化 |
| architecture 渲染器 | 1140 | 参考：boundary 聚类约束 + routeVia 侧感知 dogleg 候选 | adapter |
| dataflow / lifecycle | ~580/680 | 未细读；README 即合同 | adapter |
| assets/template.html | 14792 | **不照搬**：自写小壳，吸收机制——slot 哨兵注入、serializeScriptJson（`<`/`>`/`&` 转义）、函数替换器防 `$&` 注入、i18n/guided-views JSON script 标签、data-theme/data-preset、CSS 自定义属性 | `docgen/shell.rs` |
| viewer runtime（内联） | ~9k | 分期：v1 主题+views tab+focus；后续 Reading Depth/Passport/Route Probe/story。导出=自家 screenshot/序列化，不搬 canvas 家伙事 | 壳内联 JS（diting 自家 V8 跑） |
| SIGIL 系统 | ~40 | 16×16 语义图标 inline path，第一天吸收；精修归精美层 | 渲染共享 |
| renderDefinitions | 20 | arrowhead marker×4 + grid pattern | 同上 |
| brand-marks | 2.5k | 缓（v1 零网络） | 批后 |
| mermaid ingestion 映射 | SKILL 内 | 映射表进工具文档，LLM 读拓扑重写零坐标 JSON | 工具描述 |
| delta / migrations / recipes | — | 缓 | — |
| ordinary-model-floor benchmark | 方法论 | 我们的交付门：冻结 attempt-1、外部复验、firstPassUsable；预期高于 archify（零坐标输入） | 测试计划 |

## 3. markdown 载体与 MCP 面

- agent 输入 = markdown：散文走文档壳；图走 ` ```archify ` fenced 块（零坐标类型化
  JSON；后续 ` ```mermaid ` 走映射）。
- markdown 解析：**pulldown-cmark**（parser-only 基建，html5ever 先例）。
- MCP `render_markdown({markdown, session_id?, viewport?, theme?})`：无 session_id 返回
  {html, receipt{checks, diagnostics, sha256}}；带则产物装进 session（等价 setContent）
  返回 receipt+截图。产物=自包含单文件 HTML，零网络，确定性字节。

## 4. 引擎补课（dogfood 前置）——批1 闭环 2026-09-06

1. **`querySelector(':scope')` 不支持**：Intent Trace IIFU 死因（script:3642
   `container.querySelector(':scope > svg')` → null → addEventListener 抛）。**已修**：
   element 根查询绑 `context.scope_element`（selector.rs `bind_query_scope`），
   document 根留空=回落 html（浏览器行为），`Element.matches(':scope')` 绑被测元素；
   ops 层新 op `matches_selector`（bootstrap matches() 含 `:scope` 时直下）。
2. ~~elementFromPoint 全页重排毒化 + 点击死~~ 已修（06da22c）。
3. **viewer smoke 探针已跑**（/tmp/archify-lab/viewer_smoke.py，未入库；方法=静态服
   8931 + 引擎 CDP 8129，驱动全 toolbar 后收 Runtime console/exception）。
   三个 artifact（官方 web-app / workflow-agent-tool-call + 自产 archify_arch.html）
   全 toolbar（theme×2、preset signal-flow+classic、present×2、export 菜单+条目、
   guided view、svg pointerover）：**零 console.error 零异常**（基线是两个 TypeError）。
   `diagram.querySelector(':scope > svg')` 活体验证 = True。
4. **探针挖出第二个洞，已修**：`svg.viewBox` 无反射 → export 点击即
   `Cannot read properties of undefined (reading 'baseVal')`（renderShareCard ~script:917，
   artifact 里 7 处 `.viewBox.baseVal` 站点）。修=bootstrap Element.prototype viewBox
   getter：svg/marker/pattern/view 上反射 SVGAnimatedRect（缺/坏属性→全零 baseVal），
   其他元素 undefined——Chrome 形状。
5. 渲染质量残留（精美层，批5+ 再议，按总纲「精美最后」）：截图 1280×~1200 三张，
   toolbar/节点/箭头/标签/图例/boundary 全渲染，仅个别标签重叠的观感问题。

## 4b. 批2 竖切闭环（2026-09-08）

设计定案（用户批准）：**入口严格 markdown、内核严格 JSON**。agent 只给 markdown；
```archify fenced 块里是零坐标类型化 JSON（serde 强类型），散文走文档壳。bare fence
（无散文）= 纯图产物，同样入口。渲染词表设计参考 archify（MIT）。

落盘：

- `src/docgen/{mod,spec,sequence,shell}.rs`。mod=render→{html, receipt{checks,
  diagnostics, sha256}}；spec=类型化合同+validate_sequence（文档序诊断）+text_units
  （全角计 2、variation selector 计 0）；sequence=固定列算术 adapter（整数 0.1px i32，
  常量表 TOP_Y/PARTICIPANT_W/COL_GAP…，所有 x/y 由 adapter 算出，spec 零坐标）；
  shell=pulldown-cmark 0.13（html feature only，基建判定同 html5ever 先例）流式转
  HTML+fence 拦截。坏 fence 降级为可见代码块+diagnostics，文档不炸。
- SVG 纯表现属性：无 `<style>` 块（内联 style 泄漏全文档；diting svg v1 画属性不走
  CSS pass）、箭头显式 `<path>` 三角不用 `<marker>`、绘制序 title→lifelines→messages
  →participants 压顶。
- `render_markdown` MCP 工具（{markdown, session_id?}）：无 session 返回
  {receipt, html}；带 session 经 `SessionCommand::SetContent` 装进会话——base64
  data URL 走 page.goto 同一加载路径（无预算/无限频，本地免费），清 element_map，
  RecordedAction::SetContent 进回放日志，reply 只回 {bytes, title} 不回显 HTML。

金测三条全过 + dogfood：

- 同输入同 sha256：单测两跑字节相等；**MCP 线上两次 tools/call 哈希一致**
  （4a8c5c29…，8006 bytes）——确定性跨网络面成立。
- 结构断言：viewBox=常量算术可预测（3 参与者 404×320），全部 path 锚点 x 有界。
- dogfood（中文散文+5 参与者 9 消息四变体+注释+表格，920×1164 全页截图）视觉判读
  零缺陷：CJK 无豆腐、emphasis/return/dashed 可辨、生命线/箭头/箭头三角齐全、表格
  带边框。残差（个别标签贴边）记批5 精美层。
- 测试：docgen 21 个单测；全量 `--features screenshot` 810/0/1；clippy docgen 零新警。

分岔记录：原批2 清单里的 geometry 核心+standard 档门延入批3——sequence 族固定列
算术用不上 Sugiyama，先竖切把合同/壳/工具面钉死，批3 图布局引擎以 sequence 为
第一个"无需它"的反例校准机制/策略边界。

## 4c. 批3 闭环（2026-09-08）：图布局引擎 + workflow adapter

机制/策略分离第一次真跑通：

- `docgen/graph.rs`（机制）：矩形/线段谓词、rank 列约束求解（前向传播+后继拖拽）、
  9 族正交路由候选 + 字典序 CandidateCost（forward_reverse → crossings → corridor →
  label_deficit → interior_rhythm → bends → stretch → canvas_growth → port_displacement
  → ordinal）、有界 outside-right 升级（7 探测×24 二分）、标签摆放（横优先/长优先/
  早优先，横段抬 10px）。`docgen/workflow.rs`（策略，~1300 行）：lane/col 语义 →
  约束/走廊/场景；canon() 四集合稳定排序（节点 lane,col,id；边 id,from,to,label,
  route；相位 fromCol,toCol,id；分组 lane,fromCol,toCol,id；lane 保持文档序——作者
  的堆叠就是 spec）；compile_once = 约束装配 → solve_columns → 三趟后移（首列让位/
  顶侧端点让 lane 头标签/内容左地板）→ 垂直算术 → legend 行打包 → 逐边路由累积
  PlacedRoute → 实测边界 → viewBox 定稿。
- 整数 0.1px 纪律兑现：谓词全精确比较，参考实现围着 float 漂移修的 epsilon 栅栏
  在这里不存在；唯一无理量 hypot 在叶子处一次取整。反馈环 4 轮（rank_gaps BTreeMap
  严格增长否则判 wedge）；反馈要 32px（RANK_GAP_CLEARANCE）比约束装配的静态 28px
  高一档——参考实现同款语义差。
- v1 分岔（故意不做，留账）：绝对 pin（via/channelX/channelY/labelAt）、authored 节点
  尺寸、bias/role 豁免/mainPath/showcase、channel-label 机制（带标签对向直边静态撑宽
  rank gap——参考实现拖到反馈第 2 轮，同一不动点）、port_spread 只作用于 pair 0
  （参考两端都散）、升级二分 24 次@0.1px vs 参考 53@0.001、箭头显式三角不用 marker
  （diting svg v1 不画 marker ref）、legend 自打包、exception lane=红描红字。
- shell.rs 家族路由：diagram_type 声明优先，未声明时按在场对象推断（仅 workflow 在场
  → workflow，否则 sequence）；FenceOutcome 带 kind，receipt diagrams.type 真值。
- dogfood（自家用引擎，/mcp 全链）：3 lane/7 节点/8 边/2 相位/1 分组渲染，receipt
  零诊断；diting 截图 1170×2452 视觉判读通过；live DOM 复核 EX / Repair 前缀、
  receipt 通道 `M 746.4 94 L 746.4 66 L 94 66 L 94 94`（节点顶上方 28px）。
- dogfood 抓到的真发现：**same-lane 同 yOffset 的向后边配 return-left 必然
  PresetConflict**——from 腿跑在 from.cy==to.cy 上，为绕到 to 左侧必穿 to 盒，
  route_clears_endpoint_nodes 全 pair 拒收。这是合同不是 bug：诊断点名边并建议
  auto（顶走廊候选手工验证可行）。作者守则=同行回边用 up-channel/drop；批4 修复
  循环把这个诊断→改写闭环自动化。
- 已知残差（记精美/后续）：可行性门不看相位框（up-channel 可视觉穿过相位带，
  dogfood 截图所见）；标签-相位/分组框间距未进 cost。
- 测试：docgen 36 单测（graph 10 + workflow 4 + 壳往返 1 + 批2 21）；全量
  `--features screenshot` 825/0/1；clippy docgen 零提及。

## 4d. 批4 闭环（2026-09-08）：修复阶梯 + 带障碍 + 视口验收 + firstPassUsable 门

批3 收尾时的两个"记批4"残差这次都闭环了，外加交付门：

- **修复阶梯（repair ladder）**：preset 冲突不再直接升级诊断，先走语义替换阶梯
  ——每个替身仍过同一套可行性门重新规划，首个可行者胜；替身的 Feedback 错误
  记住（记第一个），阶梯耗尽才升级诊断。阶梯：`straight→auto`、
  `return-left→up-channel/bottom-channel`、`outside-right→auto`、
  `drop→bottom-channel/up-channel`、`bottom-channel→up-channel`、
  `up-channel→bottom-channel`。披露进 receipt `diagrams[].repairs`
  （{edge, requested, substituted}），checks 加一行自修计数——修了就说修了。
- **相位带进可行性门**：带不是硬障碍。竖直腿合法穿越（穿过相位读作流经，
  沿带跑读作属于）；横段（退化为 0 高矩形，gap 20）和标签矩形
  （LABEL_TOLERANCE −20）必须清带。有相位时 top_y 钳到带上方
  （PHASE_BAND_Y 270 − 40 = 230），up-channel 顶走廊不再视觉穿带。
- **preset 走廊语义（踩坑）**：channel 类 preset 的 via 骑 adapter 走廊
  （req.corridors.top_y/bottom_y），不做局部算术。机制/策略分的直接后果：
  测试 fixture 若把 top_y 供在节点顶以下，横跑穿节点体，门（正确地）拒收
  PresetConflict——真 adapter 永远把走廊供在 lane 顶之上/底之下。
- **视口验收**：render_markdown 带 session 时，SetContent 后追一次
  SessionCommand::Eval 跑 VIEWPORT_PROBE（innerWidth/innerHeight/scrollWidth/
  scrollHeight/diagrams/minScale——最宽图缩放比，可读性信号），grade_viewport
  分级 fits/tall/wide/oversized 进 reply.viewport，receipt.checks 加一行。
  分级是"怎么读回"不是"好不好"：tall=全页截图，wide=先放宽视口。
- **firstPassUsable 交付门（gate.rs，测试即门）**：6 篇语料（checkout/
  index-build/dogfood/cjk/seq-five/seq-min）attempt-1 必须零诊断零修复，
  sha256 冻结成表——几何动一下就是有意识的 re-freeze，不是漂移。冻结技巧：
  先让测试收集全部 drift 一次性 panic 出完整表，再钉，省 6 轮编译。
- **dogfood（原批3 失败案做 fixture，/mcp 全链）**：sha→md 配 return-left
  （批3 抓到的必然冲突案）→ 自动换 up-channel，repairs 披露，viewport fits
  （1440×820），截图视觉判读通过（通道骑相位带上方）。
- 测试：docgen 42 单测；全量 `--features screenshot` 832/0/1（+7：阶梯 2 +
  带 2 + 门 2 + 视口 1）；clippy 62 持平。

分岔记录：architecture/dataflow/lifecycle 三族 adapter 延批5——机制（graph.rs
谓词/路由/升级 + 修复阶梯）已就绪，纯 adapter 活，随精美层一起做。

## 4e. 批5 闭环（2026-09-08）：dataflow/lifecycle/architecture 三族 adapter

批4 延下来的三族 adapter 这次全部落地，docgen 从 workflow 单族扩成五族：

- **spec 三型**：DataflowSpec（stages 2..=5 + stage/row/yOffset 节点 + flows）、
  LifecycleSpec（lanes + states 按 Phase/Event/Outcome 三带分列 + transitions）、
  ArchitectureSpec（row/col 组件 + boundaries(wraps) + connections + layout 旋钮）。
  各带校验器与路由词表；lifecycle 的带几何（cy/w/h/cols）常量进 spec.rs 共享。
- **三族都是固定格点**：dataflow stage_x(i)=1000+i·2150、节点 1120×580、
  ROW_YS 五行；lifecycle Phase/Event/Outcome 带内 COL_PITCH=1540；architecture
  默认 cell 130×64px、gap 30/40px。格点固定 ⇒ 无 gap 可放宽 ⇒ Feedback 终态化：
  不可行直接诊断，preset 失配先走各族替换阶梯（dataflow：
  vertical-channel→bottom/top-channel→auto；lifecycle：drop→bottom/top、
  left↔right→auto；architecture：orthogonal-h↔v→auto），首个过门替身胜、
  repairs 披露。语料文档全 auto 构造，attempt-1 结构性不可能出修复。
- **大分岔（记档）**：三族一律 **不用带障碍**——与 workflow 相位带相反。节点
  住在 stage/boundary 框里面，框是背景画法（先画框后画线），连线穿框读作
  "穿过这个区域"，正是 deployment 视图的语义。obstacles=&[] 全族通用。
- **graph.rs 扩口**：`plan_grid_edge(req, scene, ladder)`（auto 短路 plan_route；
  authored 名字 intern 进 PRESETS 换 'static）；preset_via 补 channel 别名
  （top-channel=up-channel、vertical/left/right-channel）与 orthogonal-h/v
  狗腿。逐边局部走廊：dataflow 上 24/下 26/侧 20px，lifecycle/architecture
  上 28/下 34/侧 36/20px。
- **单位纪律（踩坑 ×3，都修在闭环里）**：常量段是"archify px × 10"（已 tenths），
  authored 旋钮是 px、进引擎 ×10——architecture 的 layout() 曾把默认 cell 也
  ×10 炸成 1300px 单元格，map_or 分流修掉；dataflow 的 measured_bounds 曾用
  content_h 做种子、viewBox 又加 CANVAS_MARGIN 双计成 630≠614，改种
  frame_bottom。第三个坑在 graph.rs：outside-right 升级探测到 ~80k tenths，
  折叠判定的 delta² 乘积 i32 溢出 panic——cross/forward 转 i64，字节中性
  （旧六哈希不变）。机制注记：升级按死侧对逐对跑，即使别的侧对已经成功。
- **标签宽度 vs 格隙**：w=max(MIN, units·49+120)（dataflow MIN=34px，余 32px）。
  7 字标签 463px 宽，同行相邻格隙只有 30px——这种边自动走顶/底走廊，不是
  修复。architecture 测试 fixture 因此把 web→cdn 的 "assets" 标签删了。
- **壳路由五族**：6 元组推断（sequence/workflow/dataflow/lifecycle/architecture）。
- **门冻结 6→9 篇**：dataflow-ingest/lifecycle-release/architecture-edge 全 auto
  构造，attempt-1 零诊断零修复，哈希入表。
- **dogfood（真 /mcp 面）**：三篇语料过 render_markdown——诊断 0、修复 0、
  sha256 与进程内门哈希逐字节一致；lifecycle 进活会话（session_create →
  SetContent → VIEWPORT_PROBE）diagrams=1、minScale 0.987、tier fits（1920×1000），
  即三族 SVG 被自家 diting 解析-布局-缩放链真渲染过一遍。
- 测试：docgen 42→55；全量 `--features screenshot` 832→845/0/1；clippy 62 持平。

## 4f. 批6a 闭环（2026-09-08）：CSS 主题层（light/dark 常量表）

精美层第一刀。数据不函数：`theme.rs` 两张 `&'static Theme` 常量表（LIGHT/DARK），
adapter 只留词汇（kinds/variants）和排版（字号线型/虚线模式），颜色一律从 theme 走。

- **烤值不烤规则（大分岔，记档）**：参考实现是 template.html 运行时 `data-theme`
  切换 + CSS 变量；我们 diting 的 SVG 绘制根本不走 CSS 通道，所以颜色在**生成时**
  烤进 presentation attributes，不用 var()/custom properties。`<html data-theme="...">`
  保留——出处钩子，不做运行时换肤。同一个 markdown 选不同 theme 出两份不同字节，
  这才是确定性友好（各自可冻结、可验哈希）。
- **LIGHT = 历史字面值逐值对齐**：所以 light 语料字节唯一漂移就是根元素多了
  `data-theme="light"` 属性，9 篇门哈希**有意重冻结**（§4d 流程照走：一次 panic
  收齐全部漂移值再钉）。DARK = zinc-950 重映射（page_bg #09090b、ink #f4f4f5、
  各 kind 槽 950 填充/400 描边/200 文字）。
- **Theme 结构**：prose 5 槽（壳用）+ ink 12 槽（ink/muted/soft/guide/edge_label/
  danger/skip/panel/panel_alt/panel_danger/frame）+ 8 个 NodeColors{fill,stroke,text}
  kind 槽。`Theme::node(kind)` 未知→neutral、"plain"→plain；`by_name("light"|"dark")`。
  lifecycle 的 state_palette 是 kind→kind 槽位映射（start→frontend…），不持有颜色。
- **接线面**：`render_with_theme(md, &'static Theme)`（`render()` 仍是 LIGHT 缺省），
  receipt 记 `"theme"`；MCP `render_markdown` 加 `theme` 参数，坏名字报错并列出
  合法值。`&'static` 穿线让 helper 直接回 `&'static str`，免 String 分配。
- **门加暗档**：DARK_CORPUS 两篇（dogfood/seq-five）冻结 + **light≠dark 反别名
  断言**——暗渲染等于亮渲染说明 theme 断流，必须炸。
- **dogfood（真 /mcp 面）**：theme=dark + session 装载 → tier tall、diting 截图
  肉眼验收：暗底白字、Client 深海军蓝/Server 墨绿（frontend/backend 槽的 950/400
  映射）、ping/pong 箭头遮罩可读，无瑕疵。
- 卫生：十六进制颜色只在 theme.rs（96 处）+ 各族测试断言里；测试基线 845→850/0/1，
  clippy 62 持平（`--bin` 口径）。

## 4g. 批6b 闭环（2026-09-09）：visual preset 数据化 + SIGIL 系统

精美层第二刀，两件事：preset 家族把「主题×画风」拆成正交两轴，SIGIL 给每个
节点盖语义章。

- **preset = 正交于 theme 的第二轴**：classic/signal-flow/blueprint/editorial
  × light/dark 八组合，`Theme::resolve(preset, mode)` 出 `&'static Theme`
  （const 表 + rvalue promotion，不需要 static）；classic 就是历史值原样，
  唯一差异是表里多了 `preset: "classic"` 名��字段，所以老调用零漂移。三张
  新表离线从参考 CSS 生成：rgba 全部对 --bg 合成成 6 位 hex（diting 不吃
  rgba），neutral 槽复用 external 变量。MCP `render_markdown` 加 `preset`
  参数（与 theme 正交，坏名字报错并列合法值）；shell 根元素加 `data-preset`，
  和 data-theme 一样是出处钩子不做运行时换肤。
- **SIGIL：13 种 16×16 语义章**：七类技术件（frontend 浏览窗 / backend
  括号 / database 圆柱 / cloud / security 盾 / messagebus 总线 / external
  出界框）+ 五态（start/active/waiting/success/failure）+ neutral 兜底。
  `data-sigil` 记的是**形状名**，tone 表只管借色——waiting 是沙漏形状借
  cloud 槽的色，不是 database 形状。这个坑我自己踩的：lifecycle 测试断言
  写成 `data-sigil="database"`，split 不中直接 unwrap 炸，才把「形状归形状、
  色归色」这条分界掰清楚。
- **两处离线归一化（diting parse_path 逼的，记档可审计）**：没有 `S` 臂
  （`_ => break` 静默截断）→ database 两段 `s` 展开成显式 `c`（反射
  c1' = 2·P0 − prev_c2）；`A` 退化成端点弦会把 cloud 压成六边形 → 三段
  圆弧按 SVG 规范 F.6.5 预抬成三次贝塞尔。另有烤值两枚：参考 CSS 在
  scale(0.6875) 组里写 stroke-width 1.35，真浏览器按 CTM 缩成 ~0.93px，
  diting 不按组变换缩 stroke，直接烤 0.93；0.76 透明度烤成 7 位 hex 尾
  字节 c2。
- **盖位**：sequence/workflow/dataflow/architecture 一律节点框左上
  (+6,+6)；lifecycle 例外走**右上**（步号占左上），插在 rect 之后、步号
  文本之前。
- **门第三次重冻结**：sigil 让 9 篇亮档全漂 + data-preset 属性，一次
  panic 收齐 13 个钉（9 亮重钉 + 2 暗重钉 + 新 PRESET_CORPUS 2 篇：
  dogfood/signal-flow-dark、seq-five/blueprint-light）。暗档和 preset 档
  各带反别名断言——preset 渲染等于同 mode 的 classic 渲染 = palette 断流，
  必须炸。
- **dogfood（真 /mcp 面）**：preset=signal-flow + theme=dark 渲染 Receipt
  图，诊断 0；装进会话 diting 全页截图肉眼验收：workflow 节点章在左上、
  lifecycle 章在右上，13 种形状可辨认没糊成坨，signal-flow 暗色协调、
  章与文字无碰撞。
- 测试：docgen +10（theme 4、sigil 4、preset 换装 1、lifecycle 章位 1）；
  全量 850→860/0/1；clippy 62 持平（`--bin` 口径）。

## 5. 分批（按总纲排序）

- **批1（技术·引擎前置）**：`:scope` 选择器 + archify artifact viewer smoke 探针 →
  引擎洞清单。
- **批2（技术·竖切）** ✅ 闭环 2026-09-08（见 §4b）：`src/docgen/` + sequence adapter（固定列算术+自动 y 堆叠）+
  壳 v1（素颜）+ render_markdown 工具 + SetContent 会话装载。geometry 核心/standard 档门延批3。
- **批3（技术·核心）** ✅ 闭环 2026-09-08（见 §4c）：graph.rs 布局引擎（整数 0.1px、
  9 族正交路由、字典序 cost、有界升级）+ workflow adapter（lane/col 约束）+ 家族路由。
  workflow-compiler 精读完成。
- **批4（流程）** ✅ 闭环 2026-09-08（见 §4d）：preset 修复阶梯（披露进 repairs）+
  相位带障碍语义 + 视口验收分级 + firstPassUsable 语料冻结门。三族 adapter 延批5。
- **批5（技术·三族）** ✅ 闭环 2026-09-08（见 §4e）：dataflow/lifecycle/architecture
  三族 adapter + graph.rs grid 边口 + 壳五族路由 + 门语料 9 篇。
- **批6a（精美·主题）** ✅ 闭环 2026-09-08（见 §4f）：light/dark 常量表 + 生成时
  烤值 + render_markdown theme 参数 + 门 9 亮有意重冻结 + 2 暗档反别名。
- **批6b（精美·preset+SIGIL）** ✅ 闭环 2026-09-09（见 §4g）：四 preset 家族
  正交于 theme + SIGIL 13 形状语义章 + 门第三次重冻结（9 亮 + 2 暗 + 2 preset
  反别名）。
- **批6c+（精美）**：showcase 档门、字号/间距节奏常数、brand-marks、
  mermaid 通道、story/Passport/Route Probe、viewer runtime。

## 6. 已读 / 未读（诚实账）

已读全文：SKILL.md、authoring/delivery/viewer-runtime 合同、benchmark README、
geometry.mjs、text-fit、diagnostics、utils、validator、grid、sequence 渲染器全文、
common+workflow schema、template 结构+:3642 现场、cli.writeDiagram；
批5 又读了 architecture/dataflow/lifecycle 渲染器全文。
未细读（开工前必读）：workflow-compiler 4400 行细节、i18n 目录、legend 内部、
bin/archify.mjs 工件检查器、engineering-profiles、delta、migrations。
