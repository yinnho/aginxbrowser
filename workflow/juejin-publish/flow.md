# juejin-publish — 掘金发文

`juejin-post` 只读文章页。这篇是写：已登录会话里
`article_draft/create` 然后 `article/publish`。2026-09-25 在账号
yinnho 上走过，`err_no` 为 0，文章页能打开。

掘金走本机 IP。`create.use_proxy` 是 false。不要给这个会话套代理。

## 登录

Chrome 在 `juejin.cn` 已登录的页面上 Copy as cURL，再：

```
POST /import/curl   {"curl": "<那份 cURL>", "use_proxy": false, "account": "juejin"}
```

会话必须停在 `juejin.cn`。相对路径 `/content_api/...` 只在这个源上带得上登录。

## 调用

```
POST /flow/run {"name": "juejin-publish", "session_id": "<sid>",
                "vars": {"args_json": "..."}}
```

`args_json`：

- `title`、`mark_content`（Markdown）必填。
- `brief_content` 可空，空则用标题。
- `category_id` 默认人工智能 `6809637773935378440`。
- `tag_ids` 默认「浏览器」「人工智能」。

默认 `args_json` 的标题是 `CHANGE_ME`。跑到这一步会直接拒绝，不建草稿。

## 步骤

1. 打开 `https://juejin.cn/editor/drafts/new`。
2. `who` — `user/get`。`err_no` 不是 0 就停。回执里只有用户名。
3. `post` — 建草稿再发布。回执给 `draft_id`、`article_id`、`url`。

发布接口成功后，公开页有时先显示找不到。以回执里的 `article_id` 为准，过几秒再打开
`https://juejin.cn/post/<article_id>`。

## 不在这里做的

- 不读文章。读用 `juejin-post`。
- 不发 V2EX。2026-08-30 的 API 2.0 文档没有创建主题的接口。
- 不发 Hacker News。没有发帖接口，自动发 Show HN 会伤账号。
