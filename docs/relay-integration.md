# 将 SSMCP 中继接入自己的网站

中继模式提供设备 WebSocket、健康检查和 MCP API。网站保有自己的登录、账户、支付和页面。网站后端完成业务授权后，以对应身份调用 MCP。设备仍在本机执行命令，网关按设备声明的允许名单路由。

## 启动

用两类独立的 32 字节随机凭据。每位控制身份各用一个，设备反向代理另用一个。凭据用 64 位十六进制字符串表示。对**字符串原文**做 SHA-256，写入配置，只把原始凭据交给相应的受信服务。

```sh
token=$(openssl rand -hex 32)
printf '%s' "$token" | sha256sum
```

```toml
[server]
listen = "127.0.0.1:8080"
public_url = "https://relay.example.com"

[relay]
device_ingress_sha256 = "<设备代理凭据的 64 位小写 SHA-256>"

[relay.principals]
alice = "<Alice 的控制凭据的 64 位小写 SHA-256>"
bob = "<Bob 的控制凭据的 64 位小写 SHA-256>"
```

将配置保存为 root 或服务用户所有的 `0600` 文件。用 `ssmcpd serve --config /etc/ssmcpd/relay.toml` 启动。已有的门户、GitHub、OAuth 存储、客户端下载和商业配置属于另一种运行模式，混用会在启动时被拒绝。已有 systemd 部署如设置了 `SSMCP_CLIENT_REGISTRY`，请从此实例的环境中移除该覆盖。

## 将设备接入中继

对外用 HTTPS 反代。将 `/api/client/connect` 仅开放给受运维方信任的设备，例如通过客户端证书或专用 VPN 验证设备身份。代理验证通过后设置固定的 `X-SSMCP-Device-Ingress` 请求头，其值为设备代理凭据原文，同时转发 WebSocket Upgrade。代理必须覆盖客户端送入的同名请求头。保护这一凭据并保持后端监听在 loopback。

受信设备可以声明允许它们的控制身份。将互不信任的设备群放到各自的中继实例和代理信任域。网站控制凭据只用于 MCP，设备代理凭据只用于建立设备连接。

下面是 Nginx HTTPS 虚拟主机内的路由示例。将示例网段替换为设备 VPN 的真实网段。该限制使用 Nginx 观察到的连接源地址。将代理凭据保存在仅代理管理员可读的配置中。

```nginx
location = /api/client/connect {
    allow 10.10.0.0/24;
    deny all;
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header X-SSMCP-Device-Ingress "<设备代理凭据原文>";
}

location = /mcp {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header X-SSMCP-Device-Ingress "";
}
```

```sh
ssmcp alice@relay.example.com --name workstation
```

设备的 `alice` 必须是配置中的身份。多位控制者可沿用客户端的 `--allow` 名单。访问设备目录和发出调用时，网关都校验调用身份与在线设备的允许名单。

## 网站后端调用

| 端点 | 用途 |
| --- | --- |
| `GET /healthz` | 检查网关进程 |
| `GET /api/client/connect` | 托管设备的 WebSocket 握手，仅设备代理使用 |
| `POST /mcp` | 从网站后端或支持预置 Bearer 的 MCP 客户端调用 |

网站后端保存控制凭据，从已验证的网站账户映射到配置的身份。为每个用户选择对应凭据，避免把凭据发给前端或让用户自行提供目标身份。正常完成 MCP 初始化后，可通过 `tools/list` 发现能力，或用下例查询允许访问的在线设备。

```sh
curl --fail-with-body https://relay.example.com/mcp \
  -H "Authorization: Bearer $SSMCP_CONTROL_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list","arguments":{}}}'
```

继续使用 MCP 的 `code_mode`、`await` 等能力。由设备发布的工具 schema 为准，执行结果保留原始任务与错误语义。HTTP 401 表示凭据缺失或无效，跨身份不可见的设备不会出现在目录中。配置在启动时加载，变更身份或轮换凭据需要重启中继，设备会重新连接。 [Code Mode](code-mode.md) · [设备路由](device-control-plane.md)

## 外部账户和月流量账单

网站负责登录、确认买家、收款和持久保存订单。中继按实际控制身份分别累计上下行字节，以两者中的较大值加旧格式历史用量计入当月账单。网站后端可读取该月余额，并为已核验的订单发放流量包。

可在现有 `[relay]` 配置中增加下表。网站运营方从身份数据库核对每个账户的固定数字 ID，将其绑定到同名的已配置 external principal。CLI 沿用 `ssmcp alice@api.lmm.best`。更改显示名不会自动改变到账账户。每个中继最多保留 4096 个绑定；删除活跃配置后，原名称及数字 ID 仍保留给原绑定。新增用户按相同规则登记，重启中继加载配置。

```toml
[relay.accounting]
issuer = "https://api.lmm.best"
state_dir = "/var/lib/ssmcpd/lmm-accounting"
service_token_sha256 = "<专用账务凭据原文的 SHA-256>"
utc_offset_minutes = 480
free_monthly_bytes = 1000000000
paid_monthly_bytes = 1000000000

[relay.accounting.users]
alice = "123"
bob = "456"
```

`users` 必须覆盖此中继的全部 principals，一个名称对应一个数字 ID。专用账务凭据为随机的 64 位小写十六进制字符串，与控制、设备凭据分别生成。状态目录专供本套账务使用，属主为服务用户，权限为 0700；已建立的目录固定绑定 issuer 和月界 UTC 偏移。标准网关的本地收款沿用独立的 `[commerce]` 配置。

### 后端接口

两个内部 POST 路由使用中继的 HTTP 监听器，接收 `Content-Type: application/json` 和 `Authorization: Bearer <账务凭据>`。同机服务可通过 loopback 直接访问，跨主机服务使用受限的私有 HTTPS 入口。公网反代仅开放客户端实际需要的路径，账务凭据留在网站后端。

| 路径 | 请求 | 成功响应 |
| --- | --- | --- |
| `/internal/v1/ssmcp/quota` | `{"user_id":"123"}` | 返回该固定账户的月用量、当月可用额度和下次恢复时间 |
| `/internal/v1/ssmcp/entitlements` | `{"user_id":"123","order_id":"purchase-26","period":"2026-11","bytes":"1000000000"}` | 返回原四个字段及 `status`。首次持久到账为 `granted`，相同订单的重放为 `replayed` |

月用量响应示例：

```json
{
  "user_id":"123","username":"alice","period":"2026-11","utc_offset_minutes":480,
  "reset_at":1796054400,"free_bytes":"1000000000","paid_bytes":"1000000000",
  "used_bytes":"30","uploaded_bytes":"8","downloaded_bytes":"30","legacy_used_bytes":"0",
  "limit_bytes":"2000000000","remaining_bytes":"1999999970"
}
```

字节数量和用户 ID 均为规范十进制字符串，`reset_at` 是下月开始时的 Unix 秒。服务端按自身时钟取自然月，已到账流量包随该月结束。首次有效购买进入持久的付费档位，之后的月份按 `paid_monthly_bytes` 发放赠送额。

网站后端将受信订单的固定用户 ID、月、流量和唯一订单号一起持久保存，再请求到账。订单号长 1–128 个字符，允许 ASCII 字母、数字、连字符、下划线和点。超时或返回 503 时保留原订单并用原数据重试。相同订单跨月重放仍可核对原回执；首次跨月发放返回 409 `period_mismatch`，交由网站按该订单处理退款或人工核对。订单号相同而字段不同返回 409 `order_conflict`。字节数和订单数达到存储边界返回 409 `ledger_limit`。401 表示服务凭据无效，404 表示账户未绑定或接口未启用，400 表示请求格式无效。此协议负责发放已确认的当月权益；退款与扣回需要网站单独处理。

## Rust 嵌入

使用 `ssmcp::broker::RelayAccess` 反序列化部署配置，调用 `Broker::headless(public_url, access)` 创建共享 Broker，再用 `ssmcp::http::router(Arc::new(broker))` 取得 Axum 路由。可以把路由合并到宿主程序中，保留单独的中继 origin 和上述代理信任边界。关闭时调用 `Broker::disconnect_all` 终止设备会话。
