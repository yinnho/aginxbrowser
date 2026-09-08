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

## 5. 分批（按总纲排序）

- **批1（技术·引擎前置）**：`:scope` 选择器 + archify artifact viewer smoke 探针 →
  引擎洞清单。
- **批2（技术·竖切）**：`src/docgen/` + sequence adapter（固定列算术+自动 y 堆叠）+
  geometry 核心 + standard 档门 + 壳 v1（素颜）+ render_markdown 工具。
  **金测三条**：同输入同 sha256；门全绿；结构断言（SVG 含该有节点/边/标签，几何有界）。
  不做像素对照——那是精美标准，且自动布局坐标本应与 archify 分岔。
- **批3（技术·核心）**：图布局引擎（分层 rank/排序/坐标 + 正交路由 + 标签摆放，
  确定性纪律全程）+ workflow adapter（lane/col 约束）为首个消费者。
  workflow-compiler 作参考实现精读。
- **批4（流程）**：自修循环全量（诊断→verified 修复→复验→升级 LLM）、receipt/视口
  验收接线、firstPassUsable benchmark 交付门、architecture/dataflow/lifecycle adapter
  补齐。
- **批5+（精美）**：CSS 主题层、visual preset 数据化、showcase 档门、字号/间距节奏
  常数、SIGIL 精修、brand-marks、mermaid 通道、story/Passport/Route Probe。

## 6. 已读 / 未读（诚实账）

已读全文：SKILL.md、authoring/delivery/viewer-runtime 合同、benchmark README、
geometry.mjs、text-fit、diagnostics、utils、validator、grid、sequence 渲染器全文、
common+workflow schema、template 结构+:3642 现场、cli.writeDiagram。
未细读（各批开工前必读）：architecture/dataflow/lifecycle 渲染器全文、
workflow-compiler 4400 行细节、i18n 目录、legend 内部、bin/archify.mjs 工件检查器、
engineering-profiles、delta、migrations。
