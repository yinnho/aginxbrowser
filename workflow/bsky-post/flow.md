# bsky-post — Bluesky 发帖 flow（app password → createRecord → verify）

跟 x-reply 同构：会话页上下文 fetch 直调 xrpc（bsky.social CORS 对页面开放，
代理走会话栈）。auth 不落 cookie，不碰 UI。

## 账号与凭证

- 账号 `aginxbrowser.bsky.social`（注册要手机验证，只能人工做一次）。
- 凭证形态是 **app password**（Settings → App passwords 建的专用密码，可随时吊销），
  不是账号主密码。权威文件：`~/Documents/usa-new/bsky-aginxbrowser-login.md`，
  jq 过滤读进命令，值永不回显。
- 会话：`/session/create {url: "https://bsky.app", use_proxy: true, keepalive: true}`，
  id 落 `/tmp/bsky_session.txt`。

## 调用

```
POST /flow/run {"name": "bsky-post", "session_id": "<sid>",
                "vars": {"args_json": "...", "creds_json": "..."}}
```

- `args_json`：`text`（必填，纯文本）、`reply_to`/`reply_to_cid`（父帖 uri+cid，
  续线程用）、`root_uri`/`root_cid`（线程根，第一条回复时 root=parent=根帖）。
  全空 = 根帖。
- `creds_json`：`identifier` + `password`，运行时从文件读，不进 transcript。

## 步骤

1. `login` — createSession（app password），token 只放页面上下文
   `window.__bsky`，saved 输出只有 handle/did，无任何秘密。
2. `post` — createRecord `app.bsky.feed.post`。300 grapheme 上限在引擎里断言
   （有 Intl.Segmenter 用它，没有退 code points）。回 uri+cid（续推要用）。
3. `verify` — getAuthorFeed 顶 10 条按文本头匹配，确认挂线。

## 规矩（对齐 x 台账）

- 发前过 300 grapheme 断言（flow 内置）＋去重台账。
- `401` = identifier/app password 不对，停手别重试；transport 超时 ≠ 失败，
   先 verify 再说。
- bsky 没有字的限频文化，但线程间隔照旧 ≥15 分钟——账号是新的。
