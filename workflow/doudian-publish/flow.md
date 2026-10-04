# doudian-publish

抖店商品发布（草稿落库）— 纯 API 链。2026-10-05 真机绿：
product_id 3846431717594366336（18 spec / 18 SKU / 主5图 / 竖5图 / 8图详情 / 运费模板 29507137 / 无品牌 596120136 / 材质 钢化玻璃），edit 模式 getSchema 回读全一致。

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
- `spec_detail`：`[{id, name, spec_values:[{id,name}]}]`；`sku_detail`：`[{spec_detail_ids:[id],
  price:"44.9", stock_info:{stock_num:999}, code}]` — spec_name/value_name/spec_desc 都是自造毒键
- `category_properties`：`{"<prop_id>":[{value_id, value_name}]}`，自由文本属性 value_id 空串合法
- 提交体 model **带 `{value:...}` 壳**（formatSchemaData 原样输出），zod 校验去壳只是埋点
- `__token` 页面级长效：resource timing 里 `/product/tproduct/` 请求的 query 挖
- `msToken = btoa(btoa(shop_id))` 本地算；`request_extra` 签名失败前端自己发 `_signError:"1"`
  （genSignatureNew 的降级通道），不是服务器硬门

## 纪律

- 只提交一次：45s 超时=结果未知，不盲重试（testpack 规则）
- 草稿→上架是店主的事，本 flow 止步于草稿回执
- 上架路径的 10001010A 风控墙：等真 msToken，别用桩硬闯

## 前置

- doudian-login（登录态）
- 图片先上传绑定（抖店素材域，回执见 docs/doudian-upload-receipts-20261004.json）

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
