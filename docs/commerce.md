# 自然月流量购买

本模块为同一个服务端进程中的已登录账户提供当月流量购买。买家由 GitHub 已验证的数字 subject 确定；浏览器不能指定到账账户、金额、币种或流量字节数。默认单价为 **人民币 1 元 / GB**，来自用户消息 `message_9d796bba3b6d3fc64ea6c335d5eec9ff`（2026-11-18）；默认关闭收款，可售容量为 0。

## 配置

将以下段落放入已有服务端 TOML。价格以最小货币单位表示：CNY 的 100 分等于 1 元。1 GB 按 1,000,000,000 字节计；每月容量是所有付费订单共享的容量，不包含免费额度。

```toml
[commerce]
enabled = false
gateway = "waffo_pancake"
environment = "test"
api_base_url = "https://api.waffo.ai"
checkout_origin = ""
merchant_id = ""
store_id = ""
product_id = ""
currency = "CNY"
currency_decimals = 2
price_per_gb_minor = 100
monthly_sellable_gb = 0
max_gb_per_order = 100
utc_offset_minutes = 480
# private_key_file = "/absolute/private/pancake-merchant.pem"
# webhook_public_key_file = "/absolute/private/pancake-webhook-test.pem"
```

启用前需要填写实际 merchant、store、一次性 product 标识、收银台 HTTPS origin、与环境对应的密钥和实际月容量。产品使用 `saas` 税类。网关不支持的币种或价格会导致创建付款失败；服务端不会转换币种、调整价格或将示例当作真实配置。API base URL 和收银台 origin 都必须是 HTTPS origin，不接受用户信息、查询字符串或片段。

商户私钥只能由服务账户读取，须为该账户拥有的常规单链接 0600 文件。回调公钥来自 Waffo 官方对应环境的公钥，不能对组或其他账户可写。商户 API 密钥本身决定 test/prod 环境；配置和密钥必须匹配。服务端只接受配置环境、公钥及店铺匹配的通知。

月界线由固定 UTC 偏移定义，默认 UTC+08:00。付费订单固定记录购买自然月和月底时间，额度在下月不结转。库存按同一月份统计所有已预留及已付款订单，因此重启不会恢复已售库存。配置月容量低于已有订单总量时，可售量按 0 显示。暂停新销售应将 monthly_sellable_gb 设为 0 并保留 enabled=true，使已有订单仍可接收支付通知；enabled=false 会同时关闭收款通知处理。

独立 `--portal-only` 进程不能开启收款。订单和实际转发扣量必须使用同一 broker 的账户存储；服务端启动时会拒绝创建第二份收款账本。启动恢复会将已核验但尚未完成额度入账的订单交给幂等到账接口重试。

## 用户入口与支付通知

| 路径 | 行为 |
| --- | --- |
| `GET /account/traffic` | 查看本月额度、可售库存与自己的订单 |
| `POST /account/traffic/orders` | 验证登录、Origin、CSRF、月份确认；按服务端价格预留库存 |
| `GET /account/traffic/orders/{id}` | 只显示该登录账户自己的订单 |
| `POST /account/traffic/orders/{id}/checkout` | 在原订单期限内继续付款，复用网关幂等键 |
| `POST /payments/waffo/webhook` | 校验原始请求签名并核对到账数据 |

在 Waffo 店铺中将 HTTP webhook 配置为 `https://ssmcp.lmm.best/payments/waffo/webhook`，订阅 `order.completed`，环境须与服务端一致。页面返回、查询参数及客户端自报的成功都不能完成到账。

创建订单先在私有账本中同步预留容量，再请求网关创建收银台会话。订单编号充当 `orderMerchantExternalId` 及创建会话的幂等键，`buyerIdentity` 固定为 `github:<numeric subject>`。金额、币种和流量均来自已保存的订单，用户在收银台修改邮箱不会改变到账账户。

服务端使用项目已有 rustls/ring 与 webpki 完成 RSA PKCS#1 v1.5 SHA-256。商户请求的签名原文为：

```text
POST
API_PATH
UNIX_SECONDS
BASE64(SHA256(EXACT_REQUEST_BODY))
```

回调签名头为 `t=<Unix毫秒>,v1=<Base64签名>`，签名原文是时间戳文本、一个点、未经改写的请求体。默认允许过去 45 分钟及未来 1 分钟，与官方 SDK 的重试容差一致；重复签名字段被拒绝。验签之后还要匹配店铺、环境、外部订单号、买家身份、已成功支付状态、实际扣款额、订单价格及币种。

两份持久账本通过幂等订单号完成恢复：订单先记为已核验付款，再将当月字节数写入额度账本，最后标记完成。中间进程退出时，启动恢复或同一通知重试不会再次增加额度。数据库写入或目录同步失败后，当前进程停止继续修改订单，避免用过期内存状态确认付款。

## 运行边界

首次有效支付通知跨过自然月到达时，订单标为“跨月到账，待联系管理员核对退款”，不增加下月额度。该状态需要运营者核对并退款，代码不会自动执行退款。月末最后 16 分钟不新建订单，已有付款入口最多在原 15 分钟窗口内继续打开。

无法确认是否已创建远端付款会话的订单会保留当月库存，不会仅凭本地超时释放。异步支付可能在会话失效后才完成，提前归还容量会导致超卖。目前每账户每月最多同时有一个未完成订单；这限制重复占位，但不是完整的反滥用或自动对账系统。未付款占位需要运营核对，或等到下月自然恢复可售容量。

订单历史保存于 `<portal.state_dir>/commerce/orders.json`，目录 0700、文件 0600，并持有排他进程锁。该实现最多保存 10,000 笔订单、16 MiB；触达上限会关闭新增订单，运营者需要在停机、核对和备份后归档。这里不保存卡号、结账 token 或原始 webhook。测试用 RSA 私钥仅为本仓库离线测试夹具，与商户密钥无关。

## 验证范围与协议依据

定向测试覆盖库存竞争、重复下单、重启后的库存、支付金额/身份/环境不符、同一付款重复通知、跨月拒绝结转、存储失败关闭和签名原文篡改。签名夹具由独立的 OpenSSL 生成，用于比对 Rust 端结果。测试不会调用真实支付、创建真实商品或改动生产配置；真实店铺、沙盒付款及生产验收仍需使用实际商户参数完成。

协议仅依据 Waffo 官方资料：

- [Pancake 文档](https://docs.waffo.ai/)
- [官方 Go SDK 商户请求实现](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/main/http_client.go)
- [请求签名原文及 RSA 算法](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/main/internal/signing/signature.go)，核验 blob `5f05eec2434708296954c77b843e1f44d9a2c94b`。
- [authenticated checkout 与幂等键](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/main/checkout.go)，核验 blob `44e6be32941cbe9c8d6465409fce7546685020f3`。
- [回调签名、公钥、45 分钟容差](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/main/webhooks.go)，核验 blob `ab43f9e4da81a54caa5d0e02ff69622091e37e1c`。
- [金额、买家身份、订单关联字段](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/main/types.go)，核验 blob `d27df6a9892a2685002e0cf6f54638b2f8d78fea`。

当前证据支持本地状态机和接口核验；真实收款是否可用仍取决于实际商户配置和端到端验收。
