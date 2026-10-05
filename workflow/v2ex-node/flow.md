# v2ex-node — 节点最新帖

`POST /flow/run {"name":"v2ex-node","vars":{"node":"jobs"}}`。

机制、代理/朴素 UA 两前提见 [v2ex-hot/flow.md](../v2ex-hot/flow.md)。
每条：id/title/url/content/replies/node.member/created。
公开 API 无搜索端点（他们渠道也是拿 Jina 兜的）——要搜索走
web-read 或 Exa 类，别硬找。
