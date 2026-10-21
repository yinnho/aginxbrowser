# taobao-upload — 淘宝图片空间传图

把图片传进淘宝图片空间，回 alicdn 地址，喂给 `taobao-publish-custom`（或其他淘宝发布 flow）。
**写店操作**：每张图都会在图片空间新增一条，不查重（淘宝允许同名）。

## 跑法

```
# 登录态：taobao-login 扫码（建议给会话挂 account，扫一次能复用）
POST /flow/run {"name":"taobao-upload","session_id":"<已登录会话>",
  "vars":{"args":{"images":[{"name":"HL25T_M01.jpg","content_base64":"…","mime_type":"image/jpeg"}, …]}}}
→ saved.upload.urls = {name: {url, fileId, width, height, size}}
```

- `images` 1–40 张，名字唯一、.jpg/.jpeg/.png；`vars.done`（name→meta）续跑跳过已传的。
- 每张图一个 eval 步（丢续体只亏一张），失败不 throw，`manifest` 汇总后统一报并提示续跑。

## 机制

页面内 `fetch('https://stream-upload.taobao.com/api/upload.api?appkey=tu&folderId=0&_input_charset=utf-8')`，
FormData 的 `file` + `name`，带 `.taobao.com` cookie；回 `{success, object:{url,fileId,pix,size}}`。
发布页里的「上传图片」是素材中心 iframe（`market.m.taobao.com/app/crs-qn/sucai-selector-ng`），
引擎里它渲染成空白，所以直接打它背后的上传接口。前置同 taobao 各 flow（两个 recipe 拦 everyhelp / qn 注入）。

## 验证记录

2026-10-21：36 张昊铂 HL 图（M×5、P×5、D×8、SKU×18）78 步全绿，回读 `pix` 与源图尺寸一致。
