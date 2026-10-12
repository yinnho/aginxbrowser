# xhs-merchant-publish — 千帆/Ark 商品发布（骨架）

> **STUB (2026-10-12).** 不提交商品。只保证：bare create → `preload recipe: xhs-sign` →
> navigate → 探测 `_webmsxyw`。有 signer 落到 `saved.stub=STUB_OK`；没有则 throw，
> 避免再打裸 `publish_render` / `batch_create_item` 吃 406。

## 已知坑（来自 2026-10-12 探测）

- Ark 页经常 **不起** `_webmsxyw`；旧 xhs-sign 吞 `x-s` 会导致必 406 —— 已在 recipe
  里改成 signer 缺失时 passthrough（`__xs_passthrough`）。
- `batch_create_item` 还可能回 `300015`（浏览器运行环境异常）或店 `deactivated`。
- 登录：用 `workflow/xhs-login` 或 `import_curl`；cookie 过期会 `-13004`。

## 下一步（真正上架）

1. 把 `publish_url` 换成真实发布页，等到 `has_signer:true`。
2. 对照 Chrome HAR 补 `publish_render` → `batch_create_item`（body 来自摩卡报告）。
3. 店状态 / 风控与签名分开处理。

## 用法

```bash
# 已有商家会话
curl -sS -X POST "$BASE/flow/run" -H 'content-type: application/json' \
  -d '{"name":"xhs-merchant-publish","session_id":"s_N"}'
```
