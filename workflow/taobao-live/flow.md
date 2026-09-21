# taobao-live — 淘宝直播首页流（verdict 门版）

live.taobao.com 首页探测流：落地就抽房间链接，撞墙（punish/滑块/登录）
就把 fact sheet 原样带回接管回执。批 175（2026-09-22）加 verdict + branch
（issue #72/#73），替代老流「撞墙死在 rooms 抽取步、报错只有选择器空」。

## 结构（6 步，线性 + 一条前跳）

```
wait readyState/title → verdict→v → front 遥测(两路都存)
  → branch v.verdict not_in [challenge, captcha, login, empty] → read
takeover: throw（回执带 fact sheet + /live?session=<id> 接管指引）
read: 抽 a[href*=live.taobao.com] 房间链接 → rooms
```

branch 用补集跳过 takeover（线性 pc 会走到末尾，throw 步必须被跳过才
有活路）——与 xhs-post 末尾的补集 branch 同一铁律。

## 真机 receipt（2026-09-22，/tmp/b175-taobao.json）

干净路径：`status: ok`、verdict=`landed`、branch 命中跳过 takeover、
elapsed 1ms（verdict 读缓存事实，零额外请求）。`rooms: []`——首页是
JS 壳，房间列表靠水合后 XHR 拉，本引擎里水合死（旧账，批 169 live 页
本机化那批记过），verdict 只看网络事实所以照样 landed：**符合设计**。
流交付的是「没撞墙 + 现场遥测」，房间数据要等引擎水合面修好或走
`/session/:id/network?filter=media` 手动抽。

## 下一步钩子

`read` 步的 note 写了路线：navigate 进房间页 → `GET /session/:id/network?filter=media`
——播放器的真流 URL 只在房间页跑起来之后出现。
