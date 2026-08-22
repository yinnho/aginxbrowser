# diting_dom(现 obscura_dom)摸底报告 — 认领 Phase 0

> 2026-08-22 摸底盘点。目标:改名 `diting_dom` 之前,把模块每一层讲清楚——它是什么、谁在用、有哪些坑、上游这两个月改了什么。

## 一句话职责

进程内 DOM 树:HTML 解析(html5ever)→ 树存储(slab + 链表指针)→ CSS 选择器查询(servo selectors)→ HTML 序列化。不带任何 JS 语义(JS 绑定在 obscura_js 侧包装)。

## 体量与文件

| 文件 | 行数 | 职责 |
|---|---|---|
| `tree.rs` | 941 | `DomTree`/`Node`/`NodeData` 数据结构与全部树操作 |
| `selector.rs` | 754 | 实现 servo `selectors` crate 的 `Element` trait,query_selector 系列 |
| `tree_sink.rs` | 323 | html5ever `TreeSink` 实现,`parse_html`/`parse_fragment` |
| `serialize.rs` | 178 | `outer_html`/`inner_html` 序列化 |
| `mod.rs` | 8 | re-export:`DomTree, NodeData, NodeId, parse_html, parse_fragment` |

共 2204 行,5 个文件,自带 21 个单元测试。

## 核心数据结构(读懂这个=读懂模块)

```
DomTree { inner: RefCell<DomTreeInner> }
DomTreeInner {
    nodes: Vec<Option<Node>>,      // slab:NodeId(u32) 即下标
    free_list: Vec<u32>,           // remove() 回收的槽位,new_node 复用
    document: NodeId,              // 恒为 NodeId(0)
    id_index: HashMap<String, NodeId>,  // id → 节点,getElementById O(1)
}
Node { id, parent, first_child, last_child, prev_sibling, next_sibling, data }
NodeData = Document | Doctype | Element{name,attrs,template_contents,...}
         | Text | Comment | ProcessingInstruction
```

关键设计决策及理由:

- **slab + Option + free_list**:节点不用 Rc,NodeId 是 Copy 的 u32;JS 侧持有的 wrapper 不会因 Rust 所有权失效;`remove()` 后槽位进 free_list 复用。代价:所有访问要过 `nodes.get(id)` 并处理 None(节点可能已被释放)。
- **RefCell 内部可变性**:html5ever 的 `TreeSink` trait 全要 `&self`,被迫 RefCell。所有 borrow 都是短借,无跨 await 持有。单线程前提——**DomTree 不是 Send**,整个模块锁死在单线程 session 模型上。
- **id_index 首插胜出**:`entry().or_insert()` 保证重复 id 时返回树序第一个,符合 spec。`remove_child`/`remove`/`update_id_index` 负责清理索引。
- **树操作带成环防护**:`append_child`/`insert_before` 拒绝自环和祖先环(HierarchyRequestError 当 no-op 处理),`descendants()` 带 nodes.len() 上限。这些防护是上游内联时就带的(注释原文 "obscura burns RAM"/"hung ebay.com")。

## 对外 API(改名后这些签名不动)

- **构造**:`parse_html(&str) -> DomTree`,`parse_fragment(&str) -> DomTree`
- **查询**:`query_selector(_all)(_from)`(selector.rs)、`get_element_by_id`、`children`、`descendants`、`ancestors`、`text_content`
- **修改**:`new_node`、`append_child`、`insert_before`、`detach`、`remove_child`(留槽)、`remove`(回收槽)、`append_text`(合并相邻文本)、`update_id_index`
- **序列化**:`outer_html`、`inner_html`
- **其他**:`document()`、`find_body_or_root`、`import_children_from`(跨树拷贝)、`len`

## 服务层依赖点(谁在用我们)

| 消费方 | 用量 | 用了什么 |
|---|---|---|
| `obscura_js/ops.rs` | **最重** | DomTree/NodeId/query_selector/text_content/get_element_by_id/outer_html —— JS DOM API 的几乎全部地基 |
| `obscura_js/runtime.rs` | 重 | DomTree/parse_html/query_selector/get_element_by_id |
| `obscura_browser/page.rs` | 中 | DomTree/parse_html/query_selector/text_content |
| `src/page.rs`、`src/server.rs`、`src/screenshot.rs` | 薄 | query_selector/NodeId(经包装层) |
| `src/render.rs`、`src/firecrawl_compat.rs` | 薄 | parse_html |
| `src/main.rs` | 薄 | parse_html/DomTree |

结论:**对外接口面很小**——JS 层是唯一的深度用户,服务层只碰 parse + query_selector。改名和重构的爆炸半径可控。

## 已知坑(认领时要处理的)

1. **`children()`/`ancestors()` 无遍历上限**(tree.rs:485,561)——`descendants()` 有防护,这两个没有。上游 2026-08-08 已修("bound children() and ancestors() walks against cyclic chains"),**我们带着这个 DoS 洞**。
2. **注释序列化不消毒**(serialize.rs:73-77)——`<!--` + 原始内容 + `-->`,注释含 `-->` 时重新序列化会破结构。上游 2026-07-01/07-13 修了两轮("neutralize all comment terminators")。
3. **serialize/textContent/import 全是递归**(serialize.rs:16,tree.rs:703 collect_text_inner,tree.rs:666 import_node_from)——恶意深嵌套页面 = 栈溢出。上游 2026-07-09 改为迭代(#367)。
4. **`set_attribute` 只按 local name 匹配**(tree.rs:123)——带命名空间的属性会撞名。上游 2026-08-04 修("match setAttribute by qualified name")。
5. **quirks mode 被吞掉**(tree_sink.rs:221 `set_quirks_mode` 空实现)——quirks 页面 class/id 选择器大小写行为不对。上游 2026-07-26 修。
6. **状态伪类全部恒不匹配**(selector.rs:375)——`:enabled/:disabled/:checked` 能解析但永远 false;`:hover/:focus` 同理。上游 2026-07-03 修了 enabled/disabled/checked。
7. **`elem_name` 的 unsafe 自引用技巧**(tree_sink.rs:11-31)——RefCell guard + 裸指针,功能正常但 fragile,认领时考虑重写。
8. **不支持 `:has()`、`:focus-within`、Shadow DOM**——伪类解析器只认 6 个;shadow 相关全部 hardcode false/None。

## 上游这两个月改了什么(2026-06-19 内联至今,44 个 commit)

上游 obscura-dom 从 2204 行涨到 5211 行(2.4 倍)。逐行相同率:tree_sink 97%、selector 91%、tree 86%、serialize 54%。

**A. 我们已经带的(内联时就有):** descendants/append/insert 成环防护、id_index、selector 缓存。

**B. 同源 bug 修复,我们大概率还带洞(认领时逐条吸收):**
- children()/ancestors() 遍历上限(2026-08-08)
- 注释序列化消毒 ×2(2026-07-01, 07-13)
- serialize/textContent/import 迭代化防爆栈(2026-07-09)
- `remove()` OOB/double-free 防护(2026-07-26)
- quirks mode 选择器大小写(2026-07-26)
- setAttribute qualified name(2026-08-04)+ setAttributeNS 系列(2026-07-26)
- :enabled/:disabled/:checked 真实状态(2026-07-03)
- `<head>` 内容在 documentElement.innerHTML 时保留(2026-08-04)
- template contents 桥到 JS `.content`(2026-07-22)
- document.write 单输入流(2026-08-15,js 侧为主)

**C. 大特性,我们不跟(至少现在):**
- Shadow DOM 全家桶(declarative shadow roots、tree scopes、slots,约 2026-07-30~08-04,10+ commits)——这是上游自研渲染引擎的地基,我们渲染走 blitz,暂不吸收
- `:has()` 选择器(2026-06-29)
- CompiledSelector/Matcher 选择器编译管线(服务他们的 CSS 引擎)
- html5ever 解析器升级(2026-07-31)
- clone_node(2026-07-31,不重新解析的深拷贝)

## 认领建议(Phase 1 开工顺序)

1. **先补特征测试**:现有 21 个单测之外,重点锁 query_selector 作用域行为、serialize 转义、id_index 重复 id 语义、template_contents——这些是服务层和 JS 层真正依赖的。
2. **吸收 B 组修复**:每条先看上游 diff 理解为什么,再手写进我们代码(不 cherry-pick merge)。B 组全是小 diff,一天能过一半。
3. **改名 `diting_dom`**:测试绿了之后做,纯机械重命名(mod.rs 的 re-export 保持不变,服务层只改 import 路径)。
4. **C 组挂账**:Shadow DOM 和 :has() 记为"已知不支持",等渲染路线(Phase 2)再议。
