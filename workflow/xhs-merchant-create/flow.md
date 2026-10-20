# xhs-merchant-create — 千帆单品上架（本地填表 + 单次提交）

走页面自己的 Pinia 表单：填 → 前端校验 → `getFormData()` 出创建体 → 逐字段核对 args →
（可选）单次 `batch_create_item` → 轮询任务 → `publish_render` 详情回读。
**默认干跑**：`vars.submit` 不为 1 时什么都不发，只回创建体摘要和 sha256。

## 跑法

```
# 1) 先 xhs-merchant-upload 拿 materials
# 2) 干跑（默认）
POST /flow/run {"name":"xhs-merchant-create",
  "vars":{"args":{
     "title":"…",
     "skus":[{"name":"规格值","price_yuan":34.9,"stock":999,"code":"HL-25-S01","image":"HL25_S01.jpg"}, …],
     "main_images":["HL25_M01.jpg",…],        # 1:1，≤10
     "desc_images":["HL25_D01.jpg",…],
     "materials":{…upload 回执的 materials…},
     "dedupe_keywords":["昊铂|HL"]            # 可选，正则，和标题全等一起查重
  }}}
# 3) 真提交：同样的 args，再加 "submit":1（只提交一次）
```

vars（默认值）：`submit:0`、`allow_duplicate:0`、`category_leaf_id`（导航贴膜
`65f995f83e946300016b0efc`，须在店铺“最近类目”里，否则传 `args.category_paths`）、
`spec_property:"套装规格"`、`logistics_plan:"偏远地区不包邮"`、`delivery_hours:48`、
`brand:"none"`（=平台“无品牌”，否则给品牌名）、`poll_ms:800`、`poll_max:150`、`expect_seller_id`。
可选 args：`long_images`（3:4 竖图名）、`category_paths`、`spec_property`。

## 出路

| 出路 | 含义 |
|---|---|
| 绿 + `saved.dry_run` | 干跑：表单已填、校验过、创建体已核对；`built.sha256` 是见证 |
| 绿 + `saved.final` | 已创建：`itemId`、回读核对全绿（价格/库存/编码/规格图/主图/详情图/可购/列表里恰好 1 条） |
| throw `dedupe:` | 店里已有同名或匹配 `dedupe_keywords` 的商品（`allow_duplicate:1` 才放行；平台自己的“重名”校验也会拦） |
| throw `validate:` / `build:` | 前端校验没过 / 创建体与 args 不一致，**没发任何写请求** |
| throw `submit:` 请求失败或无 taskId | **结果未知**，先去店里查标题再决定，flow 不重发 |
| throw `task:` | 任务失败或超时；失败任务不产生商品，超时则结果未知，别重发 |
| throw `readback:` | 商品**已存在**但和请求不符，回执里带 itemId |

## 要点（都是踩出来的）

- 模块不写死编号；`LV` 与 Pinia 按内容/`__vue_app__` 找。
- setter 收 **SKU 行对象**；规格图要 `setMainSpecImage(规格值对象, 图)`，只 `setSkuSpecImage` 不清“规格图缺失”。
- 图片对象用素材库正式 path/link/size，`width/height` 必须是 Number，`uploaderInfoModel` 是对象。
- 价格单位分；`result.success` 布尔是已知假象，只看任务 status 3 + `failData` 空。
- 查重要拉全量列表本地匹配（`search_item_v2` 关键词筛选不生效）。
- 提交按钮对应函数（`onSubmitForm` 等）不碰，直接用页面 SDK 的同一个 `batch_create_item` 体。
- 前置同 `xhs-merchant-publish`：死 jar 不进 create 页。

## 验证记录

- 2026-10-21 干跑（`allow_duplicate:1, submit:0`，昊铂 HL 18 SKU）：22 步绿，创建体 25573 字节，
  `sha256 6075291c45b05450…` 与当天真实提交的体**逐字节一致**。
- 查重步：同一份 args（不带 allow_duplicate）命中 `6ad791e60dbff300012f1fe0` 并拒绝。
- 提交→轮询→回读段沿用 2026-10-21 00:28 手工单次提交的同一组调用（itemId `6ad791e60dbff300012f1fe0`，
  18 SKU 全对、31 张图哈希 0 不符）；这一段在 flow 里**未再对店铺执行**。
