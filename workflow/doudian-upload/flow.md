# doudian-upload — 抖店图片上传（API 直传，逐张回执，可断点续传）

`fxg.jinritemai.com` 的图片上传：页内 fetch 直打 `/product/img/batchupload`，
回 CDN URL。不走页面上传 widget（widget 的两处页内坑见文末挖矿记录）。
产出的 URL 正是 `doudian-publish` args 里 `main_images` /
`portrait_images` / 规格图要的形状：**upload 出 URL → publish 吃 URL**，
同一条 `session_id` 串起来就是带图发布。

## Run

```bash
# 1. 登录态：doudian-login 出码扫完拿 session_id（或任何 fxg 登录着的会话）
# 2. 上传（单次最多 36 张，name 必须唯一）：
POST /flow/run {"name":"doudian-upload",
                "vars":{"args":{"images":[
                    {"name":"m01.jpg","content_base64":"...","mime_type":"image/jpeg"},
                    {"name":"d01.jpg","content_base64":"...","mime_type":"image/jpeg"}]}},
                "session_id":"<登录会话>"}
# → saved.upload = {count, total, urls:[...], uploaded_new, skipped_done}
#    saved.img_0..img_N = 每张一张回执 {i, name, url}（或 failed/fatal）
# 3. 发布：doudian-publish 的 main_images = saved.upload.urls
```

**断点续传**（2026-10-10 实测绿）：中途任何一步挂，receipt 带 `failed_step`
和已完成的每张 `img_i` 回执；把已落地的 URL 组成 `{name:url}` 喂回重跑：

```bash
POST /flow/run {"name":"doudian-upload",
                "vars":{"args":{...同一份 images...},
                        "done":{"m01.jpg":"https://p3-aio.ecombdimg.com/..."}},
                "session_id":"<同一会话>"}
# → done 里的整张跳过（0 次上传），只补缺的；urls 顺序仍按 images 数组序
```

## 形状：为什么是每图一步（2026-10-10 的事故换来的）

第一版是 3 块×12 张的 chunk eval。两连挂，死法都一样：**网络层 6 发
batchupload 全部 200 回包，eval 续体一个都没回来**（EVAL_TIMEOUT，120s）。
这是 #240 face-2 的指纹——fxg fresh-load 的 boot 风暴窗口里 fetch 续体
被丢，网络面对拍可证（session network 面 status=200/size 正常，页内
`_fetchRing` 环上无 resume）。加了 doudian-publish 同款 timer 哨兵门 +
6s 续静窗口也挡不住——首页风暴不是一块，是持续翻滚的模块加载。

改成**每张图一个 eval 步**（36 个字面步 `img_0..img_35`，`at_most`
分支卫兵按 `img_count` 跳过超界的）：一发 fetch 一条续体，续体丢了只
损失这一张图 + 45s（`img_ms`），回执永远带着之前所有张的 `img_i` 回执
——这就是报告里 `upload_doudian_yu7.py`「每取得一张素材 URL 立即存
回执」的引擎原生等价物。改完 6/6 绿单 + 全跳过续传绿单。

其他细节：

- **1s 间隔**（`gap_ms`）在每张 eval 开头 sleep——节流对齐报告实测节奏
  （36 张 59s）；跳过的张不产生网络请求但同样 sleep（无害，判定简单）。
- **manifest 只在最后 throw**：缺图/fatal（non-JSON=疑似登录墙）都收进
  一条带续传指引的错误；每步自己的 failed 记录不中断流程（服务器拒某
  一张，下一张照传）。
- bytes 进 eval 用分块 atob（0x8000 一段）——一次性 atob 大图爆栈是老坑。
- `window.__agxUp` 逐张落页内状态，manifest 读它合并（不引用 per-step
  save 名，被卫兵跳过的步不产生 unknown var）。导航会清 window——重跑
  靠 done map，不靠 window。
- eval 预算被引擎钳到 120s 上限（`img_ms` 45s 远在下面）。

## 绿单收据

- 2026-10-10 本机 8129（s_4 会话）：6 张 64×64 测试图全绿，
  `saved.upload.count=6`，每张独立 CDN URL（`p3-aio.ecombdimg.com/obj/
  ecom-shop-material/jpeg_m_...`）；全跳过续传重跑 `uploaded_new=0,
  skipped_done=6, urls 逐字节一致、顺序保持`（receipt-C/D，130s/96s，
  含 boot 哨兵门——全跳过也要付一次风暴门钱，~60-90s）。
- 2026-10-09 本机 8129（旧单 chunk 版）：1×M02.jpg 绿单见 #239。

## 墙与已知边界

- 未登录会话：fetch 会 302 到登录页，该张回 `fatal: non-json` →
  manifest 汇总 throw（不再像旧版一样单张 throw 丢整块进度）。
- `s_v_web_id` cookie 缺失时 query 不带 verifyFp 也照发（服务器是否
  放行未单测；登录会话正常都有这枚 cookie）。
- name 是续传 map 的键：同名不同内容会被当已传跳过。素材文件名保持
  唯一（预检步会拦重名）。

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
（缺省实测 errno:0；页面的 secureProxy 拦截器会自动给请求补
msToken/a_bogus 签名——网络面可见）。FormData 就两个字段：
`image[0]`（File）+ `extra`（`{"request_source":"pc"}`）。
