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

## 可选商业功能

接入现有网站时，可以在站点后端根据订阅、付款回执或自己的配额决定是否发送 MCP 请求。支付回调和账本由网站实现，命令中继无需支付密钥。如果希望使用 SSMCP 自带的门户、账户流量和购买流程，选择标准网关配置，并显式启用 `[commerce]`。中继模式下没有内置支付或账户配额管理 API。 [标准配置](../packaging/ssmcpd.toml.example)

## Rust 嵌入

使用 `ssmcp::broker::RelayAccess` 反序列化部署配置，调用 `Broker::headless(public_url, access)` 创建共享 Broker，再用 `ssmcp::http::router(Arc::new(broker))` 取得 Axum 路由。可以把路由合并到宿主程序中，保留单独的中继 origin 和上述代理信任边界。关闭时调用 `Broker::disconnect_all` 终止设备会话。
