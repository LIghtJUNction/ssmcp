# 显式认证 MCP peer

旧启动选项迁移见[原生工具与显式 MCP peer](optional-harnesses.md)。

## 连接另一个 ssmcp 的 MCP 网关

`--load-harness ssmcp` 接受 peer 公布的完整 MCP URL，例如 `https://peer.example/alice`。URL 指向该账号的网关；具体设备和线程需要从 peer 的目录中选择，再使用实际 ID 路由。接入完成后，本机目录以 `peer.` 前缀发布远端实际返回的工具，并保留其 schema、返回内容和错误。[peer 实现](../src/backend/peer.rs)

认证使用 peer 的 MCP OAuth 流程签发的 bearer。`--token-env` 指定保存 bearer 的环境变量名；命令行、会话记录和诊断中不保存 token 值。以下 Bash 示例从终端读入 token，避免将值写进命令历史：

```bash
read -r -s -p 'Peer OAuth bearer: ' SSMCP_PEER_TOKEN
export SSMCP_PEER_TOKEN
printf '\n'

ssmcp local --load-harness ssmcp -- \
  --endpoint https://peer.example/alice \
  --token-env SSMCP_PEER_TOKEN

unset SSMCP_PEER_TOKEN
```

HTTPS 使用项目既有的证书信任配置。明文 HTTP 仅接受字面 loopback IP；URL 中拒绝用户名密码、query 和 fragment。请求保持目标 URL 的 Host 和 Origin，经过 peer 原有的 OAuth 与授权检查；重定向被拒绝，bearer 留在明确指定的目标上。

Unix socket 传输使用本地 listener 实际显示的 HTTP URL。若 peer 以 `ssmcp local --socket` 开启 MCP socket，可在同一用户下连接它：

```sh
ssmcp local --load-harness ssmcp -- \
  --endpoint http://127.0.0.1:43123/alice \
  --token-env SSMCP_PEER_TOKEN \
  --socket /absolute/private-directory/mcp.sock
```

socket 和父目录必须由当前用户拥有且保持私有；路径禁止符号链接。`--socket` 与 HTTPS URL 会报错，以保留 TLS 的真实含义。socket 只改变连接传输，Host、Origin 和 bearer 仍对应给定 URL。

peer 连接不启动另一个 ssmcp 进程，也不拥有远端进程。客户端会拒绝本次连接自己公布的完整 MCP URL；同一主机的不同账号路径仍可明确选择。此检查针对直接自连接，不替代远端账号权限，也不为任意跨网关调用链做自动选路。关闭本机连接会停止其拥有的请求，peer 网关继续运行。

## 错误与验证范围

provider 缺失、原生握手失败、认证拒绝或目录无效时，启动返回具体错误；前台连接继续遵循已有 watchdog 策略。可使用 `--list-tools` 单独检查所选协议，也可添加 `--no-watchdog` 让连接失败后退出。

peer 使用 JSON-response MCP，协商 2025 系列协议。单个请求最多 1 MiB、单个响应最多 2 MiB；目录最多 64 页、4,096 个工具、合计 8 MiB。初始化和通知各最多等待 10 秒，单页目录 15 秒、整份目录 75 秒；显式工具调用最多等待 900 秒。超时返回错误，不将 ACK、空响应或伪造的本地返回值当作远端工具结果。

回归检查覆盖保存会话、私有 HTTP/Unix 服务、真实 schema 和返回内容，以及认证、重复游标、重定向、超时和关闭取消。[协议回归](../src/backend/peer.rs)
