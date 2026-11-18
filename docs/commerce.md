# 自然月流量购买

本模块为同一个服务端进程中的已登录账户提供当月流量购买。买家由 GitHub 已验证的数字 subject 确定；浏览器不能指定到账账户、金额、币种或流量字节数。默认税前基础单价为 **人民币 1 元 / GB**，来自用户消息 `message_9d796bba3b6d3fc64ea6c335d5eec9ff`（2026-11-18）；默认关闭收款，可售容量为 0。

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

启用前需要填写实际 merchant、store、一次性 product 标识、收银台 HTTPS origin、与环境对应的密钥和实际月容量，并解决下文的公开购买入口阻塞。产品使用 `saas` 税类。服务端不会转换币种、调整价格或将示例当作真实配置。API base URL 和收银台 origin 都必须是 HTTPS origin，不接受用户信息、查询字符串或片段。

商户私钥只能由服务账户读取，须为该账户拥有的常规单链接 0600 文件。回调公钥来自 Waffo 官方对应环境的公钥，不能对组或其他账户可写。商户 API 密钥本身决定 test/prod 环境；配置和密钥必须匹配。服务端只接受配置环境、公钥及店铺匹配的通知。

测试必须使用独立实例和独立 `portal.state_dir`。test 回调也会向该实例的账户账本写入付费额度，不能与真实用户账本共用。订单账本固定绑定 environment、merchant、store 和 product；启动时拒绝更换绑定，防止测试订单或其他店铺的未完成到账记录混入生产。

月界线由固定 UTC 偏移定义，默认 UTC+08:00。付费订单固定记录购买自然月和月底时间，额度在下月不结转。库存按同一月份统计所有已预留及已付款订单，因此重启不会恢复已售库存。配置月容量低于已有订单总量时，可售量按 0 显示。暂停新销售应将 monthly_sellable_gb 设为 0 并保留 enabled=true，使已有订单仍可接收支付通知；enabled=false 会同时关闭收款通知处理。

独立 `--portal-only` 进程不能开启收款。订单和实际转发扣量必须使用同一 broker 的账户存储；服务端启动时会拒绝创建第二份收款账本。启动恢复会将已核验但尚未完成额度入账的订单交给幂等到账接口重试。

## 价格、税额与币种

截至 2026-11-18，官方一次性产品范围如下。代码在预留库存前检查实际订单基础金额，数量输入也受该范围和本地剩余容量约束；超出范围不会占用库存。

| 计价币种 | 最小基础金额 | 最大基础金额 | 小数位 |
| --- | ---: | ---: | ---: |
| CNY | 1.00 | 1,000.00 | 2 |
| USD | 1.00 | 10,000.00 | 2 |
| EUR | 1.00 | 9,400.00 | 2 |
| GBP | 1.00 | 8,150.00 | 2 |
| JPY | 100 | 1,600,000 | 0 |
| HKD | 8.00 | 77,600.00 | 2 |

CNY 仅适用于一次性产品，付款方式为微信支付。平台另有每买家每自然月通过微信向同一商户最多支付人民币 1,000 元的限制；该渠道限制不等于本站月库存，仍可能导致支付被拒绝。[官方产品与价格限制](https://docs.waffo.ai/features/products)

下单明确发送 `priceSnapshot.amount` 与 `taxIncluded: false`，基础金额来自本地已保存的 GB 数量乘以单价。官方 API 明示此字段默认也为 false；Go SDK 的 `PriceSnapshot` 类型没有列出该可选字段，因此这里按当前 API 文档显式发送。购买页面在付款前显示税前价、计税说明、月底的准确时间和 UTC 偏移。[创建会话的价格快照](https://docs.waffo.ai/api-reference/endpoints/orders/create-checkout-session)

到账采用整数最小货币单位校验全部关系：`listPrice.subtotal == 本地基础金额`、`subtotal + taxAmount == total`、`chargedAmount == total`。存在折扣字段时还要求 `discountAmount == 0` 且 `listTotal == total`。本集成关闭优惠码，不接受订阅按比例抵扣或零金额验卡；旧事件同时缺少两个折扣字段时仍需满足完整基础价与税额等式。缺少实收金额或税前小计不会凭列表价推算到账。核验后的税额和实收额写入订单回执，同一订单重放不能改写它们。[官方回调字段](https://docs.waffo.ai/api-reference/webhooks)

例如，本地基础价 1.00 CNY、已签名通知中的税额 0.10、应付 1.10、实收 1.10 可以到账；实收 1.11、基础价 0.90，或税额与应付不一致均被拒绝。代码没有使用“实收大于等于基础价”的宽松判断。

官方本地货币文档说明，webhook 的金额和币种仍采用产品计价币种；收银台本地呈现币种和实际呈现金额通过 payment 的 `amountDetails` 查询。因此回调必须继续精确匹配本地保存的 CNY 等计价币种，不以银行账单或付款页面的换币数字替代。本站不自行换汇。[官方本地货币规则](https://docs.waffo.ai/features/local-currency)

## 用户入口与支付通知

| 路径 | 行为 |
| --- | --- |
| `GET /account/traffic` | 查看本月额度、可售库存与自己的订单 |
| `POST /account/traffic/orders` | 验证登录、Origin、CSRF、月份确认；按服务端价格预留库存 |
| `GET /account/traffic/orders/{id}` | 只显示该登录账户自己的订单 |
| `POST /account/traffic/orders/{id}/checkout` | 在原订单期限内继续付款，复用网关幂等键 |
| `POST /account/traffic/orders/{id}/reconcile` | 验证所属账户、Origin 与 CSRF，再向网关只读核对取消状态 |
| `POST /payments/waffo/webhook` | 校验原始请求签名并核对到账数据 |

在 Waffo 店铺中将 HTTP webhook 配置为 `https://ssmcp.lmm.best/payments/waffo/webhook`，订阅 `order.completed`，环境须与服务端一致。页面返回、查询参数及客户端自报的成功都不能完成到账。

创建订单先在私有账本中同步预留容量，再请求网关创建收银台会话。订单编号充当 `orderMerchantExternalId` 及创建会话的幂等键，`buyerIdentity` 固定为 `github:<numeric subject>`。金额、币种和流量均来自已保存的订单，用户在收银台修改邮箱不会改变到账账户。后端按官方 authenticated checkout 流程先签发买家 token，再创建会话；含 token 的收银台链接只出现在登录用户的 `Cache-Control: no-store` 页面，链接设置 `noreferrer` 和 `no-referrer`，不落盘、不写日志。收银台语言使用当前官方枚举 `zh-Hans`。

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

已知会话的官方 `expiresAt` 超过 60 秒后，订单页提供“核对已取消订单并释放库存”。服务端用商户签名 GraphQL 按店铺与本地外部订单号读取唯一的 canonical order，并核对商品、环境、币种和价格；只有订单明确为 `canceled`、完整付款集合均为 `failed/canceled`，或该已取消订单确实没有付款记录时，才原子释放库存。付款列表的总数必须与返回集合一致，避免分页隐藏待付或成功记录。提交证明时再次检查本地仍为 Reserved，已核验付款的订单不能被取消对账覆盖。此入口不向支付方发起取消、退款或支付操作。[订单查询](https://docs.waffo.ai/api-reference/endpoints/graphql/orders-and-payments)；[取消的终态定义](https://docs.waffo.ai/api-reference/endpoints/orders/cancel-onetime-order)

无法确认是否已创建远端会话、缺少会话到期时间、查询不到订单、同一业务号存在多个订单、返回字段不足或支付仍在进行时，继续保留当月库存。不能仅凭本地超时、空查询或付款失败释放。每账户每月最多同时有一个 Reserved 订单；证据不足时可能阻止该账户当月再次下单，需要运营联系支付方核对，或等下月购买。这不是自动修复全部支付异常的系统，也没有强制手改库存或“视为已付款”的入口。

订单历史保存于 `<portal.state_dir>/commerce/orders.json`，目录 0700、文件 0600，并持有排他进程锁。该实现最多保存 10,000 笔订单、16 MiB；触达上限会关闭新增订单，运营者需要在停机、核对和备份后归档。这里不保存卡号、结账 token 或原始 webhook。测试用 RSA 私钥仅为本仓库离线测试夹具，与商户密钥无关。

## 公开购买入口是当前生产阻塞

官方明确每个产品拥有永久购买链接，该入口无需本站 session/token，会自动创建结账会话；公开的 Store Slug 也可以创建会话。`authenticated checkout` 证明当前购买者的身份，并不能据此认定平台已禁用其他入口。当前官方公开文档与 Go SDK 未提供可核验的“只允许商户后端创建会话”或“关闭匿名、永久商品链接”的设置。[永久商品链接](https://docs.waffo.ai/features/products)；[创建会话的认证方式](https://docs.waffo.ai/api-reference/endpoints/orders/create-checkout-session)

通过平台公开入口付款可能没有本站订单号、GitHub subject 和库存预留。这样的通知绝不会自动发放额度，但用户已经付款，仍会造成业务问题。因此商品应保持测试环境或未发布；启售前必须向支付方确认并实测能否限制全部购买入口，验证匿名永久链接和 Store Slug 不能绕过本站预留付款。仅不展示链接不能解决这一问题。若平台没有相应能力，应更换可控制销售入口的集成方式，不把直接商品链接作为流量销售入口。

店铺准备提示词见 [store-setup-agent-prompt.zh-CN.md](commerce/store-setup-agent-prompt.zh-CN.md)，协议模板见 [legal/README.md](legal/README.md)。实际商户资料、经营声明、退款条件和用户可访问的协议页面仍需完成后再启售。

## 验证范围与协议依据

定向测试覆盖库存竞争、重复下单、重启后的库存、支付金额/身份/环境不符、增加税额后的精确核验、同一付款重复通知、跨月拒绝结转、确认取消才回收库存、账本环境隔离、存储失败关闭和签名原文篡改。真实本地 HTTP 测试验证登录/Origin/CSRF、账户隔离、回跳无到账权限、签名通知与恢复幂等；页面测试核验 no-store、准确到期时间和计量披露。签名夹具由独立的 OpenSSL 生成，用于比对 Rust 端结果。测试不会调用真实支付、创建真实商品或改动生产配置；真实店铺、沙盒付款及生产验收仍需使用实际商户参数完成。

协议仅依据 Waffo 官方资料。Go SDK 已固定核验到提交 `e73ecf97ab9af07f67d90b4e92133e7fc90db069`（2026-11-16），以下文件的 blob 与该提交 tree 一致：

- [Pancake 文档](https://docs.waffo.ai/)
- [官方 Go SDK 商户请求实现](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/e73ecf97ab9af07f67d90b4e92133e7fc90db069/http_client.go)
- [请求签名原文及 RSA 算法](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/e73ecf97ab9af07f67d90b4e92133e7fc90db069/internal/signing/signature.go)，核验 blob `5f05eec2434708296954c77b843e1f44d9a2c94b`。
- [authenticated checkout 与幂等键](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/e73ecf97ab9af07f67d90b4e92133e7fc90db069/checkout.go)，核验 blob `44e6be32941cbe9c8d6465409fce7546685020f3`。
- [回调签名、公钥、45 分钟容差](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/e73ecf97ab9af07f67d90b4e92133e7fc90db069/webhooks.go)，核验 blob `ab43f9e4da81a54caa5d0e02ff69622091e37e1c`。
- [金额、买家身份、订单关联字段](https://github.com/waffo-com/waffo-pancake-sdk-go/blob/e73ecf97ab9af07f67d90b4e92133e7fc90db069/types.go)，核验 blob `d27df6a9892a2685002e0cf6f54638b2f8d78fea`。

本轮与 main `52df22ee282141e095c2aa17bc53c95d062df80a` 合并后验证，额度核心对应冻结提交 `fe8bf21d202ac5d404eb625a02edbfb72706ecc6`。测试使用独立 `CARGO_TARGET_DIR`，避免并行工作树的同名测试二进制相互覆盖；命令为 `cargo test --offline --locked --lib commerce -- --test-threads=1` 和 `cargo test --offline --locked --bin ssmcpd commerce -- --test-threads=1`，保留全局 rustc wrapper 的串行锁。

当前证据支持本地状态机和协议字段核验；未执行真实商户联调或付款。公开购买入口限制仍是生产阻塞，不能将本地测试通过表述为真实收款已经可用。
