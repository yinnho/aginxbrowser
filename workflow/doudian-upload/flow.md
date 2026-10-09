# doudian-upload — 抖店主图上传（API 直传，绕开 widget）

`fxg.jinritemai.com` 的图片上传：页内 fetch 直打 `/product/img/batchupload`，
回 CDN URL。不走页面上传 widget —— widget 只是这条 XHR 的壳，且壳本身有
坑（见下）。产出的 URL 正是 `doudian-publish` args 里 `main_images` /
`portrait_images` 要的形状：**upload 出 URL → publish 吃 URL**，同一条
`session_id` 串起来就是带图发布。

## Run

```bash
# 1. 登录态：doudian-login 出码扫完拿 session_id（或任何 fxg 登录着的会话）
# 2. 上传：
POST /flow/run {"name":"doudian-upload",
                "vars":{"args_json":"{\"images\":[{\"name\":\"m02.jpg\",\"content_base64\":\"...\",\"mime_type\":\"image/jpeg\"}]}"},
                "session_id":"<登录会话>"}
# → saved.upload.urls = ["https://p3-aio.ecombdimg.com/obj/ecom-shop-material/..."]
# 3. 发布：doudian-publish 的 args_json 里 main_images = 上一步的 urls
```

`images` 每项 `{name, content_base64, mime_type}`；多图按序逐张传，
全部成功才 ok，任一张失败整单 fail 带 receipt（失败的那张、errno、msg）。

## 为什么是 API 直传（widget 挖矿记录，2026-10-09）

新上传 bundle 的 `<input type=file>` 挂在 BODY 下、0×0、无 React fiber，
selector 摸不到；引擎侧 #239 补了 filechooser 拦截面
（`INPUT[type=file].click()` 布防 `__ditingFileChooser`，selector-less
set_files 喂它）之后链路通了，但 widget 自己有两处页内坑：

1. **快喂窗口**：popover 点完「本地上传」后 ~1-2s 内 widget 会拆掉自己的
   change 监听。armed 后 ≤0.35s 喂 → 3/3 全部走到 batchupload；3s/10.9s
   喂 → 静默卡死（无 console、无 rejection，读取全完成但不上传）。
2. **上传成功不回填**：batchupload errno:0、CDN URL 已回，但表单卡片
   永不渲染缩略图（React 状态不合并，`refetchSchema` 回空 `{}`）。

所以 widget 路径即便喂得快，图也进不了表单——而 batchupload 本身不需要
widget：same-origin fetch、cookie 自动带，query 里 `verifyFp`/`fp` 取
cookie `s_v_web_id`，**msToken 和 `x-secsdk-csrf-token` 都不需要**
（缺省实测 errno:0）。FormData 就两个字段：
`image[0]`（File）+ `extra`（`{"request_source":"pc"}`）。

bytes 进 eval 用分块 atob（0x8000 一段）——一次性 atob 大图爆栈是老坑。

## 绿单收据

- 2026-10-09 本机 8129（scan-1029 账号 jar）：`/flow/run doudian-upload`
  1×M02.jpg（460615B）→ `status:ok`，`saved.upload.urls` =
  `https://p3-aio.ecombdimg.com/obj/ecom-shop-material/jpeg_m_b1e12684d4e25a00866cdd2da3adf855_sx_460615_www1254-1254`
  （字节数与源文件一致）；同会话手搓 widget 路 3/3 亦可到 batchupload
  （快喂），收据在 #239。

## 墙与已知边界

- 未登录会话：fetch 会 302 到登录页，eval 里 JSON.parse 失败 → receipt
  报 "non-JSON (login wall?)"，诚实失败。
- `s_v_web_id` cookie 缺失时 query 不带 verifyFp 也照发（服务器是否
  放行未单测；登录会话正常都有这枚 cookie）。
