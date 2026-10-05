# web-read — 任意网页正文抽取（"Jina Reader 平替"）

`POST /flow/run {"name":"web-read","vars":{"url":"https://…"}}`。

Agent-Reach 的 web 渠道是 `curl https://r.jina.ai/URL`——把你要读的 URL
发给第三方代读。这个流是**引擎本体当 reader**：goto + 抽取，零第三方、
零外发。回执：`title/description/canonical/lang/chars/truncated/text`。

## 结构（2 步）

```
create goto {{url}}
wait readyState complete（20s）
eval 抽取：article>main>[role=main]>body 选容器，clone 后剥
     script/style/nav/footer/aside/form 等噪声，正则规整空白，
     截 max_chars（默认 2 万字），全空就地 throw
```

## 实测

- 阮一峰周刊长文：5000 字截断位正确，正文从作者/日期起干净（zh-CN）。
- 404 页诚实抽取（title="404 Not Found"、118 字）——不猜不装，页面上
  是什么就是什么。

## v1 取舍与墙

- clone 节点脱离布局树后 `innerText` 退化 `textContent` 语义——空白靠
  正则规整，不做布局折叠。表格/代码块的保真度一般，够 agent 读。
- **CF 挑战站过不去**（v2ex 页面实测：导航停在 about:blank）——session
  的引擎指纹被 Cloudflare 挑战；同一站的 API 面走 v2ex-* 的朴素 UA
  http 步能过。页面级要过 CF 得等 stealth 面（另案）。
- SPA 异步内容：按需在本流副本里加 `wait selector`，通用版只等 load。
