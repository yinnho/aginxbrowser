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

## 规矩（对齐 x/zhihu 台账）

- 发布异步：必须等 verify 的 publish_state 落定才算发出。
- errcode 40001（invalid credential）停手别重试——secret 错或被改。
- **errcode 40164（invalid ip）**：公众号后台「基本配置→IP 白名单」加上本机
  出口 IP 后重跑。无白名单的号首次调用大概率撞这条。
- token 限签发：flow 每跑一遍签一个新 stable_token，v1 接受；7200s 有效期
  的跨跑复用留到真有额度压力再做。
