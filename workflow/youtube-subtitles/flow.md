# youtube-subtitles — 视频元数据 + 字幕轨（正文=pot 墙，原料交付）

`POST /flow/run {"name":"youtube-subtitles","vars":{"url":"https://www.youtube.com/watch?v=…","lang":"en"}}`。

## 前提

youtube.com 在 BLOCKED_DOMAINS，**create 里 use_proxy:true**（session 级
opt-in 全量走代理）——引擎要挂着 `AGINXBROWSER_PROXY`（本机
`socks5h://127.0.0.1:8800`）。出口没被 consent 墙（实测 socks5 8800 直落
watch 页，无 EU consent）。

## 结构（4 步：软探测→门→takeover→read）

```
wait ytInitialPlayerResponse 挂上（12s 到点必真）
branch boot=true → goto read        ← detail 是字符串只能 contains（#50 坑）
takeover: throw（代理死/consent/视频不存在，带 boot 事实）
read: videoDetails 全量 + captionTracks 全表 + 按 lang 挑轨
```

## 字幕正文：四路全墙（2026-10-05 实测）

1. `captionTracks[].baseUrl`（timedtext）：200 **空 body**——pot
   （proof-of-origin）墙，同 IP 下 curl 也空。
2. innertube `player` ANDROID 客户端换 baseUrl：400。
3. innertube `get_transcript`（ytInitialData 里现成 params + 页面
   INNERTUBE_CONTEXT）：400。
4. DOM 转录面板：`[id*="transcript"]` 零命中——polymer 面板未实例化。

处置＝**secfont 哲学**：流交付原料（全元数据 + 轨道表 + baseUrl 原样），
正文解码是另案（pot 供给链是 yt-dlp 的整个战场：bgutil/PO token 那套）。
对 agent 已可用：语言清单/时长/播放数/关键词 + baseUrl 给下游任何解码器。

## 轨道表语义

`kind:null`=人工轨、`kind:"asr"`=自动语音识别轨、`isTranslatable`
=可再翻译。`lang` 挑轨优先人工轨。
