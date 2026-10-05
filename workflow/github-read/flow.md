# github-read — 仓库元数据 + issue 列表（免认证公开读）

`POST /flow/run {"name":"github-read","vars":{"repo":"Panniantong/Agent-Reach"}}`。

对标 Agent-Reach 的 github 渠道（装 gh CLI + 配 token）——公开读这条路
两条裸 http 就够，免安装免认证。回执 `meta`（star/fork/语言/描述/默认
分支/最近推送）+ `issues`（最近更新的 issue，含 total_count）。

## 结构（4 步：探针→门→fail→读）

```
http repos/:repo            → save meta
branch meta.status==200     → goto issues（正向跳过 throw，#50 铁律）
eval throw                  ← 404（repo 名错/私有仓）或 403（限流）带原始 body 落这
http search/issues type:issue → save issues
```

- 用 **search/issues 而非 repos/:r/issues**：后者把 PR 混进 issue 列表
  （github API 已知行为），search 加 `type:issue` 干净还白送 total_count。
- 纯 http 流也自带空白会话（回执里有 session_id），eval 步照常可用。
- 限流：未认证 60 req/h/IP（meta 走 repos 限额、issues 走 search 10
  req/min）；403 会在 fail 步带 body 呈现。
- 404 路径实测：`reason: github-read: repo … http=404 body={"message":"Not Found"…}`。

## egress

api.github.com 本机可直连；引擎挂了 `AGINXBROWSER_PROXY` 时 http 步走代理
（也通）。无 CF 问题，朴素 UA 只是家族姿势统一。
