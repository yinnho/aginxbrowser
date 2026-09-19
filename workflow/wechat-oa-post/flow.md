# wechat-oa-post — 微信公众号发文 flow（http step 版：token → 封面 → draft → freepublish → 验证）

接口形状来自 aginx-carrier 的 git 历史（commit `5e4f6db` 引入 `crates/wechat-oa`，
`7df4e15` 于 2026-08-22 整族剥离——`git show 7df4e15^:crates/wechat-oa/src/api.rs`
可整份找回）。

## 路线：引擎原生 http step（路线 B，已落地）

api.weixin.qq.com **不开 CORS**，页面上下文 fetch 直调必被拦。早期版本用
navigate 同源引导绕（secret 进一次 navigate URL，落会话 network log）。现在
flow 执行器有原生 `op:"http"`（引擎侧 Rust 发请求）：天然无 CORS、凭据不进
页面上下文、不落任何会话 network log。secret 只存在于运行时 vars 里，
flow.json 本身零秘密（CHANGE_ME 桩）。

http step 契约（详见 API.md）：
- `json` 参数走**整叶插值**：`"{{args.content_html}}"` 把带引号带标签的 HTML
  作为合法 JSON 字符串嵌入——正文里出现多少引号都不会破坏请求体。
- 其余参数（url/headers/body/multipart）走文本替换。
- `save` 的结果后续步骤用 dotted 路径引用：`{{token.json.access_token}}`。
- 每步独立无 cookie client，骑 /fetch 同款安全姿态（配额门+SSRF deny-set+
  env 代理），重定向不跟（3xx 原样透出 status）。

## API 面（api.weixin.qq.com，公众号自持凭据）

| 步骤 | 端点 | 要点 |
|---|---|---|
| token | `POST /cgi-bin/stable_token` | appid+secret；stable_token 不互踩 |
| 封面 | `POST /cgi-bin/material/add_material?type=image` | multipart；freepublish **无封面必拒** |
| 草稿 | `POST /cgi-bin/draft/add` | articles[0] 带 thumb_media_id |
| 删草稿 | `POST /cgi-bin/draft/delete` | 端点是 **delete 不是 del**——`draft/del` 一律 40066 invalid url（2026-09-20 实测 json body/query 参/form 三种姿势全拒，换 delete 立即成功） |
| 发布 | `POST /cgi-bin/freepublish/submit` | media_id → publish_id，异步 |
| 验证 | `POST /cgi-bin/freepublish/get` | publish_state 0 成功 2/3/4 失败 |

## 步骤（5 步全 http，无页面依赖）

1. `token` — stable_token，creds 从 vars 来（见下）。失败信号：下一步的
   `{{token.json.access_token}}` dotted 引用 miss → flow 在该步报
   unknown var 并停，回执里 `saved.token.body` 带 errcode/errmsg 供诊断。
2. `cover` — multipart 上传封面（cover_b64），拿 thumb_media_id。
3. `draft` — draft/add，articles[0] 全字段；content 走整叶插值。
4. `submit` — freepublish/submit 拿 publish_id。
5. `verify` — freepublish/get 查一次 publish_state。**发布是异步的**，
   submit 成功 ≠ 发出。flow 引擎刻意不带循环：state=1（发布中）时由调用方
   重跑这个单步（inline flow 或再调一次）直到 0/2/3/4 落定——修复循环属于
   调用方，引擎保持 LLM-free。

失败都 fail-loudly：missing var、multipart 空部件、响应体超 8MB 帽，都会带
回执停在该步。回执 `saved` 里有每步响应原文（**含 access_token——读回执时
用 jq 过滤字段，别整段回显**）。

## 凭据

- `vars.creds` = `{app_id, app_secret}` **对象**，运行时从本地文件 jq 读入：
  ```
  jq -c '{app_id, app_secret}' ~/Documents/usa-new/wechat-aginxos-oa.json
  ```
  值不进 transcript、不进 flow.json（那文件会 commit）。
- 与 oa-gateway（oa.aginx.net，第三方平台 component 模式）无关——那是授权
  回调/消息推送网关，不管发文。

## 调用

```
POST /flow/run {"name": "wechat-oa-post",
                "vars": {"args_json": "...", "creds": {...}}}
```

- `args_json`：`title`、`content_html`（公众号正文 HTML）、`digest`（可空=
  微信自动截）、`cover_b64`（封面 PNG base64，**必填**——无封面兜底是调用方
  的事：先 batchget_material 拿素材库最新图再传 thumb，v1 未自动化）。
- `creds`：对象形式直传（见上）。

## 模板库（templates/）

正文 HTML 不手写，`md_to_args.py` 按模板拼装出 `args_json`：

```
python3 md_to_args.py article.md --title "标题" --digest 摘要 \
    --cover cover.png > args.json
# 再: jq -c '{args_json}' --rawfile args_json args.json 裹一层喂 vars
```

- 模板 = `templates/default.html`：`<template id="...">` 切块
  （container/header/h2/p/strong/link），槽位 `{{text}}`/`{{blocks}}`/`{{header}}`。
  改样式只动模板，converter 零改动；新模板 `--template` 指过去。
- md 支持面刻���小：`# 标题` 跳过、`## 小节`→h2、段落→p、`**粗体**`→strong、
  裸 URL/域名→上色 span。
- 生存规则见模板头注释：只信 inline style；外链不上 `<a>`（订阅号被剥），
  用 `<span style="color:#2563eb">` 上色。
- 2026-09-20 全链实测：模板草稿 draft/get 回读，h2 蓝条/链接色/字体栈全存活，
  `<a href` 为零（按设计）。

## 规矩（对齐 x/zhihu 台账）

- 发布异步：必须等 verify 的 publish_state 落定才算发出。
- errcode 40001（invalid credential）停手别重试——secret 错或被改。
- **errcode 40164（invalid ip）**：公众号后台「基本配置→IP 白名单」加上本机
  出口 IP 后重跑。无白名单的号首次调用大概率撞这条。白名单改动可能要管理员
  扫码确认才真生效；改完仍 40164 先去「查看」里核对名单里实际有哪几条。
  出口不固定（家庭宽带）时退路=走已有白名单 IP 的自有服务器做出口：
  `ssh -D <port> -N <server>` + 实例起时带 `AGINXBROWSER_PROXY=socks5h://
  127.0.0.1:<port>`（http step 骑 env 代理，2026-09-20 实测全链路通）。
- **errcode 48001（api unauthorized）**：freepublish 只给**已认证**号。未认证
  订阅号永远撞这条——flow 前 3 步（token/封面/草稿）照样有价值：草稿进后台
  草稿箱，最后一步人去 mp.weixin.qq.com 手动点发布（AginxOS 号 2026-09-20
  实测即此路径）。
- 正文里的外链 `<a href>` 会被微信剥掉（订阅号规则），链接文字保留为纯文本；
  建草稿前不用自己剥，但别指望读者能点。
- token 限签发：flow 每跑一遍签一个新 stable_token，v1 接受；7200s 有效期
  的跨跑复用留到真有额度压力再做。
