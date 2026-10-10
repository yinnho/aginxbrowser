# doudian-publish

抖店商品发布（草稿或直接上架）— 纯 API 链。2026-10-05 真机绿：
product_id 3846431717594366336（18 spec / 18 SKU / 主5图 / 竖5图 / 8图详情 / 运费模板 29507137 / 无品牌 596120136 / 材质 钢化玻璃），edit 模式 getSchema 回读全一致。
**2026-10-10 起 `vars.check_status:2` 直接上架绿**（原生签名已破，见下）。

## 纪律

- 只提交一次：45s 超时=结果未知，不盲重试（testpack 规则）
- `vars.check_status:1`=草稿（默认）、`2`=直接上架（审核中落库，`audit_pass:false` 属正常）

## 为什么是纯 API（不走 UI）

- 前两枪走 store.saveGoods（UI 通道）全是 `10013 组件数据解析失败`：React 表单从没见过
  塞进去的值（幽灵模型），pic 是字符串数组不是 `[{url}]`，spec/sku 用自造键。
- 店主定性「提交信息难道不是api接口」——对。提交就是一发
  `POST /product/tproduct/addWithSchema?check_status=1`，页面只需要提供 __token/cookie。

## 三发 API 链

1. `getSchema`（空模型）— 验证 hand-built context 通：`{category_id, biz_identity:'xiaodian',
   business_code:'xiaodian', capability_codes:['standard_capability'], ability:[], operation_type:'create'}`
2. `getSchema`（业务模型，spec 用占位 id `30000_0..17`）— **服务端 canonicalize**：
   spec id 变 `300000..3000017`（= spec_id+序号拼接，两位数同规则）、每行 SKU 回完整骨架、
   自由文本属性回 `value_id:"" + diy_type:0`、品牌 tags 归 `{}`。返回的 `data.model`（带壳
   58 key，`model.goods_category.value` 形状）就是可提交模型，别自己拼。
3. `POST addWithSchema?check_status=1` — body 见 flow.json；`check_status:1`=草稿、`2`=上架
   （上架路径有 10001010A 风控墙，等真 msToken，本 flow 不碰）。

## 关键形状（10013 的三具尸体换来的）

- `pic`/`main_image_three_to_four`/`long_pic`/`white_background_pic`：`[{url:"..."}]`，字符串数组=毒
- `spec_detail`：`[{id, name, spec_values:[{id,name,img_url?}]}]`；**规格值图=值对象上的
  `img_url`**（2026-10-10 两路坐实：页面自己的 zod schema F 里有 `img_url:string.optional()`；
  canonicalize 差分探针 18/18 回显保留）。6019 这类 `all_spec_pic_required:true` 品类每个值
  必带；调用方给 `args.spec_images`（与 `spec_values` 平行的 URL 数组，doudian-upload 出），
  args 校验步拦长度不齐，canon_ok 断言回显数。`sku_pic` 是自造毒键（st=500 组件数据解析失败）。
- `sku_detail`：`[{spec_detail_ids:[id], price:"44.9", stock_info:{stock_num:999}, code}]`
  — spec_name/value_name/spec_desc 都是自造毒键
- `category_properties`：`{"<prop_id>":[{value_id, value_name}]}`，自由文本属性 value_id 空串合法；
  **6019 必填品牌 1687**——漏了它 getSchema 回 `st=10002 参数错误`（不是 img_url 的锅，
  绿形状+品牌+品类切换即 st=0）
- 提交体 model **带 `{value:...}` 壳**（formatSchemaData 原样输出），zod 校验去壳只是埋点
- `__token` 页面级长效：resource timing 里 `/product/tproduct/` 请求的 query 挖
- `msToken = btoa(btoa(shop_id))` 本地算；`request_extra` 签名失败前端自己发 `_signError:"1"`
  （genSignatureNew 的降级通道），不是服务器硬门

## 原生签名已破（2026-10-10，genSignatureNew 全解码）

10-05 的旧结论「上架路径有 10001010A 风控墙，等真 msToken」作废。签名生成器在
chunk `50445.5912e03a.js` module 93250（本地 curl CDN 全量 97 chunk 挖出）：

- `_signError:"1"` 是生成器**模块加载失败时的兜底返回**——我们此前一直在发兜底标记
- 真生成器（字符表混淆，全解码）：富化源 = 表单 `$globa.edit_sign_info.{p_idy,p_key}`
  ——但 97 个 chunk 里**没有任何生产者**，getSchema 响应也不带 → 本页版本实际走默认参：
  `request_extra = {"_msToken": doubleBtoa("0") 第5位插"2"}` = `{"_msToken":"TUE9P2Q=="}`
- query 侧 msToken 仍是 `btoa(btoa(shop_id))`；secureProxy 拦截器另自动补 a_bogus
- 实测三枪（edit check_status=2 两枪 + add check_status=2 一枪）**10001010A 全程未开火**

## check_status=2 与草稿的三处校验差（10013 逐层教出来的）

| 项 | check_status=1 草稿 | check_status=2 上架 |
|---|---|---|
| 品牌 1687 | DIY 空壳 `value_id:""+value_name` 可（canon 会剥掉但不拦） | **必须真 value_id**（DIY 被 canonicalize 剥→`品牌必填`；6019 用无品牌 `596120136`） |
| 详情 desc_html | 任意字符串 | **富内容**（裸短串→`商品详情内容不能为空`，带图富文本过） |
| 规格图（6019） | 未单测差异 | img_url 逐值必带（见规格图节） |

flow 侧：`vars.check_status`（默认 1）模板进 query+body；args 校验步在 =2 且缺
`brand_value_id` 时 2s 拦；body_built 回执带 `check_status` 和 `sig` 供核账。


## 发货地址绑定 + 上架回读（steps 15-17，2026-10-10 #79）

报告里的「创建→绑发货地→回读核验」三件事全部 API 化，接在提交步后面：

1. **addr_bind**：`POST /shopuser/tshopuser/shipAddressBatchUpdate`
   `{shipAddressType:2, addressId:<vars.ship_address_id>, productIdList:[pid]}`
   query `_bid=ffa_order`。**`__token` 是页级单值**（同页 fxg_admin/ffa_order/auth
   六组请求对拍 uniqToks:1），create 页挖的 ffa_goods token 直接可用。
   绑定层=订单侧，跟商品模型里的 `shipping_origin_id` 是两套（后者绑完仍是 ∅，
   别用它当核验）。不给 `ship_address_id` = 整步 skip。
2. **addr_verify**：`shipAddressList` 带 `productIdOrName` 直查（bundle 158202 处
   的过滤参数），断言行内 `configFromSource===2 && shipAddressType===2 &&
   configAddress.addressId===args 值` 三重回显。**列表是派生视图**：实测新商品
   过审前不在列表（total:0 但 countInfo.productLevelCount 已+1），轮询
   `verify_tries×verify_gap_ms` 后仍在途=返回 `bound:'ack'+addr_pending`
   （bind 已 st:0，不算失败）；**错位回显才是 throw**。
3. **sale_verify**（只 `check_status=2` 跑）：`GET /product/tproduct/list?
   id_name_code=<pid>&draft_status=0&business_type=4` —— 页面自己的在售过滤器
   口径 `check_status:3+status:0`。枚举（93149 chunk）：check_status
   1=新建/2=审核中/3=审核已通过/4=审核未通过；status 0=售卖中/1=已下架。
   `3+0`=`on_sale:true`；`4`=驳回 throw；在途=`audit_pending` 正常返回
   （审核是服务器侧异步，昨天实测分钟~小时级，flow 不为它背锅）。

实测：addressId 12405687（南京浦口）绑 3847441891603906978 → st:0 →
行内 configFromSource:2 + shipAddressType:2 + configAddress 逐字回显 →
列表 check_status:3/status:0。

## 前置

- doudian-login（登录态）
- **带图 = 先 `doudian-upload`**：同一 session_id 上跑，产出的 CDN URL
  数组填 `main_images` / `portrait_images`（10-09 起两 flow 闭环，
  见 doudian-upload/flow.md）
- **绑地址**：`vars.ship_address_id` 给地址库 id（报告口径 12405687 南京）；
  不给=跳过绑定核验

## 引擎坑：fresh load 的 fxg 页任务泵必死（#39 已修，2026-10-05）

协议步骤全对（第三枪手工同协议命中 3846431717594366336），但 flow 重放曾卡在
引擎侧：create 页 fresh load 后 setTimeout(100ms) 几分钟不回、fetch promise
永不 settle——四连抽全 wedge。根因是 **terminate_execution 的连带伤害**，
两种死法、同一凶手（boot 期 watchdog 终止长脚本时掀掉整个 JS 栈，finally 不跑）：

- **链消失**：drain async 帧正执行 payload 时被终止 → 帧被 unwind 永不 resume，
  armed=true 谎报活着。旧逃生门要 1000 个新 push，而 fxg telemetry 是自链
  setTimeout——回调不跑就永远不再 push（实测 {qlen:160819, stale:0}）。
- **depth 泄漏**：payload/派发窗口的 `_mtDepth--` 在 finally 里，终止跳过它 →
  depth 永久 >0 → 新链永远 defer（实测 defers 829 还在涨）。

修复（bootstrap.js + runtime.rs）：逃生门计数基→时间基（lag>250ms 一个 push
即治愈）；每次 arm 附带独立 1500ms 巡逻 sleep（零 push 也能自愈）；Rust 侧
`run_event_loop` 入口在终止恢复点调 `__diting_mt_recover_termination()` 清零
泄漏 depth。回归测试 `test_terminated_payload_severed_chain_self_heals`
真复现杀链（短 watchdog 终止自旋 payload）。

修复后 flow 重放 14 步全绿：**product_id 3846444003306373208**（timer 门 20s
过、getSchema 两发 1.8s/13.5s、提交 5.4s 回 st=0）。timer gate 步骤保留当哨兵。

## args 前置校验（step1 `args_valid`，2026-10-09 焊入）

标题超限曾把整条 ~190s 的链烧完才在服务器 10013 爆（「商品标题最长不能
超过 30 个汉字（60 个字符）」）。校验步在 navigate 后 2s 内就拦：

- `title`：汉字当量 ≤30（CJK 计 1、其他计 0.5——`适用25款昊铂HL…` 28 字
  = 当量 26.0，合法）；超限报「超限（30汉字/60字符，服务器 10013）」
- `main_images` 1-5 张、`portrait_images` ≥1（3:4）、`skus` 非空、
  `freight_id`/`category_leaf_id` 必填

违反即 throw，receipt 带 `args 校验失败: …` 清单——失败成本从 190s 降到 2s。
规则源头是实测服务器口径，不是文档口径；服务器再教新规则就往这步加。

## 提交步 = eval 内 await（#240 第一面已修，形状保留，2026-10-09）

**2026-10-10 补：两个 getSchema 步同款改造。** 带 spec_images 的首跑死在 step6——
fresh 导航 boot 风暴把 clean getSchema 的 fire-and-forget 续体丢了（wait 泵 90s 空转，
#240 face-2 残面第三次现身，这次咬的是 schema 步不是提交步）。step5/step8 改成
`(async function(){ … var x = await afetch(...); window.__CLEAN=x; … })()`、
`timeout_ms:{{schema_ms}}`，wait 步保留当瞬时通过的哨兵。改完首跑即绿，副作用是
整链 190s → **57s**（wait 泵不再空转等回包）。净层 200 页内不落时，先查这两步
是不是被改回了 fire-then-wait。

fire-and-forget fetch + `wait` 泵等 `window.__SHOT` 的老形状，在一次
带图跑里死透。#240 拆成了两具尸体：

- **轮子尸体（已修，bootstrap.js + watchdog.rs）**：`_timerArmed` 持过期
  死限 + `_timerPatrol` 闩死 → `_timerArmNext` 的门拒绝一切后续 arm，
  会话级 timer/rAF 永久死亡。修法=patrol 双臂重挂（reject 不再闩死）、
  recover 钩子强愈轮子、泵侧证人 `__diting_wheel_stalled()`（堆顶过期
  600ms）并入 #227 探针、`__MT_STATE().wheel` 计数器（wakes/staleWakes/
  throws/patrols）。回归测试三条。修后 wedge 复跑：timer 跨 eval 活、
  新发 fetch 活。
- **fetch 续体丢失（罕见残面，未愈）**：重负载 boot 风暴窗口里，单发
  fire-and-forget fetch 的 `await` 续体被丢——净层 200 回包、结算已推、
  外层 promise 已 resolve（在飞表键已删），但 async 续体永不执行。零
  terminate 参与；复现是负载条件性的（重 boot=canon 12-15s 必挂，轻
  boot=1-3s 不挂）。10-09 晚挂了两道防线：shim 结算改走 `.then` 链
  （await 续体和 .then 回调是同一 resolve 的两条 V8 投递路径，链形把
  async-resume 机器从热路径上剥离）+ `_fetchRing` 64 格阶段环
  （armed/opok/resume/resp…）。九枪未再中；**下次命中直接 eval 读
  `_fetchRing.slice(-16)` 定尸**：末条是 opok 无 resume=续体丢，有
  armed 无 opok/takeok=投递死。

所以提交步保持 `(async function(){ … var r = await fetch(…);
var x = await r.text(); window.__SHOT = x; return x.slice(0,2000); })()`、
`timeout_ms:{{submit_ms}}` 的形状——eval-await 路径在 wedge 会话里 5+ 次
全 settle，对两具尸体都免疫。残面修掉前别改回 fire-then-wait。
页内 45s AbortController 兜底保留。证据链：issue #240 +
/tmp/dd-repro/wedge*.py（净层/network 面对拍记录）。

## 运行方法（2026-10-10 实跑口径）

**一键全链**（=报告 抖店YU7组合商品上传过程 的引擎化）：

```
session = doudian-login 扫码 → session_id
POST /flow/run {"name":"doudian-upload",
  "vars":{"args":{"images":[{name,content_base64,mime_type},...]}},
  "session_id":session}
→ urls = saved.upload.urls          # 主图/竖图/规格图全部从这里出

POST /flow/run {"name":"doudian-publish",
  "vars":{"args":{...全量业务参数, main_images/portrait_images/spec_images 吃 urls...},
          "check_status":2,          # 直接上架（默认 1=草稿）
          "ship_address_id":"12405687"},  # 绑发货地（不给=跳过 15-17 的绑定）
  "session_id":session}
→ outcome.product_id + addr_bind st:0 + addr_verify/sale_verify 回执
```

单跑 publish：

```
POST /flow/run
{"name":"doudian-publish",
 "session_id":"<doudian-login 回执的会话>",   ← 登录态组合
 "vars":{"args":{ ... 全量业务参数 ... }}}
```

**args 必带键**（缺一个先死于 step0 换参、后死于 step7 TypeError）：
`category_leaf_id`、`title`、`main_images[]`、`portrait_images[]`、
`desc_html`、`freight_id`、`spec_id`、`spec_name`、`spec_values[]`、
`skus[{price,stock,code}]`、`brand_prop`、`brand_value_id?`、
`brand_value_name`、`material_prop`、`material_value_name`。
可选 `spec_images[]`：与 spec_values 平行的规格值图 URL（强制规格图品类
如 6019 必给，长度不齐 args 校验步 2s 拦）。

绿单样例：`/tmp/qs-bench/pub-run.json`（10-06 原样，直接抄）；字段源头映射见
`docs/doudian-fill-payload-20261004.json`（testpack 数据，不进 git）。

**步骤 15-17 的 vars**：`ship_address_id`（可选，地址库 id）、`verify_tries`
（默认 5）、`verify_gap_ms`（默认 6000）——绑定后核验/上架回读共用的轮询预算。

## 绿单台账

| 日期 | 引擎 | product_id | 备注 |
|---|---|---|---|
| 2026-10-05 | #39 修复版 | 3846431717594366336 | 首绿（手工协议同款）+ flow 重放 3846444003306373208 |
| 2026-10-06 | 同 | 3846778561813938623 | s_18 接管跑法 |
| 2026-10-09 | **#237 修复版** | 3847273730942305306 | s_3 当日扫码会话直跑，14 步全绿（timer 门 0ms、submit 0.9s） |
| 2026-10-09 | #239 修复版 + 校验步/await 提交 | **3847304136886452316** | **首单纯 flow 带图全链**：doudian-upload 出 5 URL → publish 吃 URL；args_valid 26.0 当量放行，add_answer eval-await 直接回 `errno:0` |
| 2026-10-10 | getSchema 双步 eval-await + spec_images | **3847441335397253316** | **首单规格图绿**：6019（all_spec_pic_required）18 值 18 图，canon_ok `spec_img:18` 全保��，57s 全链（此前 fire-then-wait 死在 step6 #240 face-2） |
| 2026-10-10 | 原生签名 + check_status=2 | **3847441891603906978** | **首单直接上架绿**：request_extra `{_msToken:TUE9P2Q==}` 复刻默认生成器，无品牌 596120136 + 富详情，55.5s 全链 `st=0`；同日 edit 路径把 3847441335397253316 草稿也送上架（`audit_pass:false` 审核中） |
| 2026-10-10 | + 地址绑定三步 | **3847445690687029295** | **绑定链验收**：cs=2 + `ship_address_id:12405687`，14+1 步绿（提交 st:0 + bind st:0），addr_verify 因商品仍在审核（check_status:2，派生列表不收）返回 pending——次日过审后同款查询三重回显全中（见上节实测）；-sale_verify 轮询内在途容忍为设计行为 |

## 2026-10-09 附记

- **API 链不受 UI 挂件门控影响**：当天新版页面 bundle 的上传挂件
  `isAllowUploadBtnClick:false` + 离屏 input（点击才挂监听）让 UI 传图路径
  点不开——是引擎 filechooser 面缺口，不是店铺权限；getSchema/addWithSchema
  照常 st=0。别再从挂件旗子推「权限墙」。
- #237（MessagePort at-least-once）修复后 boot-storm timer 门照旧当哨兵——
  它测的是泵活，和投递恢复互补。
