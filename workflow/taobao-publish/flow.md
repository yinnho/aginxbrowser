# taobao-publish — 淘宝单品多规格发布（自定义规格名，颜色分类轴）

导航贴膜（类目 201305805）18 个自定义规格名的发布链。**真提交**（`立刻上架`）。先 `taobao-upload` 拿图片地址。

## 关键发现（2026-10-21 摸出来的）

- 该类目 SKU 面板的「单层展示·自定义填写规格」（`custom_-1`）提交一律 `CHK_SKU_JSON_ERROR 销售属性必填`，
  这条路在这个类目走不通。
- **正解**：把规格名当「颜色分类」(`p-1627207`) 的自定义值（上限 56 个）：
  `saleProp = {"p-1627207":[{text, value:<负数>}…]}`、行 `props[0]={name:'p-1627207',text,value,label:'颜色分类'}`、
  `salePropKey='1627207-'+value`、`customSaleProp=[]`，每行带 `skuQuality/skuDetail/skuPicture` 骨架和
  `skuParam_p-660820343=[{value:564110794,text:'清洁工具包'}]`。负数 value 随便给唯一值，服务器会重新分配。
- 服务端校验顺序 品牌→价格/库存→sku→详情。**描述缺失不会挡提交**：页面会拿主图自动补详情。
  所以"缺描述来试 sku 形状"这个办法会真的建出商品（本次误建了一件，见下）。

## 跑法

```
POST /flow/run {"name":"taobao-publish","session_id":"<已登录会话>",
  "vars":{"args":{cat_id,title,brand_id,brand_text,brand_prop_name,shipping_value?,template_id?,spec_name?,
                  main_images[],portrait_images[],desc_images[{url,width,height}],
                  skus[{text,price,stock,img,code,pkg_value,pkg_text,vid?}]},
          "item_param":"&itemId=<id>"}}      # 可选：重开已有商品走编辑模式，不新建
```

`vars.dispatch_value`（默认 `0`=48 小时内发货）。args 前置校验（标题 60 字节、规格名唯一、价格≥0.1、≤600 SKU）在 bind 之前拦。
提交只发一次（replica XHR 抢在页面自带提交器之前）；回执 `outcome` 带 `item_id` / `form_error`。

## 验证记录

- 2026-10-21 真发：昊铂 HL 18 规格，编辑模式 `itemId=1094881528917` 提交成功，重开编辑页回读：标题、18 行规格名与价格
  （16.60–98.80）、总库存 17982、主图 5、详情图 8、品牌无品牌、运费模板偏远地区不包邮 全对。
- 第二轮（编辑模式重提交）补齐：SKU 规格图——`saleProp` 每项和行 `props[0]` 都带 `img`，服务器回读 18/18 有图；
  商家编码——行字段 `skuOuterId`（args `skus[].code`），回读 HL-25-S01…S18 全对；发货时间——表单默认就是「48 小时内发货」
  （`tmDeliveryTime.value` 读回为 null 是默认值的表示，页面单选读回为 48 小时），`vars.dispatch_value` 非默认时才需要改。
- 事故：为试 sku 形状跑了一次缺描述的"实验"，页面自动补详情后真建了一件 2 规格的商品（同一个 itemId），
  随后用编辑模式改成了完整 18 规格，所以店里只有这一件。

## 合并说明（2026-10-21）

本 flow 取代旧的 `taobao-publish` / `-singlesku` / `-multisku` 三条（它们分别是 custom_-1 轴、单规格、预设颜色轴三种形状，
`custom_-1` 轴在该类目已被服务器拒绝）。预设颜色轴的用法并入：`skus[].vid` 给标准颜色值 id（如 白色 28320）就用它，
不给就生成负数自定义值。天猫见 `tmall-publish`（取代 `tmall-publish-singlesku`）。
