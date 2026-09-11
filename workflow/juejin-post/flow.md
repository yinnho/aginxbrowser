# juejin-post — read a juejin article

Post pages (`juejin.cn/post/<id>`) are server-side rendered and render
reliably in diting through the ByteDance WAF (the out-sha256.js challenge
the engine has passed since #148). The default post is a Rust/Bevy intro
article — swap `post_id` for any post.

```
POST /flow/run {"name":"juejin-post"}
POST /flow/run {"name":"juejin-post","vars":{"post_id":"7263464719901458492"}}
```

Verified green: title, author, content length all extracted.

## Selector notes

The article container is `.article-viewer` (`markdown-body` rides along) —
`.article-content`, which older scrapers use, does not exist in the current
DOM. Title is `h1.article-title`, author `.username`.

## Known wall: list/feed pages stall

Tag pages (`/tag/Rust`) and the homepage render their shell but never fire
the `content_api` XHR that fills the feed — the boot chain stalls silently
inside the ByteDance security glue (`bdms.js` + rc-client-security). SSR
metadata comes back degraded too ("0 关注，0 文章"). Post pages don't go
through that path, which is why this flow targets them. Getting the feed
client path unstuck is a separate engine battle, not a flow problem.
