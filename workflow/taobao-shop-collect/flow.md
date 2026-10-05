# taobao-shop-collect — 采集别人公开店铺在售商品（买家侧列表页）

买家视角的店内搜索列表页（`shop<N>.taobao.com/search.htm?search=y`）整页
采集：60 张卡的 itemId/标题/链接/主图/sold/利益点/SKU + **secfont 价格
token 和 shadow cps**。价格本身是字体混淆的，流不负责解码（见下文离线
伴侣），流负责把原料原样带回。

## 结构（8 步，线性 + 两条前跳，throw 步必须被跳过才有活路）

```
wait 软探测(shadowRoot 存在 AND 卡数≥min_cards)→boot
  → branch boot 干净 → goto attach
  → reboot: 重导航同 URL（boot 轮盘）
  → boot2 再探一轮
  → branch boot2 干净 → goto attach；脏 → 落进 takeover
takeover: throw（回执带两轮 boot 事实 + /live 接管指引）
attach: 软等 shadow 挂满（40s 封顶，等不满也落地继续）
read: fiber 上溯抽 itemCardData + shadow cps → cards
      （对象返回；脚本内嵌数据门：extracted<min_cards 就地 throw）
```

`shop_url` / `min_cards` 都是 vars，run 时可覆盖：
`POST /flow/run {"name":"taobao-shop-collect","vars":{"shop_url":"..."}}`。

## 页面三层坑（真机四轮跑踩全了）

1. **boot 轮盘**：同 URL 首跳可能落在店铺首页/半 hydration 态（30 张卡），
   重导航同 URL 常起完整列表（60 张）。reboot 步就是干这个的。
2. **punish 毒数据**：mtop containerfacade 被 `_____tmd_____/punish
   action=captcha` 拦时，列表骨架照样渲染、shadowRoot 照样挂，但卡数永远
   停在 30 张 SSR。只认 secfont 会把毒数据当绿——所以软探测加了
   `min_cards`（默认 40；干净页 1 是 60，店铺在售 ~110）。
3. **首页和列表同 URL 同 title**（都是「首页-店名-淘宝网」），标题/URL
   一律分不了页态，只能靠卡数。

## 为什么没有 verdict 门（重要踩坑）

- verdict 数的是**会话累计**挑战行：reboot 后当前页明明干净（60 卡全量、
  抽取 60/60 全对），第一跳的 punish 痕迹还在，照样报 challenge——
  s_10 实测假阳性。
- DOM 探针（punish/baxia iframe）也不行：iframe 在水合完的干净页上**常驻
  挂载**，s_9（毒 30 卡）和 s_10（净 60 卡）探到的是同一个 iframe。
- 这个页族的墙**只表现为数据缺席**（30 卡、mtop 数据到不了）。所以数据
  本身就是门：boot 卡数门 + read 内嵌 extracted 门，两道都是数据级判据。

## 引擎侧三个坑

- `wait` 超时是 **fail 整流**不是软失败——软探测靠「到点必真」谓词：
  到 15s 返回 `{secfont:false, cards:n}` 让 wait 正常落 matched，分支再
  决定去哪。`performance.now()` 从文档 nav 起算，reboot 后预算自动重置。
- `wait` 落的 `detail` 是**字符串**（谓词返回值被 JSON.stringify 过），
  `boot.detail.secfont` 这种 dotted path 查不进去，branch 只能
  `contains "secfont":true`（紧凑子串）。
- `eval` 步如果返回 `JSON.stringify(...)` 字符串，save 存的就是字符串，
  后面 dotted path 永远查不进——**read 必须直接返回对象**（绿路径根本
  绿不了的静音雷）。

## 价格解码（离线伴侣，不进流）

- 每张卡带 `priceToken`（`[1_<family>#51#<b64>#]`，token 前缀=字体家族
  id）和 `cps`（shadow DOM 里的码点序列）。**只要 token 就够**：wasm
  `wa.bin` + `b(0)/b(1)` 两段把 token 解回 cps，与 shadow cps 完全一致
  （两天两批 120/120 验证）。
- 字体从 `https://g.alicdn.com/secdev/secfont/1.0.0/<family>/0.bin` 取，
  fontTools 读轮廓、按 hmtx advance 累积平移合成、列投影切组
  （h<26=小数点）、保纵横比归一 40×64、模板最近相关分类。
- **合成必须按轮廓符号面积分黑白**（外环黑、counter 白洞后画）——每个
  轮廓无脑填黑会把 0 的内环填死，0 全线误判成 8（跨天对拍 3 处「改价」
  全是这个 bug，Chrome nonzero winding 是地面真值）。
- 反 OCR 机制：碎片星座字形，单码点无意义、按 pen 位置叠加才成数字；
  码点每会话多态（同价不同码点），但合成形状聚类稳定。
- 解码工具是 /tmp 里的 python+node 原型（fontTools/PIL/WebAssembly），
  依赖重，不进流；流交付 token+cps，谁要明文价谁离线跑。

## 真机 receipt

- run1（s_8）：boot 轮盘（30 卡无 secfont，软探测正确返回），死于 branch
  语法——detail 字符串坑。已修。
- run2（s_9）：reboot 生效、shadowRoot 挂上，但页面被 punish 卡 30 卡，
  verdict=challenge 接管回执——墙路径验证。
- run3（s_10）：boot2 出 **60 卡 secfont:true**，死于 verdict 会话累计
  假阳性；同会话手工抽取 60/60、itemId 集合与干净会话完全一致——证明
  该页数据是好的，verdict 门设计撤除。
- run5（s_16，v3 首**真机绿**）：boot 首轮即净（60 卡 6.8s），抽取 60/60，
  但 cps 只带了一半——read 跑得比页面渐进挂载快（页面的 1s 轮询逐个挂
  shadow root，挂满要几十秒到分钟级）。
- run6（s_17）：把挂载塞进 boot 门（att≥n 才绿）——15s 等不到挂满，绿页
  被误判成墙。教训：**挂载等待和墙判定必须拆开**（墙看卡数，挂载软等）。
- run7（s_18）：图写反的亲踩反例——branch 条件写成「脏→goto takeover」，
  干净 boot2（60 卡）直落 throw 被扔进接管。补集铁律的正解是**正向条件**
  「干净→goto」，throw 只能被脏路径直落。
- run8（s_19，**满血绿**）：轮盘中（30 卡）→reboot→boot2 净 60 卡→attach
  5.8s 挂满 60/60→read 60/60。字段覆盖：cps 60/60、token 60/60、sold
  60/60、img 60/60、skus 59/60（一张无 SKU 是单品规格卡，合法）。全程
  ~25s。
- 本地 mock 绿跑（inline flow + set_content + eval 造 45 假卡）：绿路径机制
  链预验通过（探针→branch 前跳→fiber 抽取→对象回执）。
- 绿回执：`saved.cards` = {url, title, n, extracted, cards[60]}，约 200KB，
  回执无截断。
