# taobao-orders — 买家「已买到的宝贝」全量订单采集（PC 微应用版）

登录买家账号后，把 `buyertrade.taobao.com/trade/itemlist/list_bought_items.htm`
的**全部历史订单**（真机实测 949 单 / 35 页 / 2010→2026）采回来。这是
PC 侧的 React 微应用页面，不是普通 SSR 页——整条流的骨架就是「替页面
把它自己的 App 手动 boot 起来，让真 App 自己发签名 mtop，我们在网络层
偷听响应体」。

## 结构（7 步，线性 + 一条回跳）

```
goto list_url → wait shell(#tbpc-trade-container) → boot(偷听器+找包+注入)
  → wait first_page(偷听到第一包) → paginate(fiber onChange 翻页) ⟲ loop
  → collect(解析全部响应体 → orders)
```

`loop`：`paged.grew == true` 就回跳 paginate，翻干（hasMore 尽）自然落
collect。`max_steps: 800` 兜底（35 页实际用 ~70 步预算；这是**硬顶不是
保险丝**——每页约花 2 步，800 ≈ 397 页 ≈ 万二单，更大的账户会被预算
判死且 collect 不执行、零数据，先估账户规模再调）。

## 页面层四个大坑（这个页族的墙全在「App 起不来」上）

1. **tbnav loader 在 diting 里不干活**：SSR shell 里根本没有 App 的
   script 标签；页面靠 tbnav 导航器按 `window.pcMyTaobaoMenuList` 的
   `urlList`（`loadScriptMode:"fetch"`）去拉 `tbpc-bought-list/js/main.js`
   注入——这一步在引擎里**从不发生**，12s 后 doc 级 watchdog 亮
   「Oops！页面出错了」。eval/fetch/new Function 本身都好使，不是原语
   被封，就是那个 loader 逻辑没跑。boot 步就是替它干这活：从
   pcMyTaobaoMenuList 里**现解析** main.js/main.css 地址（版本号 0.0.x
   会漂，绝不硬编码），fetch 回来 `<script>` textContent 注入。
2. **ice.js 路由 basename="/"**：注入后 App 的浏览器路由拿全路径
   `/trade/itemlist/list_bought_items.htm` 去匹配，报 `No route matches`
   404。注入**前**先 `history.pushState(null,'','/')`，路由就命中 `/`。
   （注：一旦注入过 404 状态会残留，重跑前先重导航。boot 里有
   `__booted` 哨兵，同页重跑幂等。）
3. **手调 lib.mtop.request 会被静默吞**：页面上下文里手动调
   `mtop.taobao.order.queryboughtlistv2`（baxa 签名接口）promise 永不
   settle，eval 超时、window 标记停在 unsettled——和店铺页一个死法。
   所以**让真 App 自己发**（它带自己的 baxia 签名），我们只在 fetch/XHR
   原型上挂偷听器收响应体。这是整个流的思路支点。
4. **antd 翻页器合成 click 翻到第 2 页就冻**：`.click()` 下一页按钮只
   灵一次，之后 32 次应答全是同一页 30 单（字节差异只在时间戳）；跳页
   输入框是受控组件，set value + Enter 全不触发。活路是 fiber 上溯：
   `.ant-pagination` 节点的 `__reactFiber$` 键 → `fiber.return` 爬
   （onChange 在 hop 1）→ 直接调 `memoizedProps.onChange(page, 30)`。
   **目标页码从响应数据读**（`pageControl.nextPageIndex`）——DOM 的
   active item 撒谎（run3 实测人在第 4 页它还显示 1）。

## 登录态：必须传 session_id

flow create 块不支持账号 jar（引擎缺口，值得开单）：fresh flow 会话骑
DEFAULT jar（我们这台机器的 DEFAULT jar 带着商家登录，进买家页会被 302
到 myseller）。所以登录态用**外置会话**组合：

```
POST /account/login {"name":"taobao-personal","url":"https://buyertrade.taobao.com/..."}
  → 有门（QR）→ /live?session=<id> 人扫 → 再调一次 /account/login 闭环
POST /flow/run {"name":"taobao-orders","session_id":"<登录会话>","max_steps":800}
```

会话 TTL 1800s，过期就重调 /account/login（同名复用 jar，不用重扫）。

## 响应体格式（collect 的解析对象）

- 偷听器收的是 JSONP 文本：`mtopjsonpN({...})`，剥壳再 JSON.parse。
  剥壳按容错写：允许 `/**/` 防劫持前缀、尾随 `;`、点号回调名
  （`a.b.mtopjsonpN`）、空白——阿里系网关哪天换壳不至于当场死。
- 数据在 `data.data`：`shopInfo_<orderId>.fields`（createTime/shopName/
  sellerNick）和 `orderItemInfo_<orderId>_<sub>.fields.item`（title/itemId/
  quantity/priceInfo.actualTotalFee/skuText/itemUrl）两族孤岛，按 orderId
  折叠成单条订单；跨页重复（重翻）天然去重。
- 翻页循环的判据是**数据级**的：新 orderId 数 + `pageControl.hasMore`。
  光看「偷听器不再长」不行——App 翻过末页后仍会继续应答（给的却是
  重复页），run2 就是这么烧穿 400 步预算的。反过来 hasMore 单独也不
  够，两条件一起看。

## 采集纪律（35 页真机的教训）

- **每页落盘**：live HAR 的 network log 窗口会滚，35 页翻完头两页已被
  挤丢。流本身把全部响应体收在 `window.__bodies` 再一次性 collect，
  回执 `saved.orders` 自带全量——落盘从回执做，别依赖 /har。真机对比：
  手动 HAR 采集 919 单，flow 采集 **949 单**（多出的 30 单就是窗口滚动
  丢的），919 ⊂ 949 零缺失。
- 回执 `saved.orders` = {pages, orders[]}，949 单约 500KB，无截断。
- `paid`（实付）只有近年订单带 `priceInfo.actualTotalFee`（352/949），
  早年订单结构不同——比价场景够用，全量金额场景要另想。
- **挂账两条**（真机 5 轮没踩到、但代码审阅确认存在）：① awaitBody 拿
  的是 fire 之后**新到的最后一包**——如果 App 在慢速后台 refetch 旧页，
  可能拿到 stale dup 造成误判提前停（真机未见，遇莫名早停先怀疑这条）；
  ② 全量数据困在 `window.__bodies`，collect 前一页都不落盘——中途死
  （预算判死/会话过期）就是零数据。改增量落盘是结构改动，先靠
  max_steps=800 顶住。

## 真机 receipt（5 轮到绿）

- run1：boot/翻页机制全对（60 单两页），死于 branch——PAGE 脚本
  `return JSON.stringify(...)` 存成字符串，`paged.grew` dotted path 进
  不去。就是 taobao-shop-collect 教训第 3 条，自己又踩一遍。**eval 步
  一律裸返对象。**
- run2：branch 通了，步预算 400 烧穿——**App 翻过末页还会继续应答**
  （hasMore 明明 true 但给的是重复页），「响应体变多」不能当翻页成功
  的判据。改数据级判据：新 orderId 数 + `pageControl.hasMore`。
- run3：判据改对，但第 2 页后误停——两个新发现：①antd 的 active item
  **撒谎**（人已经在第 4 页，DOM active 还是 1），当前页码只能从响应
  数据里读（`pageControl.nextPageIndex` 就是下一次该要的页）；②连发
  onChange 会拿到重复页，翻页间隔需要 ~1.5s 落定。
- run4：919/919 全覆盖判绿，但 collect 出 1898 单——`'shopInfo_'.length`
  是 9，`slice(8)` 差一，orderId 全带下划线前缀，店铺信息和商品信息
  折进了两个不同的 key。`orderItemInfo_` 是 14，那个 slice 是对的。
- run5（**满血绿**）：35 页 73 步，949 单 shop+item 全折叠完整，
  2010-07-24 → 2026-04-01，末页 hasMore=false 自然停。
