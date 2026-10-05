# v2ex-topic — 帖子详情 + 回复

`POST /flow/run {"name":"v2ex-topic","vars":{"topic_id":1000000}}`。

机制、代理/朴素 UA 两前提见 [v2ex-hot/flow.md](../v2ex-hot/flow.md)。
两条 http 步：`topics/show.json?id=`（单 id 查询也返回数组，取 [0]）+
`replies/show.json?topic_id=`（每页 100 条，要更多自己加 page 变体）。
复合（比如把回复并进主题对象）留给调用方——流只送原料。
