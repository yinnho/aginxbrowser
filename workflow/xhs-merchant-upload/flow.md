# xhs-merchant-upload — 千帆素材库传图

把一批图片放进商家素材库（根目录），回一张 `materials` 映射，直接喂给
`xhs-merchant-create`。**写店操作**：新名字会真上传并登记；库里已有的同名图
一律复用，不重复传。

## 跑法

```
# 登录态：xhs-qianfan-login 出码扫完（账号 xhs-qf-stable），jar 活着即可
POST /flow/run {"name":"xhs-merchant-upload",
                "vars":{"args":{"images":[
                   {"name":"HL25_M01.jpg","content_base64":"…","width":800,"height":800}, …]}}}
```

- `images`：1–36 张，`name` 必填且唯一、以 .jpg/.jpeg/.png 结尾；`content_base64`
  新图必填；`width/height` 缺省时页内解码取；`mime_type` 缺省按扩展名。
- `vars.on_exists`（默认 `reuse`）：库内已有同名图就复用；设别的值=遇到同名即失败。
- `vars.done`：`{名: materialId}` 续跑用，命中直接跳过。
- `vars.expect_seller_id`：填了就核对登录店铺，不符立刻 throw。
- 超过 36 张拆多次跑，已在库里的名字自动复用。

## 回执

`saved.upload.materials[名]` = `{materialId, path, link, size, width, height, extension, uploaderInfoModel}`，
全部来自**素材库回读**（整名精确检索）。素材库的正式 path/link/size 和上传阶段的不一样，
创建商品必须用这份。同名在库里出现 ≠1 次、materialId 与上传回执不符，都会在 `manifest` 步 throw。

## 机制

- 不硬编码模块号：`boot` 步按内容找 `LV`（带原生签名的统一 HTTP 客户端）和 `Yw`
  （页面原版“云端上传 + batch_add_directory_manageable”，约 1.1s/张）。
- 每张图一个 eval 步（`img_0..img_35`，后面接 `at_most` 分支跳 `manifest`），
  丢续体只亏一张；失败不 throw，记在页内，`manifest` 汇总后统一报。
- 前置同 `xhs-merchant-publish`：已在 ark 就不绕首页；`seller/info` 预检不绿直接停，
  不进 create 页（死 jar 硬闯会 401 风暴毒印杀会话）。

## 验证记录

- 2026-10-21 复用路径：36 个现有名字（HL25_*）→ 36 reused / 0 uploaded，回读映射与
  `hl-materials-20261021.json` 逐字段一致（materialId/path/link/size/宽高）。
- 新图上传路径用的是同一个 `Yw` 调用，手工在 2026-10-21 凌晨传过 36 张（哈希全对）；
  本 flow 的上传分支未再对店铺发起新写入，首次真用时请先 1–2 张验。
