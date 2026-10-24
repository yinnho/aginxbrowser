# pinduoduo-publish — 拼多多商品发布（sync + copyGoodsCommit）

按 **2026-10-12** 已跑通 raw 路径固化（回执 `docs/pdd-raw-draft-receipt-20261012.json`）：

**已登录 MMS 会话 + `__mms.fetch.sync`（同步 XHR）+ `copyGoodsCommit` → `update_goods_commit_info` → `commit/detail`**。

不走 React goods 子应用 UI；**永不**包装 `window.fetch`（PDD 指纹会识破）。

## 和旧版的差别（必读）

| | 旧版（10-10 设想 / 首版 flow） | 本版（10-12 实测） |
|---|---|---|
| 传输 | `__mms.fetch.post`（异步） | **`__mms.fetch.sync`** — 异步 post 在 Ut/`fbeZ` 链上 EVAL_TIMEOUT |
| 建草稿 | `create_new` + `action/edit` 拼全字段 | **`copyGoodsCommit`** 从店内已有商品/草稿克隆 |
| 改标题 | 写进 `action/edit` body | **`update_goods_commit_info`** 只改 `goods_name` |
| 图/SKU | args.gallery + args.skus 必填 | **随 copy 继承**；args 里 gallery/skus **可选**（CDN 备注用） |
| `action/edit` | 主路径 | **停用**（实测「价格参数不完整」/ 4000000） |
| 默认 | `submit:0` | 仍默认 **`submit:0` 只存草稿** |

绿证据（raw，非本 flow 重放）：草稿 `goods_id=1015378324592` / `commit=203445014447`，
标题 `TEST-RAW-奇骏钢化膜草稿-勿拍-20261012`，`is_shop:0`，13 图 / 4 SKU，从未打 `commit/submit`。
源商品：`1014506243098` / commit `203236873780`（婧途车品店在售奇骏膜）。

## 纪律

- **只提交一次**：`vars.submit:1` 才打 `/glide/v2/mms/edit/commit/submit`；默认 `0`=只草稿。
  结果未知先查 `commit/detail` + `goodsListByGoodsId`，**禁止盲重试**。
- **不碰** `/goods/goods-list` React UI。导航目标是 MMS shell（`/home/`）。
- **永不**在 PDD 页包装 `window.fetch`。
- 价格单位（若日后补全量 edit）仍是**分**；本版 copy 路径不改价。

## 前置

1. 登录态：先跑 [`pinduoduo-login`](../pinduoduo-login/)（回执 `live_qr` 给人扫）拿 `session_id`；或任何已登录 MMS 会话（cookie 落在 `.pinduoduo.com` / MMS）。暂无 `pinduoduo-login` flow。
2. **源商品**：店内一件已成功上架/草稿的 `source_goods_id` + `source_goods_commit_id`（copy 模板）。
3. 图片：copy 会带上源商品 gallery。若要换图——先在站外/原生 EQW2 传好拿到 CDN URL，再另议；
   本 flow **不内置上传**（`__mms.fetch.upload` 存在，但 EQW2 签名链未收编）。

## Run

```bash
# 默认：从源商品复制草稿并改名（submit=0），不公开售卖
POST /flow/run
{
  "name": "pinduoduo-publish",
  "session_id": "<已登录 MMS 会话>",
  "vars": {
    "args": {
      "title": "TEST-FLOW-奇骏钢化膜草稿-勿拍",
      "source_goods_id": "1014506243098",
      "source_goods_commit_id": "203236873780",
      "out_goods_sn": "TEST-FLOW-JT-20261012"
    },
    "submit": 0
  }
}
# → saved.ids / saved.rename / saved.draft / saved.submit_out / saved.outcome
```

源 id 也可放 vars（与 args 二选一，args 优先）：

```bash
"vars": {
  "source_goods_id": "1014506243098",
  "source_goods_commit_id": "203236873780",
  "args": { "title": "…" },
  "submit": 0
}
```

**续跑已有草稿**（跳过 copy，只改名 + 回读 / 可选 submit）：

```bash
"vars": {
  "args": {
    "title": "新标题",
    "goods_id": "1015378324592",
    "goods_commit_id": "203445014447"
  },
  "submit": 0
}
```

**直接上架**（确认草稿回读后，同一 ids）：

```bash
"vars": { "args": { "goods_id":"…", "goods_commit_id":"…", "title":"…" }, "submit": 1 }
```

## API 链（全部 `__mms.fetch.sync('POST', url, body)`）

| # | 接口 | 作用 |
|---|---|---|
| 0 | `/janus/api/checkLogin` | 登录核验 |
| 1 | `/glide/v2/mms/edit/commit/copyGoodsCommit` | **克隆草稿**（args 已带 ids 则跳过） |
| — | `/vodka/v2/mms/query/display/mall/goodsList` | 可选查重（`vars.dup_keyword`） |
| 2 | `/glide/mms/goodsCommit/action/update_goods_commit_info` | **改标题**；body 必带 `cat_id`（先 `commit/detail` 取；缺则 `args.cat_id`）。**不传** `out_goods_sn`（传了会触发更严校验「类目信息为空」） |
| 3 | `/glide/v2/mms/query/commit/detail` | 草稿独立回读 |
| 4 | `/glide/v2/mms/edit/commit/submit` | 单次公开提交（仅 `submit:1`） |
| ✓ | `goodsListByGoodsId` + `on_shop/detail` | 在售回读 |

## args 契约

### 必填（二选一模式）

**Copy 模式**

| key | 说明 |
|---|---|
| `title` | 新商品名 → `goods_name` |
| `source_goods_id` | 源商品 ID |
| `source_goods_commit_id` | 源草稿/提交 ID |

**Resume 模式**

| key | 说明 |
|---|---|
| `title` | 要写成的标题 |
| `goods_id` + `goods_commit_id` | 已有草稿，跳过 copy |

### 可选

| key | 说明 |
|---|---|
| `out_goods_sn` | 一并写入 rename |
| `goods_desc` / `share_desc` | 一并写入 rename |
| `gallery` / `skus` / `cat_id` / … | **本版不消费**（留给日后从零 edit；copy 已带图/SKU） |
| `extra` / `submit_extra` | 透传进 rename / submit body |

### vars

| key | 默认 | 说明 |
|---|---|---|
| `submit` | `0` | `1`=公开提交；`0`=仅草稿 |
| `source_goods_id` / `source_goods_commit_id` | `""` | args 未给时的兜底 |
| `dup_keyword` | `""` | 非空才查重 |
| `verify_tries` / `verify_gap_ms` | 3 / 4000 | 回读轮询 |
| `mms_home` | `https://mms.pinduoduo.com/home/` | shell 入口 |
| `boot_ms` / `api_ms` / `calm_ms` | 90000 / 60000 / 3000 | 超时与静默窗 |

## 步骤（13）

```
navigate MMS home
→ wait __mms.fetch.sync
→ timer 哨兵门 + calm
→ args_check
→ checkLogin (sync)
→ copyGoodsCommit | resume ids
→ dup_check（可选）
→ update_goods_commit_info 改名
→ commit/detail 回读
→ commit/submit（仅 submit=1）
→ goodsListByGoodsId + on_shop/detail
```

## 传图（诚实账本）

| 能力 | 状态 |
|---|---|
| copy 继承源商品 CDN gallery | **本版主路径**（10-12 草稿 13 图即此） |
| 原生 EQW2 / 选图上传 | 10-10 报告有；**未收编进 flow** |
| `__mms.fetch.upload(FormData)` | SDK 有此方法；签名/COS 链未固化 |
| 调用方自备 CDN URL | 可放 `args.gallery` 备忘；本版步骤不读取 |

## 故意不做

- 不导航 `/goods/goods-list`，不依赖子应用 chunk。
- 不包装 `window.fetch`。
- 不走 `create_new` + `action/edit`（已证失败）。
- 不在 `submit` 失败/超时时自动重打 submit。
- 不把另一店的源商品 id 写死进 vars 默认值。

## 验收建议

1. `submit:0`：`saved.ids.source==='copyGoodsCommit'`，`saved.draft.name` 等于 args.title，`is_shop:0`。
2. 人工核对草稿后，同一 ids `submit:1`。
3. `outcome.verified` 或去后台确认；否则 `pending`——勿重提。

## English

PDD listing via `__mms.fetch.sync` + `copyGoodsCommit` (clone in-shop goods) + `update_goods_commit_info` (rename) + `commit/detail`. Default `submit:0` draft-only. Never wrap `window.fetch`. Async `__mms.fetch.post` hangs on Ut/fbeZ on this engine — do not revert. Gallery/SKUs come from the source clone; upload is out of band.

## vars 合并注意

`/flow/run` 传入的 `vars` 会**覆盖**同名键；未传的键才用 flow.json 默认。
若只传 `args`/`submit`/`api_ms`，请确保 `verify_tries`/`verify_gap_ms`/`calm_ms` 仍有值，或依赖脚本内 `Number('{{x}}'||'默认')` 兜底。
rename body 必带 `cat_id`（detail 优先，其次 `args.cat_id`），**禁止**传 `out_goods_sn`。

## submit 变量遮蔽（已修）

`submit` 步的 `save` 必须叫 `submit_out`，不能叫 `submit`——否则会盖住 `vars.submit`，
后面 verify 里 `String({{submit}})` 变成 `String(skip: …)` 直接 SyntaxError。

## 2026-10-21 回读修正

`goodsListByGoodsId` 的体必须是 `{goods_id, is_onsale:1, page:1, size:10}`（只给 goods_id 会回「参数校验失败: 不能为null」）。
真提交后（`submit:1`）用它回读店铺在售列表，`on_shop/detail` 回读审核态。耗时见回执 `elapsed_ms` / `timings`
（2026-10-21 真发 114.3s：页面启动≈46s、登录检查 30.2s、copyGoodsCommit 31.8s、提交 0.7s、核验 4.7s）。
